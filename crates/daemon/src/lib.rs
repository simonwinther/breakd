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
    BreakSessionId, Command, CompletionSound, CoopConfig, CoopMode, FullscreenBehavior, IdleConfig,
    OverlaySpec, Response,
};
use breakd_ipc::{IPC_VERSION, IncomingRequest, Server};
use breakd_platform_linux::{
    EventSoundClient, HyprlandClient, IdleCapability, IdleEvent, IdleMonitor, LinuxClock,
    NotificationAction, NotificationClient, PowerEvent, StateStore, probe_wayland_globals,
    spawn_logind_monitor,
};
use breakd_scheduler::{Effect, Scheduler, SchedulerEvent, SchedulerStatus};
use breakd_tray::{TrayAction, TrayController, TrayState};
use tokio::{
    process::{Child, Command as TokioCommand},
    signal::unix::{SignalKind, signal},
    sync::{mpsc, watch},
    time::{Duration, Instant, interval},
};

mod coop;

use coop::{AcceptedEvent, CoopRuntime};

pub async fn run(recover_state: bool) -> Result<()> {
    let instance = breakd_config::RuntimeInstance::current();
    let mut config = breakd_config::load().context("load configuration")?;
    let socket_path = breakd_config::socket_path();
    // Claim the instance before touching persisted state. A duplicate launch must
    // not reset the schedule saved by the daemon that is already running.
    let server = Server::bind(&socket_path)?;
    let state_store = StateStore::new(breakd_config::state_path());
    let clock = LinuxClock;
    let now = clock.sample()?;
    let boot_id = clock.boot_id()?;
    let mut scheduler = startup_scheduler(
        config.clone(),
        boot_id,
        now,
        &socket_path,
        &state_store,
        recover_state,
    );
    state_store.save(&scheduler.snapshot())?;

    let (coop_event_sender, mut coop_event_receiver) = mpsc::channel(64);
    let mut coop = CoopRuntime::new(config.coop.clone(), coop_event_sender);

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
    let mut idle = IdleRuntime::new(config.idle.clone()).await;
    tracing::info!(capability = ?idle.capability(), "idle capability detected");
    let mut fullscreen = FullscreenMonitor::new(&config);

    let (notification_sender, mut notification_receiver) = mpsc::unbounded_channel();
    let notifications = NotificationClient::with_actions(instance.name(), notification_sender);
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
        config.notifications.actions && !coop.holds_local_schedule(),
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
            scheduler.shortcut_inhibition_enabled()
                && overlay.ready_for(scheduler.status(now).active_session),
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
                    let mut effects = scheduler.reset_after_coop_disconnect(now);
                    effects.extend(scheduler.set_fullscreen(*fullscreen.receiver.borrow(), now));
                    state_store.save(&scheduler.snapshot())?;
                    apply_effects(
                        effects, &mut overlay, &notifications, event_sounds.as_ref(),
                        config.completion.sound, config.notifications.actions,
                    ).await;
                }
                if let Some((session_id, reason, expected_completion)) = overlay.poll_failure(now.boottime_ms).await? {
                    // Release compositor bindings before retrying or sending any
                    // notification. An invisible overlay must never own them.
                    shortcut_guard.release().await;
                    let status = scheduler.status(now);
                    if status.active_session != Some(session_id)
                        || (expected_completion && status.remaining_ms.unwrap_or_default() <= 250 && !status.awaiting_resume)
                    {
                        overlay.stop_any().await;
                    } else if overlay.recovery.as_ref().is_some_and(|recovery| recovery.failures == 1) {
                        let effects = scheduler.handle_event(
                            SchedulerEvent::OverlayFailed { session_id, reason }, now,
                        );
                        apply_effects(
                            effects, &mut overlay, &notifications, event_sounds.as_ref(),
                            config.completion.sound,
                            config.notifications.actions && !coop.holds_local_schedule(),
                        ).await;
                    }
                }
                overlay.retry_if_due(scheduler.status(now).active_session).await;
                if let Some((session_id, reason)) = overlay.take_exhausted() {
                    shortcut_guard.release().await;
                    if coop.holds_local_schedule() {
                        tracing::error!(%session_id, %reason, "guest overlay recovery exhausted");
                        let notifications = notifications.clone();
                        tokio::spawn(async move {
                            let _ = notifications.notify(
                                "Break overlay unavailable",
                                "The shared break continues, but this screen could not display it. Shortcuts have been released.",
                            ).await;
                        });
                    } else {
                        let effects = scheduler.overlay_recovery_exhausted(session_id, now);
                        state_store.save(&scheduler.snapshot())?;
                        apply_effects(
                            effects, &mut overlay, &notifications, event_sounds.as_ref(),
                            config.completion.sound, config.notifications.actions,
                        ).await;
                    }
                }
                let status = scheduler.status(now);
                let waiting_for_overlay = visible_break(&status)
                    && overlay.waiting_for_ready(status.active_session);
                if (!coop.holds_local_schedule() || coop.has_fresh_snapshot()) && !waiting_for_overlay {
                    let previous = scheduler.state().clone();
                    let mut effects = scheduler.set_fullscreen(*fullscreen.receiver.borrow(), now);
                    effects.extend(scheduler.handle_event(SchedulerEvent::Tick, now));
                    if !coop.has_fresh_snapshot() {
                        persist_if_changed(&state_store, &scheduler, &previous)?;
                    }
                    apply_effects(
                        effects, &mut overlay, &notifications, event_sounds.as_ref(),
                        config.completion.sound,
                        config.notifications.actions && !coop.holds_local_schedule(),
                    ).await;
                }
            }
            result = fullscreen.receiver.changed() => {
                if result.is_ok() && !coop.holds_local_schedule() {
                    let now = clock.sample()?;
                    let previous = scheduler.state().clone();
                    let active = *fullscreen.receiver.borrow_and_update();
                    let effects = scheduler.set_fullscreen(active, now);
                    persist_if_changed(&state_store, &scheduler, &previous)?;
                    apply_effects(
                        effects, &mut overlay, &notifications, event_sounds.as_ref(),
                        config.completion.sound, config.notifications.actions,
                    ).await;
                    next_coop_publish = Instant::now();
                }
            }
            Some(action) = notification_receiver.recv() => {
                // Notifications may outlive a policy reload or room transition.
                if config.notifications.enabled && config.notifications.actions && !coop.holds_local_schedule() {
                    let command = match action {
                        NotificationAction::StartDue(due_id) => Command::StartDue { due_id },
                        NotificationAction::PostponeDue(due_id) => Command::PostponeDue { due_id },
                    };
                    if let Err(error) = execute_command(
                        &command, CommandOrigin::Local, &clock, &state_store,
                        &mut scheduler, &mut config, &mut overlay, &notifications,
                        event_sounds.as_ref(), &mut coop, &mut idle, tray.available(),
                    ).await {
                        tracing::debug!(%error, "ignored unavailable or stale reminder action");
                    }
                    next_coop_publish = Instant::now();
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
                    &mut idle,
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
                            &mut idle,
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
                    config.notifications.actions && !coop.holds_local_schedule(),
                ).await;
                if coop.is_host() {
                    next_coop_publish = Instant::now();
                }
            }
            Some(event) = idle.receiver.recv() => {
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
                    config.notifications.actions && !coop.holds_local_schedule(),
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
                            config.notifications.actions && !coop.holds_local_schedule(),
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
                            &mut idle,
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
            let (mut snapshot, scheduler_changed) = scheduler.coop_snapshot(host_id, revision, now);
            if overlay.waiting_for_ready(scheduler.status(now).active_session) {
                // Guests wait with the host while its overlay is being recovered.
                snapshot.paused = true;
                snapshot.resume_at_unix_ms = None;
            }
            if scheduler_changed {
                state_store.save(&scheduler.snapshot())?;
            }
            coop.publish(snapshot);
            next_coop_publish = Instant::now() + Duration::from_secs(1);
        }

        fullscreen.configure(&config);
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
            .reconcile(
                &config,
                scheduler.shortcut_inhibition_enabled()
                    && overlay.ready_for(scheduler.status(now).active_session),
                &status,
            )
            .await;
    }
    shortcut_guard.release().await;
    fullscreen.task.abort();
    Ok(())
}

