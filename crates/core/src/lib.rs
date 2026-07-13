use std::{fmt, str::FromStr, time::Duration};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct DurationMs(pub u64);

impl DurationMs {
    pub const fn from_millis(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_millis(self) -> u64 {
        self.0
    }

    pub fn as_duration(self) -> Duration {
        Duration::from_millis(self.0)
    }
}

impl fmt::Display for DurationMs {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        humantime::format_duration(self.as_duration()).fmt(formatter)
    }
}

impl FromStr for DurationMs {
    type Err = humantime::DurationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let duration = humantime::parse_duration(value)?;
        Ok(Self(duration.as_millis().try_into().unwrap_or(u64::MAX)))
    }
}

impl Serialize for DurationMs {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for DurationMs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DurationVisitor;

        impl<'de> de::Visitor<'de> for DurationVisitor {
            type Value = DurationMs;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a duration string such as 10m or a millisecond integer")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                value.parse().map_err(E::custom)
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(DurationMs(value))
            }
        }

        deserializer.deserialize_any(DurationVisitor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BreakKind {
    Mini,
    Long,
}

impl fmt::Display for BreakKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mini => formatter.write_str("mini"),
            Self::Long => formatter.write_str("long"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StrictMode {
    Off,
    Delay,
    Entire,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DisplayMode {
    All,
    Focused,
    Cursor,
    Primary,
    Configured,
    DimAllContentOne,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContentSelector {
    Focused,
    Cursor,
    Primary,
    Configured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Layer {
    Overlay,
    Top,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PointerMode {
    Controls,
    Block,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeyboardMode {
    None,
    OnDemand,
    Exclusive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreakTiming {
    pub interval: DurationMs,
    pub duration: DurationMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LongBreakTiming {
    pub interval: DurationMs,
    pub duration: DurationMs,
    pub after_minis: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleConfig {
    pub mini: BreakTiming,
    pub long: LongBreakTiming,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionConfig {
    pub manual_resume: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationsConfig {
    pub enabled: bool,
    pub mini_lead: DurationMs,
    pub long_lead: DurationMs,
    pub actions: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostponeRule {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub duration: DurationMs,
    #[serde(default, alias = "max_count", skip_serializing_if = "Option::is_none")]
    pub max_postponements: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostponeConfig {
    pub mini: PostponeRule,
    pub long: PostponeRule,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkipRule {
    pub enabled: bool,
}

impl Default for SkipRule {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkipConfig {
    pub mini: SkipRule,
    pub long: SkipRule,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrictConfig {
    pub mode: StrictMode,
    pub minimum_visible: DurationMs,
    pub allow_postpone_during_lockout: bool,
    #[serde(default)]
    pub inhibit_shortcuts: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayConfig {
    pub mode: DisplayMode,
    pub content_selector: ContentSelector,
    pub preferred_monitor: Option<String>,
    pub primary_monitor: Option<String>,
    pub fallback: Vec<ContentSelector>,
    pub layer: Layer,
    pub opacity: f64,
    pub dim_color: String,
    pub pointer_mode: PointerMode,
    pub keyboard_mode: KeyboardMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentConfig {
    pub show_message: bool,
    pub messages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdleConfig {
    pub enabled: bool,
    pub reset_after: DurationMs,
    pub respect_idle_inhibitors: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MissedBreakPolicy {
    ShowOnce,
    Reset,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryConfig {
    pub wake_grace: DurationMs,
    pub missed_break: MissedBreakPolicy,
    pub suspend_counts_as_break: bool,
    pub lock_counts_as_break: bool,
    pub recover_active_break: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FullscreenBehavior {
    Show,
    NotifyOnly,
    Postpone,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FullscreenConfig {
    pub behavior: FullscreenBehavior,
    pub max_delay: DurationMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupConfig {
    pub start_paused: bool,
    pub recover_state: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyprlandConfig {
    pub enabled: bool,
    #[serde(default)]
    pub submap_fallback: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
    pub format: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrayConfig {
    pub enabled: bool,
}

impl Default for TrayConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppConfig {
    pub schema_version: u32,
    pub schedule: ScheduleConfig,
    #[serde(default)]
    pub completion: CompletionConfig,
    pub notifications: NotificationsConfig,
    #[serde(default)]
    pub skip: SkipConfig,
    pub postpone: PostponeConfig,
    pub strict: StrictConfig,
    pub display: DisplayConfig,
    pub content: ContentConfig,
    pub idle: IdleConfig,
    pub recovery: RecoveryConfig,
    pub fullscreen: FullscreenConfig,
    pub startup: StartupConfig,
    pub hyprland: HyprlandConfig,
    #[serde(default)]
    pub tray: TrayConfig,
    pub logging: LoggingConfig,
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockSample {
    pub monotonic_ms: u64,
    pub boottime_ms: u64,
    pub wall_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockIdentity {
    pub boot_id: String,
    pub sample: ClockSample,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BreakSessionId(pub Uuid);

impl BreakSessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for BreakSessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for BreakSessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DueBreakId(pub Uuid);

impl DueBreakId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for DueBreakId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorIdentity {
    pub connector: Option<String>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub description: Option<String>,
    pub physical_mm: Option<(u32, u32)>,
}

impl MonitorIdentity {
    pub fn stable_id(&self) -> Option<String> {
        match (&self.make, &self.model, &self.serial) {
            (Some(make), Some(model), Some(serial)) if !serial.is_empty() => {
                Some(format!("edid:{make}:{model}:{serial}"))
            }
            _ => self
                .connector
                .as_ref()
                .map(|connector| format!("connector:{connector}")),
        }
    }

    pub fn matches_selector(&self, selector: &str) -> bool {
        self.stable_id().as_deref() == Some(selector)
            || self
                .connector
                .as_ref()
                .is_some_and(|connector| selector == format!("connector:{connector}"))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputInfo {
    pub identity: MonitorIdentity,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub transform: i32,
    pub refresh_hz: f64,
    pub focused: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", content = "args", rename_all = "kebab-case")]
pub enum Command {
    Status,
    Pause { duration: Option<DurationMs> },
    Resume,
    ResumeBreak,
    Reset,
    Skip,
    Postpone,
    Mini,
    Long,
    Toggle,
    Reload,
    Outputs,
    Doctor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    pub request_id: Uuid,
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub version: u32,
    pub request_id: Uuid,
    pub ok: bool,
    pub message: String,
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlaySpec {
    pub session_id: BreakSessionId,
    pub kind: BreakKind,
    pub duration: DurationMs,
    pub strict_remaining: DurationMs,
    pub can_skip: bool,
    pub can_postpone: bool,
    pub manual_resume: bool,
    pub message: Option<String>,
    pub socket_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_accepts_human_text() {
        let value: DurationMs = serde_json::from_str("\"2m 5s\"").unwrap();
        assert_eq!(value.as_millis(), 125_000);
    }

    #[test]
    fn monitor_prefers_edid_identity() {
        let monitor = MonitorIdentity {
            connector: Some("DP-1".into()),
            make: Some("Example".into()),
            model: Some("Panel".into()),
            serial: Some("123".into()),
            description: None,
            physical_mm: None,
        };
        assert_eq!(
            monitor.stable_id().as_deref(),
            Some("edid:Example:Panel:123")
        );
    }
}
