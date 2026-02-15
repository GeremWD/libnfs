//! # libnfs — True-async Rust bindings for libnfs (tokio-native)
//!
//! This crate integrates libnfs's callback-based async API with
//! tokio's reactor via [`AsyncFd`]. There are **no blocking threads**
//! — the NFS socket is polled directly by tokio, and each operation
//! completes through a C callback → oneshot channel bridge.
//!
//! ## Quick start — read a file over NFSv4
//!
//! ```rust,no_run
//! use libnfs::{NfsClient, NfsVersion};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), libnfs::Error> {
//!     let client = NfsClient::mount("192.168.1.10", "/export", NfsVersion::V4).await?;
//!     let data = client.read_file("/path/to/file.txt").await?;
//!     println!("{}", String::from_utf8_lossy(&data));
//!     Ok(())
//! }
//! ```

mod event_loop;
pub mod sys;

use std::ffi::{CStr, CString};
use std::os::raw::{c_int, c_void};
use std::sync::Arc;

use tokio::sync::oneshot;

// Re-export the event loop handle and command type.
pub(crate) use event_loop::{Command, NfsEventLoop};

// ── Error type ──────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to create NFS context")]
    ContextCreation,

    #[error("NFS error (code {code}): {message}")]
    Nfs { code: i32, message: String },

    #[error("failed to submit async operation: {0}")]
    Submit(String),

    #[error("invalid string argument: {0}")]
    InvalidString(#[from] std::ffi::NulError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("operation cancelled")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, Error>;

// ── NFS version ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NfsVersion {
    V3,
    V4,
}

impl NfsVersion {
    fn as_raw(self) -> c_int {
        match self {
            NfsVersion::V3 => sys::NFS_V3,
            NfsVersion::V4 => sys::NFS_V4,
        }
    }
}

// ── Stat result ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct FileStat {
    pub dev: u64,
    pub ino: u64,
    pub mode: u64,
    pub nlink: u64,
    pub uid: u64,
    pub gid: u64,
    pub rdev: u64,
    pub size: u64,
    pub blksize: u64,
    pub blocks: u64,
    pub atime: u64,
    pub atime_nsec: u64,
    pub mtime: u64,
    pub mtime_nsec: u64,
    pub ctime: u64,
    pub ctime_nsec: u64,
    pub used: u64,
}

impl From<&sys::nfs_stat_64> for FileStat {
    fn from(st: &sys::nfs_stat_64) -> Self {
        Self {
            dev: st.nfs_dev,
            ino: st.nfs_ino,
            mode: st.nfs_mode,
            nlink: st.nfs_nlink,
            uid: st.nfs_uid,
            gid: st.nfs_gid,
            rdev: st.nfs_rdev,
            size: st.nfs_size,
            blksize: st.nfs_blksize,
            blocks: st.nfs_blocks,
            atime: st.nfs_atime,
            atime_nsec: st.nfs_atime_nsec,
            mtime: st.nfs_mtime,
            mtime_nsec: st.nfs_mtime_nsec,
            ctime: st.nfs_ctime,
            ctime_nsec: st.nfs_ctime_nsec,
            used: st.nfs_used,
        }
    }
}

// ── Callback bridge ─────────────────────────────────────────────────────────

// We use a fixed set of concrete callback functions, one per result-type family.

// ── Concrete callback: result = () (mount, close, unlink, mkdir, etc.) ──

struct VoidBridge {
    tx: Option<oneshot::Sender<std::result::Result<(), (i32, String)>>>,
}

