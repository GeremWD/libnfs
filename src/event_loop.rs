//! I/O driver: bridges libnfs's fd to the tokio reactor via `AsyncFd`.
//!
//! `NfsEventLoop::start(ctx)` spawns a tokio task that:
//! 1. Wraps the libnfs fd in an `AsyncFd`.
//! 2. Polls for the events libnfs wants (`nfs_which_events`).
//! 3. Calls `nfs_service` when those events fire, which invokes
//!    any pending C callbacks (and thus completes Rust futures).
//!
//! All access to the `nfs_context` is serialised on this single task.
//! Callers submit work via [`Command`] closures sent through an mpsc
//! channel, ensuring thread safety with any tokio runtime flavour.
//!
//! **Important:** libnfs may change its underlying fd during multi-step
//! operations like NFSv3 mount (portmapper → mountd → nfsd).  After every
//! `nfs_service` call the loop re-creates the `AsyncFd` to guarantee a
//! fresh epoll registration — this handles both fd-number changes *and*
//! fd-number reuse (where the OS assigns the same number to the new socket).

use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::mpsc;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::sys;

/// A closure that will be executed on the driver task with exclusive
/// access to the `nfs_context`.
pub(crate) type Command = Box<dyn FnOnce(*mut sys::nfs_context) + Send + 'static>;

/// Handle to the background tokio task that services libnfs I/O.
pub(crate) struct EventLoop {
    /// Shared flag to tell the loop to exit.
    stop: Arc<AtomicBool>,
    /// Notify handle to wake the loop for shutdown.
    notify: Arc<Notify>,
    /// The spawned task handle (taken on drop to abort it).
    task: Option<JoinHandle<()>>,
    /// Channel for submitting [`Command`]s to the driver task.
    cmd_tx: mpsc::UnboundedSender<Command>,
}

impl EventLoop {
    /// Spawn the driver task for the given NFS context.
    ///
    /// # Safety
    /// `ctx` must be a valid, initialised `nfs_context` pointer that will
    /// outlive the driver (guaranteed by `NfsInner`'s drop order).
    pub fn start(ctx: *mut sys::nfs_context) -> crate::Result<Self> {
        let fd: RawFd = unsafe { sys::nfs_get_fd(ctx) };
        if fd < 0 {
            return Err(crate::Error::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "nfs_get_fd returned invalid fd",
            )));
        }

        let stop = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(Notify::new());
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        let stop2 = stop.clone();
        let notify2 = notify.clone();

        // SAFETY: ctx is used exclusively from this single task.
        // All external work is funnelled through the command channel.
        let ctx_ptr = CtxPtr(ctx);

        let task = tokio::spawn(async move {
            driver_loop(ctx_ptr, stop2, notify2, cmd_rx).await;
        });

        Ok(Self {
            stop,
            notify,
            task: Some(task),
            cmd_tx,
        })
    }

    /// Submit a [`Command`] closure to be executed on the driver task.
    ///
    /// The closure receives `*mut nfs_context` and runs with exclusive
    /// access — no other code touches the context concurrently.
    pub fn submit(&self, cmd: Command) -> crate::Result<()> {
        self.cmd_tx.send(cmd).map_err(|_| crate::Error::Cancelled)
    }

    /// Signal the loop to stop at its next iteration.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.notify.notify_one();
    }

    /// Take the JoinHandle out (used in NfsInner::drop to abort it).
    pub fn take_task(&mut self) -> Option<JoinHandle<()>> {
        self.task.take()
    }
}

/// New-type so we can implement `AsRawFd` without owning the fd.
struct FdWrapper(RawFd);

