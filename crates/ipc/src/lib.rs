use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use breakd_core::{Command, Request, Response};
use fs2::FileExt;
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot},
};
use uuid::Uuid;

pub const IPC_VERSION: u32 = 2;
const MAX_FRAME_SIZE: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("another breakd daemon is already running")]
    AlreadyRunning,
    #[error("daemon is unavailable at {path}: {source}")]
    Unavailable { path: PathBuf, source: io::Error },
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid IPC payload: {0}")]
    Json(#[from] serde_json::Error),
    #[error("IPC frame exceeds {MAX_FRAME_SIZE} bytes")]
    FrameTooLarge,
    #[error("IPC peer UID {peer_uid} does not match daemon UID {daemon_uid}")]
    Unauthorized { peer_uid: u32, daemon_uid: u32 },
    #[error("daemon request channel closed")]
    DaemonClosed,
}

#[derive(Debug)]
pub struct IncomingRequest {
    pub request: Request,
    pub respond_to: oneshot::Sender<Response>,
}

pub struct Server {
    listener: UnixListener,
    socket_path: PathBuf,
    _lock_file: File,
}

impl Server {
    pub fn bind(socket_path: impl AsRef<Path>) -> Result<Self, IpcError> {
        let socket_path = socket_path.as_ref().to_path_buf();
        let runtime_dir = socket_path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket has no parent"))?;
        fs::create_dir_all(runtime_dir)?;
        fs::set_permissions(runtime_dir, fs::Permissions::from_mode(0o700))?;

        let lock_path = runtime_dir.join("daemon.lock");
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600))?;
        lock_file
            .try_lock_exclusive()
            .map_err(|_| IpcError::AlreadyRunning)?;

        if socket_path.exists() {
            fs::remove_file(&socket_path)?;
        }
        let listener = UnixListener::bind(&socket_path)?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            listener,
            socket_path,
            _lock_file: lock_file,
        })
    }

    pub async fn run(self, sender: mpsc::Sender<IncomingRequest>) -> Result<(), IpcError> {
        loop {
            let (stream, _) = self.listener.accept().await?;
            let sender = sender.clone();
            tokio::spawn(async move {
                if let Err(error) = handle_connection(stream, sender).await {
                    tracing::warn!(%error, "IPC connection failed");
                }
            });
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}

pub async fn request(
    socket_path: impl AsRef<Path>,
    command: Command,
) -> Result<Response, IpcError> {
    let path = socket_path.as_ref();
    let mut stream = UnixStream::connect(path)
        .await
        .map_err(|source| IpcError::Unavailable {
            path: path.to_path_buf(),
            source,
        })?;
    let request = Request {
        version: IPC_VERSION,
        request_id: Uuid::new_v4(),
        command,
    };
    write_frame(&mut stream, &request).await?;
    read_frame(&mut stream).await
}

async fn handle_connection(
    mut stream: UnixStream,
    sender: mpsc::Sender<IncomingRequest>,
) -> Result<(), IpcError> {
    let credentials = stream.peer_cred()?;
    let peer_uid = credentials.uid();
    let daemon_uid = nix::unistd::Uid::effective().as_raw();
    if peer_uid != daemon_uid {
        return Err(IpcError::Unauthorized {
            peer_uid,
            daemon_uid,
        });
    }

    let request: Request = read_frame(&mut stream).await?;
    let request_id = request.request_id;
    if request.version != IPC_VERSION {
        write_frame(
            &mut stream,
            &Response {
                version: IPC_VERSION,
                request_id,
                ok: false,
                message: format!("unsupported IPC version {}", request.version),
                data: None,
            },
        )
        .await?;
        return Ok(());
    }

    let (respond_to, response) = oneshot::channel();
    sender
        .send(IncomingRequest {
            request,
            respond_to,
        })
        .await
        .map_err(|_| IpcError::DaemonClosed)?;
    let response = response.await.map_err(|_| IpcError::DaemonClosed)?;
    write_frame(&mut stream, &response).await
}

async fn read_frame<T>(stream: &mut UnixStream) -> Result<T, IpcError>
where
    T: serde::de::DeserializeOwned,
{
    let length = stream.read_u32().await? as usize;
    if length > MAX_FRAME_SIZE {
        return Err(IpcError::FrameTooLarge);
    }
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload).await?;
    Ok(serde_json::from_slice(&payload)?)
}

async fn write_frame<T>(stream: &mut UnixStream, value: &T) -> Result<(), IpcError>
where
    T: serde::Serialize,
{
    let payload = serde_json::to_vec(value)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(IpcError::FrameTooLarge);
    }
    stream.write_u32(payload.len() as u32).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use breakd_core::DurationMs;

    use super::*;

    #[tokio::test]
    async fn request_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("breakd/control.sock");
        let server = Server::bind(&socket).unwrap();
        let (sender, mut receiver) = mpsc::channel(4);
        let task = tokio::spawn(server.run(sender));

        let responder = tokio::spawn(async move {
            let incoming = receiver.recv().await.unwrap();
            assert_eq!(
                incoming.request.command,
                Command::Pause {
                    duration: Some(DurationMs::from_millis(1_000))
                }
            );
            incoming
                .respond_to
                .send(Response {
                    version: IPC_VERSION,
                    request_id: incoming.request.request_id,
                    ok: true,
                    message: "paused".into(),
                    data: None,
                })
                .unwrap();
        });

        let response = request(
            &socket,
            Command::Pause {
                duration: Some(DurationMs::from_millis(1_000)),
            },
        )
        .await
        .unwrap();
        assert!(response.ok);
        responder.await.unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn duplicate_server_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("breakd/control.sock");
        let _first = Server::bind(&socket).unwrap();
        assert!(matches!(
            Server::bind(&socket),
            Err(IpcError::AlreadyRunning)
        ));
    }
}