fn startup_scheduler(
    config: breakd_core::AppConfig,
    boot_id: String,
    now: breakd_core::ClockSample,
    socket_path: &Path,
    state_store: &StateStore,
    recover_state: bool,
) -> Scheduler {
    let snapshot = if recover_state {
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
    let socket_path = socket_path.display().to_string();
    match snapshot {
        Some(snapshot) => Scheduler::restore(config, boot_id, now, socket_path, snapshot),
        None => Scheduler::new(config, boot_id, now, socket_path),
    }
}

struct IdleRuntime {
    config: IdleConfig,
    monitor: Option<IdleMonitor>,
    receiver: mpsc::Receiver<IdleEvent>,
}

impl IdleRuntime {
    async fn new(config: IdleConfig) -> Self {
        let (sender, receiver) = mpsc::channel(16);
        let monitor = if config.enabled {
            let timeout_ms = config.reset_after.as_millis();
            let ignore_inhibitors = !config.respect_idle_inhibitors;
            tokio::task::spawn_blocking(move || {
                IdleMonitor::start(timeout_ms, ignore_inhibitors, sender)
            })
            .await
            .map_err(|error| tracing::warn!(%error, "idle monitor task failed"))
            .ok()
        } else {
            None
        };
        Self {
            config,
            monitor,
            receiver,
        }
    }

    fn capability(&self) -> IdleCapability {
        self.monitor
            .as_ref()
            .map_or(IdleCapability::Unavailable, IdleMonitor::capability)
    }

    async fn reconfigure(&mut self, config: &IdleConfig) -> bool {
        if self.config == *config {
            return false;
        }
        if let Some(mut monitor) = self.monitor.take() {
            let _ = tokio::task::spawn_blocking(move || monitor.stop()).await;
        }
        // A new receiver discards events queued by the old configuration.
        *self = Self::new(config.clone()).await;
        true
    }
}

struct FullscreenMonitor {
    enabled: watch::Sender<bool>,
    receiver: watch::Receiver<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl FullscreenMonitor {
    fn new(config: &breakd_core::AppConfig) -> Self {
        let (enabled, mut requested) = watch::channel(fullscreen_detection_enabled(config));
        let (sender, receiver) = watch::channel(false);
        let client = HyprlandClient::from_env().ok();
        let task = tokio::spawn(async move {
            loop {
                let enabled = *requested.borrow_and_update();
                let active = if enabled {
                    match &client {
                        Some(client) => tokio::time::timeout(
                            Duration::from_millis(500),
                            client.fullscreen_active(),
                        )
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .unwrap_or(false),
                        None => false,
                    }
                } else {
                    false
                };
                sender.send_if_modified(|current| {
                    if *current == active {
                        false
                    } else {
                        *current = active;
                        true
                    }
                });
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(750)) => {},
                    result = requested.changed() => if result.is_err() { break; },
                }
            }
        });
        Self {
            enabled,
            receiver,
            task,
        }
    }

    fn configure(&self, config: &breakd_core::AppConfig) {
        self.enabled.send_if_modified(|enabled| {
            let next = fullscreen_detection_enabled(config);
            if *enabled == next {
                false
            } else {
                *enabled = next;
                true
            }
        });
    }
}

