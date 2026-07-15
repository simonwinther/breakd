use std::fmt;

use breakd_core::{BreakKind, BreakSessionId, Command, DueBreakId, DurationMs};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CoopRole {
    Host,
    Guest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", content = "args", rename_all = "kebab-case")]
pub enum CoopAction {
    Pause { duration_ms: Option<u64> },
    Resume,
    ResumeBreak,
    Reset,
    Skip,
    Postpone,
    Mini,
    Long,
    Rest,
    Toggle,
}

impl CoopAction {
    pub fn from_command(command: &Command) -> Option<Self> {
        match command {
            Command::Pause { duration } => Some(Self::Pause {
                duration_ms: duration.map(DurationMs::as_millis),
            }),
            Command::Resume => Some(Self::Resume),
            Command::ResumeBreak => Some(Self::ResumeBreak),
            Command::Reset => Some(Self::Reset),
            Command::Skip => Some(Self::Skip),
            Command::Postpone => Some(Self::Postpone),
            Command::Mini => Some(Self::Mini),
            Command::Long => Some(Self::Long),
            Command::Rest => Some(Self::Rest),
            Command::Toggle => Some(Self::Toggle),
            Command::Status
            | Command::Reload
            | Command::Outputs
            | Command::Doctor
            | Command::CoopHost { .. }
            | Command::CoopJoin { .. }
            | Command::CoopLeave
            | Command::CoopStatus => None,
        }
    }

    pub fn into_command(self) -> Command {
        match self {
            Self::Pause { duration_ms } => Command::Pause {
                duration: duration_ms.map(DurationMs::from_millis),
            },
            Self::Resume => Command::Resume,
            Self::ResumeBreak => Command::ResumeBreak,
            Self::Reset => Command::Reset,
            Self::Skip => Command::Skip,
            Self::Postpone => Command::Postpone,
            Self::Mini => Command::Mini,
            Self::Long => Command::Long,
            Self::Rest => Command::Rest,
            Self::Toggle => Command::Toggle,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoopSnapshot {
    pub host_id: Uuid,
    pub revision: u64,
    pub generated_unix_ms: u64,
    pub paused: bool,
    pub resume_at_unix_ms: Option<u64>,
    pub phase: CoopPhase,
    pub minis_since_long: u32,
    pub longs_since_rest: u32,
    pub postpone_count: u32,
    pub can_skip: bool,
    pub can_postpone: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<CoopPolicy>,
}

/// Host-owned behavior that affects when or how every participant takes a
/// break. Presentation-only settings such as monitor selection, colors,
/// opacity, messages, and completion sounds intentionally stay local.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoopPolicy {
    pub notifications_enabled: bool,
    pub mini_notification_lead_ms: u64,
    pub long_notification_lead_ms: u64,
    pub rest_notification_lead_ms: u64,
    pub allow_postpone_during_lockout: bool,
    pub inhibit_shortcuts: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "kebab-case")]
pub enum CoopPhase {
    Working { next: ScheduledBreak },
    Break { active: SharedBreak },
    Unavailable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledBreak {
    pub due_id: DueBreakId,
    pub kind: BreakKind,
    pub starts_unix_ms: u64,
    pub duration_ms: u64,
    pub strict_duration_ms: u64,
    pub strict_entire: bool,
    pub manual_resume: bool,
    pub can_skip: bool,
    pub can_postpone: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedBreak {
    pub due_id: DueBreakId,
    pub session_id: BreakSessionId,
    pub kind: BreakKind,
    pub started_unix_ms: u64,
    pub ends_unix_ms: u64,
    pub strict_until_unix_ms: u64,
    pub strict_entire: bool,
    pub manual_resume: bool,
    pub completion_sound_emitted: bool,
    pub can_skip: bool,
    pub can_postpone: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ClientMessage {
    Hello {
        version: u32,
        role: CoopRole,
        client_id: Uuid,
    },
    Snapshot {
        snapshot: CoopSnapshot,
    },
    ActionRequest {
        request_id: Uuid,
        action: CoopAction,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ServerMessage {
    Ready {
        host_present: bool,
        guest_count: usize,
    },
    Presence {
        host_present: bool,
        guest_count: usize,
    },
    Snapshot {
        snapshot: CoopSnapshot,
    },
    ActionRequest {
        request_id: Uuid,
        action: CoopAction,
    },
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    relay_url: String,
    room_token: String,
}

impl Invite {
    pub fn new(
        relay_url: impl Into<String>,
        room_token: impl Into<String>,
    ) -> Result<Self, InviteError> {
        let relay_url = relay_url.into();
        let room_token = room_token.into().to_ascii_lowercase();
        if !valid_relay_url(&relay_url) {
            return Err(InviteError::RelayUrl);
        }
        if !valid_room_token(&room_token) {
            return Err(InviteError::RoomToken);
        }
        Ok(Self {
            relay_url,
            room_token,
        })
    }

    pub fn parse(value: &str) -> Result<Self, InviteError> {
        let (relay_url, fragment) = value.rsplit_once('#').ok_or(InviteError::Fragment)?;
        let room_token = fragment
            .strip_prefix("breakd=")
            .ok_or(InviteError::Fragment)?;
        Self::new(relay_url, room_token)
    }

    pub fn relay_url(&self) -> &str {
        &self.relay_url
    }

    pub fn room_token(&self) -> &str {
        &self.room_token
    }
}

impl fmt::Display for Invite {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}#breakd={}", self.relay_url, self.room_token)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum InviteError {
    #[error("relay must be a ws:// or wss:// URL without a fragment")]
    RelayUrl,
    #[error("room token must contain exactly 32 hexadecimal characters")]
    RoomToken,
    #[error("invite must end in #breakd=<room-token>")]
    Fragment,
}

pub fn valid_relay_url(value: &str) -> bool {
    let Some(rest) = value
        .strip_prefix("ws://")
        .or_else(|| value.strip_prefix("wss://"))
    else {
        return false;
    };
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    !authority.is_empty()
        && !authority.starts_with(':')
        && !authority.contains('@')
        && !value.contains('#')
        && !value.chars().any(char::is_whitespace)
}

pub fn valid_room_token(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn invite_round_trips_without_putting_the_token_in_the_request_url() {
        let invite = Invite::new("wss://relay.example/ws", TOKEN).unwrap();
        assert_eq!(
            invite.to_string(),
            format!("wss://relay.example/ws#breakd={TOKEN}")
        );
        let parsed = Invite::parse(&invite.to_string()).unwrap();
        assert_eq!(parsed.relay_url(), "wss://relay.example/ws");
        assert!(!parsed.relay_url().contains(TOKEN));
        assert_eq!(parsed.room_token(), TOKEN);
    }

    #[test]
    fn malformed_invites_are_rejected() {
        assert_eq!(
            Invite::parse("https://relay.example/ws#breakd=bad"),
            Err(InviteError::RelayUrl)
        );
        assert_eq!(
            Invite::parse("wss://relay.example/ws"),
            Err(InviteError::Fragment)
        );
        assert_eq!(
            Invite::parse("wss://relay.example/ws#token=abc"),
            Err(InviteError::Fragment)
        );
        assert_eq!(
            Invite::parse("ws:///ws#breakd=0123456789abcdef0123456789abcdef"),
            Err(InviteError::RelayUrl)
        );
        assert_eq!(
            Invite::parse(
                "wss://user:password@relay.example/ws#breakd=0123456789abcdef0123456789abcdef"
            ),
            Err(InviteError::RelayUrl)
        );
    }

    #[test]
    fn supported_commands_round_trip_as_actions() {
        let command = Command::Pause {
            duration: Some(DurationMs::from_millis(42_000)),
        };
        assert_eq!(
            CoopAction::from_command(&command).unwrap().into_command(),
            command
        );
        assert!(CoopAction::from_command(&Command::Status).is_none());
        assert_eq!(
            CoopAction::from_command(&Command::ResumeBreak)
                .unwrap()
                .into_command(),
            Command::ResumeBreak
        );
    }

    #[test]
    fn snapshots_without_the_optional_policy_remain_compatible() {
        let snapshot = CoopSnapshot {
            host_id: Uuid::nil(),
            revision: 1,
            generated_unix_ms: 10,
            paused: false,
            resume_at_unix_ms: None,
            phase: CoopPhase::Unavailable {
                reason: "test".into(),
            },
            minis_since_long: 0,
            longs_since_rest: 0,
            postpone_count: 0,
            can_skip: false,
            can_postpone: false,
            policy: None,
        };
        let encoded = serde_json::to_value(&snapshot).unwrap();
        assert!(encoded.get("policy").is_none());
        assert_eq!(
            serde_json::from_value::<CoopSnapshot>(encoded).unwrap(),
            snapshot
        );
    }
}
