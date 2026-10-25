use std::{
    io::{self, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc,
    },
    thread,
    time::Duration,
};

use tokio::sync::mpsc;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    backend::WaylandError,
    delegate_noop,
    protocol::{wl_callback, wl_registry, wl_seat},
};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1, ext_idle_notifier_v1,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleCapability {
    ExtIdleNotifyV2,
    ExtIdleNotifyV1,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleEvent {
    Idled,
    Resumed,
}

/// Owns the idle subscription and its Wayland connection. Replacing or dropping
/// the guard cancels and joins the worker, including during compositor startup.
#[derive(Debug)]
pub struct IdleMonitor {
    capability: IdleCapability,
    cancelled: Arc<AtomicBool>,
    wake: Option<UnixStream>,
    worker: Option<thread::JoinHandle<()>>,
}

impl IdleMonitor {
    pub fn start(
        timeout_ms: u64,
        ignore_inhibitors: bool,
        sender: mpsc::Sender<IdleEvent>,
    ) -> Self {
        let mut monitor = Self {
            capability: IdleCapability::Unavailable,
            cancelled: Arc::new(AtomicBool::new(false)),
            wake: None,
            worker: None,
        };
        let (wake, cancel_socket) = match UnixStream::pair() {
            Ok(sockets) => sockets,
            Err(error) => {
                tracing::warn!(%error, "cannot create idle monitor cancellation socket");
                return monitor;
            }
        };
        let (capability_sender, capability_receiver) = std_mpsc::sync_channel(1);
        let cancelled = monitor.cancelled.clone();
        let worker = thread::Builder::new()
            .name("breakd-idle".into())
            .spawn(move || {
                if let Err(error) = run_idle_monitor(
                    timeout_ms.min(u32::MAX as u64) as u32,
                    ignore_inhibitors,
                    sender,
                    capability_sender,
                    cancelled,
                    cancel_socket,
                ) {
                    tracing::warn!(%error, "Wayland idle monitor stopped");
                }
            });
        match worker {
            Ok(worker) => {
                monitor.wake = Some(wake);
                monitor.worker = Some(worker);
                match capability_receiver.recv_timeout(Duration::from_secs(2)) {
                    Ok(capability) => monitor.capability = capability,
                    Err(_) => monitor.stop(),
                }
            }
            Err(error) => tracing::warn!(%error, "cannot spawn Wayland idle monitor"),
        }
        monitor
    }

    pub fn capability(&self) -> IdleCapability {
        self.capability
    }

    pub fn stop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(mut wake) = self.wake.take() {
            let _ = wake.write_all(&[1]);
        }
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::warn!("Wayland idle monitor panicked");
        }
    }
}

impl Drop for IdleMonitor {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_idle_monitor(
    timeout_ms: u32,
    ignore_inhibitors: bool,
    sender: mpsc::Sender<IdleEvent>,
    capability_sender: std_mpsc::SyncSender<IdleCapability>,
    cancelled: Arc<AtomicBool>,
    cancel_socket: UnixStream,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if cancelled.load(Ordering::Acquire) || sender.is_closed() {
        return Ok(());
    }
    let connection = Connection::connect_to_env()?;
    let state = IdleState {
        timeout_ms,
        ignore_inhibitors,
        sender,
        pending_event: None,
        seat: None,
        notifier: None,
        notification: None,
        capability: IdleCapability::Unavailable,
        initialized: false,
    };
    run_idle_connection(
        connection,
        state,
        capability_sender,
        cancelled,
        cancel_socket,
    )
}

fn run_idle_connection(
    connection: Connection,
    mut state: IdleState,
    capability_sender: std_mpsc::SyncSender<IdleCapability>,
    cancelled: Arc<AtomicBool>,
    cancel_socket: UnixStream,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut event_queue = connection.new_event_queue();
    let queue_handle = event_queue.handle();
    connection.display().get_registry(&queue_handle, ());
    connection.display().sync(&queue_handle, ());
    let mut capability_sender = Some(capability_sender);
    loop {
        if cancelled.load(Ordering::Acquire) || state.sender.is_closed() {
            return Ok(());
        }
        event_queue.dispatch_pending(&mut state)?;
        state.flush_pending();
        if state.initialized
            && let Some(sender) = capability_sender.take()
        {
            let _ = sender.send(state.capability);
            if state.capability == IdleCapability::Unavailable {
                return Ok(());
            }
        }
        let writable = match connection.flush() {
            Ok(()) => false,
            Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => true,
            Err(error) => return Err(error.into()),
        };
        let Some(read) = event_queue.prepare_read() else {
            continue;
        };
        match wait_for_wayland(
            read.connection_fd().as_raw_fd(),
            cancel_socket.as_raw_fd(),
            writable,
        )? {
            PollResult::Cancelled => return Ok(()),
            PollResult::Ready => match read.read() {
                Ok(_) => {}
                Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            },
            PollResult::Retry => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollResult {
    Cancelled,
    Ready,
    Retry,
}

fn wait_for_wayland(wayland_fd: i32, cancel_fd: i32, writable: bool) -> io::Result<PollResult> {
    use nix::libc;

    let mut descriptors = [
        libc::pollfd {
            fd: wayland_fd,
            events: libc::POLLIN | if writable { libc::POLLOUT } else { 0 },
            revents: 0,
        },
        libc::pollfd {
            fd: cancel_fd,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: descriptors points to two initialized pollfd values that remain
    // alive and exclusively borrowed for the duration of poll. The FDs belong
    // to this worker and cannot be closed while it is polling.
    let result = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, 100) };
    if result < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::Interrupted {
            Ok(PollResult::Retry)
        } else {
            Err(error)
        };
    }
    if descriptors[1].revents != 0 {
        return Ok(PollResult::Cancelled);
    }
    if descriptors[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
    {
        return Ok(PollResult::Ready);
    }
    // A short timeout also retries a full event channel without blocking the
    // Wayland thread, and notices when the daemon has dropped its receiver.
    Ok(PollResult::Retry)
}

struct IdleState {
    timeout_ms: u32,
    ignore_inhibitors: bool,
    sender: mpsc::Sender<IdleEvent>,
    pending_event: Option<IdleEvent>,
    seat: Option<wl_seat::WlSeat>,
    notifier: Option<ext_idle_notifier_v1::ExtIdleNotifierV1>,
    notification: Option<ext_idle_notification_v1::ExtIdleNotificationV1>,
    capability: IdleCapability,
    initialized: bool,
}

impl IdleState {
    fn flush_pending(&mut self) {
        if let Some(event) = self.pending_event.take() {
            match self.sender.try_send(event) {
                Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
                Err(mpsc::error::TrySendError::Full(event)) => self.pending_event = Some(event),
            }
        }
    }

    fn create_notification(&mut self, queue_handle: &QueueHandle<Self>) {
        if self.notification.is_some() {
            return;
        }
        let (Some(seat), Some(notifier)) = (&self.seat, &self.notifier) else {
            return;
        };
        let version = notifier.version();
        self.notification = Some(if version >= 2 && self.ignore_inhibitors {
            self.capability = IdleCapability::ExtIdleNotifyV2;
            notifier.get_input_idle_notification(self.timeout_ms, seat, queue_handle, ())
        } else {
            self.capability = if version >= 2 {
                IdleCapability::ExtIdleNotifyV2
            } else {
                IdleCapability::ExtIdleNotifyV1
            };
            notifier.get_idle_notification(self.timeout_ms, seat, queue_handle, ())
        });
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for IdleState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        queue_handle: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(9), queue_handle, ()))
                }
                "ext_idle_notifier_v1" if state.notifier.is_none() => {
                    state.notifier = Some(registry.bind(name, version.min(2), queue_handle, ()))
                }
                _ => {}
            }
            state.create_notification(queue_handle);
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for IdleState {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.initialized = true;
        }
    }
}