fn fullscreen_detection_enabled(config: &breakd_core::AppConfig) -> bool {
    config.hyprland.enabled && config.fullscreen.behavior != FullscreenBehavior::Show
}

/// Play a preview without connecting to, or changing, the scheduler.
pub async fn preview_sound(sound: CompletionSound) -> Result<()> {
    let instance = breakd_config::RuntimeInstance::current();
    let client = EventSoundClient::new(&completion_sound_directory(instance))?;
    client.play_completion(sound)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while client.is_playing()? {
        anyhow::ensure!(Instant::now() < deadline, "sound preview timed out");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
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
    idle: &mut IdleRuntime,
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
        idle,
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
    idle: &mut IdleRuntime,
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
        Command::OverlayReady {
            session_id,
            ready_token,
        } => {
            let unavailable_ms = overlay
                .acknowledge(*session_id, *ready_token, now.boottime_ms)
                .context("overlay session is no longer awaiting readiness")?;
            if !coop.holds_local_schedule()
                && scheduler.delay_active_overlay(*session_id, unavailable_ms)
            {
                state_store.save(&scheduler.snapshot())?;
            }
            Ok(("overlay ready".into(), None))
        }
        Command::StartDue { .. } | Command::PostponeDue { .. } if coop.holds_local_schedule() => {
            anyhow::bail!("this reminder is no longer controlled by the local schedule")
        }
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
                "idle": format!("{:?}", idle.capability()),
                "overlay": overlay.diagnostics(),
                "fullscreen": {
                    "supported": hyprland.as_ref().is_some_and(HyprlandClient::available),
                    "behavior": config.fullscreen.behavior,
                    "max_delay_ms": config.fullscreen.max_delay.as_millis(),
                },
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
                config.notifications.actions && !coop.holds_local_schedule(),
            )
            .await;
            Ok(("left co-op room; local schedule reset".into(), None))
        }
        Command::CoopStatus => Ok(("co-op status".into(), Some(coop.status_json()))),
        Command::Reload => {
            let updated = breakd_config::load()?;
            let previous_coop = config.coop.clone();
            let invalidate_reminder_actions = !updated.notifications.enabled
                || !updated.notifications.actions
                || (updated.coop != config.coop && updated.coop.mode == CoopMode::Guest);
            let schedule_changed = config.schedule != updated.schedule;
            let timing_changed = config.schedule.mini.interval != updated.schedule.mini.interval
                || config.schedule.long.interval != updated.schedule.long.interval
                || config.schedule.rest.interval != updated.schedule.rest.interval
                || config.schedule.long.after_minis != updated.schedule.long.after_minis
                || config.schedule.rest.after_longs != updated.schedule.rest.after_longs;
            let logging_changed = config.logging != updated.logging;
            let overlay_changed = config.display != updated.display
                || config.content != updated.content
                || config.strict != updated.strict
                || config.skip != updated.skip
                || config.postpone != updated.postpone
                || config.completion != updated.completion;
            let mut effects = scheduler.replace_config_at(updated.clone(), now);
            *config = updated;
            if invalidate_reminder_actions {
                notifications.cancel_actions();
            }
            if idle.reconfigure(&config.idle).await {
                effects.extend(scheduler.handle_event(SchedulerEvent::ActivityResumed, now));
            }
            if config.coop != previous_coop {
                let was_guest = previous_coop.mode == CoopMode::Guest;
                coop.reconfigure(config.coop.clone());
                if config.coop.mode == CoopMode::Guest {
                    overlay.stop_any().await;
                    effects.clear();
                } else if was_guest {
                    effects = scheduler.reset_after_coop_disconnect(now);
                }
            }
            state_store.save(&scheduler.snapshot())?;
            apply_effects(
                effects,
                overlay,
                notifications,
                event_sounds,
                config.completion.sound,
                config.notifications.actions && !coop.holds_local_schedule(),
            )
            .await;
            let mut message = "Configuration applied.".to_owned();
            if timing_changed && coop.holds_local_schedule() {
                message.push_str(" The co-op host continues to control timing.");
            } else if schedule_changed && scheduler.status(now).active_session.is_some() {
                message.push_str(" The current break is preserved; new timing applies afterward.");
            } else if timing_changed && scheduler.status(now).paused {
                message.push_str(" The paused work interval now uses the new timing.");
            } else if timing_changed {
                message.push_str(" A fresh work interval starts with the new timing.");
            }
            if overlay_changed && scheduler.status(now).active_session.is_some() {
                message.push_str(" Overlay appearance and controls update on the next break.");
            }
            if config.idle.enabled && idle.capability() == IdleCapability::Unavailable {
                message.push_str(" Idle detection is unavailable in this session.");
            }
            if logging_changed {
                message.push_str(" Restart the daemon to apply logging changes.");
            }
            Ok((message, None))
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
                config.notifications.actions && !coop.holds_local_schedule(),
            )
            .await;
            Ok((
                command_message(command).into(),
                Some(serde_json::to_value(scheduler.status(now))?),
            ))
        }
    }
}

