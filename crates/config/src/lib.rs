use std::{
    env, fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use breakd_core::{
    AppConfig, BreakTiming, CompletionConfig, ContentConfig, ContentSelector, DisplayConfig,
    DisplayMode, DurationMs, FullscreenBehavior, FullscreenConfig, HyprlandConfig, IdleConfig,
    KeyboardMode, Layer, LoggingConfig, LongBreakTiming, MissedBreakPolicy, NotificationsConfig,
    PointerMode, PostponeConfig, PostponeRule, RecoveryConfig, ScheduleConfig, SkipConfig,
    SkipRule, StartupConfig, StrictConfig, StrictMode, TrayConfig,
};
use nix::unistd::Uid;
use thiserror::Error;

mod messages;

pub const CONFIG_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("failed to serialize configuration: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("failed to write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid configuration: {0}")]
    Validation(String),
}

pub fn defaults() -> AppConfig {
    AppConfig {
        schema_version: CONFIG_SCHEMA_VERSION,
        schedule: ScheduleConfig {
            mini: BreakTiming {
                interval: DurationMs::from_millis(10 * 60 * 1_000),
                duration: DurationMs::from_millis(20 * 1_000),
            },
            long: LongBreakTiming {
                interval: DurationMs::from_millis(30 * 60 * 1_000),
                duration: DurationMs::from_millis(5 * 60 * 1_000),
                after_minis: 2,
            },
        },
        completion: CompletionConfig {
            manual_resume: false,
        },
        notifications: NotificationsConfig {
            enabled: true,
            mini_lead: DurationMs::from_millis(10 * 1_000),
            long_lead: DurationMs::from_millis(30 * 1_000),
            actions: true,
        },
        skip: SkipConfig {
            mini: SkipRule { enabled: true },
            long: SkipRule { enabled: true },
        },
        postpone: PostponeConfig {
            mini: PostponeRule {
                enabled: true,
                duration: DurationMs::from_millis(2 * 60 * 1_000),
                max_postponements: None,
            },
            long: PostponeRule {
                enabled: true,
                duration: DurationMs::from_millis(5 * 60 * 1_000),
                max_postponements: None,
            },
        },
        strict: StrictConfig {
            mode: StrictMode::Delay,
            minimum_visible: DurationMs::from_millis(10 * 1_000),
            allow_postpone_during_lockout: false,
            inhibit_shortcuts: true,
        },
        display: DisplayConfig {
            mode: DisplayMode::DimAllContentOne,
            content_selector: ContentSelector::Focused,
            preferred_monitor: None,
            primary_monitor: None,
            fallback: vec![
                ContentSelector::Focused,
                ContentSelector::Cursor,
                ContentSelector::Primary,
            ],
            layer: Layer::Overlay,
            opacity: 0.88,
            dim_color: "#101418".into(),
            pointer_mode: PointerMode::Block,
            keyboard_mode: KeyboardMode::OnDemand,
        },
        content: ContentConfig {
            show_message: true,
            messages: messages::defaults(),
        },
        idle: IdleConfig {
            enabled: true,
            reset_after: DurationMs::from_millis(5 * 60 * 1_000),
            respect_idle_inhibitors: false,
        },
        recovery: RecoveryConfig {
            wake_grace: DurationMs::from_millis(3 * 1_000),
            missed_break: MissedBreakPolicy::ShowOnce,
            suspend_counts_as_break: true,
            lock_counts_as_break: true,
            recover_active_break: true,
        },
        fullscreen: FullscreenConfig {
            behavior: FullscreenBehavior::Show,
            max_delay: DurationMs::from_millis(10 * 60 * 1_000),
        },
        startup: StartupConfig {
            start_paused: false,
            recover_state: true,
        },
        hyprland: HyprlandConfig {
            enabled: true,
            submap_fallback: true,
        },
        tray: TrayConfig { enabled: true },
        logging: LoggingConfig {
            level: "info".into(),
            format: "journald".into(),
        },
    }
}

pub fn config_path() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
        .join("breakd/config.toml")
}

pub fn state_path() -> PathBuf {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local/state"))
        .join("breakd/state.json")
}

