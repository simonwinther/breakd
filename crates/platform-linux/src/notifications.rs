use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use breakd_core::DueBreakId;
use futures_util::StreamExt;
use tokio::{sync::mpsc, task::JoinHandle};
use zbus::{Connection, zvariant::Value};

const EXPIRATION_MS: i32 = 10_000;
const LISTENER_LIFETIME: Duration = Duration::from_secs(15);
const START_ACTION: &str = "start-break";
const POSTPONE_ACTION: &str = "postpone";

#[zbus::proxy(
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications",
    interface = "org.freedesktop.Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<&str>,
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn get_capabilities(&self) -> zbus::Result<Vec<String>>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

/// Actions retain the exact due break that produced their notification. The
/// scheduler must reject them once that due break is no longer current.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationAction {
    StartDue(DueBreakId),
    PostponeDue(DueBreakId),
}

#[derive(Debug)]
struct ActionListener {
    sender: mpsc::UnboundedSender<NotificationAction>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl ActionListener {
    fn cancel(&self) {
        if let Some(task) = self.task.lock().unwrap().take() {
            task.abort();
        }
    }
}

impl Drop for ActionListener {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Debug, Clone)]
pub struct NotificationClient {
    app_name: String,
    actions: Option<Arc<ActionListener>>,
}

impl Default for NotificationClient {
    fn default() -> Self {
        Self::new("breakd")
    }
}

impl NotificationClient {
    pub fn new(app_name: impl Into<String>) -> Self {
        Self {
            app_name: app_name.into(),
            actions: None,
        }
    }

    pub fn with_actions(
        app_name: impl Into<String>,
        sender: mpsc::UnboundedSender<NotificationAction>,
    ) -> Self {
        Self {
            app_name: app_name.into(),
            actions: Some(Arc::new(ActionListener {
                sender,
                task: Mutex::new(None),
            })),
        }
    }

    /// Informational notifications intentionally have no buttons.
    pub async fn notify(&self, summary: &str, body: &str) -> zbus::Result<u32> {
        let connection = Connection::session().await?;
        let proxy = NotificationsProxy::new(&connection).await?;
        self.send(&proxy, summary, body, Vec::new()).await
    }

    /// Send a due-break reminder with capability-aware buttons. There is at most
    /// one action listener, and it ends on dismissal, action, timeout or drop.
    pub async fn notify_break(
        &self,
        summary: &str,
        body: &str,
        due_id: DueBreakId,
        can_postpone: bool,
    ) -> zbus::Result<u32> {
        let connection = Connection::session().await?;
        self.notify_break_on(connection, summary, body, due_id, can_postpone)
            .await
    }

    pub fn cancel_actions(&self) {
        if let Some(listener) = &self.actions {
            listener.cancel();
        }
    }

