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
//! operations like mount (portmapper → mountd → nfsd).  The loop detects
//! fd changes via `nfs_get_fd()` and re-creates the `AsyncFd` accordingly.

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
pub(crate) struct NfsEventLoop {
    /// Shared flag to tell the loop to exit.
    stop: Arc<AtomicBool>,
    /// Notify handle to wake the loop for shutdown.
    notify: Arc<Notify>,
    /// The spawned task handle (taken on drop to abort it).
    task: Option<JoinHandle<()>>,
    /// Channel for submitting [`Command`]s to the driver task.
    cmd_tx: mpsc::UnboundedSender<Command>,
}

impl NfsEventLoop {
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

/// POLLOUT constant matching libc (used for eager write flushing).
const POLLOUT: i32 = 0x004;

async fn driver_loop(
    ctx: CtxPtr,
    stop: Arc<AtomicBool>,
    notify: Arc<Notify>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
) {
    // Track the current fd so we can detect when libnfs switches sockets
    // (e.g. portmapper → mountd → nfsd during mount).
    let mut current_fd: RawFd = unsafe { sys::nfs_get_fd(ctx.0) };
    let mut async_fd = match AsyncFd::new(FdWrapper(current_fd)) {
        Ok(a) => a,
        Err(_) => return,
    };

    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }

        // ── Detect fd change ────────────────────────────────────────
        // libnfs may close and reconnect internally (portmapper →
        // mountd → nfsd).  Re-register with epoll if the fd changed.
        let latest_fd = unsafe { sys::nfs_get_fd(ctx.0) };
        if latest_fd != current_fd {
            if latest_fd < 0 {
                break;
            }
            current_fd = latest_fd;
            async_fd = match AsyncFd::new(FdWrapper(current_fd)) {
                Ok(a) => a,
                Err(_) => break,
            };
            // The new socket may already be connected (especially on
            // localhost).  We may have missed the epoll edge, so do a
            // non-blocking poll(2) to check *actual* readiness before
            // calling nfs_service.  Previously we passed the WANTED
            // events straight to nfs_service, which told libnfs that
            // POLLOUT had occurred even when a connect() was still in
            // progress — breaking NFSv3 mounts over the network.
            let w = unsafe { sys::nfs_which_events(ctx.0) };
            if w != 0 {
                let mut pfd = libc::pollfd {
                    fd: current_fd,
                    events: w as i16,
                    revents: 0,
                };
                let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
                if ret > 0 && pfd.revents != 0 {
                    unsafe { sys::nfs_service(ctx.0, pfd.revents as i32) };
                }
            }
            continue; // re-check fd (may have changed again)
        }

        // ── Wait for I/O readiness, a command, or stop ──────────────
        let wanted = unsafe { sys::nfs_which_events(ctx.0) };
        let interest = poll_flags_to_interest(wanted);

        tokio::select! {
            ready = async_fd.ready(interest) => {
                match ready {
                    Ok(mut guard) => {
                        let revents = interest_to_poll_flags(&guard);
                        unsafe { sys::nfs_service(ctx.0, revents) };
                        guard.clear_ready();
                    }
                    Err(_) => continue,
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(cmd) => {
                        cmd(ctx.0);
                        // Drain any additional queued commands so we
                        // batch-submit before flushing writes.
                        while let Ok(cmd) = cmd_rx.try_recv() {
                            cmd(ctx.0);
                        }
                    }
                    None => break, // Channel closed — shutting down.
                }
            }
            _ = notify.notified() => {
                // Stop signal — re-check flag at top of loop.
            }
        }

        // ── Eagerly flush pending writes ────────────────────────────
        // After any wakeup we check whether libnfs has queued output.
        // This is necessary because edge-triggered epoll won't
        // re-notify for POLLOUT on an already-writable socket.
        // Use a non-blocking poll(2) to verify actual writability
        // before telling libnfs POLLOUT occurred.
        let post = unsafe { sys::nfs_which_events(ctx.0) };
        if post & POLLOUT != 0 {
            let mut pfd = libc::pollfd {
                fd: current_fd,
                events: POLLOUT as i16,
                revents: 0,
            };
            let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
            if ret > 0 && pfd.revents != 0 {
                unsafe { sys::nfs_service(ctx.0, pfd.revents as i32) };
            }
        }
    }
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
