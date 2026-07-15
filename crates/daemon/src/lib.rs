use std::{
    collections::VecDeque,
    ffi::OsString,
    fs,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{Context, Result};
use breakd_coop::{CoopAction, Invite};
use breakd_core::{
    BreakSessionId, Command, CompletionSound, CoopConfig, CoopMode, OverlaySpec, Response,
};
use breakd_ipc::{IPC_VERSION, IncomingRequest, Server};
use breakd_platform_linux::{
    EventSoundClient, HyprlandClient, IdleCapability, IdleEvent, LinuxClock, NotificationClient,
    PowerEvent, StateStore, probe_wayland_globals, spawn_idle_monitor, spawn_logind_monitor,
};
use breakd_scheduler::{Effect, Scheduler, SchedulerEvent, SchedulerStatus};
use breakd_tray::{TrayAction, TrayController, TrayState};
use tokio::{
    process::{Child, Command as TokioCommand},
    signal::unix::{SignalKind, signal},
    sync::mpsc,
    time::{Duration, Instant, interval},
};

mod coop;

use coop::{AcceptedEvent, CoopRuntime};

pub async fn run() -> Result<()> {
    let instance = breakd_config::RuntimeInstance::current();
    let mut config = breakd_config::load().context("load configuration")?;
    let socket_path = breakd_config::socket_path();
    let state_store = StateStore::new(breakd_config::state_path());
    let clock = LinuxClock;
    let now = clock.sample()?;
    let boot_id = clock.boot_id()?;
    let snapshot = if config.startup.recover_state {
        match state_store.load() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::error!(%error, "state is corrupt; starting a fresh schedule");
                let _ = state_store.quarantine_corrupt();
                None
            }
        }
    } else {
        None
    };
    let mut scheduler = match snapshot {
        Some(snapshot) => Scheduler::restore(
            config.clone(),
            boot_id,
            now,
            socket_path.display().to_string(),
            snapshot,
        ),
        None => Scheduler::new(
            config.clone(),
            boot_id,
            now,
            socket_path.display().to_string(),
        ),
    };
    state_store.save(&scheduler.snapshot())?;

    let (coop_event_sender, mut coop_event_receiver) = mpsc::channel(64);
    let mut coop = CoopRuntime::new(config.coop.clone(), coop_event_sender);

    let server = Server::bind(&socket_path)?;
    let (request_sender, mut request_receiver) = mpsc::channel::<IncomingRequest>(32);
    tokio::spawn(async move {
        if let Err(error) = server.run(request_sender).await {
            tracing::error!(%error, "IPC server stopped");
        }
    });

    let (power_sender, mut power_receiver) = mpsc::channel(16);
    if let Err(error) = spawn_logind_monitor(power_sender).await {
        tracing::warn!(%error, "logind integration unavailable");
    }
    let (idle_sender, mut idle_receiver) = mpsc::channel(16);
    let idle_capability = if config.idle.enabled {
        spawn_idle_monitor(
            config.idle.reset_after.as_millis(),
            !config.idle.respect_idle_inhibitors,
            idle_sender,
        )
    } else {
        IdleCapability::Unavailable
    };
    tracing::info!(?idle_capability, "idle capability detected");

    let notifications = NotificationClient::new(instance.name());
    let event_sounds = match EventSoundClient::new(&completion_sound_directory(instance)) {
        Ok(client) => Some(client),
        Err(error) => {
            tracing::warn!(%error, "event sound integration unavailable");
            None
        }
    };
    let mut overlay = OverlaySupervisor::default();
    apply_effects(
        if coop.holds_local_schedule() {
            Vec::new()
        } else {
            scheduler.startup_effects(now)
        },
        &mut overlay,
        &notifications,
        event_sounds.as_ref(),
        config.completion.sound,
    )
    .await;

    let (tray_sender, mut tray_receiver) = mpsc::unbounded_channel();
    let mut tray = TrayController::new(tray_sender, instance.name());
    let mut tray_enabled = config.tray.enabled;
    if let Err(error) = tray
        .set_enabled(tray_enabled, tray_state(scheduler.status(now)))
        .await
    {
        tracing::warn!(%error, "tray integration unavailable");
    }

    let mut shortcut_guard = HyprlandShortcutGuard::new(instance.hyprland_submap());
    shortcut_guard.initialize(&config).await;
    shortcut_guard
        .reconcile(
            &config,
            scheduler.shortcut_inhibition_enabled(),
            &scheduler.status(now),
        )
        .await;

    let mut ticker = interval(Duration::from_millis(250));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut recent_requests: VecDeque<(uuid::Uuid, Response)> = VecDeque::new();
    let mut terminate = signal(SignalKind::terminate()).context("install SIGTERM handler")?;
    let mut next_coop_publish = Instant::now();

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let now = clock.sample()?;
                if coop.take_fallback_transition() {
                    tracing::warn!("co-op host timed out; starting a fresh local schedule");
                    let effects = scheduler.reset_after_coop_disconnect(now);
                    state_store.save(&scheduler.snapshot())?;
                    apply_effects(
                        effects,
                        &mut overlay,
                        &notifications,
                        event_sounds.as_ref(),
                        config.completion.sound,
                    ).await;
                }
                if !coop.holds_local_schedule() || coop.has_fresh_snapshot() {
                    let previous = scheduler.state().clone();
                    let effects = scheduler.handle_event(SchedulerEvent::Tick, now);
                    if !coop.has_fresh_snapshot() {
                        persist_if_changed(&state_store, &scheduler, &previous)?;
                    }
                    apply_effects(
                        effects,
                        &mut overlay,
                        &notifications,
                        event_sounds.as_ref(),
                        config.completion.sound,
                    ).await;
                }
                if let Some((session_id, reason)) = overlay.poll_exit().await? {
                    let status = scheduler.status(now);
                    if status.active_session == Some(session_id)
                        && (status.remaining_ms.unwrap_or_default() > 1_000
                            || status.awaiting_resume)
                    {
                        let previous = scheduler.state().clone();
                        let effects = scheduler.handle_event(
                            SchedulerEvent::OverlayFailed { session_id, reason },
                            now,
                        );
                        persist_if_changed(&state_store, &scheduler, &previous)?;
                        apply_effects(
                            effects,
                            &mut overlay,
                            &notifications,
                            event_sounds.as_ref(),
                            config.completion.sound,
                        ).await;
                    }
                }
            }
            Some(incoming) = request_receiver.recv() => {
                if let Some((_, cached)) = recent_requests.iter().find(|(id, _)| *id == incoming.request.request_id) {
                    let _ = incoming.respond_to.send(cached.clone());
                    continue;
                }
                let response = handle_request(
                    &incoming,
                    &clock,
                    &state_store,
                    &mut scheduler,
                    &mut config,
                    &mut overlay,
                    &notifications,
                    event_sounds.as_ref(),
                    &mut coop,
                    idle_capability,
                    tray.available(),
                ).await;
                if coop.is_host() {
                    next_coop_publish = Instant::now();
                }
                recent_requests.push_back((incoming.request.request_id, response.clone()));
                if recent_requests.len() > 128 {
                    recent_requests.pop_front();
                }
                let _ = incoming.respond_to.send(response);
            }
            Some(action) = tray_receiver.recv() => {
                match action {
                    TrayAction::Command(command) => {
                        match execute_command(
                            &command,
                            CommandOrigin::Local,
                            &clock,
                            &state_store,
                            &mut scheduler,
                            &mut config,
                            &mut overlay,
                            &notifications,
                            event_sounds.as_ref(),
                            &mut coop,
                            idle_capability,
                            tray.available(),
                        ).await {
                            Ok((message, _)) => tracing::info!(%message, "tray command handled"),
                            Err(error) => tracing::warn!(%error, "tray command failed"),
                        }
                        if coop.is_host() {
                            next_coop_publish = Instant::now();
                        }
                    }
                    TrayAction::OpenSettings => {
                        if let Err(error) = spawn_settings().await {
                            tracing::warn!(%error, "failed to open settings");
                        }
                    }
                }
            }
            Some(event) = power_receiver.recv() => {
                if coop.holds_local_schedule() {
                    continue;
                }
                tracing::info!(?event, "logind state changed");
                let now = clock.sample()?;
                let scheduler_event = match event {
                    PowerEvent::PreparingForSleep => SchedulerEvent::SuspendStarted,
                    PowerEvent::Resumed => SchedulerEvent::SuspendEnded,
                    PowerEvent::Locked => SchedulerEvent::LockStarted,
                    PowerEvent::Unlocked => SchedulerEvent::LockEnded,
                };
                let previous = scheduler.state().clone();
                let effects = scheduler.handle_event(scheduler_event, now);
                persist_if_changed(&state_store, &scheduler, &previous)?;
                apply_effects(
                    effects,
                    &mut overlay,
                    &notifications,
                    event_sounds.as_ref(),
                    config.completion.sound,
                ).await;
                if coop.is_host() {
                    next_coop_publish = Instant::now();
                }
            }
            Some(event) = idle_receiver.recv() => {
                if coop.holds_local_schedule() {
                    continue;
                }
                let now = clock.sample()?;
                let scheduler_event = match event {
                    IdleEvent::Idled => SchedulerEvent::IdleThresholdReached,
                    IdleEvent::Resumed => SchedulerEvent::ActivityResumed,
                };
                let previous = scheduler.state().clone();
                let effects = scheduler.handle_event(scheduler_event, now);
                persist_if_changed(&state_store, &scheduler, &previous)?;
                apply_effects(
                    effects,
                    &mut overlay,
                    &notifications,
                    event_sounds.as_ref(),
                    config.completion.sound,
                ).await;
                if coop.is_host() {
                    next_coop_publish = Instant::now();
                }
            }
            Some(event) = coop_event_receiver.recv() => {
                match coop.accept(event) {
                    AcceptedEvent::Snapshot(snapshot) => {
                        let now = clock.sample()?;
                        let effects = scheduler.adopt_coop_snapshot(&snapshot, now);
                        apply_effects(
                            effects,
                            &mut overlay,
                            &notifications,
                            event_sounds.as_ref(),
                            config.completion.sound,
                        ).await;
                    }
                    AcceptedEvent::ActionRequest { request_id, action } => {
                        let command = action.into_command();
                        match execute_command(
                            &command,
                            CommandOrigin::CoopGuest,
                            &clock,
                            &state_store,
                            &mut scheduler,
                            &mut config,
                            &mut overlay,
                            &notifications,
                            event_sounds.as_ref(),
                            &mut coop,
                            idle_capability,
                            tray.available(),
                        ).await {
                            Ok((message, _)) => tracing::info!(%request_id, %message, "co-op action handled"),
                            Err(error) => tracing::warn!(%request_id, %error, "co-op action rejected"),
                        }
                        next_coop_publish = Instant::now();
                    }
                    AcceptedEvent::Error(error) => tracing::warn!(%error, "co-op relay error"),
                    AcceptedEvent::StateChanged | AcceptedEvent::Stale => {}
                }
            }
            result = tokio::signal::ctrl_c() => {
                result?;
                overlay.stop_any().await;
                break;
            }
            _ = terminate.recv() => {
                overlay.stop_any().await;
                break;
            }
        }

        if coop.is_host() && Instant::now() >= next_coop_publish {
            let now = clock.sample()?;
            let (host_id, revision) = coop.next_snapshot_identity();
            let (snapshot, scheduler_changed) = scheduler.coop_snapshot(host_id, revision, now);
            if scheduler_changed {
                state_store.save(&scheduler.snapshot())?;
            }
            coop.publish(snapshot);
            next_coop_publish = Instant::now() + Duration::from_secs(1);
        }

        let now = clock.sample()?;
        let status = scheduler.status(now);
        let next_tray_state = tray_state(status.clone());
        if config.tray.enabled != tray_enabled {
            tray_enabled = config.tray.enabled;
            if let Err(error) = tray.set_enabled(tray_enabled, next_tray_state).await {
                tracing::warn!(%error, "tray integration unavailable");
            }
        } else if tray_enabled {
            tray.update(next_tray_state).await;
        }
        shortcut_guard
            .reconcile(&config, scheduler.shortcut_inhibition_enabled(), &status)
            .await;
    }
    shortcut_guard.release().await;
    Ok(())
}

