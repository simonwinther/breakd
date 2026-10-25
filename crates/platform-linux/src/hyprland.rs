use std::{collections::HashSet, env, path::PathBuf};

use breakd_core::{MonitorIdentity, OutputInfo};
use serde::Deserialize;
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

#[derive(Debug, Error)]
pub enum HyprlandError {
    #[error("HYPRLAND_INSTANCE_SIGNATURE is unavailable")]
    MissingSignature,
    #[error("XDG_RUNTIME_DIR is unavailable")]
    MissingRuntimeDir,
    #[error("Hyprland IPC failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("Hyprland returned invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Hyprland command failed: {0}")]
    Command(String),
    #[error("invalid Hyprland submap name: {0}")]
    InvalidSubmap(String),
}

#[derive(Debug, Clone)]
pub struct HyprlandClient {
    command_socket: PathBuf,
    event_socket: PathBuf,
}

impl HyprlandClient {
    pub fn from_env() -> Result<Self, HyprlandError> {
        let runtime = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or(HyprlandError::MissingRuntimeDir)?;
        let signature =
            env::var_os("HYPRLAND_INSTANCE_SIGNATURE").ok_or(HyprlandError::MissingSignature)?;
        let base = runtime.join("hypr").join(signature);
        Ok(Self {
            command_socket: base.join(".socket.sock"),
            event_socket: base.join(".socket2.sock"),
        })
    }

    pub fn available(&self) -> bool {
        self.command_socket.exists() && self.event_socket.exists()
    }

    pub fn event_socket(&self) -> &PathBuf {
        &self.event_socket
    }

    pub async fn outputs(&self) -> Result<Vec<OutputInfo>, HyprlandError> {
        let monitors: Vec<HyprMonitor> = self.query_json("monitors all").await?;
        Ok(monitors.into_iter().map(OutputInfo::from).collect())
    }

    pub async fn cursor_position(&self) -> Result<(i32, i32), HyprlandError> {
        let cursor: CursorPosition = self.query_json("cursorpos").await?;
        Ok((cursor.x.round() as i32, cursor.y.round() as i32))
    }

    pub async fn locked(&self) -> Result<bool, HyprlandError> {
        #[derive(Deserialize)]
        struct Locked {
            locked: bool,
        }
        Ok(self.query_json::<Locked>("locked").await?.locked)
    }

    /// Whether an enabled, awake output currently displays a fullscreen window.
    /// Inspect all visible workspaces, including special workspaces, rather than
    /// only the focused window so a presentation on a second display is covered.
    pub async fn fullscreen_active(&self) -> Result<bool, HyprlandError> {
        let monitors: Vec<FullscreenMonitor> = self.query_json("monitors").await?;
        let visible_workspaces: HashSet<i64> = monitors
            .into_iter()
            .filter(|monitor| !monitor.disabled && monitor.dpms_status)
            .flat_map(|monitor| {
                [monitor.active_workspace, monitor.special_workspace]
                    .into_iter()
                    .flatten()
                    .map(|workspace| workspace.id)
                    .filter(|id| *id != 0)
            })
            .collect();
        if visible_workspaces.is_empty() {
            return Ok(false);
        }
        let clients: Vec<FullscreenWindow> = self.query_json("clients").await?;
        Ok(clients.into_iter().any(|window| {
            window.mapped
                && !window.hidden
                && visible_workspaces.contains(&window.workspace.id)
                // Hyprland's internal state is a bitmask: 1 is maximized,
                // 2 is fullscreen, 3 is both. Client-only fullscreen does not
                // cover the display and should not defer breaks.
                && window.fullscreen & 2 != 0
        }))
    }

    pub async fn current_submap(&self) -> Result<String, HyprlandError> {
        self.query_json("submap").await
    }

