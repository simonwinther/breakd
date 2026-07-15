use std::time::Duration;

use anyhow::{Context, Result, bail};
use breakd_coop::{
    ClientMessage, CoopAction, CoopRole, CoopSnapshot, Invite, PROTOCOL_VERSION, ServerMessage,
};
use breakd_core::{CoopConfig, CoopMode};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{Instant, MissedTickBehavior, interval_at, sleep},
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};
use uuid::Uuid;

const MAX_MESSAGE_BYTES: usize = 128 * 1024;

#[derive(Debug)]
pub struct CoopEvent {
    generation: u64,
    kind: CoopEventKind,
}

#[derive(Debug)]
enum CoopEventKind {
    Connected,
    Disconnected(String),
    Presence {
        host_present: bool,
        guest_count: usize,
    },
    Snapshot(CoopSnapshot),
    ActionRequest {
        request_id: Uuid,
        action: CoopAction,
    },
    Error(String),
}

#[derive(Debug)]
pub enum AcceptedEvent {
    StateChanged,
    Snapshot(CoopSnapshot),
    ActionRequest {
        request_id: Uuid,
        action: CoopAction,
    },
    Error(String),
    Stale,
}

pub struct CoopRuntime {
    config: CoopConfig,
    generation: u64,
    event_sender: mpsc::Sender<CoopEvent>,
    task: Option<JoinHandle<()>>,
    snapshot_sender: watch::Sender<Option<CoopSnapshot>>,
    action_sender: mpsc::Sender<(Uuid, CoopAction, Instant)>,
    connected: bool,
    host_present: bool,
    guest_count: usize,
    started_at: Instant,
    last_snapshot_at: Option<Instant>,
    last_snapshot_version: Option<(Uuid, u64)>,
    disconnect_reason: Option<String>,
    was_holding_local: bool,
    host_id: Uuid,
    next_revision: u64,
}

impl CoopRuntime {
    pub fn new(config: CoopConfig, event_sender: mpsc::Sender<CoopEvent>) -> Self {
        let (snapshot_sender, _) = watch::channel(None);
        let (action_sender, _) = mpsc::channel(1);
        let mut runtime = Self {
            config: CoopConfig::default(),
            generation: 0,
            event_sender,
            task: None,
            snapshot_sender,
            action_sender,
            connected: false,
            host_present: false,
            guest_count: 0,
            started_at: Instant::now(),
            last_snapshot_at: None,
            last_snapshot_version: None,
            disconnect_reason: None,
            was_holding_local: false,
            host_id: Uuid::new_v4(),
            next_revision: 0,
        };
        runtime.reconfigure(config);
        runtime
    }

    pub fn reconfigure(&mut self, config: CoopConfig) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.generation = self.generation.wrapping_add(1);
        self.config = config;
        self.connected = false;
        self.host_present = false;
        self.guest_count = 0;
        self.started_at = Instant::now();
        self.last_snapshot_at = None;
        self.last_snapshot_version = None;
        self.disconnect_reason = None;
        self.was_holding_local = self.config.mode == CoopMode::Guest;
        self.host_id = Uuid::new_v4();
        self.next_revision = 0;

        let (snapshot_sender, snapshot_receiver) = watch::channel(None);
        let (action_sender, action_receiver) = mpsc::channel(32);
        self.snapshot_sender = snapshot_sender;
        self.action_sender = action_sender;