fn completion_sound_directory(instance: breakd_config::RuntimeInstance) -> PathBuf {
    match instance {
        breakd_config::RuntimeInstance::Production => std::env::current_exe()
            .ok()
            .and_then(|executable| installed_data_directory(&executable))
            .filter(|directory| directory.is_dir())
            .unwrap_or_else(|| PathBuf::from("/usr/share/breakd")),
        breakd_config::RuntimeInstance::Development => {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../platform-linux/assets")
        }
    }
}

fn installed_data_directory(executable: &Path) -> Option<PathBuf> {
    executable
        .parent()?
        .parent()
        .map(|prefix| prefix.join("share/breakd"))
}

async fn spawn_settings() -> Result<()> {
    let executable = std::env::current_exe().context("resolve breakd executable")?;
    let mut child = TokioCommand::new(executable)
        .arg("settings")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start settings process")?;
    tokio::spawn(async move {
        if let Err(error) = child.wait().await {
            tracing::warn!(%error, "settings process could not be reaped");
        }
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_request(
    incoming: &IncomingRequest,
    clock: &LinuxClock,
    state_store: &StateStore,
    scheduler: &mut Scheduler,
    config: &mut breakd_core::AppConfig,
    overlay: &mut OverlaySupervisor,
    notifications: &NotificationClient,
    event_sounds: Option<&EventSoundClient>,
    coop: &mut CoopRuntime,
    idle_capability: IdleCapability,
    tray_available: bool,
) -> Response {
    let request_id = incoming.request.request_id;
    let result = execute_command(
        &incoming.request.command,
        CommandOrigin::Local,
        clock,
        state_store,
        scheduler,
        config,
        overlay,
        notifications,
        event_sounds,
        coop,
        idle_capability,
        tray_available,
    )
    .await;

    match result {
        Ok((message, data)) => Response {
            version: IPC_VERSION,
            request_id,
            ok: true,
            message,
            data,
        },
        Err(error) => Response {
            version: IPC_VERSION,
            request_id,
            ok: false,
            message: format!("{error:#}"),
            data: None,
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommandOrigin {
    Local,
    CoopGuest,
}

#[allow(clippy::too_many_arguments)]
async fn execute_command(
    command: &Command,
    origin: CommandOrigin,
    clock: &LinuxClock,
    state_store: &StateStore,
    scheduler: &mut Scheduler,
    config: &mut breakd_core::AppConfig,
    overlay: &mut OverlaySupervisor,
    notifications: &NotificationClient,
    event_sounds: Option<&EventSoundClient>,
    coop: &mut CoopRuntime,
    idle_capability: IdleCapability,
    tray_available: bool,
) -> Result<(String, Option<serde_json::Value>)> {
    let now = clock.sample()?;
    if coop.holds_local_schedule()
        && let Some(action) = CoopAction::from_command(command)
    {
        let request_id = coop.request_action(action)?;
        return Ok((
            "request sent to co-op host".into(),
            Some(serde_json::json!({ "request_id": request_id })),
        ));
    }
    match command {
        Command::Status => Ok((
            "status".into(),
            Some(serde_json::to_value(scheduler.status(now))?),
        )),
        Command::Outputs => {
            let outputs = match HyprlandClient::from_env() {
                Ok(client) => client.outputs().await.unwrap_or_default(),
                Err(_) => Vec::new(),
            };
            Ok(("outputs".into(), Some(serde_json::to_value(outputs)?)))
        }
        Command::Doctor => {
            let instance = breakd_config::RuntimeInstance::current();
            let break_submap = instance.hyprland_submap();
            let hyprland = HyprlandClient::from_env().ok();
            let hyprland_current_submap = match &hyprland {
                Some(client) => client.current_submap().await.ok(),
                None => None,
            };
            let breakd_submap_registered = match &hyprland {
                Some(client) => client.submap_exists(break_submap).await.ok(),
                None => None,
            };
            let notification_capabilities = notifications.capabilities().await.unwrap_or_default();
            let wayland_globals = tokio::task::spawn_blocking(probe_wayland_globals)
                .await
                .context("Wayland probe task failed")??;
            let protocol = |name: &str| {
                wayland_globals
                    .iter()
                    .find(|global| global.interface == name)
                    .map(|global| global.version)
            };
            let report = serde_json::json!({
                "instance": instance.name(),
                "config_path": breakd_config::config_path(),
                "state_path": state_store.path(),
                "socket_path": breakd_config::socket_path(),
                "wayland_display": std::env::var("WAYLAND_DISPLAY").ok(),
                "hyprland_ipc": hyprland.as_ref().is_some_and(HyprlandClient::available),
                "hyprland": {
                    "submap_fallback_enabled": config.hyprland.submap_fallback,
                    "current_submap": hyprland_current_submap,
                    "breakd_submap_registered": breakd_submap_registered,
                },
                "idle": format!("{idle_capability:?}"),
                "notifications": notification_capabilities,
                "tray": {
                    "enabled": config.tray.enabled,
                    "available": tray_available,
                },
                "protocols": {
                    "zwlr_layer_shell_v1": protocol("zwlr_layer_shell_v1"),
                    "ext_idle_notifier_v1": protocol("ext_idle_notifier_v1"),
                    "wp_fractional_scale_manager_v1": protocol("wp_fractional_scale_manager_v1"),
                    "zxdg_output_manager_v1": protocol("zxdg_output_manager_v1"),
                    "zwp_keyboard_shortcuts_inhibit_manager_v1": protocol("zwp_keyboard_shortcuts_inhibit_manager_v1"),
                },
                "wayland_globals": wayland_globals,
                "boot_id": clock.boot_id()?,
            });
            Ok(("doctor report".into(), Some(report)))
        }
        Command::CoopHost { relay_url } => {
            let token = uuid::Uuid::new_v4().simple().to_string();
            let invite = Invite::new(relay_url, &token)?;
            config.coop = CoopConfig {
                mode: CoopMode::Host,
                relay_url: Some(invite.relay_url().into()),
                room_token: Some(invite.room_token().into()),
                disconnect_grace: config.coop.disconnect_grace,
            };
            breakd_config::save(config)?;
            coop.reconfigure(config.coop.clone());
            Ok((
                "co-op room created".into(),
                Some(serde_json::json!({
                    "invite": invite.to_string(),
                    "status": coop.status_json(),
                })),
            ))
        }
        Command::CoopJoin { invite } => {
            let invite = Invite::parse(invite)?;
            config.coop = CoopConfig {
                mode: CoopMode::Guest,
                relay_url: Some(invite.relay_url().into()),
                room_token: Some(invite.room_token().into()),
                disconnect_grace: config.coop.disconnect_grace,
            };
            breakd_config::save(config)?;
            coop.reconfigure(config.coop.clone());
            overlay.stop_any().await;
            Ok(("joining co-op room".into(), Some(coop.status_json())))
        }
        Command::CoopLeave => {
            let disconnect_grace = config.coop.disconnect_grace;
            config.coop = CoopConfig {
                disconnect_grace,
                ..CoopConfig::default()
            };
            breakd_config::save(config)?;
            coop.reconfigure(config.coop.clone());
            let effects = scheduler.reset_after_coop_disconnect(now);
            state_store.save(&scheduler.snapshot())?;
            apply_effects(
                effects,
                overlay,
                notifications,
                event_sounds,
                config.completion.sound,
            )
            .await;
            Ok(("left co-op room; local schedule reset".into(), None))
        }
        Command::CoopStatus => Ok(("co-op status".into(), Some(coop.status_json()))),
        Command::Reload => {
            let updated = breakd_config::load()?;
            let previous_coop = config.coop.clone();
            scheduler.replace_config(updated.clone());
            *config = updated;
            if config.coop != previous_coop {
                let was_guest = previous_coop.mode == CoopMode::Guest;
                coop.reconfigure(config.coop.clone());
                if config.coop.mode == CoopMode::Guest {
                    overlay.stop_any().await;
                } else if was_guest {
                    let effects = scheduler.reset_after_coop_disconnect(now);
                    apply_effects(
                        effects,
                        overlay,
                        notifications,
                        event_sounds,
                        config.completion.sound,
                    )
                    .await;
                }
            }
            state_store.save(&scheduler.snapshot())?;
            Ok((
                "configuration reloaded; restart for idle-monitor or logging changes".into(),
                None,
            ))
        }
        command => {
            let previous = scheduler.state().clone();
            let effects =
                if origin == CommandOrigin::CoopGuest && matches!(command, Command::ResumeBreak) {
                    scheduler.handle_coop_resume_request(now)
                } else {
                    scheduler.handle_command(command, now)
                }
                .map_err(anyhow::Error::from)?;
            persist_if_changed(state_store, scheduler, &previous)?;
            apply_effects(
                effects,
                overlay,
                notifications,
                event_sounds,
                config.completion.sound,
            )
            .await;
            Ok((
                command_message(command).into(),
                Some(serde_json::to_value(scheduler.status(now))?),
            ))
        }
    }
}

fn tray_state(status: SchedulerStatus) -> TrayState {
    TrayState {
        paused: status.paused,
        active_kind: status
            .active_session
            .is_some()
            .then_some(status.break_kind)
            .flatten(),
        remaining_seconds: status
            .remaining_ms
            .map(|milliseconds| milliseconds.saturating_add(999) / 1_000),
        awaiting_resume: status.awaiting_resume,
        can_skip: status.can_skip,
        can_postpone: status.can_postpone,
    }
}

struct HyprlandShortcutGuard {
    client: Option<HyprlandClient>,
    submap: &'static str,
    blocking: bool,
    previous_submap: Option<String>,
    last_check: Option<Instant>,
}

impl HyprlandShortcutGuard {
    fn new(submap: &'static str) -> Self {
        Self {
            client: HyprlandClient::from_env().ok(),
            submap,
            blocking: false,
            previous_submap: None,
            last_check: None,
        }
    }

    async fn initialize(&mut self, config: &breakd_core::AppConfig) {
        if !submap_fallback_available(config) {
            return;
        }
        let Some(client) = &self.client else {
            tracing::warn!("Hyprland submap fallback unavailable: IPC is not configured");
            return;
        };
        match client.reset_submap_if_active(self.submap).await {
            Ok(true) => tracing::warn!("reset stale breakd Hyprland submap"),
            Ok(false) => {}
            Err(error) => tracing::warn!(%error, "failed to inspect stale Hyprland submap"),
        }
        if let Err(error) = client.ensure_submap(self.submap).await {
            tracing::warn!(%error, "failed to register breakd Hyprland submap");
        }
    }

    async fn reconcile(
        &mut self,
        config: &breakd_core::AppConfig,
        inhibit_shortcuts: bool,
        status: &SchedulerStatus,
    ) {
        let should_block = submap_fallback_available(config)
            && inhibit_shortcuts
            && matches!(
                status.state.as_str(),
                "mini-break" | "long-break" | "rest-break"
            );
        if !should_block {
            if self.blocking {
                self.release().await;
            }
            return;
        }

        if self
            .last_check
            .is_some_and(|last_check| last_check.elapsed() < Duration::from_secs(1))
        {
            return;
        }
        self.last_check = Some(Instant::now());

        let Some(client) = &self.client else {
            return;
        };
        if let Err(error) = client.ensure_submap(self.submap).await {
            tracing::warn!(%error, "failed to register breakd Hyprland submap");
            return;
        }
        let current = match client.current_submap().await {
            Ok(current) => current,
            Err(error) => {
                tracing::warn!(%error, "failed to query active Hyprland submap");
                return;
            }
        };
        if current == self.submap {
            self.blocking = true;
            return;
        }
        if !self.blocking {
            self.previous_submap = Some(current);
        }
        match client.set_submap(self.submap).await {
            Ok(()) => {
                self.blocking = true;
                tracing::info!("entered breakd Hyprland submap");
            }
            Err(error) => tracing::warn!(%error, "failed to enter breakd Hyprland submap"),
        }
    }

    async fn release(&mut self) {
        let Some(client) = &self.client else {
            self.blocking = false;
            self.previous_submap = None;
            return;
        };
        let current = client.current_submap().await.ok();
        if current.as_deref() == Some(self.submap) {
            let target = self
                .previous_submap
                .take()
                .unwrap_or_else(|| "default".into());
            let result = if target == "default" || target == "reset" {
                client.set_submap("reset").await
            } else if client.submap_exists(&target).await.unwrap_or(false) {
                client.set_submap(&target).await
            } else {
                client.set_submap("reset").await
            };
            match result {
                Ok(()) => tracing::info!("left breakd Hyprland submap"),
                Err(error) => tracing::warn!(%error, "failed to leave breakd Hyprland submap"),
            }
        }
        self.blocking = false;
        self.previous_submap = None;
        self.last_check = None;
    }
}

fn submap_fallback_available(config: &breakd_core::AppConfig) -> bool {
    config.hyprland.enabled && config.hyprland.submap_fallback
}

fn persist_if_changed(
    state_store: &StateStore,
    scheduler: &Scheduler,
    previous: &breakd_scheduler::SchedulerState,
) -> Result<()> {
    if scheduler.state() != previous {
        state_store.save(&scheduler.snapshot())?;
    }
    Ok(())
}

async fn apply_effects(
    effects: Vec<Effect>,
    overlay: &mut OverlaySupervisor,
    notifications: &NotificationClient,
    event_sounds: Option<&EventSoundClient>,
    completion_sound: CompletionSound,
) {
    for effect in effects {
        match effect {
            Effect::Notify { summary, body } => {
                let notifications = notifications.clone();
                tokio::spawn(async move {
                    if let Err(error) = notifications.notify(&summary, &body).await {
                        tracing::warn!(%error, "desktop notification failed");
                    }
                });
            }
            Effect::PlayCompletionSound => {
                if let Some(event_sounds) = event_sounds
                    && let Err(error) = event_sounds.play_completion(completion_sound)
                {
                    tracing::warn!(%error, "completion sound failed");
                }
            }
            Effect::StartOverlay(spec) => {
                if let Err(error) = overlay.start(spec).await {
                    tracing::error!(%error, "overlay failed to start");
                    let _ = notifications
                        .notify(
                            "Break overlay unavailable",
                            "The break timer is still running. Check breakd doctor and the user journal.",
                        )
                        .await;
                }
            }
            Effect::StopOverlay { session_id } => overlay.stop(session_id).await,
            Effect::OverlayDegraded { session_id, reason } => {
                tracing::warn!(%session_id, %reason, "overlay degraded");
            }
        }
    }
}

#[derive(Default)]
struct OverlaySupervisor {
    active: Option<(BreakSessionId, Child)>,
}

impl OverlaySupervisor {
    async fn start(&mut self, spec: OverlaySpec) -> Result<()> {
        self.stop_any().await;
        let executable = std::env::current_exe()?;
        let serialized = serde_json::to_string(&spec)?;
        let wayland_display = overlay_wayland_display()?;
        let mut command = TokioCommand::new(executable);
        command
            .arg("overlay")
            .env("BREAKD_OVERLAY_SPEC", serialized)
            .env("WAYLAND_DISPLAY", &wayland_display)
            .env("XDG_SESSION_TYPE", "wayland")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let child = command.spawn().context("spawn overlay child")?;
        self.active = Some((spec.session_id, child));
        Ok(())
    }

    async fn stop(&mut self, session_id: BreakSessionId) {
        if self
            .active
            .as_ref()
            .is_some_and(|(id, _)| *id == session_id)
        {
            self.stop_any().await;
        }
    }

    async fn stop_any(&mut self) {
        if let Some((_, mut child)) = self.active.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }

    async fn poll_exit(&mut self) -> Result<Option<(BreakSessionId, String)>> {
        let Some((session_id, child)) = self.active.as_mut() else {
            return Ok(None);
        };
        let Some(status) = child.try_wait()? else {
            return Ok(None);
        };
        let session_id = *session_id;
        self.active = None;
        Ok(Some((session_id, format!("overlay exited with {status}"))))
    }
}

fn overlay_wayland_display() -> Result<OsString> {
    if let Some(display) = std::env::var_os("WAYLAND_DISPLAY").filter(|value| !value.is_empty()) {
        return Ok(display);
    }

    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("WAYLAND_DISPLAY is unset and XDG_RUNTIME_DIR is unavailable")?;
    discover_wayland_display(&runtime_dir)
}

fn discover_wayland_display(runtime_dir: &Path) -> Result<OsString> {
    let mut candidates = fs::read_dir(runtime_dir)
        .with_context(|| {
            format!(
                "inspect Wayland runtime directory {}",
                runtime_dir.display()
            )
        })?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("wayland-"))
                && entry.file_type().is_ok_and(|kind| kind.is_socket())
        })
        .map(|entry| entry.file_name())
        .collect::<Vec<_>>();
    candidates.sort();

    match candidates.as_slice() {
        [display_name] => {
            tracing::info!(display = %display_name.to_string_lossy(), "discovered Wayland display for overlay");
            Ok(display_name.clone())
        }
        [] => anyhow::bail!(
            "WAYLAND_DISPLAY is unset and no Wayland socket exists in {}",
            runtime_dir.display()
        ),
        displays => anyhow::bail!(
            "WAYLAND_DISPLAY is unset and multiple Wayland sockets exist in {}: {}",
            runtime_dir.display(),
            displays
                .iter()
                .map(|display| display.to_string_lossy())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn command_message(command: &Command) -> &'static str {
    match command {
        Command::Pause { .. } => "schedule paused",
        Command::Resume => "schedule resumed",
        Command::ResumeBreak => "break completed",
        Command::Reset => "schedule reset",
        Command::Skip => "break skipped",
        Command::Postpone => "break postponed",
        Command::Mini => "mini break started",
        Command::Long => "long break started",
        Command::Rest => "rest break started",
        Command::Toggle => "schedule toggled",
        Command::Status
        | Command::Reload
        | Command::Outputs
        | Command::Doctor
        | Command::CoopHost { .. }
        | Command::CoopJoin { .. }
        | Command::CoopLeave
        | Command::CoopStatus => "ok",
    }
}

pub fn socket_exists(path: &Path) -> bool {
    path.exists()
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use super::*;

    #[test]
    fn installed_data_directory_follows_the_executable_prefix() {
        assert_eq!(
            installed_data_directory(Path::new("/nix/store/hash-breakd/bin/breakd")),
            Some(PathBuf::from("/nix/store/hash-breakd/share/breakd"))
        );
    }

    #[test]
    fn discovers_the_only_live_wayland_socket() {
        let runtime_dir =
            std::env::temp_dir().join(format!("breakd-wayland-discovery-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&runtime_dir).unwrap();
        fs::write(runtime_dir.join("wayland-1.lock"), "ignored").unwrap();
        let listener = UnixListener::bind(runtime_dir.join("wayland-1")).unwrap();

        assert_eq!(
            discover_wayland_display(&runtime_dir).unwrap(),
            OsString::from("wayland-1")
        );

        drop(listener);
        fs::remove_dir_all(runtime_dir).unwrap();
    }
}
