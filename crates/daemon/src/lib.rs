use std::{collections::VecDeque, path::Path, process::Stdio};

use anyhow::{Context, Result};
use breakd_core::{BreakSessionId, Command, OverlaySpec, Response};
use breakd_ipc::{IPC_VERSION, IncomingRequest, Server};
use breakd_platform_linux::{
    HyprlandClient, IdleCapability, IdleEvent, LinuxClock, NotificationClient, PowerEvent,
    StateStore, probe_wayland_globals, spawn_idle_monitor, spawn_logind_monitor,
};
use breakd_scheduler::{Effect, Scheduler, SchedulerEvent};
use tokio::{
    process::{Child, Command as TokioCommand},
    sync::mpsc,
    time::{Duration, interval},
};

pub async fn run() -> Result<()> {
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

    let notifications = NotificationClient;
    let mut overlay = OverlaySupervisor::default();
    apply_effects(scheduler.startup_effects(now), &mut overlay, &notifications).await;

    let mut ticker = interval(Duration::from_millis(250));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut recent_requests: VecDeque<(uuid::Uuid, Response)> = VecDeque::new();

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let now = clock.sample()?;
                let previous = scheduler.state().clone();
                let effects = scheduler.handle_event(SchedulerEvent::Tick, now);
                persist_if_changed(&state_store, &scheduler, &previous)?;
                apply_effects(effects, &mut overlay, &notifications).await;
                if let Some((session_id, reason)) = overlay.poll_exit().await? {
                    let status = scheduler.status(now);
                    if status.active_session == Some(session_id)
                        && status.remaining_ms.unwrap_or_default() > 1_000
                    {
                        let previous = scheduler.state().clone();
                        let effects = scheduler.handle_event(
                            SchedulerEvent::OverlayFailed { session_id, reason },
                            now,
                        );
                        persist_if_changed(&state_store, &scheduler, &previous)?;
                        apply_effects(effects, &mut overlay, &notifications).await;
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
                    idle_capability,
                ).await;
                recent_requests.push_back((incoming.request.request_id, response.clone()));
                if recent_requests.len() > 128 {
                    recent_requests.pop_front();
                }
                let _ = incoming.respond_to.send(response);
            }
            Some(event) = power_receiver.recv() => {
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
                apply_effects(effects, &mut overlay, &notifications).await;
            }
            Some(event) = idle_receiver.recv() => {
                let now = clock.sample()?;
                let scheduler_event = match event {
                    IdleEvent::Idled => SchedulerEvent::IdleThresholdReached,
                    IdleEvent::Resumed => SchedulerEvent::ActivityResumed,
                };
                let previous = scheduler.state().clone();
                let effects = scheduler.handle_event(scheduler_event, now);
                persist_if_changed(&state_store, &scheduler, &previous)?;
                apply_effects(effects, &mut overlay, &notifications).await;
            }
            result = tokio::signal::ctrl_c() => {
                result?;
                overlay.stop_any().await;
                break;
            }
        }
    }
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
    idle_capability: IdleCapability,
) -> Response {
    let request_id = incoming.request.request_id;
    let result: Result<(String, Option<serde_json::Value>)> = async {
        let now = clock.sample()?;
        match &incoming.request.command {
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
                let hyprland = HyprlandClient::from_env().ok();
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
                    "config_path": breakd_config::config_path(),
                    "state_path": state_store.path(),
                    "socket_path": breakd_config::socket_path(),
                    "wayland_display": std::env::var("WAYLAND_DISPLAY").ok(),
                    "hyprland_ipc": hyprland.as_ref().is_some_and(HyprlandClient::available),
                    "idle": format!("{idle_capability:?}"),
                    "notifications": notification_capabilities,
                    "protocols": {
                        "zwlr_layer_shell_v1": protocol("zwlr_layer_shell_v1"),
                        "ext_idle_notifier_v1": protocol("ext_idle_notifier_v1"),
                        "wp_fractional_scale_manager_v1": protocol("wp_fractional_scale_manager_v1"),
                        "zxdg_output_manager_v1": protocol("zxdg_output_manager_v1"),
                    },
                    "wayland_globals": wayland_globals,
                    "boot_id": clock.boot_id()?,
                });
                Ok(("doctor report".into(), Some(report)))
            }
            Command::Reload => {
                let updated = breakd_config::load()?;
                scheduler.replace_config(updated.clone());
                *config = updated;
                state_store.save(&scheduler.snapshot())?;
                Ok((
                    "configuration reloaded; restart for idle-monitor or logging changes".into(),
                    None,
                ))
            }
            command => {
                let previous = scheduler.state().clone();
                let effects = scheduler
                    .handle_command(command, now)
                    .map_err(anyhow::Error::from)?;
                persist_if_changed(state_store, scheduler, &previous)?;
                apply_effects(effects, overlay, notifications).await;
                Ok((command_message(command).into(), Some(serde_json::to_value(scheduler.status(now))?)))
            }
        }
    }
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
        let child = TokioCommand::new(executable)
            .arg("overlay")
            .env("BREAKD_OVERLAY_SPEC", serialized)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("spawn overlay child")?;
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

fn command_message(command: &Command) -> &'static str {
    match command {
        Command::Pause { .. } => "schedule paused",
        Command::Resume => "schedule resumed",
        Command::Reset => "schedule reset",
        Command::Skip => "break skipped",
        Command::Postpone => "break postponed",
        Command::Mini => "mini break started",
        Command::Long => "long break started",
        Command::Toggle => "schedule toggled",
        Command::Status | Command::Reload | Command::Outputs | Command::Doctor => "ok",
    }
}

pub fn socket_exists(path: &Path) -> bool {
    path.exists()
}
