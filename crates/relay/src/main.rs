use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use breakd_coop::{ClientMessage, CoopRole, PROTOCOL_VERSION, ServerMessage, valid_room_token};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use tokio::{net::TcpListener, sync::mpsc, time::timeout};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{ErrorResponse, Request, Response},
        http::{HeaderValue, StatusCode},
    },
};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

const MAX_MESSAGE_BYTES: usize = 128 * 1024;

#[derive(Debug, Parser)]
#[command(
    name = "breakd-relay",
    version,
    about = "Small room relay for breakd co-op"
)]
struct Arguments {
    /// Address to listen on. Put a TLS reverse proxy in front for public use.
    #[arg(long, default_value = "127.0.0.1:8787")]
    listen: SocketAddr,
    /// Maximum host and guest connections in one room.
    #[arg(long, default_value_t = 8)]
    max_room_size: usize,
    /// Maximum number of simultaneously live rooms.
    #[arg(long, default_value_t = 256)]
    max_rooms: usize,
}

#[derive(Default)]
struct Room {
    host: Option<Peer>,
    guests: HashMap<Uuid, Peer>,
    latest_snapshot: Option<breakd_coop::CoopSnapshot>,
}

#[derive(Clone)]
struct Peer {
    connection_id: Uuid,
    sender: mpsc::Sender<Message>,
}

type Rooms = Arc<tokio::sync::Mutex<HashMap<String, Room>>>;

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    let arguments = Arguments::parse();
    if !(2..=64).contains(&arguments.max_room_size) {
        bail!("--max-room-size must be between 2 and 64");
    }
    if !(1..=65_536).contains(&arguments.max_rooms) {
        bail!("--max-rooms must be between 1 and 65536");
    }
    let listener = TcpListener::bind(arguments.listen)
        .await
        .with_context(|| format!("bind {}", arguments.listen))?;
    tracing::info!(listen = %arguments.listen, "co-op relay listening");
    let rooms = Rooms::default();

    loop {
        let (stream, remote) = listener.accept().await?;
        let rooms = rooms.clone();
        let max_room_size = arguments.max_room_size;
        let max_rooms = arguments.max_rooms;
        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, rooms, max_room_size, max_rooms).await {
                tracing::debug!(%remote, %error, "co-op connection ended");
            }
        });
    }
}