    pub async fn submap_exists(&self, name: &str) -> Result<bool, HyprlandError> {
        validate_submap_name(name)?;
        let bindings: Vec<HyprBind> = self.query_json("binds").await?;
        Ok(bindings.iter().any(|binding| binding.submap == name))
    }

    pub async fn ensure_submap(&self, name: &str) -> Result<(), HyprlandError> {
        validate_submap_name(name)?;
        if self.submap_exists(name).await? {
            return Ok(());
        }

        match self.command(&format!("keyword submap {name}")).await {
            Ok(_) => {
                let setup_result = self
                    .command("keyword bind CTRL_ALT_SHIFT_SUPER, F24, exec, true")
                    .await;
                // Always leave the legacy parser's binding context, including
                // when registering the sentinel bind fails.
                let reset_result = self.command("keyword submap reset").await;
                setup_result?;
                reset_result?;
            }
            Err(HyprlandError::Command(message))
                if message.contains("keyword can't work with non-legacy parsers") =>
            {
                // Hyprland 0.55+ can use Lua instead of the legacy parser.
                // define_submap scopes the bind without changing the active
                // input submap. Names are validated before interpolation.
                self.command(&format!(
                    "eval hl.define_submap(\"{name}\", function() \
                     hl.bind(\"CTRL + ALT + SHIFT + SUPER + F24\", function() end) end)"
                ))
                .await?;
            }
            Err(error) => return Err(error),
        }

        if !self.submap_exists(name).await? {
            return Err(HyprlandError::Command(format!(
                "submap {name} was not registered"
            )));
        }
        Ok(())
    }