        let Some(role) = self.role() else {
            return;
        };
        let (Some(relay_url), Some(room_token)) = (
            self.config.relay_url.clone(),
            self.config.room_token.clone(),
        ) else {
            self.disconnect_reason = Some("co-op configuration is incomplete".into());
            return;
        };
        let generation = self.generation;
        let events = self.event_sender.clone();
        self.task = Some(tokio::spawn(connection_loop(
            ConnectionConfig {
                relay_url,
                room_token,
                role,
                client_id: Uuid::new_v4(),
            },
            generation,
            events,
            snapshot_receiver,
            action_receiver,
        )));
    }

    pub fn accept(&mut self, event: CoopEvent) -> AcceptedEvent {
        if event.generation != self.generation {
            return AcceptedEvent::Stale;
        }
        match event.kind {
            CoopEventKind::Connected => {
                self.connected = true;
                self.disconnect_reason = None;
                AcceptedEvent::StateChanged
            }
            CoopEventKind::Disconnected(reason) => {
                self.connected = false;
                self.host_present = false;
                self.disconnect_reason = Some(reason);
                AcceptedEvent::StateChanged
            }
            CoopEventKind::Presence {
                host_present,
                guest_count,
            } => {
                self.host_present = host_present;
                self.guest_count = guest_count;
                AcceptedEvent::StateChanged
            }
            CoopEventKind::Snapshot(snapshot) => {
                let version = (snapshot.host_id, snapshot.revision);
                let is_newer = self
                    .last_snapshot_version
                    .is_none_or(|(host_id, revision)| {
                        host_id != snapshot.host_id || snapshot.revision > revision
                    });
                if !is_newer || self.config.mode != CoopMode::Guest {
                    return AcceptedEvent::Stale;
                }
                self.last_snapshot_version = Some(version);
                self.last_snapshot_at = Some(Instant::now());
                self.host_present = true;
                AcceptedEvent::Snapshot(snapshot)
            }
            CoopEventKind::ActionRequest { request_id, action } => {
                if self.config.mode == CoopMode::Host {
                    AcceptedEvent::ActionRequest { request_id, action }
                } else {
                    AcceptedEvent::Stale
                }
            }
            CoopEventKind::Error(error) => AcceptedEvent::Error(error),
        }
    }

    pub fn role(&self) -> Option<CoopRole> {
        match self.config.mode {
            CoopMode::Off => None,
            CoopMode::Host => Some(CoopRole::Host),
            CoopMode::Guest => Some(CoopRole::Guest),
        }
    }

    pub fn is_host(&self) -> bool {
        self.config.mode == CoopMode::Host
    }

    pub fn holds_local_schedule(&self) -> bool {
        if self.config.mode != CoopMode::Guest {
            return false;
        }
        self.last_snapshot_at.map_or_else(
            || self.started_at.elapsed() <= self.config.disconnect_grace.as_duration(),
            |last| last.elapsed() <= self.config.disconnect_grace.as_duration(),
        )
    }

    pub fn has_fresh_snapshot(&self) -> bool {
        self.config.mode == CoopMode::Guest
            && self
                .last_snapshot_at
                .is_some_and(|last| last.elapsed() <= self.config.disconnect_grace.as_duration())
    }

    pub fn take_fallback_transition(&mut self) -> bool {
        let holding = self.holds_local_schedule();
        let transitioned = self.was_holding_local && !holding;
        self.was_holding_local = holding;
        transitioned
    }

    pub fn publish(&self, snapshot: CoopSnapshot) {
        if self.is_host() {
            self.snapshot_sender.send_replace(Some(snapshot));
        }
    }

    pub fn next_snapshot_identity(&mut self) -> (Uuid, u64) {
        let revision = self.next_revision;
        self.next_revision = self.next_revision.wrapping_add(1);
        (self.host_id, revision)
    }

    pub fn request_action(&self, action: CoopAction) -> Result<Uuid> {
        if self.config.mode != CoopMode::Guest {
            bail!("co-op actions can only be forwarded by a guest");
        }
        if !self.connected || !self.host_present {
            bail!("co-op host is not connected");
        }
        let request_id = Uuid::new_v4();
        self.action_sender
            .try_send((request_id, action, Instant::now()))
            .context("co-op action queue is full")?;
        Ok(request_id)
    }

    pub fn status_json(&self) -> serde_json::Value {
        let invite = match (
            self.config.mode,
            self.config.relay_url.as_deref(),
            self.config.room_token.as_deref(),
        ) {
            (CoopMode::Host, Some(relay), Some(token)) => Invite::new(relay, token)
                .ok()
                .map(|invite| invite.to_string()),
            _ => None,
        };
        json!({
            "mode": match self.config.mode {
                CoopMode::Off => "off",
                CoopMode::Host => "host",
                CoopMode::Guest => "guest",
            },
            "relay_url": self.config.relay_url,
            "connected": self.connected,
            "host_present": self.host_present,
            "guest_count": self.guest_count,
            "following_host": self.has_fresh_snapshot(),
            "disconnect_grace_ms": self.config.disconnect_grace.as_millis(),
            "last_snapshot_age_ms": self.last_snapshot_at.map(|at| {
                u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX)
            }),
            "last_error": self.disconnect_reason,
            "invite": invite,
        })
    }
}