pub fn runtime_dir() -> PathBuf {
    env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", Uid::effective())))
        .join("breakd")
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("control.sock")
}

pub fn load() -> Result<AppConfig, ConfigError> {
    load_from(config_path())
}

pub fn load_from(path: PathBuf) -> Result<AppConfig, ConfigError> {
    if !path.exists() {
        let config = defaults();
        validate(&config)?;
        return Ok(config);
    }

    let source = fs::read_to_string(&path).map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })?;
    let config = toml::from_str(&source).map_err(|source| ConfigError::Parse {
        path: path.clone(),
        source,
    })?;
    validate(&config)?;
    Ok(config)
}

pub fn save(config: &AppConfig) -> Result<(), ConfigError> {
    save_to(&config_path(), config)
}

pub fn save_to(path: &Path, config: &AppConfig) -> Result<(), ConfigError> {
    validate(config)?;
    let mut encoded = toml::to_string_pretty(config)?;
    if !encoded.ends_with('\n') {
        encoded.push('\n');
    }

    let parent = path.parent().ok_or_else(|| {
        ConfigError::Validation("configuration path has no parent directory".into())
    })?;
    fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
        path: parent.to_path_buf(),
        source,
    })?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(|source| {
        ConfigError::Write {
            path: parent.to_path_buf(),
            source,
        }
    })?;

    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    temporary
        .write_all(encoded.as_bytes())
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    temporary
        .persist(path)
        .map_err(|error| ConfigError::Write {
            path: path.to_path_buf(),
            source: error.error,
        })?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
        ConfigError::Write {
            path: path.to_path_buf(),
            source,
        }
    })?;
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| ConfigError::Write {
            path: parent.to_path_buf(),
            source,
        })
}

pub fn validate(config: &AppConfig) -> Result<(), ConfigError> {
    if config.schema_version != CONFIG_SCHEMA_VERSION {
        return Err(ConfigError::Validation(format!(
            "unsupported schema_version {}; expected {CONFIG_SCHEMA_VERSION}",
            config.schema_version
        )));
    }
    if config.schedule.mini.interval.as_millis() == 0
        || config.schedule.mini.duration.as_millis() == 0
    {
        return Err(ConfigError::Validation(
            "mini interval and duration must be positive".into(),
        ));
    }
    if config.schedule.mini.duration >= config.schedule.mini.interval {
        return Err(ConfigError::Validation(
            "mini duration must be shorter than mini interval".into(),
        ));
    }
    if config.schedule.long.interval.as_millis() == 0
        || config.schedule.long.duration.as_millis() == 0
        || config.schedule.long.after_minis == 0
    {
        return Err(ConfigError::Validation(
            "long interval, duration, and after_minis must be positive".into(),
        ));
    }
    if config.notifications.mini_lead >= config.schedule.mini.interval
        || config.notifications.long_lead >= config.schedule.long.interval
    {
        return Err(ConfigError::Validation(
            "notification lead must be shorter than its interval".into(),
        ));
    }
    for (name, rule) in [
        ("postpone.mini", &config.postpone.mini),
        ("postpone.long", &config.postpone.long),
    ] {
        if rule.enabled && rule.duration.as_millis() == 0 {
            return Err(ConfigError::Validation(format!(
                "{name}.duration must be positive when postponement is enabled"
            )));
        }
        if rule.enabled && rule.max_postponements == Some(0) {
            return Err(ConfigError::Validation(format!(
                "{name}.max_postponements must be positive when postponement is enabled"
            )));
        }
    }
    if !(0.0..=1.0).contains(&config.display.opacity) {
        return Err(ConfigError::Validation(
            "display.opacity must be between 0 and 1".into(),
        ));
    }
    if config.display.dim_color.len() != 7
        || !config.display.dim_color.starts_with('#')
        || !config.display.dim_color[1..]
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err(ConfigError::Validation(
            "display.dim_color must be a #RRGGBB value".into(),
        ));
    }
    if config.fullscreen.behavior != FullscreenBehavior::Show {
        return Err(ConfigError::Validation(
            "only fullscreen.behavior = \"show\" is supported in this release".into(),
        ));
    }
    if !matches!(
        config.logging.level.as_str(),
        "error" | "warn" | "info" | "debug" | "trace"
    ) {
        return Err(ConfigError::Validation(
            "logging.level must be error, warn, info, debug, or trace".into(),
        ));
    }
    if !matches!(
        config.logging.format.as_str(),
        "journald" | "compact" | "json"
    ) {
        return Err(ConfigError::Validation(
            "logging.format must be journald, compact, or json".into(),
        ));
    }
    Ok(())
}