impl Dispatch<ext_idle_notification_v1::ExtIdleNotificationV1, ()> for IdleState {
    fn event(
        state: &mut Self,
        _: &ext_idle_notification_v1::ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => state.pending_event = Some(IdleEvent::Idled),
            ext_idle_notification_v1::Event::Resumed => {
                state.pending_event = Some(IdleEvent::Resumed)
            }
            _ => {}
        }
    }
}

delegate_noop!(IdleState: ignore wl_seat::WlSeat);
delegate_noop!(IdleState: ignore ext_idle_notifier_v1::ExtIdleNotifierV1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_wakes_an_unresponsive_wayland_socket() {
        let (wayland, _compositor) = UnixStream::pair().unwrap();
        let (mut cancel, cancel_receiver) = UnixStream::pair().unwrap();
        cancel.write_all(&[1]).unwrap();
        assert_eq!(
            wait_for_wayland(wayland.as_raw_fd(), cancel_receiver.as_raw_fd(), false).unwrap(),
            PollResult::Cancelled
        );
    }

    #[test]
    fn stop_joins_worker_when_compositor_never_replies_to_registry() {
        let (wayland, _compositor) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(wayland).unwrap();
        let (wake, cancel_socket) = UnixStream::pair().unwrap();
        let (sender, _receiver) = mpsc::channel(1);
        let (capability_sender, _capability_receiver) = std_mpsc::sync_channel(1);
        let (finished_sender, finished_receiver) = std_mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let worker = thread::spawn(move || {
            let result = run_idle_connection(
                connection,
                IdleState {
                    timeout_ms: 1_000,
                    ignore_inhibitors: false,
                    sender,
                    pending_event: None,
                    seat: None,
                    notifier: None,
                    notification: None,
                    capability: IdleCapability::Unavailable,
                    initialized: false,
                },
                capability_sender,
                worker_cancelled,
                cancel_socket,
            );
            finished_sender.send(result.is_ok()).unwrap();
        });
        let mut monitor = IdleMonitor {
            capability: IdleCapability::Unavailable,
            cancelled,
            wake: Some(wake),
            worker: Some(worker),
        };
        monitor.stop();
        assert!(monitor.worker.is_none());
        assert!(finished_receiver.try_recv().unwrap());
        // Stopping twice, followed by Drop, must also be harmless.
        monitor.stop();
    }

    #[test]
    fn full_event_channel_keeps_the_latest_state_without_blocking() {
        let (sender, mut receiver) = mpsc::channel(1);
        sender.try_send(IdleEvent::Idled).unwrap();
        let mut state = IdleState {
            timeout_ms: 1_000,
            ignore_inhibitors: false,
            sender,
            pending_event: Some(IdleEvent::Resumed),
            seat: None,
            notifier: None,
            notification: None,
            capability: IdleCapability::Unavailable,
            initialized: true,
        };
        state.flush_pending();
        assert_eq!(state.pending_event, Some(IdleEvent::Resumed));
        assert_eq!(receiver.try_recv().unwrap(), IdleEvent::Idled);
        state.flush_pending();
        assert_eq!(state.pending_event, None);
        assert_eq!(receiver.try_recv().unwrap(), IdleEvent::Resumed);
    }
}
