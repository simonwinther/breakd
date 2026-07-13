use std::{env, path::PathBuf};

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

    pub async fn query_json<T>(&self, command: &str) -> Result<T, HyprlandError>
    where
        T: serde::de::DeserializeOwned,
    {
        let mut stream = UnixStream::connect(&self.command_socket).await?;
        stream.write_all(format!("j/{command}").as_bytes()).await?;
        stream.shutdown().await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        Ok(serde_json::from_slice(&response)?)
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

fn nonempty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}