    async fn notify_break_on(
        &self,
        connection: Connection,
        summary: &str,
        body: &str,
        due_id: DueBreakId,
        can_postpone: bool,
    ) -> zbus::Result<u32> {
        self.cancel_actions();
        let proxy = NotificationsProxy::new(&connection).await?;
        let Some(listener) = &self.actions else {
            return self.send(&proxy, summary, body, Vec::new()).await;
        };
        let supported = proxy
            .get_capabilities()
            .await
            .unwrap_or_default()
            .iter()
            .any(|capability| capability == "actions");
        if !supported || listener.sender.is_closed() {
            return self.send(&proxy, summary, body, Vec::new()).await;
        }

        // Subscribe before Notify: a fast notification server can emit an
        // action or close signal before its Notify reply reaches the client.
        let streams = tokio::try_join!(
            proxy.receive_action_invoked(),
            proxy.receive_notification_closed(),
        );
        let (mut actions, mut closed) = match streams {
            Ok(streams) => streams,
            Err(error) => {
                tracing::warn!(%error, "notification actions unavailable; sending plain reminder");
                return self.send(&proxy, summary, body, Vec::new()).await;
            }
        };
        let mut buttons = vec![START_ACTION, "Start break"];
        if can_postpone {
            buttons.extend([POSTPONE_ACTION, "Postpone"]);
        }
        let notification_id = self.send(&proxy, summary, body, buttons).await?;
        let sender = listener.sender.clone();
        let task = tokio::spawn(async move {
            // Keep this D-Bus connection alive for the registered signal
            // streams. Expiration also bounds servers that omit Closed.
            let _connection = connection;
            let lifetime = tokio::time::sleep(LISTENER_LIFETIME);
            tokio::pin!(lifetime);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut lifetime => break,
                    _ = sender.closed() => break,
                    signal = actions.next() => {
                        let Some(signal) = signal else { break };
                        match signal.args() {
                            Ok(args) if args.id == notification_id => {
                                if let Some(action) = action_for_key(
                                    args.action_key,
                                    due_id,
                                    can_postpone,
                                ) {
                                    let _ = sender.send(action);
                                    break;
                                }
                            }
                            Ok(_) => {}
                            Err(error) => tracing::warn!(%error, "invalid notification action"),
                        }
                    }
                    signal = closed.next() => {
                        let Some(signal) = signal else { break };
                        match signal.args() {
                            Ok(args) if args.id == notification_id => break,
                            Ok(_) => {}
                            Err(error) => tracing::warn!(%error, "invalid notification closure"),
                        }
                    }
                }
            }
        });
        if let Some(previous) = listener.task.lock().unwrap().replace(task) {
            previous.abort();
        }
        Ok(notification_id)
    }

    async fn send(
        &self,
        proxy: &NotificationsProxy<'_>,
        summary: &str,
        body: &str,
        actions: Vec<&str>,
    ) -> zbus::Result<u32> {
        proxy
            .notify(
                &self.app_name,
                0,
                "appointment-soon",
                summary,
                body,
                actions,
                HashMap::new(),
                EXPIRATION_MS,
            )
            .await
    }

    pub async fn capabilities(&self) -> zbus::Result<Vec<String>> {
        let connection = Connection::session().await?;
        NotificationsProxy::new(&connection)
            .await?
            .get_capabilities()
            .await
    }
}