impl std::os::unix::io::AsRawFd for FdWrapper {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

/// Send wrapper around `*mut nfs_context`.
struct CtxPtr(*mut sys::nfs_context);
unsafe impl Send for CtxPtr {}

/// Create (or re-create) an `AsyncFd` for the current libnfs fd.
/// Returns `None` if the fd is invalid.
fn make_async_fd(ctx: *mut sys::nfs_context) -> Option<(RawFd, AsyncFd<FdWrapper>)> {
    let fd: RawFd = unsafe { sys::nfs_get_fd(ctx) };
    if fd < 0 {
        eprintln!("[libnfs] make_async_fd: nfs_get_fd returned {fd} (invalid)");
        return None;
    }
    match AsyncFd::new(FdWrapper(fd)) {
        Ok(afd) => {
            eprintln!("[libnfs] make_async_fd: registered fd={fd} with epoll");
            Some((fd, afd))
        }
        Err(e) => {
            eprintln!("[libnfs] make_async_fd: AsyncFd::new(fd={fd}) failed: {e}");
            None
        }
    }
}

/// Format poll flags as a human-readable string.
fn fmt_poll(flags: i32) -> String {
    let mut s = String::new();
    if flags & 0x001 != 0 {
        s.push_str("POLLIN");
    }
    if flags & 0x004 != 0 {
        if !s.is_empty() {
            s.push('|');
        }
        s.push_str("POLLOUT");
    }
    if flags & 0x008 != 0 {
        if !s.is_empty() {
            s.push('|');
        }
        s.push_str("POLLERR");
    }
    if flags & 0x010 != 0 {
        if !s.is_empty() {
            s.push('|');
        }
        s.push_str("POLLHUP");
    }
    if s.is_empty() {
        s.push_str("(none)");
    }
    s
}

/// Do a non-blocking `poll(2)` on `fd` for `wanted` events and, if any
/// are ready, feed them to `nfs_service`.  Returns `true` if service
/// was called.
fn try_service(ctx: *mut sys::nfs_context, fd: RawFd, wanted: i32) -> bool {
    if wanted == 0 {
        return false;
    }
    let mut pfd = libc::pollfd {
        fd,
        events: wanted as i16,
        revents: 0,
    };
    let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
    if ret > 0 && pfd.revents != 0 {
        eprintln!(
            "[libnfs] try_service: fd={fd} wanted={} ready={} → nfs_service",
            fmt_poll(wanted),
            fmt_poll(pfd.revents as i32)
        );
        let fd_before = unsafe { sys::nfs_get_fd(ctx) };
        unsafe { sys::nfs_service(ctx, pfd.revents as i32) };
        let fd_after = unsafe { sys::nfs_get_fd(ctx) };
        if fd_after != fd_before {
            eprintln!(
                "[libnfs] try_service: fd changed {fd_before} → {fd_after} after nfs_service"
            );
        }
        true
    } else {
        false
    }
}

async fn driver_loop(
    ctx: CtxPtr,
    stop: Arc<AtomicBool>,
    notify: Arc<Notify>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
) {
    eprintln!("[libnfs] driver_loop: starting");
    let (mut current_fd, mut async_fd) = match make_async_fd(ctx.0) {
        Some(pair) => pair,
        None => {
            eprintln!("[libnfs] driver_loop: initial make_async_fd failed, exiting");
            return;
        }
    };
    eprintln!("[libnfs] driver_loop: initial fd={current_fd}");

    // Eagerly service on startup: the initial connect may have already
    // completed between nfs_mount_async() and the task starting.
    let w = unsafe { sys::nfs_which_events(ctx.0) };
    eprintln!(
        "[libnfs] driver_loop: startup eager service, wanted={}",
        fmt_poll(w)
    );
    try_service(ctx.0, current_fd, w);

    let mut iteration: u64 = 0;

    loop {
        iteration += 1;

        if stop.load(Ordering::Acquire) {
            eprintln!("[libnfs] driver_loop: stop flag set, exiting");
            break;
        }

        // ── Refresh AsyncFd ─────────────────────────────────────────
        let latest_fd = unsafe { sys::nfs_get_fd(ctx.0) };
        if latest_fd < 0 {
            eprintln!(
                "[libnfs] driver_loop [#{iteration}]: fd={latest_fd} (invalid), sleeping 5ms"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            let retry_fd = unsafe { sys::nfs_get_fd(ctx.0) };
            if retry_fd < 0 {
                eprintln!(
                    "[libnfs] driver_loop [#{iteration}]: fd still {retry_fd} after sleep, exiting"
                );
                break;
            }
        }
        // Re-check after potential sleep.
        let latest_fd = unsafe { sys::nfs_get_fd(ctx.0) };
        if latest_fd < 0 {
            eprintln!("[libnfs] driver_loop [#{iteration}]: fd={latest_fd} after recheck, exiting");
            break;
        }
        if latest_fd != current_fd {
            eprintln!("[libnfs] driver_loop [#{iteration}]: fd changed {current_fd} → {latest_fd}");
            current_fd = latest_fd;
            async_fd = match AsyncFd::new(FdWrapper(current_fd)) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("[libnfs] driver_loop [#{iteration}]: AsyncFd::new(fd={current_fd}) failed: {e}, exiting");
                    break;
                }
            };
            // Eagerly check for ready events on the new fd.
            let w = unsafe { sys::nfs_which_events(ctx.0) };
            eprintln!(
                "[libnfs] driver_loop [#{iteration}]: new fd eager service, wanted={}",
                fmt_poll(w)
            );
            if try_service(ctx.0, current_fd, w) {
                continue; // fd may have changed again
            }
        }

        // ── Wait for I/O readiness, a command, or stop ──────────────
        let wanted = unsafe { sys::nfs_which_events(ctx.0) };
        let interest = poll_flags_to_interest(wanted);

        // Only log every Nth iteration to avoid flooding, but always
        // log interesting state changes.
        if iteration <= 20 || iteration % 100 == 0 {
            eprintln!(
                "[libnfs] driver_loop [#{iteration}]: waiting on fd={current_fd} wanted={}",
                fmt_poll(wanted)
            );
        }

        tokio::select! {
            ready = async_fd.ready(interest) => {
                match ready {
                    Ok(mut guard) => {
                        let revents = interest_to_poll_flags(&guard);
                        eprintln!("[libnfs] driver_loop [#{iteration}]: ready fd={current_fd} revents={}", fmt_poll(revents));
                        let fd_before = unsafe { sys::nfs_get_fd(ctx.0) };
                        unsafe { sys::nfs_service(ctx.0, revents) };
                        let fd_after = unsafe { sys::nfs_get_fd(ctx.0) };
                        if fd_after != fd_before {
                            eprintln!("[libnfs] driver_loop [#{iteration}]: fd changed {fd_before} → {fd_after} after nfs_service");
                        }
                        guard.clear_ready();
                    }
                    Err(e) => {
                        eprintln!("[libnfs] driver_loop [#{iteration}]: ready() error: {e}");
                    }
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(cmd) => {
                        eprintln!("[libnfs] driver_loop [#{iteration}]: executing command");
                        cmd(ctx.0);
                        let mut extra = 0u32;
                        while let Ok(cmd) = cmd_rx.try_recv() {
                            cmd(ctx.0);
                            extra += 1;
                        }
                        if extra > 0 {
                            eprintln!("[libnfs] driver_loop [#{iteration}]: drained {extra} extra commands");
                        }
                    }
                    None => {
                        eprintln!("[libnfs] driver_loop [#{iteration}]: command channel closed, exiting");
                        break;
                    }
                }
            }
            _ = notify.notified() => {
                eprintln!("[libnfs] driver_loop [#{iteration}]: notified (stop signal)");
            }
        }

        // ── After any wakeup, re-create AsyncFd ────────────────────
        let new_fd = unsafe { sys::nfs_get_fd(ctx.0) };
        if new_fd >= 0 {
            if new_fd != current_fd {
                eprintln!("[libnfs] driver_loop [#{iteration}]: post-wakeup fd changed {current_fd} → {new_fd}");
            }
            current_fd = new_fd;
            async_fd = match AsyncFd::new(FdWrapper(current_fd)) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("[libnfs] driver_loop [#{iteration}]: post-wakeup AsyncFd::new(fd={current_fd}) failed: {e}, exiting");
                    break;
                }
            };
            // Eagerly service any pending events on the (possibly new) fd.
            let post = unsafe { sys::nfs_which_events(ctx.0) };
            try_service(ctx.0, current_fd, post);
        } else {
            eprintln!("[libnfs] driver_loop [#{iteration}]: post-wakeup fd={new_fd} (invalid)");
        }
    }
    eprintln!("[libnfs] driver_loop: exited main loop");
}

/// Map libnfs `nfs_which_events()` return (POLLIN/POLLOUT bitmask) to
/// tokio `Interest`.
fn poll_flags_to_interest(events: i32) -> Interest {
    let pollin = 0x001; // libc::POLLIN
    let pollout = 0x004; // libc::POLLOUT

    let r = events & pollin != 0;
    let w = events & pollout != 0;

    match (r, w) {
        (true, true) => Interest::READABLE | Interest::WRITABLE,
        (true, false) => Interest::READABLE,
        (false, true) => Interest::WRITABLE,
        // Default to readable if libnfs says 0 (shouldn't happen).
        (false, false) => Interest::READABLE,
    }
}

/// Convert a resolved `AsyncFdReadyGuard`'s interest back to poll flags
/// so we can pass them to `nfs_service`.
fn interest_to_poll_flags(guard: &tokio::io::unix::AsyncFdReadyGuard<'_, FdWrapper>) -> i32 {
    let mut flags = 0i32;
    let ready = guard.ready();
    if ready.is_readable() {
        flags |= 0x001; // POLLIN
    }
    if ready.is_writable() {
        flags |= 0x004; // POLLOUT
    }
    flags
}
