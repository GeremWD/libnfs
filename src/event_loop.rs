//! I/O driver: bridges libnfs's fd to a `poll(2)` loop on a dedicated thread.
//!
//! This design matches the reference C event loop from the libnfs examples:
//! each iteration reads the current fd and wanted events with `nfs_get_fd` /
//! `nfs_which_events`, blocks in `poll(2)`, then passes the **real** `revents`
//! to `nfs_service`.
//!
//! This naturally handles fd changes during multi-step operations (NFSv3
//! portmapper → mountd → nfsd) because the fd is re-read every iteration —
//! no epoll re-registration is needed.
//!
//! Commands from async Rust code arrive via a `tokio::sync::mpsc` channel;
//! a wake-pipe ensures `poll` returns promptly when a new command is queued
//! or a stop is requested.

use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use tokio::sync::mpsc;

use crate::sys;

/// A closure executed on the driver thread with exclusive access to the
/// `nfs_context`.
pub(crate) type Command = Box<dyn FnOnce(*mut sys::nfs_context) + Send + 'static>;

/// Handle to the background thread that services libnfs I/O.
pub(crate) struct EventLoop {
    /// Shared flag to tell the loop to exit.
    stop: Arc<AtomicBool>,
    /// The poll thread (taken on drop to join it).
    thread: Option<JoinHandle<()>>,
    /// Sender for submitting [`Command`]s to the driver thread.
    cmd_tx: mpsc::UnboundedSender<Command>,
    /// Write end of the wake pipe (wakes `poll(2)` on command submit / stop).
    wake_w: RawFd,
}

/// Send wrapper around `*mut nfs_context`.
struct CtxPtr(*mut sys::nfs_context);
unsafe impl Send for CtxPtr {}

impl EventLoop {
    /// Spawn the driver thread for the given NFS context.
    ///
    /// # Safety
    /// `ctx` must be a valid, initialised `nfs_context` pointer that will
    /// outlive the driver (guaranteed by `NfsInner`'s drop order).
    pub fn start(ctx: *mut sys::nfs_context) -> crate::Result<Self> {
        let fd: RawFd = unsafe { sys::nfs_get_fd(ctx) };
        if fd < 0 {
            return Err(crate::Error::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("nfs_get_fd returned {fd}"),
            )));
        }

        let stop = Arc::new(AtomicBool::new(false));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        // Create a pipe so we can wake poll(2) from async code.
        let mut pipe_fds = [0i32; 2];
        if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
            return Err(crate::Error::Io(std::io::Error::last_os_error()));
        }
        let wake_r = pipe_fds[0];
        let wake_w = pipe_fds[1];

        // Non-blocking read end so the drain-loop never stalls.
        unsafe {
            let flags = libc::fcntl(wake_r, libc::F_GETFL);
            libc::fcntl(wake_r, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        let stop2 = stop.clone();
        let ctx_ptr = CtxPtr(ctx);

        let thread = std::thread::Builder::new()
            .name("libnfs-poll".into())
            .spawn(move || {
                let ctx = ctx_ptr;
                driver_loop(ctx.0, stop2, cmd_rx, wake_r);
                unsafe { libc::close(wake_r) };
            })
            .map_err(crate::Error::Io)?;

        eprintln!("[libnfs] EventLoop::start: initial fd={fd}, poll thread spawned");

        Ok(Self {
            stop,
            thread: Some(thread),
            cmd_tx,
            wake_w,
        })
    }

    /// Submit a [`Command`] closure to be executed on the driver thread.
    pub fn submit(&self, cmd: Command) -> crate::Result<()> {
        self.cmd_tx.send(cmd).map_err(|_| crate::Error::Cancelled)?;
        self.wake();
        Ok(())
    }

    /// Signal the loop to stop and wake it.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.wake();
    }

    /// Take the thread handle out (used in `NfsInner::drop` to join it).
    pub fn take_task(&mut self) -> Option<JoinHandle<()>> {
        self.thread.take()
    }

    /// Write one byte to the wake pipe so `poll(2)` returns immediately.
    fn wake(&self) {
        let b: u8 = 1;
        unsafe {
            libc::write(self.wake_w, &b as *const u8 as *const libc::c_void, 1);
        }
    }
}