    pub async fn set_submap(&self, name: &str) -> Result<(), HyprlandError> {
        validate_submap_name(name)?;
        match self.command(&format!("dispatch submap {name}")).await {
            Ok(_) => {}
            Err(HyprlandError::Command(message))
                if message.contains("dispatch in lua is a shorthand for hl.dispatch") =>
            {
                self.command(&format!("dispatch hl.dsp.submap(\"{name}\")"))
                    .await?;
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }

    pub async fn reset_submap_if_active(&self, name: &str) -> Result<bool, HyprlandError> {
        validate_submap_name(name)?;
        if self.current_submap().await? != name {
            return Ok(false);
        }
        self.set_submap("reset").await?;
        Ok(true)
    }

    pub async fn query_json<T>(&self, command: &str) -> Result<T, HyprlandError>
    where
        T: serde::de::DeserializeOwned,
    {
        let response = self.exchange(&format!("j/{command}")).await?;
        Ok(serde_json::from_slice(&response)?)
    }

    async fn exchange(&self, command: &str) -> Result<Vec<u8>, HyprlandError> {
        tokio::time::timeout(std::time::Duration::from_millis(500), async {
            let mut stream = UnixStream::connect(&self.command_socket).await?;
            stream.write_all(command.as_bytes()).await?;
            stream.shutdown().await?;
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await?;
            Ok::<_, std::io::Error>(response)
        })
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Hyprland IPC response timed out",
            )
        })?
        .map_err(HyprlandError::from)
    }

    async fn command(&self, command: &str) -> Result<String, HyprlandError> {
        let response = String::from_utf8(self.exchange(command).await?)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let response = response.trim().to_owned();
        if response.is_empty() || response.lines().any(|line| line.trim() != "ok") {
            return Err(HyprlandError::Command(response));
        }
        Ok(response)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HyprMonitor {
    name: String,
    description: String,
    make: String,
    model: String,
    serial: String,
    width: u32,
    height: u32,
    physical_width: u32,
    physical_height: u32,
    refresh_rate: f64,
    x: i32,
    y: i32,
    scale: f64,
    transform: i32,
    focused: bool,
    disabled: bool,
}

impl From<HyprMonitor> for OutputInfo {
    fn from(monitor: HyprMonitor) -> Self {
        Self {
            identity: MonitorIdentity {
                connector: Some(monitor.name),
                make: nonempty(monitor.make),
                model: nonempty(monitor.model),
                serial: nonempty(monitor.serial),
                description: nonempty(monitor.description),
                physical_mm: Some((monitor.physical_width, monitor.physical_height)),
            },
            width: monitor.width,
            height: monitor.height,
            x: monitor.x,
            y: monitor.y,
            scale: monitor.scale,
            transform: monitor.transform,
            refresh_hz: monitor.refresh_rate,
            focused: monitor.focused,
            enabled: !monitor.disabled,
        }
    }
}

#[derive(Debug, Deserialize)]
struct CursorPosition {
    x: f64,
    y: f64,
}

#[derive(Debug, Deserialize)]
struct HyprBind {
    submap: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FullscreenMonitor {
    active_workspace: Option<WorkspaceIdentity>,
    special_workspace: Option<WorkspaceIdentity>,
    #[serde(default)]
    disabled: bool,
    #[serde(default = "default_true")]
    dpms_status: bool,
}

#[derive(Debug, Deserialize)]
struct WorkspaceIdentity {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct FullscreenWindow {
    workspace: WorkspaceIdentity,
    mapped: bool,
    hidden: bool,
    fullscreen: u32,
}

fn default_true() -> bool {
    true
}

fn validate_submap_name(name: &str) -> Result<(), HyprlandError> {
    if !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        Ok(())
    } else {
        Err(HyprlandError::InvalidSubmap(name.into()))
    }
}

fn nonempty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use tokio::net::UnixListener;

    use super::*;

    #[tokio::test]
    async fn unresponsive_compositor_has_a_bounded_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("hypr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let client = HyprlandClient {
            command_socket: socket,
            event_socket: directory.path().join("events.sock"),
        };
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(2), client.current_submap())
                .await
                .expect("IPC timeout must keep the daemon responsive");
        assert!(matches!(result, Err(HyprlandError::Io(error))
            if error.kind() == std::io::ErrorKind::TimedOut));
        server.abort();
    }

    fn mock_client(
        exchanges: Vec<(&'static str, &'static str)>,
    ) -> (
        tempfile::TempDir,
        HyprlandClient,
        tokio::task::JoinHandle<()>,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("hypr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let client = HyprlandClient {
            command_socket: socket,
            event_socket: directory.path().join("events.sock"),
        };
        let task = tokio::spawn(async move {
            for (expected, response) in exchanges {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = String::new();
                stream.read_to_string(&mut request).await.unwrap();
                assert_eq!(request, expected);
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (directory, client, task)
    }

    #[test]
    fn validates_submap_names_before_ipc_use() {
        assert!(validate_submap_name("breakd").is_ok());
        assert!(validate_submap_name("break-reminder_2").is_ok());
        assert!(validate_submap_name("").is_err());
        assert!(validate_submap_name("breakd;dispatch workspace 1").is_err());
    }

    #[tokio::test]
    async fn finds_fullscreen_on_an_unfocused_monitor_or_special_workspace() {
        let (_directory, client, server) = mock_client(vec![
            (
                "j/monitors",
                r#"[{"activeWorkspace":{"id":1},"focused":true},{"activeWorkspace":{"id":2},"specialWorkspace":{"id":-99},"focused":false}]"#,
            ),
            (
                "j/clients",
                r#"[{"workspace":{"id":-99},"mapped":true,"hidden":false,"fullscreen":3}]"#,
            ),
        ]);

        assert!(client.fullscreen_active().await.unwrap());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn ignores_fullscreen_on_inactive_disabled_or_sleeping_workspaces() {
        let (_directory, client, server) = mock_client(vec![
            (
                "j/monitors",
                r#"[{"activeWorkspace":{"id":1}},{"activeWorkspace":{"id":2},"disabled":true},{"activeWorkspace":{"id":3},"dpmsStatus":false}]"#,
            ),
            (
                "j/clients",
                r#"[{"workspace":{"id":2},"mapped":true,"hidden":false,"fullscreen":2},{"workspace":{"id":3},"mapped":true,"hidden":false,"fullscreen":2},{"workspace":{"id":4},"mapped":true,"hidden":false,"fullscreen":2}]"#,
            ),
        ]);

        assert!(!client.fullscreen_active().await.unwrap());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn ignores_maximized_hidden_and_unmapped_windows() {
        let (_directory, client, server) = mock_client(vec![
            ("j/monitors", r#"[{"activeWorkspace":{"id":1}}]"#),
            (
                "j/clients",
                r#"[{"workspace":{"id":1},"mapped":true,"hidden":false,"fullscreen":1},{"workspace":{"id":1},"mapped":true,"hidden":true,"fullscreen":2},{"workspace":{"id":1},"mapped":false,"hidden":false,"fullscreen":2},{"workspace":{"id":1},"mapped":true,"hidden":false,"fullscreen":0,"fullscreenClient":2}]"#,
            ),
        ]);

        assert!(!client.fullscreen_active().await.unwrap());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn no_visible_workspaces_does_not_query_clients() {
        let (_directory, client, server) = mock_client(vec![("j/monitors", "[]")]);
        assert!(!client.fullscreen_active().await.unwrap());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_fullscreen_response_reports_an_error() {
        let (_directory, client, server) = mock_client(vec![("j/monitors", "not JSON")]);
        assert!(matches!(
            client.fullscreen_active().await,
            Err(HyprlandError::Json(_))
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn registers_submap_with_lua_parser() {
        let (_directory, client, server) = mock_client(vec![
            ("j/binds", "[]"),
            (
                "keyword submap breakd",
                "keyword can't work with non-legacy parsers. Use eval.",
            ),
            (
                "eval hl.define_submap(\"breakd\", function() hl.bind(\"CTRL + ALT + SHIFT + SUPER + F24\", function() end) end)",
                "ok",
            ),
            ("j/binds", r#"[{"submap":"breakd"}]"#),
        ]);

        client.ensure_submap("breakd").await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn registers_submap_with_legacy_parser() {
        let (_directory, client, server) = mock_client(vec![
            ("j/binds", "[]"),
            ("keyword submap breakd", "ok"),
            ("keyword bind CTRL_ALT_SHIFT_SUPER, F24, exec, true", "ok"),
            ("keyword submap reset", "ok"),
            ("j/binds", r#"[{"submap":"breakd"}]"#),
        ]);

        client.ensure_submap("breakd").await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn resets_legacy_binding_context_after_registration_failure() {
        let (_directory, client, server) = mock_client(vec![
            ("j/binds", "[]"),
            ("keyword submap breakd", "ok"),
            (
                "keyword bind CTRL_ALT_SHIFT_SUPER, F24, exec, true",
                "invalid bind",
            ),
            ("keyword submap reset", "ok"),
        ]);

        assert!(matches!(
            client.ensure_submap("breakd").await,
            Err(HyprlandError::Command(message)) if message == "invalid bind"
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn resets_stale_submap_with_lua_parser() {
        let (_directory, client, server) = mock_client(vec![
            ("j/submap", r#""breakd""#),
            (
                "dispatch submap reset",
                "error: [string \"return hl.dispatch(submap reset)\"]:1: ')' expected near 'reset'\n\n → Note: dispatch in lua is a shorthand for hl.dispatch(...), your syntax might need to be updated.",
            ),
            ("dispatch hl.dsp.submap(\"reset\")", "ok"),
        ]);

        assert!(client.reset_submap_if_active("breakd").await.unwrap());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn keeps_legacy_dispatch_errors() {
        let (_directory, client, server) =
            mock_client(vec![("dispatch submap breakd", "Cannot find submap")]);

        assert!(matches!(
            client.set_submap("breakd").await,
            Err(HyprlandError::Command(message)) if message == "Cannot find submap"
        ));
        server.await.unwrap();
    }
}