impl Drop for CoopRuntime {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

struct ConnectionConfig {
    relay_url: String,
    room_token: String,
    role: CoopRole,
    client_id: Uuid,
}

async fn connection_loop(
    config: ConnectionConfig,
    generation: u64,
    events: mpsc::Sender<CoopEvent>,
    mut snapshots: watch::Receiver<Option<CoopSnapshot>>,
    mut actions: mpsc::Receiver<(Uuid, CoopAction, Instant)>,
) {
    let mut retry = Duration::from_secs(1);
    loop {
        let mut established = false;
        let result = connect_once(
            &config,
            generation,
            &events,
            &mut snapshots,
            &mut actions,
            &mut established,
        )
        .await;
        let reason = result
            .err()
            .map_or_else(|| "connection closed".into(), |error| format!("{error:#}"));
        let _ = events
            .send(CoopEvent {
                generation,
                kind: CoopEventKind::Disconnected(reason),
            })
            .await;
        if established {
            retry = Duration::from_secs(1);
        }
        sleep(retry).await;
        if !established {
            retry = (retry * 2).min(Duration::from_secs(30));
        }
    }
}

async fn connect_once(
    config: &ConnectionConfig,
    generation: u64,
    events: &mpsc::Sender<CoopEvent>,
    snapshots: &mut watch::Receiver<Option<CoopSnapshot>>,
    actions: &mut mpsc::Receiver<(Uuid, CoopAction, Instant)>,
    established: &mut bool,
) -> Result<()> {
    let mut request = config
        .relay_url
        .as_str()
        .into_client_request()
        .context("invalid relay URL")?;
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", config.room_token))
            .context("invalid room token")?,
    );
    let (mut socket, _) = connect_async(request)
        .await
        .context("connect to co-op relay")?;
    send_json(
        &mut socket,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            role: config.role,
            client_id: config.client_id,
        },
    )
    .await?;
    let initial_snapshot = snapshots.borrow().clone();
    if config.role == CoopRole::Host
        && let Some(snapshot) = initial_snapshot
    {
        send_json(&mut socket, &ClientMessage::Snapshot { snapshot }).await?;
    }
    let _ = events
        .send(CoopEvent {
            generation,
            kind: CoopEventKind::Connected,
        })
        .await;
    *established = true;

    let mut heartbeat = interval_at(
        Instant::now() + Duration::from_secs(20),
        Duration::from_secs(20),
    );
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            incoming = socket.next() => {
                let message = incoming.context("relay closed the WebSocket")??;
                match message {
                    Message::Text(text) => {
                        if text.len() > MAX_MESSAGE_BYTES {
                            bail!("relay message exceeds {MAX_MESSAGE_BYTES} bytes");
                        }
                        let message: ServerMessage = serde_json::from_str(text.as_ref())
                            .context("invalid relay message")?;
                        let kind = match message {
                            ServerMessage::Ready { host_present, guest_count }
                            | ServerMessage::Presence { host_present, guest_count } => {
                                CoopEventKind::Presence { host_present, guest_count }
                            }
                            ServerMessage::Snapshot { snapshot } => CoopEventKind::Snapshot(snapshot),
                            ServerMessage::ActionRequest { request_id, action } => {
                                CoopEventKind::ActionRequest { request_id, action }
                            }
                            ServerMessage::Error { code, message } => {
                                CoopEventKind::Error(format!("{code}: {message}"))
                            }
                        };
                        let _ = events.send(CoopEvent { generation, kind }).await;
                    }
                    Message::Ping(payload) => socket.send(Message::Pong(payload)).await?,
                    Message::Close(_) => return Ok(()),
                    Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
                }
            }
            changed = snapshots.changed(), if config.role == CoopRole::Host => {
                changed.context("co-op snapshot publisher stopped")?;
                let latest_snapshot = snapshots.borrow().clone();
                if let Some(snapshot) = latest_snapshot {
                    send_json(&mut socket, &ClientMessage::Snapshot { snapshot }).await?;
                }
            }
            action = actions.recv(), if config.role == CoopRole::Guest => {
                let (request_id, action, queued_at) = action.context("co-op action publisher stopped")?;
                if queued_at.elapsed() > Duration::from_secs(2) {
                    let _ = events
                        .send(CoopEvent {
                            generation,
                            kind: CoopEventKind::Error(format!(
                                "action {request_id} expired while reconnecting"
                            )),
                        })
                        .await;
                    continue;
                }
                send_json(&mut socket, &ClientMessage::ActionRequest { request_id, action }).await?;
            }
            _ = heartbeat.tick() => {
                socket.send(Message::Ping(Vec::new().into())).await?;
            }
        }
    }
}

async fn send_json<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
    message: &ClientMessage,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let encoded = serde_json::to_string(message)?;
    socket.send(Message::Text(encoded.into())).await?;
    Ok(())
}
