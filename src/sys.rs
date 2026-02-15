//! Raw FFI bindings for libnfs.
//!
//! Hand-written to cover the async API surface used by this crate.
//! Linked via `pkg-config` in `build.rs`.

#![allow(non_camel_case_types, dead_code)]

use std::os::raw::{c_char, c_int, c_void};

// ── Opaque types ────────────────────────────────────────────────────────────

/// Opaque NFS context.
#[repr(C)]
pub struct nfs_context {
    _opaque: [u8; 0],
}

/// Opaque open-file handle.
#[repr(C)]
pub struct nfsfh {
    _opaque: [u8; 0],
}

/// Opaque directory handle.
#[repr(C)]
pub struct nfsdir {
    _opaque: [u8; 0],
}

// ── Directory entry ─────────────────────────────────────────────────────────

/// Matches `struct nfsdirent` from `<nfsc/libnfs.h>`.
#[repr(C)]
pub struct nfsdirent {
    pub next: *mut nfsdirent,
    pub name: *mut c_char,
    pub inode: u64,
    pub r#type: u32,
    pub mode: u32,
    pub size: u64,
    pub atime: libc_timeval,
    pub mtime: libc_timeval,
    pub ctime: libc_timeval,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub dev: u64,
    pub rdev: u64,
    pub blksize: u64,
    pub blocks: u64,
    pub used: u64,
    pub atime_nsec: u32,
    pub mtime_nsec: u32,
    pub ctime_nsec: u32,
}

/// Mirrors `struct timeval` (used inside `nfsdirent`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct libc_timeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

// ── Stat ────────────────────────────────────────────────────────────────────

/// Matches `struct nfs_stat_64` from `<nfsc/libnfs.h>`.
#[repr(C)]
#[derive(Debug, Clone)]
pub struct nfs_stat_64 {
    pub nfs_dev: u64,
    pub nfs_ino: u64,
    pub nfs_mode: u64,
    pub nfs_nlink: u64,
    pub nfs_uid: u64,
    pub nfs_gid: u64,
    pub nfs_rdev: u64,
    pub nfs_size: u64,
    pub nfs_blksize: u64,
    pub nfs_blocks: u64,
    pub nfs_atime: u64,
    pub nfs_mtime: u64,
    pub nfs_ctime: u64,
    pub nfs_atime_nsec: u64,
    pub nfs_mtime_nsec: u64,
    pub nfs_ctime_nsec: u64,
    pub nfs_used: u64,
}

// ── Callback type ───────────────────────────────────────────────────────────

/// `typedef void (*nfs_cb)(int err, struct nfs_context *nfs, void *data, void *private_data);`
pub type nfs_cb = unsafe extern "C" fn(
    err: c_int,
    nfs: *mut nfs_context,
    data: *mut c_void,
    private_data: *mut c_void,
);

// ── Constants ───────────────────────────────────────────────────────────────

pub const NFS_V3: c_int = 3;
pub const NFS_V4: c_int = 4;
pub const O_RDONLY: c_int = 0;

// ── Extern functions ────────────────────────────────────────────────────────

extern "C" {
    // ── Context lifecycle ───────────────────────────────────────────────
    pub fn nfs_init_context() -> *mut nfs_context;
    pub fn nfs_destroy_context(nfs: *mut nfs_context);
    pub fn nfs_set_version(nfs: *mut nfs_context, version: c_int);
    pub fn nfs_set_uid(nfs: *mut nfs_context, uid: c_int);
    pub fn nfs_set_gid(nfs: *mut nfs_context, gid: c_int);
    pub fn nfs_get_error(nfs: *mut nfs_context) -> *mut c_char;

    // ── Event-loop plumbing ─────────────────────────────────────────────
    pub fn nfs_get_fd(nfs: *mut nfs_context) -> c_int;
    pub fn nfs_which_events(nfs: *mut nfs_context) -> c_int;
    pub fn nfs_service(nfs: *mut nfs_context, revents: c_int) -> c_int;

    // ── Mount / Umount ──────────────────────────────────────────────────
    pub fn nfs_mount_async(
        nfs: *mut nfs_context,
        server: *const c_char,
        exportname: *const c_char,
        cb: nfs_cb,
        private_data: *mut c_void,
    ) -> c_int;

    // ── Stat ────────────────────────────────────────────────────────────
    pub fn nfs_stat64_async(
        nfs: *mut nfs_context,
        path: *const c_char,
        cb: nfs_cb,
        private_data: *mut c_void,
    ) -> c_int;

    pub fn nfs_fstat64_async(
        nfs: *mut nfs_context,
        nfsfh: *mut nfsfh,
        cb: nfs_cb,
        private_data: *mut c_void,
    ) -> c_int;

    // ── Open / Close ────────────────────────────────────────────────────
    pub fn nfs_open_async(
        nfs: *mut nfs_context,
        path: *const c_char,
        flags: c_int,
        cb: nfs_cb,
        private_data: *mut c_void,
    ) -> c_int;

    pub fn nfs_close_async(
        nfs: *mut nfs_context,
        nfsfh: *mut nfsfh,
        cb: nfs_cb,
        private_data: *mut c_void,
    ) -> c_int;

    // ── Read ────────────────────────────────────────────────────────────
    pub fn nfs_pread_async(
        nfs: *mut nfs_context,
        nfsfh: *mut nfsfh,
        offset: u64,
        count: u64,
        cb: nfs_cb,
        private_data: *mut c_void,
    ) -> c_int;

    pub fn nfs_read_async(
        nfs: *mut nfs_context,
        nfsfh: *mut nfsfh,
        count: u64,
        cb: nfs_cb,
        private_data: *mut c_void,
    ) -> c_int;

    // ── Directory ───────────────────────────────────────────────────────
    pub fn nfs_opendir_async(
        nfs: *mut nfs_context,
        path: *const c_char,
        cb: nfs_cb,
        private_data: *mut c_void,
    ) -> c_int;

    pub fn nfs_readdir(nfs: *mut nfs_context, nfsdir: *mut nfsdir) -> *mut nfsdirent;
    pub fn nfs_closedir(nfs: *mut nfs_context, nfsdir: *mut nfsdir);
}
