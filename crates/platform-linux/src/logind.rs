use futures_util::StreamExt;
use tokio::sync::mpsc;
use zbus::{Connection, zvariant::OwnedObjectPath};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerEvent {
    PreparingForSleep,
    Resumed,
    Locked,
    Unlocked,
}

#[zbus::proxy(
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1",
    interface = "org.freedesktop.login1.Manager"
)]
trait LoginManager {
    fn get_session(&self, session_id: &str) -> zbus::Result<OwnedObjectPath>;

    fn get_session_by_pid(&self, pid: u32) -> zbus::Result<OwnedObjectPath>;

    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

#[zbus::proxy(
    default_service = "org.freedesktop.login1",
    interface = "org.freedesktop.login1.Session"
)]
trait LoginSession {
    #[zbus(property)]
    fn locked_hint(&self) -> zbus::Result<bool>;
}

pub async fn spawn_logind_monitor(sender: mpsc::Sender<PowerEvent>) -> zbus::Result<()> {
    let connection = Connection::system().await?;
    let manager = LoginManagerProxy::new(&connection).await?;
    let session_path = match std::env::var("XDG_SESSION_ID") {
        Ok(session_id) => manager.get_session(&session_id).await?,
        Err(_) => manager.get_session_by_pid(std::process::id()).await?,
    };
    let session = LoginSessionProxy::builder(&connection)
        .path(session_path)?
        .build()
        .await?;
    let mut sleep_events = manager.receive_prepare_for_sleep().await?;
    let mut locked_events = session.receive_locked_hint_changed().await;

    tokio::spawn(async move {
        loop {
            let event = tokio::select! {
                Some(signal) = sleep_events.next() => {
                    match signal.args() {
                        Ok(args) if args.start => Some(PowerEvent::PreparingForSleep),
                        Ok(_) => Some(PowerEvent::Resumed),
                        Err(error) => {
                            tracing::warn!(%error, "invalid logind sleep signal");
                            None
                        }
                    }
                }
                Some(change) = locked_events.next() => {
                    match change.get().await {
                        Ok(true) => Some(PowerEvent::Locked),
                        Ok(false) => Some(PowerEvent::Unlocked),
                        Err(error) => {
                            tracing::warn!(%error, "invalid logind lock state");
                            None
                        }
                    }
                },
                else => break,
            };
            if let Some(event) = event
                && sender.send(event).await.is_err()
            {
                break;
            }
        }
    });
    Ok(())
}