pub fn example_toml() -> String {
    toml::to_string_pretty(&defaults()).expect("default configuration is serializable")
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_round_trips_through_toml() {
        let encoded = example_toml();
        let decoded: AppConfig = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded, defaults());
    }

    #[test]
    fn checked_in_example_matches_defaults() {
        let decoded: AppConfig =
            toml::from_str(include_str!("../../../config.example.toml")).unwrap();
        assert_eq!(decoded, defaults());
    }

    #[test]
    fn invalid_opacity_is_rejected() {
        let mut config = defaults();
        config.display.opacity = 1.1;
        assert!(validate(&config).is_err());
    }

    #[test]
    fn invalid_dim_color_is_rejected() {
        let mut config = defaults();
        config.display.dim_color = "#nothex".into();
        assert!(validate(&config).is_err());
    }

    #[test]
    fn missing_file_uses_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let config = load_from(directory.path().join("missing.toml")).unwrap();
        assert_eq!(config, defaults());
    }

    #[test]
    fn unsupported_fullscreen_policy_is_rejected() {
        let mut config = defaults();
        config.fullscreen.behavior = FullscreenBehavior::Postpone;
        assert!(validate(&config).is_err());
    }

    #[test]
    fn old_postpone_rules_remain_enabled() {
        let mut value: toml::Value = toml::from_str(&example_toml()).unwrap();
        let root = value.as_table_mut().unwrap();
        root.remove("completion");
        root.remove("tray");
        root.remove("skip");
        root["postpone"]["mini"]
            .as_table_mut()
            .unwrap()
            .remove("enabled");
        root["postpone"]["long"]
            .as_table_mut()
            .unwrap()
            .remove("enabled");
        root["strict"]
            .as_table_mut()
            .unwrap()
            .remove("inhibit_shortcuts");
        root["hyprland"]
            .as_table_mut()
            .unwrap()
            .remove("submap_fallback");
        let source = toml::to_string(&value).unwrap();
        let decoded: AppConfig = toml::from_str(&source).unwrap();
        assert!(decoded.postpone.mini.enabled);
        assert!(decoded.postpone.long.enabled);
        assert_eq!(decoded.postpone.mini.max_postponements, None);
        assert_eq!(decoded.postpone.long.max_postponements, None);
        assert!(decoded.skip.mini.enabled);
        assert!(decoded.skip.long.enabled);
        assert!(!decoded.strict.inhibit_shortcuts);
        assert!(!decoded.hyprland.submap_fallback);
        assert!(!decoded.completion.manual_resume);
        assert!(decoded.tray.enabled);
    }

    #[test]
    fn old_max_count_name_is_accepted() {
        let mut value: toml::Value = toml::from_str(&example_toml()).unwrap();
        value["postpone"]["mini"]
            .as_table_mut()
            .unwrap()
            .insert("max_count".into(), toml::Value::Integer(2));

        let source = toml::to_string(&value).unwrap();
        let decoded: AppConfig = toml::from_str(&source).unwrap();
        assert_eq!(decoded.postpone.mini.max_postponements, Some(2));
    }

    #[test]
    fn save_is_atomic_private_and_loadable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("breakd/config.toml");
        let config = defaults();

        save_to(&path, &config).unwrap();

        assert_eq!(load_from(path.clone()).unwrap(), config);
        let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn invalid_config_does_not_replace_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = defaults();
        save_to(&path, &original).unwrap();
        let original_bytes = fs::read(&path).unwrap();

        let mut invalid = original;
        invalid.display.opacity = 2.0;
        assert!(save_to(&path, &invalid).is_err());
        assert_eq!(fs::read(path).unwrap(), original_bytes);
    }
}