fn visible_break(status: &SchedulerStatus) -> bool {
    matches!(
        status.state.as_str(),
        "mini-break" | "long-break" | "rest-break"
    )
}

fn tray_state(status: SchedulerStatus) -> TrayState {
    TrayState {
        paused: status.paused,
        deferred: status.deferred,
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
            if self.blocking || self.previous_submap.is_some() {
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
        let current = match client.current_submap().await {
            Ok(current) => current,
            Err(error) => {
                tracing::warn!(%error, "will retry releasing breakd Hyprland submap");
                return;
            }
        };
        if current == self.submap {
            let target = self
                .previous_submap
                .clone()
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
                Err(error) => {
                    tracing::warn!(%error, "will retry leaving breakd Hyprland submap");
                    return;
                }
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
    notification_actions: bool,
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
            Effect::NotifyDue {
                due_id,
                summary,
                body,
                can_postpone,
            } => {
                let notifications = notifications.clone();
                tokio::spawn(async move {
                    let result = if notification_actions {
                        notifications
                            .notify_break(&summary, &body, due_id, can_postpone)
                            .await
                    } else {
                        notifications.notify(&summary, &body).await
                    };
                    if let Err(error) = result {
                        tracing::warn!(%error, "break reminder failed");
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
                    let notifications = notifications.clone();
                    tokio::spawn(async move {
                        let _ = notifications.notify(
                            "Break overlay unavailable",
                            "Trying to display the break again. Shortcuts remain available while it recovers.",
                        ).await;
                    });
                }
            }
            Effect::StopOverlay { session_id } => overlay.stop(session_id).await,
            Effect::OverlayDegraded { session_id, reason } => {
                tracing::warn!(%session_id, %reason, "overlay degraded");
            }
        }
    }
}

const MAX_OVERLAY_FAILURES: u32 = 3;
const OVERLAY_READY_TIMEOUT: Duration = Duration::from_secs(5);

struct OverlayChild {
    session_id: BreakSessionId,
    ready_token: BreakSessionId,
    child: Child,
    ready: bool,
    ready_deadline: Instant,
}

struct OverlayRecovery {
    spec: OverlaySpec,
    failures: u32,
    retry_at: Option<Instant>,
    unavailable_since_boot_ms: Option<u64>,
    visible_since_boot_ms: Option<u64>,
    exhausted_reported: bool,
    last_error: String,
}

impl OverlayRecovery {
    fn new(spec: OverlaySpec, now_boot_ms: u64) -> Self {
        Self {
            spec,
            failures: 0,
            retry_at: None,
            unavailable_since_boot_ms: Some(now_boot_ms),
            visible_since_boot_ms: None,
            exhausted_reported: false,
            last_error: String::new(),
        }
    }

    fn failed(&mut self, reason: String, now: Instant, now_boot_ms: u64) {
        if let Some(visible_since) = self.visible_since_boot_ms.take() {
            let visible_ms = now_boot_ms.saturating_sub(visible_since);
            self.spec.duration = breakd_core::DurationMs::from_millis(
                self.spec.duration.as_millis().saturating_sub(visible_ms),
            );
            self.spec.strict_remaining = breakd_core::DurationMs::from_millis(
                self.spec
                    .strict_remaining
                    .as_millis()
                    .saturating_sub(visible_ms),
            );
        }
        self.unavailable_since_boot_ms.get_or_insert(now_boot_ms);
        self.failures = self.failures.saturating_add(1);
        self.retry_at = (self.failures < MAX_OVERLAY_FAILURES)
            .then(|| now + Duration::from_secs(u64::from(self.failures)));
        self.last_error = reason;
    }

    fn acknowledge(&mut self, now_boot_ms: u64) -> u64 {
        let unavailable_ms = self
            .unavailable_since_boot_ms
            .take()
            .map_or(0, |started| now_boot_ms.saturating_sub(started));
        self.visible_since_boot_ms = Some(now_boot_ms);
        self.retry_at = None;
        unavailable_ms
    }
}

#[derive(Default)]
struct OverlaySupervisor {
    active: Option<OverlayChild>,
    recovery: Option<OverlayRecovery>,
}

impl OverlaySupervisor {
    async fn start(&mut self, spec: OverlaySpec) -> Result<()> {
        self.stop_any().await;
        let now = LinuxClock.sample()?;
        self.recovery = Some(OverlayRecovery::new(spec.clone(), now.boottime_ms));
        self.attempt(spec).await
    }

    async fn attempt(&mut self, spec: OverlaySpec) -> Result<()> {
        let result = self.spawn_child(&spec).await;
        if let Err(error) = &result {
            self.record_failure(format!("{error:#}"), LinuxClock.sample()?.boottime_ms);
        }
        result
    }

    async fn spawn_child(&mut self, spec: &OverlaySpec) -> Result<()> {
        let executable = std::env::current_exe()?;
        let mut spec = spec.clone();
        let ready_token = BreakSessionId::new();
        spec.ready_token = Some(ready_token);
        let serialized = serde_json::to_string(&spec)?;
        let wayland_display = overlay_wayland_display()?;
        let child = TokioCommand::new(executable)
            .arg("overlay")
            .env("BREAKD_OVERLAY_SPEC", serialized)
            .env("WAYLAND_DISPLAY", &wayland_display)
            .env("XDG_SESSION_TYPE", "wayland")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("spawn overlay child")?;
        self.active = Some(OverlayChild {
            session_id: spec.session_id,
            ready_token,
            child,
            ready: false,
            ready_deadline: Instant::now() + OVERLAY_READY_TIMEOUT,
        });
        Ok(())
    }

    fn record_failure(&mut self, reason: String, now_boot_ms: u64) {
        if let Some(recovery) = self.recovery.as_mut() {
            recovery.failed(reason, Instant::now(), now_boot_ms);
        }
    }

    fn ready_for(&self, session_id: Option<BreakSessionId>) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| Some(active.session_id) == session_id && active.ready)
    }

    fn waiting_for_ready(&self, session_id: Option<BreakSessionId>) -> bool {
        self.recovery.as_ref().is_some_and(|recovery| {
            Some(recovery.spec.session_id) == session_id
                && recovery.failures < MAX_OVERLAY_FAILURES
                && !self.ready_for(session_id)
        })
    }

    fn acknowledge(
        &mut self,
        session_id: BreakSessionId,
        ready_token: BreakSessionId,
        now_boot_ms: u64,
    ) -> Option<u64> {
        let active = self.active.as_mut()?;
        if active.session_id != session_id || active.ready_token != ready_token || active.ready {
            return None;
        }
        if active.child.try_wait().ok().flatten().is_some() {
            return None;
        }
        active.ready = true;
        self.recovery
            .as_mut()
            .map(|recovery| recovery.acknowledge(now_boot_ms))
    }

    async fn retry_if_due(&mut self, session_id: Option<BreakSessionId>) {
        let spec = self.recovery.as_ref().and_then(|recovery| {
            (Some(recovery.spec.session_id) == session_id
                && self.active.is_none()
                && recovery.retry_at.is_some_and(|at| Instant::now() >= at))
            .then(|| recovery.spec.clone())
        });
        if let Some(spec) = spec {
            if let Some(recovery) = &mut self.recovery {
                recovery.retry_at = None;
            }
            if let Err(error) = self.attempt(spec).await {
                tracing::warn!(%error, "overlay recovery attempt failed");
            }
        }
    }

    fn take_exhausted(&mut self) -> Option<(BreakSessionId, String)> {
        let recovery = self.recovery.as_mut()?;
        if recovery.failures < MAX_OVERLAY_FAILURES || recovery.exhausted_reported {
            return None;
        }
        recovery.exhausted_reported = true;
        Some((recovery.spec.session_id, recovery.last_error.clone()))
    }

    async fn stop(&mut self, session_id: BreakSessionId) {
        if self
            .recovery
            .as_ref()
            .is_some_and(|recovery| recovery.spec.session_id == session_id)
        {
            self.stop_any().await;
        }
    }

    async fn stop_any(&mut self) {
        self.recovery = None;
        if let Some(mut active) = self.active.take() {
            let _ = active.child.kill().await;
            let _ = active.child.wait().await;
        }
    }

    async fn poll_failure(
        &mut self,
        now_boot_ms: u64,
    ) -> Result<Option<(BreakSessionId, String, bool)>> {
        let automatic = self
            .recovery
            .as_ref()
            .is_some_and(|recovery| !recovery.spec.manual_resume);
        let Some(active) = self.active.as_mut() else {
            return Ok(None);
        };
        let (reason, expected_completion) = match active.child.try_wait()? {
            Some(status) => (
                format!("overlay exited with {status}"),
                status.success() && active.ready && automatic,
            ),
            None if !active.ready && Instant::now() >= active.ready_deadline => {
                let _ = active.child.kill().await;
                let _ = active.child.wait().await;
                (
                    "overlay did not display a window before its startup deadline".into(),
                    false,
                )
            }
            None => return Ok(None),
        };
        let session_id = active.session_id;
        self.active = None;
        self.record_failure(reason.clone(), now_boot_ms);
        Ok(Some((session_id, reason, expected_completion)))
    }

    fn diagnostics(&self) -> serde_json::Value {
        serde_json::json!({
            "ready": self.active.as_ref().is_some_and(|active| active.ready),
            "session": self.recovery.as_ref().map(|recovery| recovery.spec.session_id),
            "failures": self.recovery.as_ref().map_or(0, |recovery| recovery.failures),
            "retry_pending": self.recovery.as_ref().is_some_and(|recovery| recovery.retry_at.is_some()),
        })
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
        Command::StartDue { .. } => "break started",
        Command::PostponeDue { .. } => "break postponed",
        Command::Mini => "mini break started",
        Command::Long => "long break started",
        Command::Rest => "rest break started",
        Command::Toggle => "schedule toggled",
        Command::Status
        | Command::OverlayReady { .. }
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

    fn overlay_spec() -> OverlaySpec {
        OverlaySpec {
            session_id: BreakSessionId::new(),
            preview: false,
            ready_token: None,
            kind: breakd_core::BreakKind::Mini,
            duration: breakd_core::DurationMs::from_millis(20_000),
            strict_remaining: breakd_core::DurationMs::from_millis(10_000),
            can_skip: true,
            can_postpone: true,
            manual_resume: false,
            allow_postpone_during_lockout: false,
            inhibit_shortcuts: true,
            message: None,
            socket_path: "unused".into(),
        }
    }

    #[test]
    fn overlay_retries_are_bounded_and_only_visible_time_is_consumed() {
        let mut recovery = OverlayRecovery::new(overlay_spec(), 1_000);
        assert_eq!(recovery.acknowledge(2_000), 1_000);
        let now = Instant::now();
        recovery.failed("crash".into(), now, 5_000);
        assert_eq!(recovery.spec.duration.as_millis(), 17_000);
        assert_eq!(recovery.spec.strict_remaining.as_millis(), 7_000);
        assert_eq!(recovery.retry_at, Some(now + Duration::from_secs(1)));
        recovery.failed("retry failed".into(), now, 6_000);
        assert_eq!(recovery.spec.duration.as_millis(), 17_000);
        assert_eq!(recovery.retry_at, Some(now + Duration::from_secs(2)));
        recovery.failed("retry failed".into(), now, 8_000);
        assert!(recovery.retry_at.is_none());
        assert_eq!(recovery.failures, MAX_OVERLAY_FAILURES);
    }

    async fn supervisor_with_child(manual_resume: bool, mapped: bool) -> OverlaySupervisor {
        let mut spec = overlay_spec();
        spec.manual_resume = manual_resume;
        let child = TokioCommand::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        OverlaySupervisor {
            active: Some(OverlayChild {
                session_id: spec.session_id,
                ready_token: BreakSessionId::new(),
                child,
                ready: mapped,
                ready_deadline: Instant::now() + OVERLAY_READY_TIMEOUT,
            }),
            recovery: Some(OverlayRecovery::new(spec, 1_000)),
        }
    }

    #[tokio::test]
    async fn shortcuts_wait_for_the_current_attempts_mapped_acknowledgement() {
        let mut overlay = supervisor_with_child(false, false).await;
        let active = overlay.active.as_ref().unwrap();
        let session = active.session_id;
        let token = active.ready_token;
        assert!(!overlay.ready_for(Some(session)));
        assert!(
            overlay
                .acknowledge(session, BreakSessionId::new(), 2_000)
                .is_none()
        );
        assert!(!overlay.ready_for(Some(session)));
        assert_eq!(overlay.acknowledge(session, token, 2_000), Some(1_000));
        assert!(overlay.ready_for(Some(session)));
        assert!(overlay.acknowledge(session, token, 2_001).is_none());
        overlay.stop_any().await;
        assert!(!overlay.ready_for(Some(session)));
        assert!(!overlay.waiting_for_ready(Some(session)));
    }

    #[tokio::test]
    async fn a_manual_resume_overlay_exit_is_always_recovered() {
        let mut overlay = supervisor_with_child(true, true).await;
        let session = overlay.active.as_ref().unwrap().session_id;
        overlay.active.as_mut().unwrap().child.kill().await.unwrap();
        let (failed, _, expected_completion) = overlay.poll_failure(2_000).await.unwrap().unwrap();
        assert_eq!(failed, session);
        assert!(!expected_completion);
        assert!(overlay.waiting_for_ready(Some(session)));
        assert!(!overlay.ready_for(Some(session)));
        overlay.stop_any().await;
    }

    #[tokio::test]
    async fn an_overlay_that_never_maps_is_killed_and_retried() {
        let mut overlay = supervisor_with_child(false, false).await;
        let session = overlay.active.as_ref().unwrap().session_id;
        overlay.active.as_mut().unwrap().ready_deadline = Instant::now();
        let (_, reason, expected_completion) = overlay.poll_failure(7_000).await.unwrap().unwrap();
        assert!(reason.contains("startup deadline"));
        assert!(!expected_completion);
        assert!(overlay.active.is_none());
        assert!(overlay.waiting_for_ready(Some(session)));
        overlay.stop_any().await;
    }

    fn clock(milliseconds: u64) -> breakd_core::ClockSample {
        breakd_core::ClockSample {
            monotonic_ms: milliseconds,
            boottime_ms: milliseconds,
            wall_unix_ms: milliseconds,
        }
    }

    #[test]
    fn normal_start_discards_saved_breaks_pauses_and_overdue_timers() {
        for command in [
            None,
            Some(Command::Mini),
            Some(Command::Long),
            Some(Command::Rest),
            Some(Command::Pause { duration: None }),
            Some(Command::Pause {
                duration: Some(breakd_core::DurationMs::from_millis(60_000)),
            }),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let store = StateStore::new(directory.path().join("state.json"));
            let config = breakd_config::defaults();
            let mut previous =
                Scheduler::new(config.clone(), "boot".into(), clock(0), "socket".into());
            if let Some(command) = &command {
                previous.handle_command(command, clock(1)).unwrap();
            }
            store.save(&previous.snapshot()).unwrap();

            let now = clock(3_600_000);
            let restarted = startup_scheduler(
                config.clone(),
                "boot".into(),
                now,
                Path::new("socket"),
                &store,
                false,
            );
            let status = restarted.status(now);
            assert_eq!(status.state, "running", "saved command: {command:?}");
            assert_eq!(
                status.remaining_ms,
                Some(config.schedule.mini.interval.as_millis())
            );
            assert_eq!(status.minis_since_long, 0);
            assert_eq!(status.longs_since_rest, 0);
            assert!(status.active_session.is_none());
            assert!(restarted.startup_effects(now).is_empty());
        }
    }

    #[test]
    fn normal_start_resets_a_completed_break_waiting_for_manual_resume() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore::new(directory.path().join("state.json"));
        let mut config = breakd_config::defaults();
        config.completion.manual_resume = true;
        let mut previous = Scheduler::new(config.clone(), "boot".into(), clock(0), "socket".into());
        previous.handle_command(&Command::Mini, clock(0)).unwrap();
        let now = clock(config.schedule.mini.duration.as_millis());
        previous.handle_event(SchedulerEvent::Tick, now);
        assert!(previous.status(now).awaiting_resume);
        store.save(&previous.snapshot()).unwrap();

        let restarted = startup_scheduler(
            config,
            "boot".into(),
            now,
            Path::new("socket"),
            &store,
            false,
        );
        assert_eq!(restarted.status(now).state, "running");
        assert!(!restarted.status(now).awaiting_resume);
        assert!(restarted.startup_effects(now).is_empty());
    }

    #[test]
    fn normal_start_resets_completed_break_counters() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore::new(directory.path().join("state.json"));
        let config = breakd_config::defaults();
        let mut previous = Scheduler::new(config.clone(), "boot".into(), clock(0), "socket".into());
        previous.handle_command(&Command::Mini, clock(0)).unwrap();
        let mini_ended = config.schedule.mini.duration.as_millis();
        previous.handle_event(SchedulerEvent::Tick, clock(mini_ended));
        assert_eq!(previous.status(clock(mini_ended)).minis_since_long, 1);
        previous
            .handle_command(&Command::Long, clock(mini_ended))
            .unwrap();
        let now = clock(mini_ended + config.schedule.long.duration.as_millis());
        previous.handle_event(SchedulerEvent::Tick, now);
        assert_eq!(previous.status(now).longs_since_rest, 1);
        store.save(&previous.snapshot()).unwrap();

        let restarted = startup_scheduler(
            config.clone(),
            "boot".into(),
            now,
            Path::new("socket"),
            &store,
            false,
        );
        let status = restarted.status(now);
        assert_eq!(status.minis_since_long, 0);
        assert_eq!(status.longs_since_rest, 0);
        assert_eq!(
            status.remaining_ms,
            Some(config.schedule.mini.interval.as_millis())
        );
    }

    #[test]
    fn normal_start_honors_start_paused_without_restoring_an_old_break() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore::new(directory.path().join("state.json"));
        let mut config = breakd_config::defaults();
        let mut previous = Scheduler::new(config.clone(), "boot".into(), clock(0), "socket".into());
        previous.handle_command(&Command::Long, clock(0)).unwrap();
        store.save(&previous.snapshot()).unwrap();
        config.startup.start_paused = true;

        let now = clock(10);
        let restarted = startup_scheduler(
            config.clone(),
            "boot".into(),
            now,
            Path::new("socket"),
            &store,
            false,
        );
        assert!(restarted.status(now).paused);
        assert_eq!(
            restarted.status(now).remaining_ms,
            Some(config.schedule.mini.interval.as_millis())
        );
        assert!(restarted.status(now).active_session.is_none());
        assert!(restarted.startup_effects(now).is_empty());
    }

    #[test]
    fn saved_break_recovery_requires_an_explicit_startup_request() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore::new(directory.path().join("state.json"));
        let config = breakd_config::defaults();
        let mut previous = Scheduler::new(config.clone(), "boot".into(), clock(0), "socket".into());
        previous.handle_command(&Command::Mini, clock(0)).unwrap();
        store.save(&previous.snapshot()).unwrap();

        let now = clock(10);
        let recovered = startup_scheduler(
            config,
            "boot".into(),
            now,
            Path::new("socket"),
            &store,
            true,
        );
        assert_eq!(recovered.status(now).state, "mini-break");
        assert_eq!(
            recovered.status(now).active_session,
            previous.status(now).active_session
        );
        assert!(matches!(
            recovered.startup_effects(now).as_slice(),
            [Effect::StartOverlay(_)]
        ));
    }

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