fn action_for_key(key: &str, due_id: DueBreakId, can_postpone: bool) -> Option<NotificationAction> {
    match key {
        START_ACTION => Some(NotificationAction::StartDue(due_id)),
        POSTPONE_ACTION if can_postpone => Some(NotificationAction::PostponeDue(due_id)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use tokio::io::{AsyncBufReadExt, BufReader};
    use zbus::{connection::Builder, object_server::SignalEmitter, zvariant::OwnedValue};

    use super::*;

    struct PrivateBus {
        _process: tokio::process::Child,
        _directory: tempfile::TempDir,
        address: String,
    }

    impl PrivateBus {
        async fn start() -> Option<Self> {
            let directory = tempfile::tempdir().unwrap();
            let address = format!("unix:path={}", directory.path().join("bus").display());
            let process = tokio::process::Command::new("dbus-daemon")
                .args([
                    "--session",
                    "--nofork",
                    "--nopidfile",
                    "--print-address=1",
                    &format!("--address={address}"),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn();
            let mut process = match process {
                Ok(process) => process,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!("skipping private notification bus test: dbus-daemon unavailable");
                    return None;
                }
                Err(error) => panic!("cannot start private notification bus: {error}"),
            };
            let mut reader = BufReader::new(process.stdout.take().unwrap());
            let mut announced = String::new();
            tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut announced))
                .await
                .expect("private bus startup timed out")
                .unwrap();
            assert!(!announced.trim().is_empty());
            Some(Self {
                _process: process,
                _directory: directory,
                address: announced.trim().into(),
            })
        }

        async fn connect(&self) -> Connection {
            Builder::address(self.address.as_str())
                .unwrap()
                .method_timeout(Duration::from_secs(2))
                .build()
                .await
                .unwrap()
        }
    }

    struct FakeNotifications {
        supports_actions: bool,
        buttons: Arc<Mutex<Vec<String>>>,
        signals_before_reply: Vec<(u32, &'static str)>,
        close_before_reply: bool,
    }

    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl FakeNotifications {
        fn get_capabilities(&self) -> Vec<String> {
            if self.supports_actions {
                vec!["actions".into()]
            } else {
                Vec::new()
            }
        }

        #[allow(clippy::too_many_arguments)]
        async fn notify(
            &self,
            _app_name: &str,
            _replaces_id: u32,
            _app_icon: &str,
            _summary: &str,
            _body: &str,
            actions: Vec<String>,
            _hints: HashMap<String, OwnedValue>,
            _expire_timeout: i32,
            #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        ) -> zbus::fdo::Result<u32> {
            *self.buttons.lock().unwrap() = actions;
            for signal in &self.signals_before_reply {
                emitter
                    .emit("org.freedesktop.Notifications", "ActionInvoked", signal)
                    .await
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            }
            if self.close_before_reply {
                emitter
                    .emit(
                        "org.freedesktop.Notifications",
                        "NotificationClosed",
                        &(7_u32, 2_u32),
                    )
                    .await
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            }
            Ok(7)
        }
    }

    async fn serve(bus: &PrivateBus, notifications: FakeNotifications) -> Connection {
        Builder::address(bus.address.as_str())
            .unwrap()
            .name("org.freedesktop.Notifications")
            .unwrap()
            .serve_at("/org/freedesktop/Notifications", notifications)
            .unwrap()
            .build()
            .await
            .unwrap()
    }

    #[test]
    fn actions_preserve_the_due_break_identity() {
        let due_id = DueBreakId::new();
        assert_eq!(
            action_for_key(START_ACTION, due_id, false),
            Some(NotificationAction::StartDue(due_id))
        );
        assert_eq!(
            action_for_key(POSTPONE_ACTION, due_id, true),
            Some(NotificationAction::PostponeDue(due_id))
        );
    }

    #[test]
    fn ignores_unoffered_or_unknown_notification_actions() {
        let due_id = DueBreakId::new();
        assert_eq!(action_for_key(POSTPONE_ACTION, due_id, false), None);
        assert_eq!(action_for_key("default", due_id, true), None);
        assert_eq!(action_for_key("", due_id, true), None);
    }

    #[tokio::test]
    async fn receives_action_emitted_before_notify_reply_and_close() {
        let Some(bus) = PrivateBus::start().await else {
            return;
        };
        let buttons = Arc::new(Mutex::new(Vec::new()));
        let _server = serve(
            &bus,
            FakeNotifications {
                supports_actions: true,
                buttons: buttons.clone(),
                signals_before_reply: vec![(7, START_ACTION)],
                close_before_reply: true,
            },
        )
        .await;
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let notifications = NotificationClient::with_actions("test-breakd", sender);
        let due_id = DueBreakId::new();
        assert_eq!(
            notifications
                .notify_break_on(bus.connect().await, "Due", "Take a break", due_id, true)
                .await
                .unwrap(),
            7
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                .await
                .unwrap(),
            Some(NotificationAction::StartDue(due_id))
        );
        assert_eq!(
            *buttons.lock().unwrap(),
            [START_ACTION, "Start break", POSTPONE_ACTION, "Postpone"]
        );
    }

    #[tokio::test]
    async fn ignores_other_notifications_and_unoffered_postpone() {
        let Some(bus) = PrivateBus::start().await else {
            return;
        };
        let buttons = Arc::new(Mutex::new(Vec::new()));
        let _server = serve(
            &bus,
            FakeNotifications {
                supports_actions: true,
                buttons: buttons.clone(),
                signals_before_reply: vec![(999, START_ACTION), (7, POSTPONE_ACTION)],
                close_before_reply: true,
            },
        )
        .await;
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let notifications = NotificationClient::with_actions("test-breakd", sender);
        notifications
            .notify_break_on(
                bus.connect().await,
                "Due",
                "Take a break",
                DueBreakId::new(),
                false,
            )
            .await
            .unwrap();
        let task = notifications
            .actions
            .as_ref()
            .unwrap()
            .task
            .lock()
            .unwrap()
            .take()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(receiver.try_recv().is_err());
        assert_eq!(*buttons.lock().unwrap(), [START_ACTION, "Start break"]);
    }

    #[tokio::test]
    async fn unsupported_actions_fall_back_to_a_plain_notification() {
        let Some(bus) = PrivateBus::start().await else {
            return;
        };
        let buttons = Arc::new(Mutex::new(Vec::new()));
        let _server = serve(
            &bus,
            FakeNotifications {
                supports_actions: false,
                buttons: buttons.clone(),
                signals_before_reply: Vec::new(),
                close_before_reply: false,
            },
        )
        .await;
        let (sender, _receiver) = mpsc::unbounded_channel();
        let notifications = NotificationClient::with_actions("test-breakd", sender);
        notifications
            .notify_break_on(
                bus.connect().await,
                "Due",
                "Take a break",
                DueBreakId::new(),
                true,
            )
            .await
            .unwrap();
        assert!(buttons.lock().unwrap().is_empty());
        assert!(
            notifications
                .actions
                .as_ref()
                .unwrap()
                .task
                .lock()
                .unwrap()
                .is_none()
        );
    }
}
