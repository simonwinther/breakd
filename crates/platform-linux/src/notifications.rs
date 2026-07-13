use std::collections::HashMap;

use zbus::{Connection, zvariant::Value};

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
}

#[derive(Debug, Clone)]
pub struct NotificationClient {
    app_name: String,
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
        }
    }

    pub async fn notify(&self, summary: &str, body: &str) -> zbus::Result<u32> {
        let connection = Connection::session().await?;
        let proxy = NotificationsProxy::new(&connection).await?;
        proxy
            .notify(
                &self.app_name,
                0,
                "appointment-soon",
                summary,
                body,
                Vec::new(),
                HashMap::new(),
                10_000,
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