impl Drop for EventLoop {
    fn drop(&mut self) {
        // Close the write end of the wake pipe.
        if self.wake_w >= 0 {
            unsafe { libc::close(self.wake_w) };
            self.wake_w = -1;
        }
    }
}

/// Format `poll(2)` revents as a human-readable string.
fn fmt_revents(revents: i16) -> String {
    let r = revents as i32;
    let mut parts = Vec::new();
    if r & (libc::POLLIN as i32) != 0 {
        parts.push("POLLIN");
    }
    if r & (libc::POLLOUT as i32) != 0 {
        parts.push("POLLOUT");
    }
    if r & (libc::POLLERR as i32) != 0 {
        parts.push("POLLERR");
    }
    if r & (libc::POLLHUP as i32) != 0 {
        parts.push("POLLHUP");
    }
    if r & (libc::POLLNVAL as i32) != 0 {
        parts.push("POLLNVAL");
    }
    if parts.is_empty() {
        format!("0x{:x}", revents)
    } else {
        parts.join("|")
    }
}

/// The `poll(2)` loop — runs on a dedicated thread, mirrors the reference
/// C event loop from the libnfs repository:
///
/// ```c
/// for (;;) {
///     pfds[0].fd     = nfs_get_fd(nfs);
///     pfds[0].events = nfs_which_events(nfs);
///     poll(pfds, 1, -1);
///     nfs_service(nfs, pfds[0].revents);
/// }
/// ```
fn driver_loop(
    ctx: *mut sys::nfs_context,
    stop: Arc<AtomicBool>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    wake_r: RawFd,
) {
    eprintln!("[libnfs] driver_loop: started (poll-based)");

    loop {
        if stop.load(Ordering::Acquire) {
            eprintln!("[libnfs] driver_loop: stop flag set, exiting");
            break;
        }

        // ── Execute queued commands ─────────────────────────────────
        while let Ok(cmd) = cmd_rx.try_recv() {
            cmd(ctx);
        }

        // ── Read current fd + wanted events (re-read every iteration
        //    so fd changes are handled automatically) ────────────────
        let fd = unsafe { sys::nfs_get_fd(ctx) };
        if fd < 0 {
            eprintln!("[libnfs] driver_loop: nfs_get_fd={fd} (invalid), exiting");
            break;
        }
        let events = unsafe { sys::nfs_which_events(ctx) };

        // ── poll(2) — block until NFS activity or wake-pipe ─────────
        let mut pfds = [
            libc::pollfd {
                fd,
                events: events as i16,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_r,
                events: libc::POLLIN as i16,
                revents: 0,
            },
        ];

        let ret = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };

        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            eprintln!("[libnfs] driver_loop: poll failed: {err}");
            break;
        }

        // ── Drain the wake pipe ─────────────────────────────────────
        if pfds[1].revents != 0 {
            let mut buf = [0u8; 64];
            loop {
                let n =
                    unsafe { libc::read(wake_r, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n <= 0 {
                    break;
                }
            }
        }

        // ── Feed the *real* revents to nfs_service ──────────────────
        if pfds[0].revents != 0 {
            eprintln!(
                "[libnfs] driver_loop: fd={fd} revents={} → nfs_service",
                fmt_revents(pfds[0].revents)
            );
            let rc = unsafe { sys::nfs_service(ctx, pfds[0].revents as i32) };
            if rc < 0 {
                let err = crate::get_error_message(ctx, rc, std::ptr::null_mut());
                eprintln!("[libnfs] driver_loop: nfs_service returned {rc}: {err}");
            }
        }
    }

    eprintln!("[libnfs] driver_loop: exited");
}
