use std::{sync::mpsc as std_mpsc, thread, time::Duration};

use tokio::sync::mpsc;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop,
    protocol::{wl_registry, wl_seat},
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

pub fn spawn_idle_monitor(
    timeout_ms: u64,
    ignore_inhibitors: bool,
    sender: mpsc::Sender<IdleEvent>,
) -> IdleCapability {
    let (capability_sender, capability_receiver) = std_mpsc::sync_channel(1);
    thread::Builder::new()
        .name("breakd-idle".into())
        .spawn(move || {
            let result = run_idle_monitor(
                timeout_ms.min(u32::MAX as u64) as u32,
                ignore_inhibitors,
                sender,
                capability_sender.clone(),
            );
            if let Err(error) = result {
                let _ = capability_sender.send(IdleCapability::Unavailable);
                tracing::warn!(%error, "Wayland idle monitor stopped");
            }
        })
        .ok();
    capability_receiver
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or(IdleCapability::Unavailable)
}

fn run_idle_monitor(
    timeout_ms: u32,
    ignore_inhibitors: bool,
    sender: mpsc::Sender<IdleEvent>,
    capability_sender: std_mpsc::SyncSender<IdleCapability>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let connection = Connection::connect_to_env()?;
    let mut event_queue = connection.new_event_queue();
    let queue_handle = event_queue.handle();
    connection.display().get_registry(&queue_handle, ());
    let mut state = IdleState {
        timeout_ms,
        ignore_inhibitors,
        sender,
        seat: None,
        notifier: None,
        notification: None,
        capability: IdleCapability::Unavailable,
    };
    event_queue.roundtrip(&mut state)?;
    state.create_notification(&queue_handle);
    capability_sender.send(state.capability).ok();
    if state.capability == IdleCapability::Unavailable {
        return Ok(());
    }
    loop {
        event_queue.blocking_dispatch(&mut state)?;
    }
}

struct IdleState {
    timeout_ms: u32,
    ignore_inhibitors: bool,
    sender: mpsc::Sender<IdleEvent>,
    seat: Option<wl_seat::WlSeat>,
    notifier: Option<ext_idle_notifier_v1::ExtIdleNotifierV1>,
    notification: Option<ext_idle_notification_v1::ExtIdleNotificationV1>,
    capability: IdleCapability,
}

impl IdleState {
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

impl Dispatch<ext_idle_notification_v1::ExtIdleNotificationV1, ()> for IdleState {
    fn event(
        state: &mut Self,
        _: &ext_idle_notification_v1::ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let event = match event {
            ext_idle_notification_v1::Event::Idled => Some(IdleEvent::Idled),
            ext_idle_notification_v1::Event::Resumed => Some(IdleEvent::Resumed),
            _ => None,
        };
        if let Some(event) = event {
            let _ = state.sender.blocking_send(event);
        }
    }
}

delegate_noop!(IdleState: ignore wl_seat::WlSeat);
delegate_noop!(IdleState: ignore ext_idle_notifier_v1::ExtIdleNotifierV1);