unsafe extern "C" fn void_cb(
    err: c_int,
    nfs: *mut sys::nfs_context,
    _data: *mut c_void,
    private_data: *mut c_void,
) {
    let mut bridge = Box::from_raw(private_data as *mut VoidBridge);
    let result = if err < 0 {
        let msg = if nfs.is_null() {
            String::from("unknown error")
        } else {
            let p = sys::nfs_get_error(nfs);
            if p.is_null() {
                String::from("unknown error")
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        Err((err as i32, msg))
    } else {
        Ok(())
    };
    if let Some(tx) = bridge.tx.take() {
        let _ = tx.send(result);
    }
}

// ── Concrete callback: result = *mut nfsfh (open) ───────────────────────

struct FhBridge {
    tx: Option<oneshot::Sender<std::result::Result<FhPtr, (i32, String)>>>,
}

unsafe extern "C" fn fh_cb(
    err: c_int,
    nfs: *mut sys::nfs_context,
    data: *mut c_void,
    private_data: *mut c_void,
) {
    let mut bridge = Box::from_raw(private_data as *mut FhBridge);
    let result = if err < 0 {
        let msg = crate::get_error_string(nfs);
        Err((err as i32, msg))
    } else {
        Ok(FhPtr(data as *mut sys::nfsfh))
    };
    if let Some(tx) = bridge.tx.take() {
        let _ = tx.send(result);
    }
}

// ── Concrete callback: result = nfs_stat_64 (stat64) ────────────────────

struct StatBridge {
    tx: Option<oneshot::Sender<std::result::Result<sys::nfs_stat_64, (i32, String)>>>,
}

unsafe extern "C" fn stat_cb(
    err: c_int,
    nfs: *mut sys::nfs_context,
    data: *mut c_void,
    private_data: *mut c_void,
) {
    let mut bridge = Box::from_raw(private_data as *mut StatBridge);
    let result = if err < 0 {
        let msg = crate::get_error_string(nfs);
        Err((err as i32, msg))
    } else {
        let st = &*(data as *const sys::nfs_stat_64);
        Ok(st.clone())
    };
    if let Some(tx) = bridge.tx.take() {
        let _ = tx.send(result);
    }
}

// ── Concrete callback: result = i32 (read — returns bytes read) ─────────

struct ReadBridge {
    tx: Option<oneshot::Sender<std::result::Result<Vec<u8>, (i32, String)>>>,
}

unsafe extern "C" fn read_cb(
    err: c_int,
    nfs: *mut sys::nfs_context,
    data: *mut c_void,
    private_data: *mut c_void,
) {
    let mut bridge = Box::from_raw(private_data as *mut ReadBridge);
    let result = if err < 0 {
        let msg = crate::get_error_string(nfs);
        Err((err as i32, msg))
    } else {
        // For read, `err` is the number of bytes read on success.
        // `data` points to the read buffer provided by libnfs.
        let bytes_read = err as usize;
        let slice = std::slice::from_raw_parts(data as *const u8, bytes_read);
        Ok(slice.to_vec())
    };
    if let Some(tx) = bridge.tx.take() {
        let _ = tx.send(result);
    }
}

// ── Concrete callback: result = *mut nfsdir (opendir) ───────────────────

struct DirBridge {
    tx: Option<oneshot::Sender<std::result::Result<DirPtr, (i32, String)>>>,
}

unsafe extern "C" fn dir_cb(
    err: c_int,
    nfs: *mut sys::nfs_context,
    data: *mut c_void,
    private_data: *mut c_void,
) {
    let mut bridge = Box::from_raw(private_data as *mut DirBridge);
    let result = if err < 0 {
        let msg = crate::get_error_string(nfs);
        Err((err as i32, msg))
    } else {
        Ok(DirPtr(data as *mut sys::nfsdir))
    };
    if let Some(tx) = bridge.tx.take() {
        let _ = tx.send(result);
    }
}

// ── Helper ──────────────────────────────────────────────────────────────────

fn get_error_string(nfs: *mut sys::nfs_context) -> String {
    unsafe {
        if nfs.is_null() {
            return String::from("unknown error");
        }
        let p = sys::nfs_get_error(nfs);
        if p.is_null() {
            String::from("unknown error")
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

fn bridge_err(r: std::result::Result<(), (i32, String)>) -> Result<()> {
    r.map_err(|(code, message)| Error::Nfs { code, message })
}

fn bridge_result<T>(r: std::result::Result<T, (i32, String)>) -> Result<T> {
    r.map_err(|(code, message)| Error::Nfs { code, message })
}

// ── NfsClient ───────────────────────────────────────────────────────────────

/// A true-async NFS client connected to one server/export.
///
/// Internally owns the `nfs_context` and an event-loop task that
/// drives libnfs I/O on the tokio reactor.
///
/// All public methods are `async` and non-blocking.
pub struct NfsClient {
    inner: Arc<NfsInner>,
}

/// Shared interior — the raw context plus a handle to the event loop.
struct NfsInner {
    /// Raw libnfs context pointer.  After construction, this is only
    /// accessed from the event-loop task (via [`Command`] closures)
    /// and in [`Drop`].  Never touched directly from client methods.
    ctx: *mut sys::nfs_context,
    /// Handle to the event-loop background task.
    event_loop: NfsEventLoop,
}

// Safety: All post-construction access to nfs_context is serialised
// through the event-loop task via the command channel.  The only
// other access is in Drop, which aborts the event-loop task first.
unsafe impl Send for NfsInner {}
unsafe impl Sync for NfsInner {}

// ── Send wrappers for raw pointers used in Command closures ─────────────

/// Send wrapper for `*mut sys::nfsfh` (file handle pointer).
#[derive(Clone, Copy)]
struct FhPtr(*mut sys::nfsfh);
unsafe impl Send for FhPtr {}
impl FhPtr {
    fn get(self) -> *mut sys::nfsfh {
        self.0
    }
}

/// Send wrapper for `*mut sys::nfsdir` (directory handle pointer).
#[derive(Clone, Copy)]
struct DirPtr(*mut sys::nfsdir);
unsafe impl Send for DirPtr {}
impl DirPtr {
    fn get(self) -> *mut sys::nfsdir {
        self.0
    }
}

impl Drop for NfsInner {
    fn drop(&mut self) {
        // 1. Signal the event loop to stop.
        self.event_loop.stop();

        // 2. Abort the task so nfs_service() is no longer called.
        //    After abort(), the task is cancelled at its next yield point
        //    and will not touch the nfs_context again.
        if let Some(task) = self.event_loop.take_task() {
            task.abort();
        }

        // 3. Small yield to let the runtime process the abort.
        //    Use a thread::sleep rather than async — we're in Drop.
        std::thread::sleep(std::time::Duration::from_millis(1));

        // 4. Now safe to destroy the context.
        if !self.ctx.is_null() {
            unsafe { sys::nfs_destroy_context(self.ctx) };
        }
    }
}

impl NfsClient {
    /// Connect to an NFS server, mount the export, and return a client
    /// ready for async file operations.
    ///
    /// ```rust,no_run
    /// # use libnfs::{NfsClient, NfsVersion};
    /// # async fn run() -> libnfs::Result<()> {
    /// let client = NfsClient::mount("10.0.0.1", "/", NfsVersion::V4).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn mount(server: &str, export: &str, version: NfsVersion) -> Result<Self> {
        let ctx = unsafe { sys::nfs_init_context() };
        if ctx.is_null() {
            return Err(Error::ContextCreation);
        }
        unsafe { sys::nfs_set_version(ctx, version.as_raw()) };

        // Submit the async mount.
        let server_c = CString::new(server)?;
        let export_c = CString::new(export)?;
        let (tx, rx) = oneshot::channel();

        let bridge = Box::new(VoidBridge { tx: Some(tx) });
        let private_data = Box::into_raw(bridge) as *mut c_void;

        let rc = unsafe {
            sys::nfs_mount_async(
                ctx,
                server_c.as_ptr(),
                export_c.as_ptr(),
                void_cb,
                private_data,
            )
        };
        if rc != 0 {
            // Reclaim the bridge so it doesn't leak.
            unsafe { drop(Box::from_raw(private_data as *mut VoidBridge)) };
            let msg = get_error_string(ctx);
            unsafe { sys::nfs_destroy_context(ctx) };
            return Err(Error::Submit(msg));
        }

        // Start the event loop to drive this mount to completion.
        let event_loop = NfsEventLoop::start(ctx)?;

        let result = rx.await.map_err(|_| Error::Cancelled)?;
        bridge_err(result)?;

        let inner = Arc::new(NfsInner { ctx, event_loop });
        Ok(Self { inner })
    }

    /// Connect with custom UID/GID.
    pub async fn mount_as(
        server: &str,
        export: &str,
        version: NfsVersion,
        uid: u32,
        gid: u32,
    ) -> Result<Self> {
        let ctx = unsafe { sys::nfs_init_context() };
        if ctx.is_null() {
            return Err(Error::ContextCreation);
        }
        unsafe {
            sys::nfs_set_version(ctx, version.as_raw());
            sys::nfs_set_uid(ctx, uid as c_int);
            sys::nfs_set_gid(ctx, gid as c_int);
        };

        let server_c = CString::new(server)?;
        let export_c = CString::new(export)?;
        let (tx, rx) = oneshot::channel();

        let bridge = Box::new(VoidBridge { tx: Some(tx) });
        let private_data = Box::into_raw(bridge) as *mut c_void;

        let rc = unsafe {
            sys::nfs_mount_async(
                ctx,
                server_c.as_ptr(),
                export_c.as_ptr(),
                void_cb,
                private_data,
            )
        };
        if rc != 0 {
            unsafe { drop(Box::from_raw(private_data as *mut VoidBridge)) };
            let msg = get_error_string(ctx);
            unsafe { sys::nfs_destroy_context(ctx) };
            return Err(Error::Submit(msg));
        }

        let event_loop = NfsEventLoop::start(ctx)?;

        let result = rx.await.map_err(|_| Error::Cancelled)?;
        bridge_err(result)?;

        let inner = Arc::new(NfsInner { ctx, event_loop });
        Ok(Self { inner })
    }

    // ── Helpers ─────────────────────────────────────────────────────────

    /// Submit a command closure to run on the event-loop task with
    /// exclusive access to the `nfs_context`.
    fn submit(&self, cmd: Command) -> Result<()> {
        self.inner.event_loop.submit(cmd)
    }

    // ── Stat ────────────────────────────────────────────────────────────

    /// Stat a file by path.
    pub async fn stat(&self, path: &str) -> Result<FileStat> {
        let path_c = CString::new(path)?;
        let (tx, rx) = oneshot::channel();

        self.submit(Box::new(move |ctx| {
            let bridge = Box::new(StatBridge { tx: Some(tx) });
            let pd = Box::into_raw(bridge) as *mut c_void;
            let rc = unsafe { sys::nfs_stat64_async(ctx, path_c.as_ptr(), stat_cb, pd) };
            if rc != 0 {
                let mut bridge = unsafe { Box::from_raw(pd as *mut StatBridge) };
                let msg = get_error_string(ctx);
                if let Some(tx) = bridge.tx.take() {
                    let _ = tx.send(Err((-1, msg)));
                }
            }
        }))?;

        let st = bridge_result(rx.await.map_err(|_| Error::Cancelled)?)?;
        Ok(FileStat::from(&st))
    }

    // ── Open / Close ────────────────────────────────────────────────────

    /// Open a file for reading.
    pub async fn open(&self, path: &str) -> Result<NfsFile> {
        let path_c = CString::new(path)?;
        let (tx, rx) = oneshot::channel();

        self.submit(Box::new(move |ctx| {
            let bridge = Box::new(FhBridge { tx: Some(tx) });
            let pd = Box::into_raw(bridge) as *mut c_void;
            let rc = unsafe { sys::nfs_open_async(ctx, path_c.as_ptr(), sys::O_RDONLY, fh_cb, pd) };
            if rc != 0 {
                let mut bridge = unsafe { Box::from_raw(pd as *mut FhBridge) };
                let msg = get_error_string(ctx);
                if let Some(tx) = bridge.tx.take() {
                    let _ = tx.send(Err((-1, msg)));
                }
            }
        }))?;

        let fh = bridge_result(rx.await.map_err(|_| Error::Cancelled)?)?;
        Ok(NfsFile {
            client: self.inner.clone(),
            fh: fh.get(),
        })
    }

    // ── High-level: read entire file ────────────────────────────────────

    /// Read the full contents of a file and return them as `Vec<u8>`.
    ///
    /// ```rust,no_run
    /// # use libnfs::{NfsClient, NfsVersion};
    /// # async fn run() -> libnfs::Result<()> {
    /// let client = NfsClient::mount("10.0.0.1", "/", NfsVersion::V4).await?;
    /// let data = client.read_file("/share/hello.txt").await?;
    /// println!("{}", String::from_utf8_lossy(&data));
    /// # Ok(())
    /// # }
    /// ```
    pub async fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        let st = self.stat(path).await?;
        let size = st.size as usize;

        let file = self.open(path).await?;

        let mut result = Vec::with_capacity(size);
        let mut offset: u64 = 0;

        while (offset as usize) < size {
            let remaining = size - offset as usize;
            let chunk = file.pread(remaining as u64, offset).await?;
            if chunk.is_empty() {
                break;
            }
            offset += chunk.len() as u64;
            result.extend_from_slice(&chunk);
        }

        file.close().await?;
        Ok(result)
    }

    /// Convenience: read a file and return it as a UTF-8 `String`.
    pub async fn read_file_to_string(&self, path: &str) -> Result<String> {
        let bytes = self.read_file(path).await?;
        String::from_utf8(bytes)
            .map_err(|e| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
    }

    // ── Directory listing ───────────────────────────────────────────────

    /// List entries in a directory.
    pub async fn readdir(&self, path: &str) -> Result<Vec<String>> {
        let path_c = CString::new(path)?;
        let (tx, rx) = oneshot::channel();

        self.submit(Box::new(move |ctx| {
            let bridge = Box::new(DirBridge { tx: Some(tx) });
            let pd = Box::into_raw(bridge) as *mut c_void;
            let rc = unsafe { sys::nfs_opendir_async(ctx, path_c.as_ptr(), dir_cb, pd) };
            if rc != 0 {
                let mut bridge = unsafe { Box::from_raw(pd as *mut DirBridge) };
                let msg = get_error_string(ctx);
                if let Some(tx) = bridge.tx.take() {
                    let _ = tx.send(Err((-1, msg)));
                }
            }
        }))?;

        let dir = bridge_result(rx.await.map_err(|_| Error::Cancelled)?)?;

        // nfs_readdir / nfs_closedir also access nfs_context, so run
        // them on the event-loop task too.
        let (entries_tx, entries_rx) = oneshot::channel();

        self.submit(Box::new(move |ctx| {
            let mut entries = Vec::new();
            loop {
                let ent = unsafe { sys::nfs_readdir(ctx, dir.get()) };
                if ent.is_null() {
                    break;
                }
                let name = unsafe { CStr::from_ptr((*ent).name) }
                    .to_string_lossy()
                    .into_owned();
                entries.push(name);
            }
            unsafe { sys::nfs_closedir(ctx, dir.get()) };
            let _ = entries_tx.send(entries);
        }))?;

        entries_rx.await.map_err(|_| Error::Cancelled)
    }
}

// ── NfsFile ─────────────────────────────────────────────────────────────────

/// An open file handle on an NFS share.
///
/// All reads/writes are truly async — they submit libnfs async operations
/// and await completion through the tokio reactor.
pub struct NfsFile {
    client: Arc<NfsInner>,
    fh: *mut sys::nfsfh,
}

unsafe impl Send for NfsFile {}
unsafe impl Sync for NfsFile {}

impl NfsFile {
    /// Submit a command closure to run on the event-loop task.
    fn submit(&self, cmd: Command) -> Result<()> {
        self.client.event_loop.submit(cmd)
    }

    /// Read up to `count` bytes at `offset`. Returns the data read.
    pub async fn pread(&self, count: u64, offset: u64) -> Result<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        let fh = FhPtr(self.fh);

        self.submit(Box::new(move |ctx| {
            let bridge = Box::new(ReadBridge { tx: Some(tx) });
            let pd = Box::into_raw(bridge) as *mut c_void;
            let rc = unsafe { sys::nfs_pread_async(ctx, fh.get(), offset, count, read_cb, pd) };
            if rc != 0 {
                let mut bridge = unsafe { Box::from_raw(pd as *mut ReadBridge) };
                let msg = get_error_string(ctx);
                if let Some(tx) = bridge.tx.take() {
                    let _ = tx.send(Err((-1, msg)));
                }
            }
        }))?;

        let data = bridge_result(rx.await.map_err(|_| Error::Cancelled)?)?;
        Ok(data)
    }

    /// Sequential read of up to `count` bytes at the current offset.
    pub async fn read(&self, count: u64) -> Result<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        let fh = FhPtr(self.fh);

        self.submit(Box::new(move |ctx| {
            let bridge = Box::new(ReadBridge { tx: Some(tx) });
            let pd = Box::into_raw(bridge) as *mut c_void;
            let rc = unsafe { sys::nfs_read_async(ctx, fh.get(), count, read_cb, pd) };
            if rc != 0 {
                let mut bridge = unsafe { Box::from_raw(pd as *mut ReadBridge) };
                let msg = get_error_string(ctx);
                if let Some(tx) = bridge.tx.take() {
                    let _ = tx.send(Err((-1, msg)));
                }
            }
        }))?;

        let data = bridge_result(rx.await.map_err(|_| Error::Cancelled)?)?;
        Ok(data)
    }

    /// Stat the open file.
    pub async fn fstat(&self) -> Result<FileStat> {
        let (tx, rx) = oneshot::channel();
        let fh = FhPtr(self.fh);

        self.submit(Box::new(move |ctx| {
            let bridge = Box::new(StatBridge { tx: Some(tx) });
            let pd = Box::into_raw(bridge) as *mut c_void;
            let rc = unsafe { sys::nfs_fstat64_async(ctx, fh.get(), stat_cb, pd) };
            if rc != 0 {
                let mut bridge = unsafe { Box::from_raw(pd as *mut StatBridge) };
                let msg = get_error_string(ctx);
                if let Some(tx) = bridge.tx.take() {
                    let _ = tx.send(Err((-1, msg)));
                }
            }
        }))?;

        let st = bridge_result(rx.await.map_err(|_| Error::Cancelled)?)?;
        Ok(FileStat::from(&st))
    }

    /// Close the file handle.
    pub async fn close(self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        let fh = FhPtr(self.fh);

        self.submit(Box::new(move |ctx| {
            let bridge = Box::new(VoidBridge { tx: Some(tx) });
            let pd = Box::into_raw(bridge) as *mut c_void;
            let rc = unsafe { sys::nfs_close_async(ctx, fh.get(), void_cb, pd) };
            if rc != 0 {
                let mut bridge = unsafe { Box::from_raw(pd as *mut VoidBridge) };
                let msg = get_error_string(ctx);
                if let Some(tx) = bridge.tx.take() {
                    let _ = tx.send(Err((-1, msg)));
                }
            }
        }))?;

        let result = rx.await.map_err(|_| Error::Cancelled)?;
        // Don't let Drop also close it.
        std::mem::forget(self);
        bridge_err(result)
    }
}

impl Drop for NfsFile {
    fn drop(&mut self) {
        // Best-effort close. In async code prefer calling
        // `file.close().await` explicitly.
        if !self.fh.is_null() {
            let fh = FhPtr(self.fh);
            let _ = self.client.event_loop.submit(Box::new(move |ctx| unsafe {
                let bridge = Box::new(VoidBridge { tx: None });
                let pd = Box::into_raw(bridge) as *mut c_void;
                sys::nfs_close_async(ctx, fh.get(), void_cb, pd);
            }));
        }
    }
}

// ── Convenience free function ───────────────────────────────────────────────

/// Read an entire file from an NFS server in one call.
///
/// ```rust,no_run
/// # async fn run() -> libnfs::Result<()> {
/// let data = libnfs::read_file_from_server(
///     "10.0.0.1", "/", "/share/hello.txt", libnfs::NfsVersion::V4,
/// ).await?;
/// # Ok(())
/// # }
/// ```
pub async fn read_file_from_server(
    server: &str,
    export: &str,
    file_path: &str,
    version: NfsVersion,
) -> Result<Vec<u8>> {
    let client = NfsClient::mount(server, export, version).await?;
    client.read_file(file_path).await
}