#[allow(clippy::result_large_err)] // tungstenite's handshake callback owns this response type.
async fn handle_connection(
    stream: tokio::net::TcpStream,
    rooms: Rooms,
    max_room_size: usize,
    max_rooms: usize,
) -> Result<()> {
    let token_slot = Arc::new(std::sync::Mutex::new(None));
    let callback_slot = token_slot.clone();
    let mut socket =
        accept_hdr_async(
            stream,
            move |request: &Request, response: Response| match bearer_token(request) {
                Some(token) => {
                    *callback_slot.lock().expect("token mutex poisoned") = Some(token);
                    Ok(response)
                }
                None => Err(handshake_error(
                    StatusCode::UNAUTHORIZED,
                    "missing or invalid bearer token",
                )),
            },
        )
        .await
        .context("WebSocket handshake failed")?;
    let room_token = token_slot
        .lock()
        .expect("token mutex poisoned")
        .take()
        .context("authenticated room token was not retained")?;

    let hello = timeout(Duration::from_secs(5), socket.next())
        .await
        .context("hello timed out")?
        .context("connection closed before hello")??;
    let (role, _client_id) = match parse_client_message(hello)? {
        ClientMessage::Hello {
            version,
            role,
            client_id,
        } if version == PROTOCOL_VERSION => (role, client_id),
        ClientMessage::Hello { version, .. } => {
            send_server(
                &mut socket,
                &ServerMessage::Error {
                    code: "protocol-version".into(),
                    message: format!(
                        "client protocol {version} is unsupported; expected {PROTOCOL_VERSION}"
                    ),
                },
            )
            .await?;
            bail!("unsupported protocol version {version}");
        }
        _ => bail!("the first message must be hello"),
    };

    let connection_id = Uuid::new_v4();
    let (outgoing_sender, mut outgoing_receiver) = mpsc::channel(32);
    let peer = Peer {
        connection_id,
        sender: outgoing_sender.clone(),
    };
    let initial =
        match register_peer(&rooms, &room_token, role, peer, max_room_size, max_rooms).await {
            Ok(initial) => initial,
            Err(error) => {
                send_server(
                    &mut socket,
                    &ServerMessage::Error {
                        code: "room-rejected".into(),
                        message: error.to_string(),
                    },
                )
                .await?;
                return Err(error);
            }
        };
    for message in initial {
        let _ = outgoing_sender.try_send(server_message(&message)?);
    }
    broadcast_presence(&rooms, &room_token).await;

    let (mut sink, mut source) = socket.split();
    let result = async {
        loop {
            tokio::select! {
                incoming = source.next() => {
                    let message = incoming.context("WebSocket closed")??;
                    match message {
                        Message::Text(text) => {
                            if text.len() > MAX_MESSAGE_BYTES {
                                bail!("message exceeds {MAX_MESSAGE_BYTES} bytes");
                            }
                            let message: ClientMessage = serde_json::from_str(text.as_ref())
                                .context("invalid client message")?;
                            route_message(
                                &rooms,
                                &room_token,
                                connection_id,
                                role,
                                message,
                                &outgoing_sender,
                            ).await?;
                        }
                        Message::Ping(payload) => sink.send(Message::Pong(payload)).await?,
                        Message::Close(_) => break,
                        Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
                    }
                }
                outgoing = outgoing_receiver.recv() => {
                    let Some(message) = outgoing else { break; };
                    sink.send(message).await?;
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;

    remove_peer(&rooms, &room_token, connection_id, role).await;
    broadcast_presence(&rooms, &room_token).await;
    result
}

async fn register_peer(
    rooms: &Rooms,
    room_token: &str,
    role: CoopRole,
    peer: Peer,
    max_room_size: usize,
    max_rooms: usize,
) -> Result<Vec<ServerMessage>> {
    let mut rooms = rooms.lock().await;
    if role == CoopRole::Guest && !rooms.contains_key(room_token) {
        bail!("room host is not connected");
    }
    if !rooms.contains_key(room_token) && rooms.len() >= max_rooms {
        bail!("relay room limit reached");
    }
    let room = rooms.entry(room_token.to_owned()).or_default();
    let size = room.guests.len() + usize::from(room.host.is_some());
    if size >= max_room_size {
        bail!("room is full");
    }
    match role {
        CoopRole::Host if room.host.is_some() => bail!("room already has a host"),
        CoopRole::Host => room.host = Some(peer),
        CoopRole::Guest => {
            room.guests.insert(peer.connection_id, peer);
        }
    }
    let mut messages = vec![ServerMessage::Ready {
        host_present: room.host.is_some(),
        guest_count: room.guests.len(),
    }];
    if role == CoopRole::Guest
        && let Some(snapshot) = room.latest_snapshot.clone()
    {
        messages.push(ServerMessage::Snapshot { snapshot });
    }
    Ok(messages)
}

async fn route_message(
    rooms: &Rooms,
    room_token: &str,
    connection_id: Uuid,
    role: CoopRole,
    message: ClientMessage,
    own_sender: &mpsc::Sender<Message>,
) -> Result<()> {
    match (role, message) {
        (CoopRole::Host, ClientMessage::Snapshot { snapshot }) => {
            let recipients = {
                let mut rooms = rooms.lock().await;
                let room = rooms.get_mut(room_token).context("room disappeared")?;
                if room.host.as_ref().map(|peer| peer.connection_id) != Some(connection_id) {
                    bail!("connection is no longer the room host");
                }
                room.latest_snapshot = Some(snapshot.clone());
                room.guests
                    .values()
                    .map(|peer| peer.sender.clone())
                    .collect::<Vec<_>>()
            };
            let message = server_message(&ServerMessage::Snapshot { snapshot })?;
            for recipient in recipients {
                let _ = recipient.try_send(message.clone());
            }
        }
        (CoopRole::Guest, ClientMessage::ActionRequest { request_id, action }) => {
            let host = rooms
                .lock()
                .await
                .get(room_token)
                .and_then(|room| room.host.as_ref())
                .map(|peer| peer.sender.clone());
            if let Some(host) = host {
                host.try_send(server_message(&ServerMessage::ActionRequest {
                    request_id,
                    action,
                })?)
                .context("host outbound queue is full")?;
            } else {
                let _ = own_sender.try_send(server_message(&ServerMessage::Error {
                    code: "host-unavailable".into(),
                    message: "the room host is not connected".into(),
                })?);
            }
        }
        (_, ClientMessage::Hello { .. }) => {
            send_protocol_error(own_sender, "hello may only be sent once")?;
        }
        (CoopRole::Host, ClientMessage::ActionRequest { .. }) => {
            send_protocol_error(own_sender, "hosts cannot send action requests")?;
        }
        (CoopRole::Guest, ClientMessage::Snapshot { .. }) => {
            send_protocol_error(own_sender, "guests cannot publish snapshots")?;
        }
    }
    Ok(())
}

async fn remove_peer(rooms: &Rooms, room_token: &str, connection_id: Uuid, role: CoopRole) {
    let mut rooms = rooms.lock().await;
    let Some(room) = rooms.get_mut(room_token) else {
        return;
    };
    match role {
        CoopRole::Host
            if room.host.as_ref().map(|peer| peer.connection_id) == Some(connection_id) =>
        {
            room.host = None;
            room.latest_snapshot = None;
        }
        CoopRole::Guest => {
            room.guests.remove(&connection_id);
        }
        CoopRole::Host => {}
    }
    if room.host.is_none() && room.guests.is_empty() {
        rooms.remove(room_token);
    }
}

async fn broadcast_presence(rooms: &Rooms, room_token: &str) {
    let (message, recipients) = {
        let rooms = rooms.lock().await;
        let Some(room) = rooms.get(room_token) else {
            return;
        };
        let message = ServerMessage::Presence {
            host_present: room.host.is_some(),
            guest_count: room.guests.len(),
        };
        let recipients = room
            .host
            .iter()
            .chain(room.guests.values())
            .map(|peer| peer.sender.clone())
            .collect::<Vec<_>>();
        (message, recipients)
    };
    let Ok(message) = server_message(&message) else {
        return;
    };
    for recipient in recipients {
        let _ = recipient.try_send(message.clone());
    }
}

fn bearer_token(request: &Request) -> Option<String> {
    let value = request.headers().get("authorization")?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?;
    valid_room_token(token).then(|| token.to_ascii_lowercase())
}

fn handshake_error(status: StatusCode, message: &str) -> ErrorResponse {
    let mut response = ErrorResponse::new(Some(message.to_owned()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

fn parse_client_message(message: Message) -> Result<ClientMessage> {
    let Message::Text(text) = message else {
        bail!("client message must be JSON text");
    };
    if text.len() > MAX_MESSAGE_BYTES {
        bail!("message exceeds {MAX_MESSAGE_BYTES} bytes");
    }
    serde_json::from_str(text.as_ref()).context("invalid client message")
}

fn server_message(message: &ServerMessage) -> Result<Message> {
    Ok(Message::Text(serde_json::to_string(message)?.into()))
}

async fn send_server<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
    message: &ServerMessage,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    socket.send(server_message(message)?).await?;
    Ok(())
}

fn send_protocol_error(sender: &mpsc::Sender<Message>, message: &str) -> Result<()> {
    let _ = sender.try_send(server_message(&ServerMessage::Error {
        code: "protocol".into(),
        message: message.into(),
    })?);
    Ok(())
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("breakd_relay=info,warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

#[cfg(test)]
mod tests {
    use breakd_coop::{CoopAction, CoopPhase, CoopPolicy, CoopSnapshot, ScheduledBreak};
    use breakd_core::{BreakKind, DueBreakId};

    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn room_token_only_comes_from_the_authorization_header() {
        let request = Request::builder()
            .uri("/ws?room=not-a-room-token")
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(())
            .unwrap();
        assert_eq!(bearer_token(&request).as_deref(), Some(TOKEN));
    }

    #[tokio::test]
    async fn a_room_accepts_only_one_host() {
        let rooms = Rooms::default();
        let (sender, _) = mpsc::channel(4);
        let first = Peer {
            connection_id: Uuid::new_v4(),
            sender: sender.clone(),
        };
        register_peer(&rooms, TOKEN, CoopRole::Host, first, 8, 256)
            .await
            .unwrap();
        let second = Peer {
            connection_id: Uuid::new_v4(),
            sender,
        };
        assert!(
            register_peer(&rooms, TOKEN, CoopRole::Host, second, 8, 256)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn guests_cannot_allocate_rooms_without_a_host() {
        let rooms = Rooms::default();
        let (sender, _) = mpsc::channel(4);
        let guest = Peer {
            connection_id: Uuid::new_v4(),
            sender,
        };
        assert!(
            register_peer(&rooms, TOKEN, CoopRole::Guest, guest, 8, 256)
                .await
                .is_err()
        );
        assert!(rooms.lock().await.is_empty());
    }

    #[tokio::test]
    async fn snapshots_and_actions_only_travel_in_the_authoritative_direction() {
        let rooms = Rooms::default();
        let (host_sender, mut host_receiver) = mpsc::channel(4);
        let host_id = Uuid::new_v4();
        register_peer(
            &rooms,
            TOKEN,
            CoopRole::Host,
            Peer {
                connection_id: host_id,
                sender: host_sender.clone(),
            },
            8,
            256,
        )
        .await
        .unwrap();
        let (guest_sender, mut guest_receiver) = mpsc::channel(4);
        let guest_id = Uuid::new_v4();
        register_peer(
            &rooms,
            TOKEN,
            CoopRole::Guest,
            Peer {
                connection_id: guest_id,
                sender: guest_sender.clone(),
            },
            8,
            256,
        )
        .await
        .unwrap();

        let snapshot = CoopSnapshot {
            host_id,
            revision: 1,
            generated_unix_ms: 10,
            paused: false,
            resume_at_unix_ms: None,
            phase: CoopPhase::Working {
                next: ScheduledBreak {
                    due_id: DueBreakId(Uuid::new_v4()),
                    kind: BreakKind::Mini,
                    starts_unix_ms: 1_000,
                    duration_ms: 20_000,
                    strict_duration_ms: 0,
                    strict_entire: false,
                    manual_resume: false,
                    can_skip: true,
                    can_postpone: true,
                },
            },
            minis_since_long: 0,
            longs_since_rest: 0,
            postpone_count: 0,
            can_skip: false,
            can_postpone: false,
            policy: Some(CoopPolicy {
                notifications_enabled: true,
                mini_notification_lead_ms: 20_000,
                long_notification_lead_ms: 40_000,
                rest_notification_lead_ms: 60_000,
                allow_postpone_during_lockout: true,
                inhibit_shortcuts: true,
            }),
        };
        route_message(
            &rooms,
            TOKEN,
            host_id,
            CoopRole::Host,
            ClientMessage::Snapshot {
                snapshot: snapshot.clone(),
            },
            &host_sender,
        )
        .await
        .unwrap();
        let Message::Text(forwarded) = guest_receiver.recv().await.unwrap() else {
            panic!("snapshot was not forwarded as text");
        };
        assert_eq!(
            serde_json::from_str::<ServerMessage>(forwarded.as_ref()).unwrap(),
            ServerMessage::Snapshot { snapshot }
        );

        let request_id = Uuid::new_v4();
        route_message(
            &rooms,
            TOKEN,
            guest_id,
            CoopRole::Guest,
            ClientMessage::ActionRequest {
                request_id,
                action: CoopAction::Skip,
            },
            &guest_sender,
        )
        .await
        .unwrap();
        let Message::Text(forwarded) = host_receiver.recv().await.unwrap() else {
            panic!("action was not forwarded as text");
        };
        assert_eq!(
            serde_json::from_str::<ServerMessage>(forwarded.as_ref()).unwrap(),
            ServerMessage::ActionRequest {
                request_id,
                action: CoopAction::Skip,
            }
        );
    }
}
