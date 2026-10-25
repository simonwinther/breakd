use breakd_coop::{CoopPhase, CoopPolicy, CoopSnapshot, ScheduledBreak, SharedBreak};
use breakd_core::{
    AppConfig, BreakKind, BreakSessionId, ClockSample, Command, DueBreakId, DurationMs,
    FullscreenBehavior, MissedBreakPolicy, OverlaySpec, StrictMode,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingBreak {
    pub id: DueBreakId,
    pub kind: BreakKind,
    pub postpone_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirrored: Option<MirroredBreakPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirroredBreakPolicy {
    pub duration_ms: u64,
    pub strict_duration_ms: u64,
    pub strict_entire: bool,
    pub manual_resume: bool,
    pub can_skip: bool,
    pub can_postpone: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleContext {
    pub cycle_started_mono_ms: u64,
    pub next_due_mono_ms: u64,
    pub minis_since_long: u32,
    #[serde(default)]
    pub longs_since_rest: u32,
    #[serde(default)]
    pub rest_cycle_started_mono_ms: u64,
    pub pending: Option<PendingBreak>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveBreak {
    pub context: ScheduleContext,
    pub due: PendingBreak,
    pub session_id: BreakSessionId,
    pub started_boot_ms: u64,
    pub ends_boot_ms: u64,
    pub strict_until_boot_ms: u64,
    #[serde(default)]
    pub manual_resume: bool,
    #[serde(default)]
    pub completion_sound_emitted: bool,
    #[serde(default)]
    pub resume_requested: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SuspendReason {
    Sleep,
    Lock,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuspensionActivity {
    sleeping: bool,
    locked: bool,
    last_clock: ClockSample,
    frozen_boot_ms: u64,
}

impl SuspensionActivity {
    fn new(reason: SuspendReason, now: ClockSample) -> Self {
        Self {
            sleeping: reason == SuspendReason::Sleep,
            locked: reason == SuspendReason::Lock,
            last_clock: now,
            frozen_boot_ms: 0,
        }
    }

    fn is_active(&self, reason: SuspendReason) -> bool {
        match reason {
            SuspendReason::Sleep => self.sleeping,
            SuspendReason::Lock => self.locked,
        }
    }

    fn set_active(&mut self, reason: SuspendReason, active: bool) {
        match reason {
            SuspendReason::Sleep => self.sleeping = active,
            SuspendReason::Lock => self.locked = active,
        }
    }

    fn advance(&mut self, now: ClockSample, sleep_counts: bool, lock_counts: bool) {
        // Sleep policy governs time asleep, even when the session is locked.
        // Lock policy governs the remaining time with a locked desktop.
        let counts_as_break = if self.sleeping {
            sleep_counts
        } else {
            lock_counts
        };
        if !counts_as_break {
            self.frozen_boot_ms = self
                .frozen_boot_ms
                .saturating_add(now.boottime_ms.saturating_sub(self.last_clock.boottime_ms));
        }
        self.last_clock = now;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum SchedulerState {
    Running {
        context: ScheduleContext,
    },
    PreMiniBreak {
        context: ScheduleContext,
    },
    MiniBreak {
        active: ActiveBreak,
    },
    PreLongBreak {
        context: ScheduleContext,
    },
    LongBreak {
        active: ActiveBreak,
    },
    PreRestBreak {
        context: ScheduleContext,
    },
    DeferredBreak {
        context: ScheduleContext,
        original_due_mono_ms: u64,
    },
    RestBreak {
        active: ActiveBreak,
    },
    PausedIndefinitely {
        inner: Box<SchedulerState>,
        paused_at: ClockSample,
    },
    PausedUntil {
        inner: Box<SchedulerState>,
        paused_at: ClockSample,
        resume_mono_ms: u64,
    },
    Suspended {
        reason: SuspendReason,
        inner: Box<SchedulerState>,
        started_at: ClockSample,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overlap: Option<SuspensionActivity>,
    },
    IdleReset {
        started_boot_ms: u64,
    },
    Recovering {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema_version: u32,
    pub boot_id: String,
    pub state: SchedulerState,
    pub last_clock: ClockSample,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerEvent {
    Tick,
    SuspendStarted,
    SuspendEnded,
    LockStarted,
    LockEnded,
    IdleThresholdReached,
    ActivityResumed,
    OverlayFailed {
        session_id: BreakSessionId,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Notify {
        summary: String,
        body: String,
    },
    NotifyDue {
        summary: String,
        body: String,
        due_id: DueBreakId,
        can_postpone: bool,
    },
    PlayCompletionSound,
    StartOverlay(OverlaySpec),
    StopOverlay {
        session_id: BreakSessionId,
    },
    OverlayDegraded {
        session_id: BreakSessionId,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SchedulerStatus {
    pub state: String,
    pub paused: bool,
    pub break_kind: Option<BreakKind>,
    pub remaining_ms: Option<u64>,
    pub awaiting_resume: bool,
    pub minis_since_long: u32,
    pub longs_since_rest: u32,
    pub active_session: Option<BreakSessionId>,
    pub postpone_count: u32,
    pub can_skip: bool,
    pub can_postpone: bool,
    pub deferred: bool,
    pub deferred_until_ms: Option<u64>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SchedulerError {
    #[error("no break is active")]
    NoActiveBreak,
    #[error("a break is already active")]
    BreakAlreadyActive,
    #[error("strict mode does not allow dismissal yet")]
    StrictMode,
    #[error("skipping is disabled for this break")]
    SkipDisabled,
    #[error("the postpone limit has been reached")]
    PostponeLimit,
    #[error("postponement is disabled for this break")]
    PostponeDisabled,
    #[error("the scheduler is already paused")]
    AlreadyPaused,
    #[error("the break has not finished or manual resume is disabled")]
    NotAwaitingResume,
    #[error("the completed break is waiting for manual resume")]
    AwaitingResume,
    #[error("the scheduler is not paused")]
    NotPaused,
    #[error("command is handled by the daemon rather than the scheduler")]
    DaemonCommand,
    #[error("this break reminder has expired")]
    StaleDueBreak,
}

#[derive(Debug, Clone)]
pub struct Scheduler {
    config: AppConfig,
    boot_id: String,
    socket_path: String,
    state: SchedulerState,
    last_clock: ClockSample,
    coop_policy: Option<CoopPolicy>,
    last_notified_due: Option<DueBreakId>,
    fullscreen: bool,
}

impl Scheduler {
    pub fn new(config: AppConfig, boot_id: String, now: ClockSample, socket_path: String) -> Self {
        let mut scheduler = Self {
            state: Self::fresh_running(&config, now),
            config,
            boot_id,
            socket_path,
            last_clock: now,
            coop_policy: None,
            last_notified_due: None,
            fullscreen: false,
        };
        if scheduler.config.startup.start_paused {
            scheduler.state = SchedulerState::PausedIndefinitely {
                inner: Box::new(scheduler.state.clone()),
                paused_at: now,
            };
        }
        scheduler
    }

    pub fn restore(
        config: AppConfig,
        boot_id: String,
        now: ClockSample,
        socket_path: String,
        snapshot: Snapshot,
    ) -> Self {
        let mut state = if snapshot.schema_version != STATE_SCHEMA_VERSION {
            SchedulerState::Recovering {
                reason: format!("unsupported state schema {}", snapshot.schema_version),
            }
        } else if snapshot.boot_id != boot_id {
            Self::fresh_running(&config, now)
        } else {
            snapshot.state
        };

        if (!config.recovery.recover_active_break && active_break_in(&state).is_some())
            || (config.recovery.missed_break == MissedBreakPolicy::Reset
                && state_is_overdue(&state, now))
        {
            state = Self::fresh_running(&config, now);
        }

        let mut scheduler = Self {
            config,
            boot_id,
            socket_path,
            state,
            last_clock: now,
            coop_policy: None,
            last_notified_due: None,
            fullscreen: false,
        };
        if matches!(scheduler.state, SchedulerState::Recovering { .. }) {
            scheduler.state = Self::fresh_running(&scheduler.config, now);
        }
        scheduler
    }

    pub fn state(&self) -> &SchedulerState {
        &self.state
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            schema_version: STATE_SCHEMA_VERSION,
            boot_id: self.boot_id.clone(),
            state: self.state.clone(),
            last_clock: self.last_clock,
        }
    }

    pub fn replace_config(&mut self, config: AppConfig) {
        self.replace_config_at(config, self.last_clock);
    }

    /// Interval or cycle-rule changes grant a fresh work interval. User pauses
    /// keep their original clock; an active break keeps its timing.
    pub fn replace_config_at(&mut self, config: AppConfig, now: ClockSample) -> Vec<Effect> {
        let intervals_changed = self.config.schedule.mini.interval != config.schedule.mini.interval
            || self.config.schedule.long.interval != config.schedule.long.interval
            || self.config.schedule.rest.interval != config.schedule.rest.interval
            || self.config.schedule.long.after_minis != config.schedule.long.after_minis
            || self.config.schedule.rest.after_longs != config.schedule.rest.after_longs;
        let fullscreen_changed = self.config.fullscreen != config.fullscreen;
        self.config = config;
        self.last_clock = now;
        if intervals_changed && self.coop_policy.is_none() {
            self.state = reschedule_work(
                self.state.clone(),
                self.config.schedule.mini.interval.as_millis(),
                now,
            );
            self.last_notified_due = None;
        } else if fullscreen_changed {
            update_deferred_limit(
                &mut self.state,
                self.config.fullscreen.max_delay.as_millis(),
            );
            if pending_context(&self.state).is_some() {
                return self.tick(now);
            }
        }
        Vec::new()
    }

    /// Only automatically scheduled breaks are delayed. Explicit break and
    /// notification actions remain available while a fullscreen app is active.
    pub fn set_fullscreen(&mut self, fullscreen: bool, now: ClockSample) -> Vec<Effect> {
        self.fullscreen = fullscreen;
        self.last_clock = now;
        if pending_context(&self.state).is_some() {
            self.tick(now)
        } else {
            Vec::new()
        }
    }

    pub fn startup_effects(&self, now: ClockSample) -> Vec<Effect> {
        visible_active(&self.state)
            .filter(|active| active.manual_resume || active.ends_boot_ms > now.boottime_ms)
            .map(|active| vec![Effect::StartOverlay(self.overlay_spec(active, now))])
            .unwrap_or_default()
    }

    pub fn overlay_recovery_exhausted(
        &mut self,
        session_id: BreakSessionId,
        now: ClockSample,
    ) -> Vec<Effect> {
        let Some(active) =
            visible_active(&self.state).filter(|active| active.session_id == session_id)
        else {
            return Vec::new();
        };
        let mut context = active.context.clone();
        context.pending = None;
        context.next_due_mono_ms = now
            .monotonic_ms
            .saturating_add(self.config.schedule.mini.interval.as_millis());
        self.state = SchedulerState::Running { context };
        self.last_clock = now;
        self.last_notified_due = None;
        vec![
            Effect::StopOverlay { session_id },
            Effect::Notify {
                summary: "Break could not be shown".into(),
                body: "The overlay could not be recovered. Your work timer has restarted; no break was counted.".into(),
            },
        ]
    }

    /// Exclude time when no overlay was mapped from a local break's rest time.
    /// The daemon owns readiness and only calls this for its current session.
    pub fn delay_active_overlay(
        &mut self,
        session_id: BreakSessionId,
        unavailable_ms: u64,
    ) -> bool {
        match &mut self.state {
            SchedulerState::MiniBreak { active }
            | SchedulerState::LongBreak { active }
            | SchedulerState::RestBreak { active }
                if active.session_id == session_id =>
            {
                active.started_boot_ms = active.started_boot_ms.saturating_add(unavailable_ms);
                active.ends_boot_ms = active.ends_boot_ms.saturating_add(unavailable_ms);
                active.strict_until_boot_ms =
                    active.strict_until_boot_ms.saturating_add(unavailable_ms);
                true
            }
            _ => false,
        }
    }

    pub fn handle_event(&mut self, event: SchedulerEvent, now: ClockSample) -> Vec<Effect> {
        self.last_clock = now;
        match event {
            SchedulerEvent::Tick => self.tick(now),
            SchedulerEvent::SuspendStarted => self.suspend(SuspendReason::Sleep, now),
            SchedulerEvent::SuspendEnded => self.resume_suspended(SuspendReason::Sleep, now),
            SchedulerEvent::LockStarted => self.suspend(SuspendReason::Lock, now),
            SchedulerEvent::LockEnded => self.resume_suspended(SuspendReason::Lock, now),
            SchedulerEvent::IdleThresholdReached => {
                if matches!(
                    self.state,
                    SchedulerState::PausedIndefinitely { .. }
                        | SchedulerState::PausedUntil { .. }
                        | SchedulerState::Suspended { .. }
                ) || self
                    .active_break()
                    .is_some_and(|active| active.manual_resume)
                {
                    Vec::new()
                } else {
                    self.enter_idle_reset(now)
                }
            }
            SchedulerEvent::ActivityResumed => {
                if matches!(self.state, SchedulerState::IdleReset { .. }) {
                    self.state = Self::fresh_running(&self.config, now);
                }
                Vec::new()
            }
            SchedulerEvent::OverlayFailed { session_id, reason } => {
                if self
                    .active_break()
                    .is_some_and(|active| active.session_id == session_id)
                {
                    vec![
                        Effect::OverlayDegraded {
                            session_id,
                            reason: reason.clone(),
                        },
                        Effect::Notify {
                            summary: "Break overlay unavailable".into(),
                            body: format!("The break is still active: {reason}"),
                        },
                    ]
                } else {
                    Vec::new()
                }
            }
        }
    }

    pub fn handle_command(
        &mut self,
        command: &Command,
        now: ClockSample,
    ) -> Result<Vec<Effect>, SchedulerError> {
        self.last_clock = now;
        match command {
            Command::Pause { duration } => self.pause(*duration, now),
            Command::Resume => self.resume_paused(now),
            Command::ResumeBreak => self.resume_break(now),
            Command::Reset => {
                self.ensure_dismissal_allowed(now)?;
                Ok(self.reset(now))
            }
            Command::Skip => self.skip(now),
            Command::Postpone => self.postpone(now),
            Command::StartDue { due_id } => self.start_due(*due_id, now),
            Command::PostponeDue { due_id } => self.postpone_due(*due_id, now),
            Command::Mini => self.manual_break(BreakKind::Mini, now),
            Command::Long => self.manual_break(BreakKind::Long, now),
            Command::Rest => self.manual_break(BreakKind::Rest, now),
            Command::Toggle => {
                if matches!(
                    self.state,
                    SchedulerState::PausedIndefinitely { .. } | SchedulerState::PausedUntil { .. }
                ) {
                    self.resume_paused(now)
                } else {
                    self.pause(None, now)
                }
            }
            Command::Status
            | Command::Reload
            | Command::Outputs
            | Command::Doctor
            | Command::CoopHost { .. }
            | Command::CoopJoin { .. }
            | Command::CoopLeave
            | Command::CoopStatus => Err(SchedulerError::DaemonCommand),
            Command::OverlayReady { .. } => Err(SchedulerError::DaemonCommand),
        }
    }

    /// A guest overlay can reach its wall-clock deadline a fraction before the
    /// host due to clock and scheduler jitter. Queue only that forwarded
    /// request when it arrives within one second of the authoritative host
    /// deadline; local commands continue to require an already-finished break.
    pub fn handle_coop_resume_request(
        &mut self,
        now: ClockSample,
    ) -> Result<Vec<Effect>, SchedulerError> {
        self.last_clock = now;
        let mut active = self
            .active_break()
            .cloned()
            .ok_or(SchedulerError::NoActiveBreak)?;
        if !active.manual_resume {
            return Err(SchedulerError::NotAwaitingResume);
        }
        if awaiting_manual_resume(&active, now) {
            return Ok(self.finish_active(active, now));
        }
        const EARLY_RESUME_TOLERANCE_MS: u64 = 1_000;
        if active.ends_boot_ms.saturating_sub(now.boottime_ms) > EARLY_RESUME_TOLERANCE_MS {
            return Err(SchedulerError::NotAwaitingResume);
        }
        active.resume_requested = true;
        self.state = match active.due.kind {
            BreakKind::Mini => SchedulerState::MiniBreak { active },
            BreakKind::Long => SchedulerState::LongBreak { active },
            BreakKind::Rest => SchedulerState::RestBreak { active },
        };
        Ok(Vec::new())
    }

    pub fn status(&self, now: ClockSample) -> SchedulerStatus {
        let context = self.context();
        let active = self.active_break();
        let paused_at = paused_at_in(&self.state);
        let paused = paused_at.is_some();
        let countdown_now = paused_at.unwrap_or(now);
        let awaiting_resume =
            active.is_some_and(|active| awaiting_manual_resume(active, countdown_now));
        let deferred = deferred_in(&self.state);
        let remaining_ms = if let Some(active) = active {
            Some(
                active
                    .ends_boot_ms
                    .saturating_sub(countdown_now.boottime_ms),
            )
        } else {
            context.map(|context| {
                context
                    .next_due_mono_ms
                    .saturating_sub(countdown_now.monotonic_ms)
            })
        };
        SchedulerStatus {
            state: state_name(&self.state).into(),
            paused,
            break_kind: active.map(|active| active.due.kind).or_else(|| {
                context.and_then(|context| context.pending.as_ref().map(|due| due.kind))
            }),
            remaining_ms,
            awaiting_resume,
            minis_since_long: context.map_or(0, |context| context.minis_since_long),
            longs_since_rest: context.map_or(0, |context| context.longs_since_rest),
            active_session: active.map(|active| active.session_id),
            postpone_count: active
                .map(|active| active.due.postpone_count)
                .or_else(|| {
                    context
                        .and_then(|context| context.pending.as_ref().map(|due| due.postpone_count))
                })
                .unwrap_or(0),
            can_skip: !paused
                && !awaiting_resume
                && active.is_some_and(|active| {
                    active.due.mirrored.as_ref().map_or_else(
                        || self.skip_available(active.due.kind),
                        |policy| policy.can_skip,
                    ) && !self.dismissal_locked(active, now)
                }),
            can_postpone: !paused
                && !awaiting_resume
                && (active.is_some_and(|active| {
                    active.due.mirrored.as_ref().map_or_else(
                        || self.postpone_allowed(active, now),
                        |policy| policy.can_postpone,
                    )
                }) || pending_context(&self.state)
                    .and_then(|context| context.pending.as_ref())
                    .is_some_and(|due| self.pending_postpone_available(due))),
            deferred,
            deferred_until_ms: deferred
                .then(|| now.wall_unix_ms.saturating_add(remaining_ms.unwrap_or(0))),
        }
    }

    /// Whether the active coordination policy asks this desktop to inhibit
    /// compositor shortcuts. The guest keeps its local integration mechanism,
    /// while the host decides whether inhibition is required.
    pub fn shortcut_inhibition_enabled(&self) -> bool {
        self.coop_policy.as_ref().map_or(
            self.config.strict.mode != StrictMode::Off && self.config.strict.inhibit_shortcuts,
            |policy| policy.inhibit_shortcuts,
        )
    }

    pub fn coop_snapshot(
        &mut self,
        host_id: uuid::Uuid,
        revision: u64,
        now: ClockSample,
    ) -> (CoopSnapshot, bool) {
        let prepared_pending = prepare_pending_for_coop(&mut self.state, &self.config);
        let status = self.status(now);
        let paused = matches!(
            self.state,
            SchedulerState::PausedIndefinitely { .. } | SchedulerState::PausedUntil { .. }
        );
        let resume_at_unix_ms = match &self.state {
            SchedulerState::PausedUntil { resume_mono_ms, .. } => {
                Some(wall_deadline_from_mono(*resume_mono_ms, now))
            }
            _ => None,
        };
        let phase = if matches!(
            self.state,
            SchedulerState::Suspended { .. }
                | SchedulerState::IdleReset { .. }
                | SchedulerState::Recovering { .. }
        ) {
            CoopPhase::Unavailable {
                reason: state_name(&self.state).into(),
            }
        } else if let Some(active) = self.active_break() {
            CoopPhase::Break {
                active: SharedBreak {
                    due_id: active.due.id,
                    session_id: active.session_id,
                    kind: active.due.kind,
                    started_unix_ms: wall_deadline_from_boot(active.started_boot_ms, now),
                    ends_unix_ms: wall_deadline_from_boot(active.ends_boot_ms, now),
                    strict_until_unix_ms: wall_deadline_from_boot(active.strict_until_boot_ms, now),
                    strict_entire: self.config.strict.mode == StrictMode::Entire,
                    manual_resume: active.manual_resume,
                    completion_sound_emitted: active.completion_sound_emitted,
                    can_skip: self.skip_available(active.due.kind),
                    can_postpone: self.postpone_available(active),
                },
            }
        } else if let Some(context) = self.context() {
            let due = context
                .pending
                .as_ref()
                .expect("co-op snapshot prepares a pending break");
            let duration_ms = self.break_duration(due.kind);
            // Advertise an anticipated deferral before its original deadline,
            // so guests do not predict a break while the host is presenting.
            let shared_due_mono_ms = if self.fullscreen
                && self.coop_policy.is_none()
                && self.config.fullscreen.behavior != FullscreenBehavior::Show
                && !deferred_in(&self.state)
            {
                context
                    .next_due_mono_ms
                    .saturating_add(self.config.fullscreen.max_delay.as_millis())
            } else {
                context.next_due_mono_ms
            };
            CoopPhase::Working {
                next: ScheduledBreak {
                    due_id: due.id,
                    kind: due.kind,
                    starts_unix_ms: wall_deadline_from_mono(shared_due_mono_ms, now),
                    duration_ms,
                    strict_duration_ms: self.strict_duration(duration_ms),
                    strict_entire: self.config.strict.mode == StrictMode::Entire,
                    manual_resume: self.config.completion.manual_resume,
                    can_skip: self.skip_available(due.kind),
                    can_postpone: self.postpone_available_for(due.kind, due.postpone_count),
                },
            }
        } else {
            CoopPhase::Unavailable {
                reason: state_name(&self.state).into(),
            }
        };

        (
            CoopSnapshot {
                host_id,
                revision,
                generated_unix_ms: now.wall_unix_ms,
                paused,
                resume_at_unix_ms,
                phase,
                minis_since_long: status.minis_since_long,
                longs_since_rest: status.longs_since_rest,
                postpone_count: status.postpone_count,
                can_skip: status.can_skip,
                can_postpone: status.can_postpone,
                policy: Some(CoopPolicy {
                    notifications_enabled: self.config.notifications.enabled,
                    mini_notification_lead_ms: self.config.notifications.mini_lead.as_millis(),
                    long_notification_lead_ms: self.config.notifications.long_lead.as_millis(),
                    rest_notification_lead_ms: self.config.notifications.rest_lead.as_millis(),
                    allow_postpone_during_lockout: self.config.strict.allow_postpone_during_lockout,
                    inhibit_shortcuts: self.config.strict.mode != StrictMode::Off
                        && self.config.strict.inhibit_shortcuts,
                }),
            },
            prepared_pending,
        )
    }

    pub fn adopt_coop_snapshot(
        &mut self,
        snapshot: &CoopSnapshot,
        now: ClockSample,
    ) -> Vec<Effect> {
        self.coop_policy = snapshot.policy.clone();
        let old_visible = visible_active(&self.state).map(|active| active.session_id);
        let context = |pending: Option<PendingBreak>, next_due_mono_ms: u64| ScheduleContext {
            cycle_started_mono_ms: now.monotonic_ms,
            next_due_mono_ms,
            minis_since_long: snapshot.minis_since_long,
            longs_since_rest: snapshot.longs_since_rest,
            rest_cycle_started_mono_ms: now.monotonic_ms,
            pending,
        };

        let base = match &snapshot.phase {
            CoopPhase::Working { next } => {
                let policy = MirroredBreakPolicy {
                    duration_ms: next.duration_ms,
                    strict_duration_ms: next.strict_duration_ms,
                    strict_entire: next.strict_entire,
                    manual_resume: next.manual_resume,
                    can_skip: next.can_skip,
                    can_postpone: next.can_postpone,
                };
                let due = PendingBreak {
                    id: next.due_id,
                    kind: next.kind,
                    postpone_count: snapshot.postpone_count,
                    mirrored: Some(policy),
                };
                if now.wall_unix_ms >= next.starts_unix_ms {
                    let active = ActiveBreak {
                        context: context(None, now.monotonic_ms),
                        due,
                        session_id: BreakSessionId(next.due_id.0),
                        started_boot_ms: boot_deadline_from_wall(next.starts_unix_ms, now),
                        ends_boot_ms: boot_deadline_from_wall(
                            next.starts_unix_ms.saturating_add(next.duration_ms),
                            now,
                        ),
                        strict_until_boot_ms: boot_deadline_from_wall(
                            next.starts_unix_ms.saturating_add(next.strict_duration_ms),
                            now,
                        ),
                        manual_resume: next.manual_resume,
                        completion_sound_emitted: false,
                        resume_requested: false,
                    };
                    match next.kind {
                        BreakKind::Mini => SchedulerState::MiniBreak { active },
                        BreakKind::Long => SchedulerState::LongBreak { active },
                        BreakKind::Rest => SchedulerState::RestBreak { active },
                    }
                } else {
                    SchedulerState::Running {
                        context: context(
                            Some(due),
                            mono_deadline_from_wall(next.starts_unix_ms, now),
                        ),
                    }
                }
            }
            CoopPhase::Break { active } => {
                let ends_boot_ms = boot_deadline_from_wall(active.ends_unix_ms, now);
                let strict_until_boot_ms =
                    boot_deadline_from_wall(active.strict_until_unix_ms, now);
                let duration_ms = active.ends_unix_ms.saturating_sub(active.started_unix_ms);
                let strict_duration_ms = active
                    .strict_until_unix_ms
                    .saturating_sub(active.started_unix_ms);
                let active = ActiveBreak {
                    context: context(None, now.monotonic_ms),
                    due: PendingBreak {
                        id: active.due_id,
                        kind: active.kind,
                        postpone_count: snapshot.postpone_count,
                        mirrored: Some(MirroredBreakPolicy {
                            duration_ms,
                            strict_duration_ms,
                            strict_entire: active.strict_entire,
                            manual_resume: active.manual_resume,
                            can_skip: active.can_skip,
                            can_postpone: active.can_postpone,
                        }),
                    },
                    session_id: active.session_id,
                    started_boot_ms: boot_deadline_from_wall(active.started_unix_ms, now),
                    ends_boot_ms,
                    strict_until_boot_ms,
                    manual_resume: active.manual_resume,
                    completion_sound_emitted: active.completion_sound_emitted,
                    resume_requested: false,
                };
                match active.due.kind {
                    BreakKind::Mini => SchedulerState::MiniBreak { active },
                    BreakKind::Long => SchedulerState::LongBreak { active },
                    BreakKind::Rest => SchedulerState::RestBreak { active },
                }
            }
            CoopPhase::Unavailable { reason } => SchedulerState::Recovering {
                reason: format!("co-op host unavailable: {reason}"),
            },
        };
        self.state = if snapshot.paused {
            match snapshot.resume_at_unix_ms {
                Some(resume_at) => SchedulerState::PausedUntil {
                    inner: Box::new(base),
                    paused_at: now,
                    resume_mono_ms: mono_deadline_from_wall(resume_at, now),
                },
                None => SchedulerState::PausedIndefinitely {
                    inner: Box::new(base),
                    paused_at: now,
                },
            }
        } else {
            base
        };
        self.last_clock = now;

        let new_visible = visible_active(&self.state);
        let mut effects = Vec::with_capacity(2);
        if old_visible != new_visible.map(|active| active.session_id) {
            if let Some(session_id) = old_visible {
                effects.push(Effect::StopOverlay { session_id });
            }
            if let Some(active) = new_visible
                && (active.manual_resume || active.ends_boot_ms > now.boottime_ms)
            {
                effects.push(Effect::StartOverlay(self.overlay_spec(active, now)));
            }
        }
        effects
    }

    pub fn reset_after_coop_disconnect(&mut self, now: ClockSample) -> Vec<Effect> {
        self.coop_policy = None;
        self.last_clock = now;
        self.reset(now)
    }

    fn fresh_running(config: &AppConfig, now: ClockSample) -> SchedulerState {
        SchedulerState::Running {
            context: ScheduleContext {
                cycle_started_mono_ms: now.monotonic_ms,
                next_due_mono_ms: now
                    .monotonic_ms
                    .saturating_add(config.schedule.mini.interval.as_millis()),
                minis_since_long: 0,
                longs_since_rest: 0,
                rest_cycle_started_mono_ms: now.monotonic_ms,
                pending: None,
            },
        }
    }

    fn tick(&mut self, now: ClockSample) -> Vec<Effect> {
        match self.state.clone() {
            SchedulerState::Running { mut context } => {
                let kind = context
                    .pending
                    .as_ref()
                    .map(|due| due.kind)
                    .unwrap_or_else(|| self.next_kind(&context, context.next_due_mono_ms));
                let quiet_fullscreen = self.fullscreen
                    && self.coop_policy.is_none()
                    && self.config.fullscreen.behavior == FullscreenBehavior::Postpone;
                let lead = if quiet_fullscreen {
                    0
                } else {
                    self.notification_lead(kind)
                };
                if now.monotonic_ms >= context.next_due_mono_ms.saturating_sub(lead) {
                    context.pending.get_or_insert_with(|| PendingBreak {
                        id: DueBreakId::new(),
                        kind,
                        postpone_count: 0,
                        mirrored: None,
                    });
                    if now.monotonic_ms >= context.next_due_mono_ms {
                        return self.begin_scheduled_break(kind, context, now);
                    }
                    let due_id = context
                        .pending
                        .as_ref()
                        .expect("pre-break context has a pending break")
                        .id;
                    let remaining_ms = context.next_due_mono_ms.saturating_sub(now.monotonic_ms);
                    let can_postpone = context
                        .pending
                        .as_ref()
                        .is_some_and(|due| self.pending_postpone_available(due));
                    self.state = match kind {
                        BreakKind::Mini => SchedulerState::PreMiniBreak { context },
                        BreakKind::Long => SchedulerState::PreLongBreak { context },
                        BreakKind::Rest => SchedulerState::PreRestBreak { context },
                    };
                    if self.notifications_enabled() && self.last_notified_due != Some(due_id) {
                        self.last_notified_due = Some(due_id);
                        return vec![Effect::NotifyDue {
                            summary: format!("{} break soon", title_kind(kind)),
                            body: if self.fullscreen
                                && self.coop_policy.is_none()
                                && self.config.fullscreen.behavior == FullscreenBehavior::NotifyOnly
                            {
                                format!(
                                    "Break due in {}. Fullscreen delays the overlay until you leave fullscreen, for at most {} after it is due.",
                                    DurationMs::from_millis(remaining_ms),
                                    self.config.fullscreen.max_delay,
                                )
                            } else {
                                format!("Starts in {}", DurationMs::from_millis(remaining_ms))
                            },
                            due_id,
                            can_postpone,
                        }];
                    }
                }
                Vec::new()
            }
            SchedulerState::PreMiniBreak { context }
                if now.monotonic_ms >= context.next_due_mono_ms =>
            {
                self.begin_scheduled_break(BreakKind::Mini, context, now)
            }
            SchedulerState::PreLongBreak { context }
                if now.monotonic_ms >= context.next_due_mono_ms =>
            {
                self.begin_scheduled_break(BreakKind::Long, context, now)
            }
            SchedulerState::PreRestBreak { context }
                if now.monotonic_ms >= context.next_due_mono_ms =>
            {
                self.begin_scheduled_break(BreakKind::Rest, context, now)
            }
            SchedulerState::DeferredBreak { context, .. }
                if !self.fullscreen
                    || self.config.fullscreen.behavior == FullscreenBehavior::Show
                    || now.monotonic_ms >= context.next_due_mono_ms =>
            {
                let due = context
                    .pending
                    .clone()
                    .expect("a deferred break is pending");
                self.begin_break(due.kind, context, due, now)
            }
            SchedulerState::MiniBreak { active }
            | SchedulerState::LongBreak { active }
            | SchedulerState::RestBreak { active }
                if now.boottime_ms >= active.ends_boot_ms =>
            {
                self.expire_active(active, now)
            }
            SchedulerState::PausedUntil {
                inner,
                paused_at,
                resume_mono_ms,
            } if now.monotonic_ms >= resume_mono_ms => {
                self.state = shift_state(
                    *inner,
                    now.monotonic_ms.saturating_sub(paused_at.monotonic_ms),
                    now.boottime_ms.saturating_sub(paused_at.boottime_ms),
                );
                self.startup_effects(now)
            }
            _ => Vec::new(),
        }
    }

    fn next_kind(&self, context: &ScheduleContext, at_mono_ms: u64) -> BreakKind {
        let rest_elapsed = at_mono_ms.saturating_sub(context.rest_cycle_started_mono_ms)
            >= self.config.schedule.rest.interval.as_millis();
        if rest_elapsed && context.longs_since_rest >= self.config.schedule.rest.after_longs {
            return BreakKind::Rest;
        }
        let long_elapsed = at_mono_ms.saturating_sub(context.cycle_started_mono_ms)
            >= self.config.schedule.long.interval.as_millis();
        if long_elapsed && context.minis_since_long >= self.config.schedule.long.after_minis {
            BreakKind::Long
        } else {
            BreakKind::Mini
        }
    }

    fn notification_lead(&self, kind: BreakKind) -> u64 {
        if !self.notifications_enabled() {
            return 0;
        }
        if let Some(policy) = &self.coop_policy {
            return match kind {
                BreakKind::Mini => policy.mini_notification_lead_ms,
                BreakKind::Long => policy.long_notification_lead_ms,
                BreakKind::Rest => policy.rest_notification_lead_ms,
            };
        }
        match kind {
            BreakKind::Mini => self.config.notifications.mini_lead.as_millis(),
            BreakKind::Long => self.config.notifications.long_lead.as_millis(),
            BreakKind::Rest => self.config.notifications.rest_lead.as_millis(),
        }
    }

    fn notifications_enabled(&self) -> bool {
        self.coop_policy
            .as_ref()
            .map_or(self.config.notifications.enabled, |policy| {
                policy.notifications_enabled
            })
    }

    fn begin_scheduled_break(
        &mut self,
        kind: BreakKind,
        mut context: ScheduleContext,
        now: ClockSample,
    ) -> Vec<Effect> {
        let due = context.pending.clone().unwrap_or_else(|| PendingBreak {
            id: DueBreakId::new(),
            kind,
            postpone_count: 0,
            mirrored: None,
        });
        if self.fullscreen
            && self.coop_policy.is_none()
            && self.config.fullscreen.behavior != FullscreenBehavior::Show
        {
            let original_due_mono_ms = context.next_due_mono_ms;
            let maximum =
                original_due_mono_ms.saturating_add(self.config.fullscreen.max_delay.as_millis());
            if now.monotonic_ms < maximum {
                let kind = due.kind;
                let can_postpone = self.pending_postpone_available(&due);
                let due_id = due.id;
                context.pending = Some(due);
                context.next_due_mono_ms = maximum;
                self.state = SchedulerState::DeferredBreak {
                    context,
                    original_due_mono_ms,
                };
                return if self.notifications_enabled()
                    && self.config.fullscreen.behavior == FullscreenBehavior::NotifyOnly
                {
                    vec![Effect::NotifyDue {
                        summary: format!("{} break due", title_kind(kind)),
                        body: format!(
                            "Fullscreen is active. Starts when you leave fullscreen or within {}.",
                            DurationMs::from_millis(maximum.saturating_sub(now.monotonic_ms))
                        ),
                        due_id,
                        can_postpone,
                    }]
                } else {
                    Vec::new()
                };
            }
        }
        self.begin_break(kind, context, due, now)
    }

    fn start_due(
        &mut self,
        due_id: DueBreakId,
        now: ClockSample,
    ) -> Result<Vec<Effect>, SchedulerError> {
        let context = pending_context(&self.state)
            .filter(|context| context.pending.as_ref().is_some_and(|due| due.id == due_id))
            .cloned()
            .ok_or(SchedulerError::StaleDueBreak)?;
        let due = context
            .pending
            .clone()
            .ok_or(SchedulerError::StaleDueBreak)?;
        Ok(self.begin_break(due.kind, context, due, now))
    }

    fn postpone_due(
        &mut self,
        due_id: DueBreakId,
        now: ClockSample,
    ) -> Result<Vec<Effect>, SchedulerError> {
        let mut context = pending_context(&self.state)
            .filter(|context| context.pending.as_ref().is_some_and(|due| due.id == due_id))
            .cloned()
            .ok_or(SchedulerError::StaleDueBreak)?;
        let due = context
            .pending
            .as_mut()
            .ok_or(SchedulerError::StaleDueBreak)?;
        if !self.pending_postpone_available(due) {
            let enabled = match due.kind {
                BreakKind::Mini => self.config.postpone.mini.enabled,
                BreakKind::Long => self.config.postpone.long.enabled,
                BreakKind::Rest => self.config.postpone.rest.enabled,
            };
            return Err(if enabled {
                SchedulerError::PostponeLimit
            } else {
                SchedulerError::PostponeDisabled
            });
        }
        let rule = match due.kind {
            BreakKind::Mini => &self.config.postpone.mini,
            BreakKind::Long => &self.config.postpone.long,
            BreakKind::Rest => &self.config.postpone.rest,
        };
        due.postpone_count = due.postpone_count.saturating_add(1);
        due.id = DueBreakId::new();
        context.next_due_mono_ms = now.monotonic_ms.saturating_add(rule.duration.as_millis());
        self.state = SchedulerState::Running { context };
        Ok(Vec::new())
    }

    fn manual_break(
        &mut self,
        kind: BreakKind,
        now: ClockSample,
    ) -> Result<Vec<Effect>, SchedulerError> {
        if self.active_break().is_some() {
            return Err(SchedulerError::BreakAlreadyActive);
        }
        let context = self.context().cloned().unwrap_or(ScheduleContext {
            cycle_started_mono_ms: now.monotonic_ms,
            next_due_mono_ms: now.monotonic_ms,
            minis_since_long: 0,
            longs_since_rest: 0,
            rest_cycle_started_mono_ms: now.monotonic_ms,
            pending: None,
        });
        let due = PendingBreak {
            id: DueBreakId::new(),
            kind,
            postpone_count: 0,
            mirrored: None,
        };
        Ok(self.begin_break(kind, context, due, now))
    }

    fn begin_break(
        &mut self,
        kind: BreakKind,
        context: ScheduleContext,
        due: PendingBreak,
        now: ClockSample,
    ) -> Vec<Effect> {
        let duration = due
            .mirrored
            .as_ref()
            .map_or_else(|| self.break_duration(kind), |policy| policy.duration_ms);
        let ends_boot_ms = now.boottime_ms.saturating_add(duration);
        let strict_until_boot_ms =
            now.boottime_ms
                .saturating_add(due.mirrored.as_ref().map_or_else(
                    || self.strict_duration(duration),
                    |policy| policy.strict_duration_ms,
                ));
        let manual_resume = due
            .mirrored
            .as_ref()
            .map_or(self.config.completion.manual_resume, |policy| {
                policy.manual_resume
            });
        let session_id = BreakSessionId(due.id.0);
        let active = ActiveBreak {
            context,
            due,
            session_id,
            started_boot_ms: now.boottime_ms,
            ends_boot_ms,
            strict_until_boot_ms,
            manual_resume,
            completion_sound_emitted: false,
            resume_requested: false,
        };
        let effect = Effect::StartOverlay(self.overlay_spec(&active, now));
        self.state = match kind {
            BreakKind::Mini => SchedulerState::MiniBreak { active },
            BreakKind::Long => SchedulerState::LongBreak { active },
            BreakKind::Rest => SchedulerState::RestBreak { active },
        };
        vec![effect]
    }

    fn expire_active(&mut self, mut active: ActiveBreak, now: ClockSample) -> Vec<Effect> {
        if !active.manual_resume || active.resume_requested {
            return self.finish_active(active, now);
        }

        let play_completion_sound = !active.completion_sound_emitted;
        active.completion_sound_emitted = true;
        self.state = match active.due.kind {
            BreakKind::Mini => SchedulerState::MiniBreak { active },
            BreakKind::Long => SchedulerState::LongBreak { active },
            BreakKind::Rest => SchedulerState::RestBreak { active },
        };
        play_completion_sound
            .then_some(Effect::PlayCompletionSound)
            .into_iter()
            .collect()
    }

    fn finish_active(&mut self, active: ActiveBreak, now: ClockSample) -> Vec<Effect> {
        let play_completion_sound =
            now.boottime_ms >= active.ends_boot_ms && !active.completion_sound_emitted;
        let session_id = active.session_id;
        let mut context = active.context;
        match active.due.kind {
            BreakKind::Mini => {
                context.minis_since_long = context.minis_since_long.saturating_add(1);
            }
            BreakKind::Long => {
                context.minis_since_long = 0;
                context.cycle_started_mono_ms = now.monotonic_ms;
                context.longs_since_rest = context.longs_since_rest.saturating_add(1);
            }
            BreakKind::Rest => {
                context.minis_since_long = 0;
                context.cycle_started_mono_ms = now.monotonic_ms;
                context.longs_since_rest = 0;
                context.rest_cycle_started_mono_ms = now.monotonic_ms;
            }
        }
        context.pending = None;
        context.next_due_mono_ms = now
            .monotonic_ms
            .saturating_add(self.config.schedule.mini.interval.as_millis());
        self.state = SchedulerState::Running { context };
        let mut effects = Vec::with_capacity(2);
        if play_completion_sound {
            effects.push(Effect::PlayCompletionSound);
        }
        effects.push(Effect::StopOverlay { session_id });
        effects
    }

    fn skip(&mut self, now: ClockSample) -> Result<Vec<Effect>, SchedulerError> {
        let active = self
            .active_break()
            .cloned()
            .ok_or(SchedulerError::NoActiveBreak)?;
        if awaiting_manual_resume(&active, now) {
            return Err(SchedulerError::AwaitingResume);
        }
        if !self.skip_available(active.due.kind) {
            return Err(SchedulerError::SkipDisabled);
        }
        if self.dismissal_locked(&active, now) {
            return Err(SchedulerError::StrictMode);
        }
        Ok(self.finish_active(active, now))
    }

    fn resume_break(&mut self, now: ClockSample) -> Result<Vec<Effect>, SchedulerError> {
        let active = self
            .active_break()
            .cloned()
            .ok_or(SchedulerError::NoActiveBreak)?;
        if !awaiting_manual_resume(&active, now) {
            return Err(SchedulerError::NotAwaitingResume);
        }
        Ok(self.finish_active(active, now))
    }

    fn ensure_dismissal_allowed(&self, now: ClockSample) -> Result<(), SchedulerError> {
        if let Some(active) = self.active_break() {
            if awaiting_manual_resume(active, now) {
                return Err(SchedulerError::AwaitingResume);
            }
            if !self.skip_available(active.due.kind) {
                return Err(SchedulerError::SkipDisabled);
            }
            if now.boottime_ms < active.strict_until_boot_ms {
                return Err(SchedulerError::StrictMode);
            }
        }
        Ok(())
    }

    fn postpone(&mut self, now: ClockSample) -> Result<Vec<Effect>, SchedulerError> {
        if let Some(due) = pending_context(&self.state).and_then(|context| context.pending.as_ref())
        {
            return self.postpone_due(due.id, now);
        }
        let mut active = self
            .active_break()
            .cloned()
            .ok_or(SchedulerError::NoActiveBreak)?;
        if awaiting_manual_resume(&active, now) {
            return Err(SchedulerError::AwaitingResume);
        }
        let rule = match active.due.kind {
            BreakKind::Mini => &self.config.postpone.mini,
            BreakKind::Long => &self.config.postpone.long,
            BreakKind::Rest => &self.config.postpone.rest,
        };
        if !rule.enabled {
            return Err(SchedulerError::PostponeDisabled);
        }
        if self.dismissal_locked(&active, now) && !self.config.strict.allow_postpone_during_lockout
        {
            return Err(SchedulerError::StrictMode);
        }
        if rule
            .max_postponements
            .is_some_and(|maximum| active.due.postpone_count >= maximum)
        {
            return Err(SchedulerError::PostponeLimit);
        }
        active.due.postpone_count = active.due.postpone_count.saturating_add(1);
        let session_id = active.session_id;
        active.due.id = DueBreakId::new();
        let mut context = active.context;
        context.next_due_mono_ms = now.monotonic_ms.saturating_add(rule.duration.as_millis());
        context.pending = Some(active.due);
        self.state = SchedulerState::Running { context };
        Ok(vec![Effect::StopOverlay { session_id }])
    }

    fn pause(
        &mut self,
        duration: Option<DurationMs>,
        now: ClockSample,
    ) -> Result<Vec<Effect>, SchedulerError> {
        if matches!(
            self.state,
            SchedulerState::PausedIndefinitely { .. } | SchedulerState::PausedUntil { .. }
        ) {
            return Err(SchedulerError::AlreadyPaused);
        }
        self.ensure_dismissal_allowed(now)?;
        let effects = self
            .active_break()
            .map(|active| Effect::StopOverlay {
                session_id: active.session_id,
            })
            .into_iter()
            .collect();
        let inner = Box::new(self.state.clone());
        self.state = match duration {
            Some(duration) => SchedulerState::PausedUntil {
                inner,
                paused_at: now,
                resume_mono_ms: now.monotonic_ms.saturating_add(duration.as_millis()),
            },
            None => SchedulerState::PausedIndefinitely {
                inner,
                paused_at: now,
            },
        };
        Ok(effects)
    }

    fn resume_paused(&mut self, now: ClockSample) -> Result<Vec<Effect>, SchedulerError> {
        let (inner, paused_at) = match self.state.clone() {
            SchedulerState::PausedIndefinitely { inner, paused_at }
            | SchedulerState::PausedUntil {
                inner, paused_at, ..
            } => (inner, paused_at),
            _ => return Err(SchedulerError::NotPaused),
        };
        self.state = shift_state(
            *inner,
            now.monotonic_ms.saturating_sub(paused_at.monotonic_ms),
            now.boottime_ms.saturating_sub(paused_at.boottime_ms),
        );
        Ok(self.startup_effects(now))
    }

    fn suspend(&mut self, reason: SuspendReason, now: ClockSample) -> Vec<Effect> {
        if let SchedulerState::Suspended {
            reason: initial_reason,
            started_at,
            overlap,
            ..
        } = &mut self.state
        {
            if *initial_reason != reason || overlap.is_some() {
                let activity = overlap
                    .get_or_insert_with(|| SuspensionActivity::new(*initial_reason, *started_at));
                activity.advance(
                    now,
                    self.config.recovery.suspend_counts_as_break,
                    self.config.recovery.lock_counts_as_break,
                );
                activity.set_active(reason, true);
            }
            return Vec::new();
        }
        let effects = self
            .active_break()
            .map(|active| Effect::StopOverlay {
                session_id: active.session_id,
            })
            .into_iter()
            .collect();
        self.state = SchedulerState::Suspended {
            reason,
            inner: Box::new(self.state.clone()),
            started_at: now,
            overlap: None,
        };
        effects
    }

    fn resume_suspended(&mut self, expected: SuspendReason, now: ClockSample) -> Vec<Effect> {
        let SchedulerState::Suspended {
            reason,
            inner,
            started_at,
            overlap,
        } = self.state.clone()
        else {
            return Vec::new();
        };
        let frozen_boot_ms = if let Some(mut activity) = overlap {
            if !activity.is_active(expected) {
                return Vec::new();
            }
            activity.advance(
                now,
                self.config.recovery.suspend_counts_as_break,
                self.config.recovery.lock_counts_as_break,
            );
            activity.set_active(expected, false);
            if activity.sleeping || activity.locked {
                self.state = SchedulerState::Suspended {
                    reason,
                    inner,
                    started_at,
                    overlap: Some(activity),
                };
                return Vec::new();
            }
            Some(activity.frozen_boot_ms)
        } else {
            if reason != expected {
                return Vec::new();
            }
            None
        };
        // Desktop inactivity must not discard a pause the user chose. Its
        // original clock also accounts for time spent locked or asleep when
        // the pause eventually resumes, without adding a second wake pause.
        if matches!(
            inner.as_ref(),
            SchedulerState::PausedIndefinitely { .. } | SchedulerState::PausedUntil { .. }
        ) {
            self.state = *inner;
            return self.tick(now);
        }
        let elapsed_boot = now.boottime_ms.saturating_sub(started_at.boottime_ms);
        let holds_for_manual_resume =
            active_break_in(&inner).is_some_and(|active| active.manual_resume);
        if elapsed_boot >= self.config.idle.reset_after.as_millis() && !holds_for_manual_resume {
            self.state = Self::fresh_running(&self.config, now);
            return Vec::new();
        }

        let counts_as_break = match reason {
            SuspendReason::Sleep => self.config.recovery.suspend_counts_as_break,
            SuspendReason::Lock => self.config.recovery.lock_counts_as_break,
        };
        let mono_delta = if reason == SuspendReason::Lock || frozen_boot_ms.is_some() {
            now.monotonic_ms.saturating_sub(started_at.monotonic_ms)
        } else {
            0
        };
        let boot_delta = frozen_boot_ms.unwrap_or(if counts_as_break { 0 } else { elapsed_boot });
        self.state = shift_state(*inner, mono_delta, boot_delta);
        let wake_grace = self.config.recovery.wake_grace.as_millis();
        if wake_grace > 0 {
            self.state = SchedulerState::PausedUntil {
                inner: Box::new(self.state.clone()),
                paused_at: now,
                resume_mono_ms: now.monotonic_ms.saturating_add(wake_grace),
            };
            return Vec::new();
        }
        let mut effects = self.tick(now);
        if self.active_break().is_some() {
            effects.extend(self.startup_effects(now));
        }
        effects
    }

    fn enter_idle_reset(&mut self, now: ClockSample) -> Vec<Effect> {
        let effects = self
            .active_break()
            .map(|active| Effect::StopOverlay {
                session_id: active.session_id,
            })
            .into_iter()
            .collect();
        self.state = SchedulerState::IdleReset {
            started_boot_ms: now.boottime_ms,
        };
        effects
    }

    fn reset(&mut self, now: ClockSample) -> Vec<Effect> {
        let effects = self
            .active_break()
            .map(|active| Effect::StopOverlay {
                session_id: active.session_id,
            })
            .into_iter()
            .collect();
        self.state = Self::fresh_running(&self.config, now);
        effects
    }

    fn break_duration(&self, kind: BreakKind) -> u64 {
        match kind {
            BreakKind::Mini => self.config.schedule.mini.duration.as_millis(),
            BreakKind::Long => self.config.schedule.long.duration.as_millis(),
            BreakKind::Rest => self.config.schedule.rest.duration.as_millis(),
        }
    }

    fn strict_duration(&self, break_duration: u64) -> u64 {
        match self.config.strict.mode {
            StrictMode::Off => 0,
            StrictMode::Delay => self
                .config
                .strict
                .minimum_visible
                .as_millis()
                .min(break_duration),
            StrictMode::Entire => break_duration,
        }
    }

    fn overlay_spec(&self, active: &ActiveBreak, now: ClockSample) -> OverlaySpec {
        let message =
            if self.config.content.show_message && !self.config.content.messages.is_empty() {
                let index = message_index(
                    active.due.id.0.as_u128(),
                    self.config.content.messages.len(),
                );
                Some(self.config.content.messages[index].clone())
            } else {
                None
            };
        OverlaySpec {
            session_id: active.session_id,
            ready_token: None,
            preview: false,
            kind: active.due.kind,
            duration: DurationMs::from_millis(active.ends_boot_ms.saturating_sub(now.boottime_ms)),
            strict_remaining: DurationMs::from_millis(
                if active
                    .due
                    .mirrored
                    .as_ref()
                    .map_or(self.config.strict.mode == StrictMode::Entire, |policy| {
                        policy.strict_entire
                    })
                {
                    active.strict_until_boot_ms.saturating_sub(now.boottime_ms)
                } else {
                    0
                },
            ),
            can_skip: active.due.mirrored.as_ref().map_or_else(
                || self.skip_available(active.due.kind),
                |policy| policy.can_skip,
            ),
            can_postpone: active.due.mirrored.as_ref().map_or_else(
                || self.postpone_available(active),
                |policy| policy.can_postpone,
            ),
            manual_resume: active.manual_resume,
            allow_postpone_during_lockout: self
                .coop_policy
                .as_ref()
                .map_or(self.config.strict.allow_postpone_during_lockout, |policy| {
                    policy.allow_postpone_during_lockout
                }),
            inhibit_shortcuts: self.shortcut_inhibition_enabled(),
            message,
            socket_path: self.socket_path.clone(),
        }
    }

    fn skip_available(&self, kind: BreakKind) -> bool {
        match kind {
            BreakKind::Mini => self.config.skip.mini.enabled,
            BreakKind::Long => self.config.skip.long.enabled,
            BreakKind::Rest => self.config.skip.rest.enabled,
        }
    }

    fn postpone_allowed(&self, active: &ActiveBreak, now: ClockSample) -> bool {
        self.postpone_available(active)
            && (!self.dismissal_locked(active, now)
                || self.config.strict.allow_postpone_during_lockout)
    }

    /// Whether strict mode currently blocks dismissing the break with skip or
    /// postpone. Only `StrictMode::Entire` holds those controls for the whole
    /// break; the `Delay` mode's minimum-visible window intentionally does not
    /// gate skip or postpone, so the user can act on the break immediately.
    fn dismissal_locked(&self, active: &ActiveBreak, now: ClockSample) -> bool {
        active
            .due
            .mirrored
            .as_ref()
            .map_or(self.config.strict.mode == StrictMode::Entire, |policy| {
                policy.strict_entire
            })
            && now.boottime_ms < active.strict_until_boot_ms
    }

    fn postpone_available(&self, active: &ActiveBreak) -> bool {
        self.postpone_available_for(active.due.kind, active.due.postpone_count)
    }

    fn postpone_available_for(&self, kind: BreakKind, postpone_count: u32) -> bool {
        let rule = match kind {
            BreakKind::Mini => &self.config.postpone.mini,
            BreakKind::Long => &self.config.postpone.long,
            BreakKind::Rest => &self.config.postpone.rest,
        };
        rule.enabled
            && rule
                .max_postponements
                .is_none_or(|maximum| postpone_count < maximum)
    }

    fn pending_postpone_available(&self, due: &PendingBreak) -> bool {
        due.mirrored.as_ref().map_or_else(
            || self.postpone_available_for(due.kind, due.postpone_count),
            |policy| policy.can_postpone,
        )
    }

    fn active_break(&self) -> Option<&ActiveBreak> {
        active_break_in(&self.state)
    }

    fn context(&self) -> Option<&ScheduleContext> {
        context_in(&self.state)
    }
}

fn active_break_in(state: &SchedulerState) -> Option<&ActiveBreak> {
    match state {
        SchedulerState::MiniBreak { active }
        | SchedulerState::LongBreak { active }
        | SchedulerState::RestBreak { active } => Some(active),
        SchedulerState::PausedIndefinitely { inner, .. }
        | SchedulerState::PausedUntil { inner, .. }
        | SchedulerState::Suspended { inner, .. } => active_break_in(inner),
        _ => None,
    }
}

fn pending_context(state: &SchedulerState) -> Option<&ScheduleContext> {
    match state {
        SchedulerState::Running { context }
        | SchedulerState::PreMiniBreak { context }
        | SchedulerState::PreLongBreak { context }
        | SchedulerState::PreRestBreak { context }
        | SchedulerState::DeferredBreak { context, .. } => Some(context),
        _ => None,
    }
}

fn deferred_in(state: &SchedulerState) -> bool {
    match state {
        SchedulerState::DeferredBreak { .. } => true,
        SchedulerState::PausedIndefinitely { inner, .. }
        | SchedulerState::PausedUntil { inner, .. }
        | SchedulerState::Suspended { inner, .. } => deferred_in(inner),
        _ => false,
    }
}

fn reschedule_work(state: SchedulerState, interval_ms: u64, now: ClockSample) -> SchedulerState {
    match state {
        SchedulerState::Running { mut context }
        | SchedulerState::PreMiniBreak { mut context }
        | SchedulerState::PreLongBreak { mut context }
        | SchedulerState::PreRestBreak { mut context }
        | SchedulerState::DeferredBreak { mut context, .. } => {
            context.pending = None;
            context.next_due_mono_ms = now.monotonic_ms.saturating_add(interval_ms);
            SchedulerState::Running { context }
        }
        SchedulerState::PausedIndefinitely { inner, paused_at } => {
            SchedulerState::PausedIndefinitely {
                inner: Box::new(reschedule_work(*inner, interval_ms, paused_at)),
                paused_at,
            }
        }
        SchedulerState::PausedUntil {
            inner,
            paused_at,
            resume_mono_ms,
        } => SchedulerState::PausedUntil {
            inner: Box::new(reschedule_work(*inner, interval_ms, paused_at)),
            paused_at,
            resume_mono_ms,
        },
        SchedulerState::Suspended {
            inner,
            reason,
            started_at,
            overlap,
        } => SchedulerState::Suspended {
            inner: Box::new(reschedule_work(*inner, interval_ms, started_at)),
            reason,
            started_at,
            overlap,
        },
        other => other,
    }
}

fn update_deferred_limit(state: &mut SchedulerState, max_delay_ms: u64) {
    match state {
        SchedulerState::DeferredBreak {
            context,
            original_due_mono_ms,
        } => {
            context.next_due_mono_ms = original_due_mono_ms.saturating_add(max_delay_ms);
        }
        SchedulerState::PausedIndefinitely { inner, .. }
        | SchedulerState::PausedUntil { inner, .. }
        | SchedulerState::Suspended { inner, .. } => update_deferred_limit(inner, max_delay_ms),
        _ => {}
    }
}

fn paused_at_in(state: &SchedulerState) -> Option<ClockSample> {
    match state {
        SchedulerState::PausedIndefinitely { paused_at, .. }
        | SchedulerState::PausedUntil { paused_at, .. } => Some(*paused_at),
        SchedulerState::Suspended { inner, .. } => paused_at_in(inner),
        _ => None,
    }
}

fn visible_active(state: &SchedulerState) -> Option<&ActiveBreak> {
    match state {
        SchedulerState::MiniBreak { active }
        | SchedulerState::LongBreak { active }
        | SchedulerState::RestBreak { active } => Some(active),
        _ => None,
    }
}

fn prepare_pending_for_coop(state: &mut SchedulerState, config: &AppConfig) -> bool {
    match state {
        SchedulerState::Running { context }
        | SchedulerState::PreMiniBreak { context }
        | SchedulerState::PreLongBreak { context }
        | SchedulerState::PreRestBreak { context }
        | SchedulerState::DeferredBreak { context, .. } => {
            if context.pending.is_some() {
                return false;
            }
            let rest_elapsed = context
                .next_due_mono_ms
                .saturating_sub(context.rest_cycle_started_mono_ms)
                >= config.schedule.rest.interval.as_millis();
            let long_elapsed = context
                .next_due_mono_ms
                .saturating_sub(context.cycle_started_mono_ms)
                >= config.schedule.long.interval.as_millis();
            let kind = if rest_elapsed
                && context.longs_since_rest >= config.schedule.rest.after_longs
            {
                BreakKind::Rest
            } else if long_elapsed && context.minis_since_long >= config.schedule.long.after_minis {
                BreakKind::Long
            } else {
                BreakKind::Mini
            };
            context.pending = Some(PendingBreak {
                id: DueBreakId::new(),
                kind,
                postpone_count: 0,
                mirrored: None,
            });
            true
        }
        SchedulerState::PausedIndefinitely { inner, .. }
        | SchedulerState::PausedUntil { inner, .. } => prepare_pending_for_coop(inner, config),
        SchedulerState::MiniBreak { .. }
        | SchedulerState::LongBreak { .. }
        | SchedulerState::RestBreak { .. }
        | SchedulerState::Suspended { .. }
        | SchedulerState::IdleReset { .. }
        | SchedulerState::Recovering { .. } => false,
    }
}

fn wall_deadline_from_mono(deadline_mono_ms: u64, now: ClockSample) -> u64 {
    translate_deadline(deadline_mono_ms, now.monotonic_ms, now.wall_unix_ms)
}

fn wall_deadline_from_boot(deadline_boot_ms: u64, now: ClockSample) -> u64 {
    translate_deadline(deadline_boot_ms, now.boottime_ms, now.wall_unix_ms)
}

fn mono_deadline_from_wall(deadline_wall_ms: u64, now: ClockSample) -> u64 {
    translate_deadline(deadline_wall_ms, now.wall_unix_ms, now.monotonic_ms)
}

fn boot_deadline_from_wall(deadline_wall_ms: u64, now: ClockSample) -> u64 {
    translate_deadline(deadline_wall_ms, now.wall_unix_ms, now.boottime_ms)
}

fn translate_deadline(deadline: u64, source_now: u64, destination_now: u64) -> u64 {
    if deadline >= source_now {
        destination_now.saturating_add(deadline - source_now)
    } else {
        destination_now.saturating_sub(source_now - deadline)
    }
}

fn context_in(state: &SchedulerState) -> Option<&ScheduleContext> {
    match state {
        SchedulerState::Running { context }
        | SchedulerState::PreMiniBreak { context }
        | SchedulerState::PreLongBreak { context }
        | SchedulerState::PreRestBreak { context }
        | SchedulerState::DeferredBreak { context, .. } => Some(context),
        SchedulerState::MiniBreak { active }
        | SchedulerState::LongBreak { active }
        | SchedulerState::RestBreak { active } => Some(&active.context),
        SchedulerState::PausedIndefinitely { inner, .. }
        | SchedulerState::PausedUntil { inner, .. }
        | SchedulerState::Suspended { inner, .. } => context_in(inner),
        SchedulerState::IdleReset { .. } | SchedulerState::Recovering { .. } => None,
    }
}

fn state_is_overdue(state: &SchedulerState, now: ClockSample) -> bool {
    match state {
        SchedulerState::Running { context }
        | SchedulerState::PreMiniBreak { context }
        | SchedulerState::PreLongBreak { context }
        | SchedulerState::PreRestBreak { context }
        | SchedulerState::DeferredBreak { context, .. } => {
            now.monotonic_ms >= context.next_due_mono_ms
        }
        SchedulerState::MiniBreak { active }
        | SchedulerState::LongBreak { active }
        | SchedulerState::RestBreak { active } => {
            !active.manual_resume && now.boottime_ms >= active.ends_boot_ms
        }
        SchedulerState::PausedIndefinitely { .. }
        | SchedulerState::PausedUntil { .. }
        | SchedulerState::Suspended { .. }
        | SchedulerState::IdleReset { .. }
        | SchedulerState::Recovering { .. } => false,
    }
}

fn shift_state(state: SchedulerState, mono_delta: u64, boot_delta: u64) -> SchedulerState {
    match state {
        SchedulerState::Running { mut context } => {
            shift_context(&mut context, mono_delta);
            SchedulerState::Running { context }
        }
        SchedulerState::PreMiniBreak { mut context } => {
            shift_context(&mut context, mono_delta);
            SchedulerState::PreMiniBreak { context }
        }
        SchedulerState::PreLongBreak { mut context } => {
            shift_context(&mut context, mono_delta);
            SchedulerState::PreLongBreak { context }
        }
        SchedulerState::PreRestBreak { mut context } => {
            shift_context(&mut context, mono_delta);
            SchedulerState::PreRestBreak { context }
        }
        SchedulerState::DeferredBreak {
            mut context,
            original_due_mono_ms,
        } => {
            shift_context(&mut context, mono_delta);
            SchedulerState::DeferredBreak {
                context,
                original_due_mono_ms: original_due_mono_ms.saturating_add(mono_delta),
            }
        }
        SchedulerState::MiniBreak { mut active } => {
            shift_active(&mut active, mono_delta, boot_delta);
            SchedulerState::MiniBreak { active }
        }
        SchedulerState::LongBreak { mut active } => {
            shift_active(&mut active, mono_delta, boot_delta);
            SchedulerState::LongBreak { active }
        }
        SchedulerState::RestBreak { mut active } => {
            shift_active(&mut active, mono_delta, boot_delta);
            SchedulerState::RestBreak { active }
        }
        other => other,
    }
}

fn shift_context(context: &mut ScheduleContext, mono_delta: u64) {
    context.cycle_started_mono_ms = context.cycle_started_mono_ms.saturating_add(mono_delta);
    context.rest_cycle_started_mono_ms = context
        .rest_cycle_started_mono_ms
        .saturating_add(mono_delta);
    context.next_due_mono_ms = context.next_due_mono_ms.saturating_add(mono_delta);
}

fn shift_active(active: &mut ActiveBreak, mono_delta: u64, boot_delta: u64) {
    shift_context(&mut active.context, mono_delta);
    active.started_boot_ms = active.started_boot_ms.saturating_add(boot_delta);
    active.ends_boot_ms = active.ends_boot_ms.saturating_add(boot_delta);
    active.strict_until_boot_ms = active.strict_until_boot_ms.saturating_add(boot_delta);
}

fn state_name(state: &SchedulerState) -> &'static str {
    match state {
        SchedulerState::Running { .. } => "running",
        SchedulerState::PreMiniBreak { .. } => "pre-mini-break",
        SchedulerState::MiniBreak { .. } => "mini-break",
        SchedulerState::PreLongBreak { .. } => "pre-long-break",
        SchedulerState::LongBreak { .. } => "long-break",
        SchedulerState::PreRestBreak { .. } => "pre-rest-break",
        SchedulerState::DeferredBreak { .. } => "fullscreen-deferred",
        SchedulerState::RestBreak { .. } => "rest-break",
        SchedulerState::PausedIndefinitely { .. } => "paused-indefinitely",
        SchedulerState::PausedUntil { .. } => "paused-until",
        SchedulerState::Suspended { .. } => "suspended",
        SchedulerState::IdleReset { .. } => "idle-reset",
        SchedulerState::Recovering { .. } => "recovering",
    }
}

fn title_kind(kind: BreakKind) -> &'static str {
    match kind {
        BreakKind::Mini => "Mini",
        BreakKind::Long => "Long",
        BreakKind::Rest => "Rest",
    }
}

fn message_index(due_id: u128, message_count: usize) -> usize {
    (due_id % message_count as u128) as usize
}

fn awaiting_manual_resume(active: &ActiveBreak, now: ClockSample) -> bool {
    active.manual_resume && now.boottime_ms >= active.ends_boot_ms
}

#[cfg(test)]
mod tests {
    use breakd_config::defaults;
    use proptest::prelude::*;

    use super::*;

    fn clock(ms: u64) -> ClockSample {
        ClockSample {
            monotonic_ms: ms,
            boottime_ms: ms,
            wall_unix_ms: 1_700_000_000_000 + ms,
        }
    }

    fn test_scheduler() -> Scheduler {
        let mut config = defaults();
        config.schedule.mini.interval = DurationMs::from_millis(1_000);
        config.schedule.mini.duration = DurationMs::from_millis(100);
        config.schedule.long.interval = DurationMs::from_millis(3_000);
        config.schedule.long.duration = DurationMs::from_millis(300);
        config.schedule.rest.interval = DurationMs::from_millis(8_000);
        config.schedule.rest.duration = DurationMs::from_millis(500);
        config.schedule.rest.after_longs = 2;
        config.notifications.mini_lead = DurationMs::from_millis(100);
        config.notifications.long_lead = DurationMs::from_millis(100);
        config.notifications.rest_lead = DurationMs::from_millis(100);
        config.strict.minimum_visible = DurationMs::from_millis(20);
        config.postpone.mini.max_postponements = Some(1);
        config.postpone.long.max_postponements = Some(1);
        config.postpone.rest.max_postponements = Some(1);
        Scheduler::new(config, "boot".into(), clock(0), "/tmp/breakd.sock".into())
    }

    fn fullscreen_scheduler(behavior: FullscreenBehavior) -> Scheduler {
        let mut scheduler = test_scheduler();
        scheduler.config.fullscreen.behavior = behavior;
        scheduler.config.fullscreen.max_delay = DurationMs::from_millis(500);
        scheduler.set_fullscreen(true, clock(0));
        scheduler
    }

    #[test]
    fn fullscreen_breaks_wait_for_exit_and_keep_a_bounded_deadline() {
        for behavior in [FullscreenBehavior::Postpone, FullscreenBehavior::NotifyOnly] {
            let mut scheduler = fullscreen_scheduler(behavior);
            let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(1_000));
            if behavior == FullscreenBehavior::Postpone {
                assert!(effects.is_empty());
            } else {
                assert!(matches!(effects.as_slice(), [Effect::NotifyDue { .. }]));
            }
            let status = scheduler.status(clock(1_000));
            assert!(status.deferred);
            assert_eq!(status.state, "fullscreen-deferred");
            assert_eq!(status.remaining_ms, Some(500));
            assert_eq!(status.deferred_until_ms, Some(clock(1_500).wall_unix_ms));
            assert!(scheduler.set_fullscreen(true, clock(1_200)).is_empty());
            let effects = scheduler.set_fullscreen(false, clock(1_200));
            assert!(matches!(effects.as_slice(), [Effect::StartOverlay(_)]));
            assert!(!scheduler.status(clock(1_200)).deferred);
        }
    }

    #[test]
    fn fullscreen_postpone_is_quiet_until_the_overlay_is_allowed() {
        let mut scheduler = fullscreen_scheduler(FullscreenBehavior::Postpone);
        assert!(
            scheduler
                .handle_event(SchedulerEvent::Tick, clock(900))
                .is_empty()
        );
        assert!(
            scheduler
                .handle_event(SchedulerEvent::Tick, clock(1_000))
                .is_empty()
        );
        assert!(scheduler.status(clock(1_000)).deferred);
        assert!(
            scheduler
                .handle_event(SchedulerEvent::Tick, clock(1_400))
                .is_empty()
        );
        assert!(matches!(
            scheduler.set_fullscreen(false, clock(1_450)).as_slice(),
            [Effect::StartOverlay(_)]
        ));

        let mut before_due = fullscreen_scheduler(FullscreenBehavior::Postpone);
        before_due.set_fullscreen(false, clock(800));
        assert!(matches!(
            before_due
                .handle_event(SchedulerEvent::Tick, clock(900))
                .as_slice(),
            [Effect::NotifyDue { .. }]
        ));
    }

    #[test]
    fn fullscreen_notify_only_explains_deferral_and_keeps_due_actions() {
        let mut scheduler = fullscreen_scheduler(FullscreenBehavior::NotifyOnly);
        let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(900));
        let [Effect::NotifyDue { body, due_id, .. }] = effects.as_slice() else {
            panic!("missing lead reminder")
        };
        assert!(body.contains("Fullscreen delays the overlay"));
        assert!(body.contains(&scheduler.config.fullscreen.max_delay.to_string()));
        let expected_id = *due_id;
        let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(1_000));
        let [
            Effect::NotifyDue {
                summary,
                body,
                due_id,
                can_postpone,
            },
        ] = effects.as_slice()
        else {
            panic!("missing due reminder")
        };
        assert_eq!(summary, "Mini break due");
        assert!(body.contains("leave fullscreen"));
        assert_eq!(*due_id, expected_id);
        assert!(can_postpone);
        assert!(scheduler.status(clock(1_000)).deferred);
    }

    #[test]
    fn fullscreen_maximum_is_measured_from_original_due_time() {
        let mut scheduler = fullscreen_scheduler(FullscreenBehavior::Postpone);
        scheduler.handle_event(SchedulerEvent::Tick, clock(1_400));
        assert_eq!(scheduler.status(clock(1_400)).remaining_ms, Some(100));
        assert!(
            scheduler
                .handle_event(SchedulerEvent::Tick, clock(1_499))
                .is_empty()
        );
        let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(1_500));
        assert!(matches!(effects.as_slice(), [Effect::StartOverlay(_)]));

        let mut late = fullscreen_scheduler(FullscreenBehavior::Postpone);
        let effects = late.handle_event(SchedulerEvent::Tick, clock(1_800));
        assert!(matches!(effects.as_slice(), [Effect::StartOverlay(_)]));
    }

    #[test]
    fn show_policy_and_manual_breaks_bypass_fullscreen_deferral() {
        let mut scheduler = fullscreen_scheduler(FullscreenBehavior::Show);
        let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(1_000));
        assert!(matches!(effects.as_slice(), [Effect::StartOverlay(_)]));
        for command in [Command::Mini, Command::Long, Command::Rest] {
            let mut scheduler = fullscreen_scheduler(FullscreenBehavior::Postpone);
            let effects = scheduler.handle_command(&command, clock(20)).unwrap();
            assert!(matches!(effects.as_slice(), [Effect::StartOverlay(_)]));
        }
    }

    #[test]
    fn fullscreen_sampling_cannot_complete_an_unmapped_active_break() {
        let mut scheduler = test_scheduler();
        scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
        let session_id = scheduler.status(clock(0)).active_session;
        assert!(scheduler.set_fullscreen(true, clock(200)).is_empty());
        assert!(scheduler.set_fullscreen(false, clock(201)).is_empty());
        assert_eq!(scheduler.status(clock(201)).active_session, session_id);
        assert_eq!(scheduler.status(clock(201)).minis_since_long, 0);
        let mut config = scheduler.config.clone();
        config.fullscreen.behavior = FullscreenBehavior::Postpone;
        assert!(scheduler.replace_config_at(config, clock(202)).is_empty());
        assert_eq!(scheduler.status(clock(202)).active_session, session_id);
        assert_eq!(scheduler.status(clock(202)).minis_since_long, 0);
    }

    #[test]
    fn notification_start_acts_on_only_its_pending_break() {
        let mut scheduler = fullscreen_scheduler(FullscreenBehavior::NotifyOnly);
        let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(1_000));
        let [Effect::NotifyDue { due_id, .. }] = effects.as_slice() else {
            panic!("missing reminder")
        };
        let command = Command::StartDue { due_id: *due_id };
        assert!(matches!(
            scheduler
                .handle_command(&command, clock(1_001))
                .unwrap()
                .as_slice(),
            [Effect::StartOverlay(_)]
        ));
        assert_eq!(
            scheduler.handle_command(&command, clock(1_002)),
            Err(SchedulerError::StaleDueBreak)
        );
        scheduler.handle_event(SchedulerEvent::Tick, clock(1_101));
        assert_eq!(
            scheduler.handle_command(&command, clock(1_102)),
            Err(SchedulerError::StaleDueBreak)
        );
        assert_eq!(scheduler.status(clock(1_102)).active_session, None);
        scheduler.handle_event(SchedulerEvent::Tick, clock(2_001));
        assert!(matches!(
            scheduler.state(),
            SchedulerState::PreMiniBreak { .. }
        ));
        assert_eq!(
            scheduler.handle_command(&command, clock(2_002)),
            Err(SchedulerError::StaleDueBreak)
        );
        assert_eq!(scheduler.status(clock(2_002)).active_session, None);
    }

    #[test]
    fn notification_postpone_preserves_limits_and_invalidates_old_actions() {
        let mut scheduler = fullscreen_scheduler(FullscreenBehavior::NotifyOnly);
        scheduler.config.postpone.mini.duration = DurationMs::from_millis(200);
        let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(900));
        let [
            Effect::NotifyDue {
                due_id,
                can_postpone,
                ..
            },
        ] = effects.as_slice()
        else {
            panic!("missing reminder")
        };
        assert!(can_postpone);
        let old_id = *due_id;
        scheduler
            .handle_command(&Command::PostponeDue { due_id: old_id }, clock(910))
            .unwrap();
        assert_eq!(scheduler.status(clock(910)).postpone_count, 1);
        assert_eq!(scheduler.status(clock(910)).remaining_ms, Some(200));
        assert_eq!(
            scheduler.handle_command(&Command::StartDue { due_id: old_id }, clock(911)),
            Err(SchedulerError::StaleDueBreak)
        );
        assert_eq!(
            scheduler.handle_command(&Command::PostponeDue { due_id: old_id }, clock(911)),
            Err(SchedulerError::StaleDueBreak)
        );
        let new_id = scheduler.context().unwrap().pending.as_ref().unwrap().id;
        assert_eq!(
            scheduler.handle_command(&Command::PostponeDue { due_id: new_id }, clock(912)),
            Err(SchedulerError::PostponeLimit)
        );
        let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(1_110));
        assert!(matches!(
            effects.as_slice(),
            [Effect::NotifyDue {
                can_postpone: false,
                ..
            }]
        ));
        assert_eq!(scheduler.status(clock(1_110)).postpone_count, 1);
    }

    #[test]
    fn paused_reminder_actions_are_rejected_and_reload_invalidates_old_ids() {
        let mut scheduler = test_scheduler();
        let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(900));
        let [Effect::NotifyDue { due_id, .. }] = effects.as_slice() else {
            panic!("missing reminder")
        };
        let old_id = *due_id;
        scheduler
            .handle_command(&Command::Pause { duration: None }, clock(910))
            .unwrap();
        assert_eq!(
            scheduler.handle_command(&Command::StartDue { due_id: old_id }, clock(920)),
            Err(SchedulerError::StaleDueBreak)
        );
        let mut config = scheduler.config.clone();
        config.schedule.mini.interval = DurationMs::from_millis(500);
        scheduler.replace_config_at(config, clock(1_100));
        assert_eq!(scheduler.status(clock(1_200)).remaining_ms, Some(500));
        scheduler
            .handle_command(&Command::Resume, clock(1_300))
            .unwrap();
        assert_eq!(scheduler.status(clock(1_300)).remaining_ms, Some(500));
        assert_eq!(
            scheduler.handle_command(&Command::StartDue { due_id: old_id }, clock(1_301)),
            Err(SchedulerError::StaleDueBreak)
        );
    }

    #[test]
    fn interval_reload_grants_fresh_time_and_preserves_cycle_progress() {
        let mut scheduler = test_scheduler();
        scheduler.handle_command(&Command::Long, clock(0)).unwrap();
        scheduler.handle_event(SchedulerEvent::Tick, clock(300));
        scheduler.handle_event(SchedulerEvent::Tick, clock(1_200));
        let mut config = scheduler.config.clone();
        config.schedule.mini.interval = DurationMs::from_millis(200);
        let effects = scheduler.replace_config_at(config, clock(1_250));
        assert!(effects.is_empty());
        let status = scheduler.status(clock(1_250));
        assert_eq!(status.remaining_ms, Some(200));
        assert_eq!(status.longs_since_rest, 1);
        assert_eq!(status.active_session, None);
        assert_eq!(scheduler.context().unwrap().cycle_started_mono_ms, 300);
    }

    #[test]
    fn non_interval_reload_keeps_pending_deadline_and_active_reload_keeps_break_timing() {
        let mut scheduler = test_scheduler();
        let mut config = scheduler.config.clone();
        config.content.show_message = false;
        scheduler.replace_config_at(config, clock(400));
        assert_eq!(scheduler.status(clock(400)).remaining_ms, Some(600));
        scheduler
            .handle_command(&Command::Mini, clock(500))
            .unwrap();
        let session = scheduler.status(clock(500)).active_session;
        let mut config = scheduler.config.clone();
        config.schedule.mini.duration = DurationMs::from_millis(5_000);
        config.schedule.mini.interval = DurationMs::from_millis(10_000);
        scheduler.replace_config_at(config, clock(550));
        assert_eq!(scheduler.status(clock(550)).remaining_ms, Some(50));
        assert_eq!(scheduler.status(clock(550)).active_session, session);
        scheduler.handle_event(SchedulerEvent::Tick, clock(600));
        assert_eq!(scheduler.status(clock(600)).remaining_ms, Some(10_000));
    }

    #[test]
    fn disabling_deferral_releases_a_waiting_break() {
        let mut scheduler = fullscreen_scheduler(FullscreenBehavior::Postpone);
        scheduler.handle_event(SchedulerEvent::Tick, clock(1_000));
        let mut config = scheduler.config.clone();
        config.fullscreen.behavior = FullscreenBehavior::Show;
        let effects = scheduler.replace_config_at(config, clock(1_100));
        assert!(matches!(effects.as_slice(), [Effect::StartOverlay(_)]));
    }

    #[test]
    fn guest_config_reload_does_not_replace_the_hosts_pending_break() {
        let mut host = test_scheduler();
        let (snapshot, _) = host.coop_snapshot(uuid::Uuid::nil(), 1, clock(0));
        let mut guest = fullscreen_scheduler(FullscreenBehavior::Postpone);
        guest.adopt_coop_snapshot(&snapshot, clock(0));
        let pending = guest.context().unwrap().pending.clone();
        let mut config = guest.config.clone();
        config.schedule.mini.interval = DurationMs::from_millis(2_000);
        guest.replace_config_at(config, clock(400));
        assert_eq!(guest.context().unwrap().pending, pending);
        assert_eq!(guest.status(clock(400)).remaining_ms, Some(600));
        let effects = guest.handle_event(SchedulerEvent::Tick, clock(1_000));
        assert!(matches!(effects.as_slice(), [Effect::StartOverlay(_)]));
        assert!(!guest.status(clock(1_000)).deferred);
    }

    #[test]
    fn host_advertises_fullscreen_deferral_before_guests_predict_the_original_deadline() {
        let mut host = fullscreen_scheduler(FullscreenBehavior::Postpone);
        let (snapshot, _) = host.coop_snapshot(uuid::Uuid::nil(), 1, clock(500));
        let CoopPhase::Working { next } = &snapshot.phase else {
            panic!("expected work")
        };
        assert_eq!(next.starts_unix_ms, clock(1_500).wall_unix_ms);
        let mut guest = test_scheduler();
        guest.adopt_coop_snapshot(&snapshot, clock(500));
        let effects = guest.handle_event(SchedulerEvent::Tick, clock(1_000));
        assert!(effects.is_empty());
        assert_eq!(guest.status(clock(1_000)).active_session, None);
        assert_eq!(guest.status(clock(1_000)).remaining_ms, Some(500));
        let mut deferred_host = host.clone();
        deferred_host.handle_event(SchedulerEvent::Tick, clock(1_000));
        let (snapshot, _) = deferred_host.coop_snapshot(uuid::Uuid::nil(), 2, clock(1_000));
        let CoopPhase::Working { next } = &snapshot.phase else {
            panic!("expected work")
        };
        assert_eq!(next.starts_unix_ms, clock(1_500).wall_unix_ms);
        host.set_fullscreen(false, clock(700));
        let (snapshot, _) = host.coop_snapshot(uuid::Uuid::nil(), 2, clock(700));
        let CoopPhase::Working { next } = &snapshot.phase else {
            panic!("expected work")
        };
        assert_eq!(next.starts_unix_ms, clock(1_000).wall_unix_ms);
    }

    #[test]
    fn overlay_exhaustion_restarts_work_without_counting_an_unseen_break() {
        let mut scheduler = test_scheduler();
        scheduler.config.completion.manual_resume = true;
        scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
        let session_id = scheduler.status(clock(0)).active_session.unwrap();
        assert!(
            scheduler
                .overlay_recovery_exhausted(BreakSessionId::new(), clock(50))
                .is_empty()
        );
        let effects = scheduler.overlay_recovery_exhausted(session_id, clock(150));
        assert!(matches!(
            effects.as_slice(),
            [Effect::StopOverlay { .. }, Effect::Notify { .. }]
        ));
        let status = scheduler.status(clock(150));
        assert_eq!(status.active_session, None);
        assert_eq!(status.minis_since_long, 0);
        assert_eq!(status.remaining_ms, Some(1_000));
        assert!(
            scheduler
                .overlay_recovery_exhausted(session_id, clock(151))
                .is_empty()
        );
    }

    #[test]
    fn unavailable_overlay_time_extends_rest_and_strict_time_without_shifting_work_cycles() {
        let mut scheduler = test_scheduler();
        scheduler.config.strict.mode = StrictMode::Entire;
        scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
        let session_id = scheduler.status(clock(0)).active_session.unwrap();
        assert!(!scheduler.delay_active_overlay(BreakSessionId::new(), 200));
        assert!(scheduler.delay_active_overlay(session_id, 200));
        let active = scheduler.active_break().unwrap();
        assert_eq!(active.started_boot_ms, 200);
        assert_eq!(active.ends_boot_ms, 300);
        assert_eq!(active.strict_until_boot_ms, 300);
        assert_eq!(active.context.cycle_started_mono_ms, 0);
        assert_eq!(scheduler.status(clock(200)).remaining_ms, Some(100));
        assert!(!scheduler.status(clock(200)).can_skip);
        assert!(
            scheduler
                .handle_event(SchedulerEvent::Tick, clock(299))
                .is_empty()
        );
        assert!(matches!(
            scheduler
                .handle_event(SchedulerEvent::Tick, clock(300))
                .as_slice(),
            [Effect::PlayCompletionSound, Effect::StopOverlay { .. }]
        ));
    }

    #[test]
    fn message_selection_uses_the_break_id() {
        assert_eq!(message_index(0, 3), 0);
        assert_eq!(message_index(1, 3), 1);
        assert_eq!(message_index(5, 3), 2);
    }

    #[test]
    fn manual_resume_waits_after_a_mini_break() {
        let mut scheduler = test_scheduler();
        scheduler.config.completion.manual_resume = true;
        let effects = scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
        let [Effect::StartOverlay(spec)] = effects.as_slice() else {
            panic!("expected an overlay");
        };
        assert!(spec.manual_resume);
        assert_eq!(
            scheduler.handle_command(&Command::ResumeBreak, clock(99)),
            Err(SchedulerError::NotAwaitingResume)
        );

        let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(100));
        assert_eq!(effects, vec![Effect::PlayCompletionSound]);
        assert!(
            scheduler
                .handle_event(SchedulerEvent::Tick, clock(101))
                .is_empty()
        );
        let status = scheduler.status(clock(100));
        assert!(status.awaiting_resume);
        assert!(!status.can_skip);
        assert!(!status.can_postpone);
        assert_eq!(
            scheduler.handle_command(&Command::Skip, clock(100)),
            Err(SchedulerError::AwaitingResume)
        );

        let effects = scheduler
            .handle_command(&Command::ResumeBreak, clock(100))
            .unwrap();
        assert!(matches!(effects.as_slice(), [Effect::StopOverlay { .. }]));
        assert!(matches!(scheduler.state(), SchedulerState::Running { .. }));
        assert_eq!(scheduler.status(clock(100)).minis_since_long, 1);
        assert_eq!(scheduler.status(clock(100)).remaining_ms, Some(1_000));
    }

    #[test]
    fn manual_resume_applies_to_long_breaks() {
        let mut scheduler = test_scheduler();
        scheduler.config.completion.manual_resume = true;
        scheduler.handle_command(&Command::Long, clock(0)).unwrap();

        let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(300));
        assert_eq!(effects, vec![Effect::PlayCompletionSound]);
        assert!(
            scheduler
                .handle_event(SchedulerEvent::Tick, clock(301))
                .is_empty()
        );
        assert!(scheduler.status(clock(300)).awaiting_resume);
        scheduler
            .handle_command(&Command::ResumeBreak, clock(300))
            .unwrap();
        assert!(matches!(scheduler.state(), SchedulerState::Running { .. }));
        assert_eq!(scheduler.status(clock(300)).minis_since_long, 0);
    }

    #[test]
    fn immediate_manual_resume_still_plays_the_completion_sound() {
        let mut scheduler = test_scheduler();
        scheduler.config.completion.manual_resume = true;
        scheduler.handle_command(&Command::Mini, clock(0)).unwrap();

        let effects = scheduler
            .handle_command(&Command::ResumeBreak, clock(100))
            .unwrap();
        assert!(matches!(
            effects.as_slice(),
            [Effect::PlayCompletionSound, Effect::StopOverlay { .. }]
        ));
    }

    #[test]
    fn restored_manual_break_does_not_replay_the_completion_sound() {
        let mut scheduler = test_scheduler();
        scheduler.config.completion.manual_resume = true;
        scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
        assert_eq!(
            scheduler.handle_event(SchedulerEvent::Tick, clock(100)),
            vec![Effect::PlayCompletionSound]
        );

        let mut restored = Scheduler::restore(
            scheduler.config.clone(),
            "boot".into(),
            clock(101),
            "/tmp/breakd.sock".into(),
            scheduler.snapshot(),
        );
        assert!(
            restored
                .handle_event(SchedulerEvent::Tick, clock(101))
                .is_empty()
        );
    }

    #[test]
    fn automatic_resume_remains_the_default() {
        let mut scheduler = test_scheduler();
        scheduler.handle_command(&Command::Mini, clock(0)).unwrap();

        let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(100));
        assert!(matches!(
            effects.as_slice(),
            [Effect::PlayCompletionSound, Effect::StopOverlay { .. }]
        ));
        assert!(!scheduler.status(clock(100)).awaiting_resume);
        assert!(matches!(scheduler.state(), SchedulerState::Running { .. }));
    }

    #[test]
    fn dismissing_a_break_early_does_not_play_the_completion_sound() {
        let mut scheduler = test_scheduler();
        scheduler.handle_command(&Command::Mini, clock(0)).unwrap();

        let effects = scheduler.handle_command(&Command::Skip, clock(20)).unwrap();
        assert!(matches!(effects.as_slice(), [Effect::StopOverlay { .. }]));
        assert!(
            scheduler
                .handle_event(SchedulerEvent::Tick, clock(100))
                .is_empty()
        );
    }

    #[test]
    fn coop_guests_start_the_same_session_with_the_hosts_duration() {
        let mut host = test_scheduler();
        let mut guest = test_scheduler();
        let mut guest_config = guest.config.clone();
        guest_config.schedule.mini.duration = DurationMs::from_millis(800);
        guest.replace_config(guest_config);

        let (snapshot, _) = host.coop_snapshot(uuid::Uuid::nil(), 1, clock(0));
        assert!(guest.adopt_coop_snapshot(&snapshot, clock(50)).is_empty());

        let host_effects = host.handle_event(SchedulerEvent::Tick, clock(1_000));
        let guest_effects = guest.handle_event(SchedulerEvent::Tick, clock(1_000));
        let host_session = host.status(clock(1_000)).active_session.unwrap();
        let guest_status = guest.status(clock(1_000));

        assert_eq!(guest_status.active_session, Some(host_session));
        assert_eq!(guest_status.remaining_ms, Some(100));
        assert!(matches!(host_effects.as_slice(), [Effect::StartOverlay(_)]));
        assert!(matches!(
            guest_effects.as_slice(),
            [Effect::StartOverlay(_)]
        ));
    }

    #[test]
    fn coop_host_owns_manual_resume_and_notification_timing() {
        let mut host = test_scheduler();
        host.config.completion.manual_resume = true;
        host.config.notifications.enabled = true;
        host.config.notifications.mini_lead = DurationMs::from_millis(400);
        host.config.strict.mode = StrictMode::Entire;
        host.config.strict.allow_postpone_during_lockout = true;
        host.config.strict.inhibit_shortcuts = true;

        let mut guest = test_scheduler();
        guest.config.completion.manual_resume = false;
        guest.config.notifications.enabled = false;
        guest.config.notifications.mini_lead = DurationMs::from_millis(10);
        guest.config.strict.mode = StrictMode::Off;
        guest.config.strict.allow_postpone_during_lockout = false;
        guest.config.strict.inhibit_shortcuts = false;

        let (mut snapshot, _) = host.coop_snapshot(uuid::Uuid::nil(), 1, clock(0));
        guest.adopt_coop_snapshot(&snapshot, clock(0));
        let notification = guest.handle_event(SchedulerEvent::Tick, clock(600));
        assert!(matches!(
            notification.as_slice(),
            [Effect::NotifyDue { .. }]
        ));

        snapshot.revision = 2;
        guest.adopt_coop_snapshot(&snapshot, clock(610));
        assert!(
            guest
                .handle_event(SchedulerEvent::Tick, clock(610))
                .is_empty(),
            "regular snapshots must not repeat the same notification"
        );

        let effects = guest.handle_event(SchedulerEvent::Tick, clock(1_000));
        let [Effect::StartOverlay(spec)] = effects.as_slice() else {
            panic!("expected the mirrored break overlay");
        };
        assert!(spec.manual_resume);
        assert!(spec.allow_postpone_during_lockout);
        assert!(spec.inhibit_shortcuts);
        assert!(guest.status(clock(1_100)).awaiting_resume);
    }

    #[test]
    fn a_manual_resume_request_just_before_deadline_is_honored_at_zero() {
        let mut scheduler = test_scheduler();
        scheduler.config.completion.manual_resume = true;
        scheduler.handle_command(&Command::Mini, clock(0)).unwrap();

        assert!(
            scheduler
                .handle_coop_resume_request(clock(99))
                .unwrap()
                .is_empty()
        );
        let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(100));
        assert!(matches!(
            effects.as_slice(),
            [Effect::PlayCompletionSound, Effect::StopOverlay { .. }]
        ));
        assert!(matches!(scheduler.state(), SchedulerState::Running { .. }));
    }

    #[test]
    fn a_working_snapshot_arriving_just_after_deadline_starts_without_flicker() {
        let mut host = test_scheduler();
        let mut guest = test_scheduler();
        let (snapshot, _) = host.coop_snapshot(uuid::Uuid::nil(), 1, clock(0));

        let effects = guest.adopt_coop_snapshot(&snapshot, clock(1_001));
        let [Effect::StartOverlay(spec)] = effects.as_slice() else {
            panic!("expected a late working snapshot to start its break");
        };
        let CoopPhase::Working { next } = &snapshot.phase else {
            panic!("host should still be working");
        };
        assert_eq!(spec.session_id.0, next.due_id.0);
        assert_eq!(spec.duration, DurationMs::from_millis(99));
    }

    #[test]
    fn coop_guest_uses_its_own_break_message_when_joining_mid_break() {
        let mut host = test_scheduler();
        host.handle_command(&Command::Mini, clock(0)).unwrap();
        let (snapshot, _) = host.coop_snapshot(uuid::Uuid::nil(), 1, clock(20));

        let mut guest = test_scheduler();
        let mut guest_config = guest.config.clone();
        guest_config.content.messages = vec!["A local message".into()];
        guest.replace_config(guest_config);
        let effects = guest.adopt_coop_snapshot(&snapshot, clock(20));

        let [Effect::StartOverlay(spec)] = effects.as_slice() else {
            panic!("expected the mirrored overlay to start");
        };
        assert_eq!(spec.message.as_deref(), Some("A local message"));
        assert_eq!(spec.duration, DurationMs::from_millis(80));
    }

    #[test]
    fn idle_does_not_dismiss_a_manual_resume_break() {
        let mut scheduler = test_scheduler();
        scheduler.config.completion.manual_resume = true;
        scheduler.handle_command(&Command::Long, clock(0)).unwrap();

        let effects = scheduler.handle_event(SchedulerEvent::IdleThresholdReached, clock(300));
        assert!(effects.is_empty());
        assert!(scheduler.status(clock(300)).awaiting_resume);
        assert!(matches!(
            scheduler.state(),
            SchedulerState::LongBreak { .. }
        ));
    }

    #[test]
    fn lock_recovery_restores_an_expired_manual_resume_break() {
        let mut scheduler = test_scheduler();
        scheduler.config.completion.manual_resume = true;
        scheduler.config.recovery.wake_grace = DurationMs::from_millis(0);
        scheduler.config.idle.reset_after = DurationMs::from_millis(500);
        scheduler.handle_command(&Command::Long, clock(0)).unwrap();
        scheduler.handle_event(SchedulerEvent::LockStarted, clock(10));

        let effects = scheduler.handle_event(SchedulerEvent::LockEnded, clock(600));
        let [Effect::PlayCompletionSound, Effect::StartOverlay(spec)] = effects.as_slice() else {
            panic!("expected the manual-resume overlay to be restored");
        };
        assert!(spec.manual_resume);
        assert_eq!(spec.duration, DurationMs::from_millis(0));
        assert!(scheduler.status(clock(600)).awaiting_resume);
    }

    #[test]
    fn running_enters_prebreak_then_break() {
        let mut scheduler = test_scheduler();
        let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(900));
        assert!(matches!(
            scheduler.state(),
            SchedulerState::PreMiniBreak { .. }
        ));
        assert!(matches!(effects.as_slice(), [Effect::NotifyDue { .. }]));

        let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(1_000));
        assert!(matches!(
            scheduler.state(),
            SchedulerState::MiniBreak { .. }
        ));
        assert!(matches!(effects.as_slice(), [Effect::StartOverlay(_)]));
    }

    #[test]
    fn delay_mode_allows_immediate_skip_and_postpone() {
        let mut scheduler = test_scheduler();
        let effects = scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
        let Effect::StartOverlay(spec) = &effects[0] else {
            panic!("expected an overlay");
        };
        assert!(spec.can_skip);
        assert!(spec.can_postpone);
        // The minimum-visible delay no longer gates skip or postpone, so the
        // overlay enables both controls from the first frame.
        assert_eq!(spec.strict_remaining, DurationMs::from_millis(0));
        assert!(scheduler.status(clock(1)).can_skip);
        assert!(scheduler.status(clock(1)).can_postpone);
        assert!(
            scheduler
                .handle_command(&Command::Postpone, clock(1))
                .is_ok()
        );
    }

    #[test]
    fn entire_strict_mode_locks_skip_and_postpone_for_the_whole_break() {
        let mut scheduler = test_scheduler();
        scheduler.config.strict.mode = StrictMode::Entire;
        let effects = scheduler.handle_command(&Command::Long, clock(0)).unwrap();
        let Effect::StartOverlay(spec) = &effects[0] else {
            panic!("expected an overlay");
        };
        // Entire mode keeps the controls locked for the whole break.
        assert_eq!(spec.strict_remaining, spec.duration);
        assert!(!scheduler.status(clock(10)).can_skip);
        assert!(!scheduler.status(clock(10)).can_postpone);
        assert_eq!(
            scheduler.handle_command(&Command::Skip, clock(10)),
            Err(SchedulerError::StrictMode)
        );
        assert_eq!(
            scheduler.handle_command(&Command::Postpone, clock(10)),
            Err(SchedulerError::StrictMode)
        );

        // The postpone-during-lockout escape hatch still applies in Entire mode.
        scheduler.config.strict.allow_postpone_during_lockout = true;
        assert!(scheduler.status(clock(10)).can_postpone);
        assert!(
            scheduler
                .handle_command(&Command::Postpone, clock(10))
                .is_ok()
        );
    }

    #[test]
    fn strict_mode_rejects_pause_and_reset_loopholes() {
        let mut scheduler = test_scheduler();
        scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
        assert_eq!(
            scheduler.handle_command(&Command::Pause { duration: None }, clock(10)),
            Err(SchedulerError::StrictMode)
        );
        assert_eq!(
            scheduler.handle_command(&Command::Reset, clock(10)),
            Err(SchedulerError::StrictMode)
        );
    }

    #[test]
    fn postpone_preserves_due_and_enforces_limit() {
        let mut scheduler = test_scheduler();
        scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
        scheduler
            .handle_command(&Command::Postpone, clock(20))
            .unwrap();
        let status = scheduler.status(clock(20));
        assert_eq!(status.postpone_count, 1);

        let due = match scheduler.state() {
            SchedulerState::Running { context } => context.next_due_mono_ms,
            state => panic!("unexpected state {state:?}"),
        };
        scheduler.handle_event(SchedulerEvent::Tick, clock(due));
        assert_eq!(
            scheduler.handle_command(&Command::Postpone, clock(due + 20)),
            Err(SchedulerError::PostponeLimit)
        );
    }

    #[test]
    fn two_postponements_are_allowed_before_the_control_disappears() {
        let mut scheduler = test_scheduler();
        scheduler.config.postpone.mini.max_postponements = Some(2);
        scheduler.handle_command(&Command::Mini, clock(0)).unwrap();

        scheduler
            .handle_command(&Command::Postpone, clock(20))
            .unwrap();
        let first_due = match scheduler.state() {
            SchedulerState::Running { context } => context.next_due_mono_ms,
            state => panic!("unexpected state {state:?}"),
        };
        let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(first_due));
        let [Effect::StartOverlay(spec)] = effects.as_slice() else {
            panic!("expected the first postponed overlay");
        };
        assert!(spec.can_postpone);

        scheduler
            .handle_command(&Command::Postpone, clock(first_due + 20))
            .unwrap();
        let second_due = match scheduler.state() {
            SchedulerState::Running { context } => context.next_due_mono_ms,
            state => panic!("unexpected state {state:?}"),
        };
        let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(second_due));
        let [Effect::StartOverlay(spec)] = effects.as_slice() else {
            panic!("expected the second postponed overlay");
        };
        assert!(!spec.can_postpone);
        assert!(!scheduler.status(clock(second_due + 20)).can_postpone);
        assert_eq!(
            scheduler.handle_command(&Command::Postpone, clock(second_due + 20)),
            Err(SchedulerError::PostponeLimit)
        );
    }

    #[test]
    fn omitted_postponement_limit_allows_repeated_postponement() {
        let mut scheduler = test_scheduler();
        scheduler.config.postpone.mini.max_postponements = None;
        scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
        let mut now = 20;

        for expected_count in 1..=3 {
            assert!(scheduler.status(clock(now)).can_postpone);
            scheduler
                .handle_command(&Command::Postpone, clock(now))
                .unwrap();
            assert_eq!(scheduler.status(clock(now)).postpone_count, expected_count);

            let due = match scheduler.state() {
                SchedulerState::Running { context } => context.next_due_mono_ms,
                state => panic!("unexpected state {state:?}"),
            };
            let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(due));
            let [Effect::StartOverlay(spec)] = effects.as_slice() else {
                panic!("expected a postponed overlay");
            };
            assert!(spec.can_postpone);
            now = due + 20;
        }
    }

    #[test]
    fn mini_postpone_can_be_disabled_independently() {
        let mut scheduler = test_scheduler();
        scheduler.config.postpone.mini.enabled = false;
        let effects = scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
        let Effect::StartOverlay(spec) = &effects[0] else {
            panic!("expected an overlay");
        };
        assert!(!spec.can_postpone);
        assert!(!scheduler.status(clock(20)).can_postpone);
        assert_eq!(
            scheduler.handle_command(&Command::Postpone, clock(20)),
            Err(SchedulerError::PostponeDisabled)
        );
    }

    #[test]
    fn mini_skip_can_be_disabled_independently() {
        let mut scheduler = test_scheduler();
        scheduler.config.skip.mini.enabled = false;
        let effects = scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
        let Effect::StartOverlay(spec) = &effects[0] else {
            panic!("expected an overlay");
        };
        assert!(!spec.can_skip);
        assert!(spec.can_postpone);
        assert!(!scheduler.status(clock(20)).can_skip);
        assert!(scheduler.status(clock(20)).can_postpone);
        assert_eq!(
            scheduler.handle_command(&Command::Skip, clock(20)),
            Err(SchedulerError::SkipDisabled)
        );
        assert_eq!(
            scheduler.handle_command(&Command::Pause { duration: None }, clock(20)),
            Err(SchedulerError::SkipDisabled)
        );
        assert_eq!(
            scheduler.handle_command(&Command::Reset, clock(20)),
            Err(SchedulerError::SkipDisabled)
        );
        assert!(
            scheduler
                .handle_command(&Command::Postpone, clock(20))
                .is_ok()
        );
    }

    #[test]
    fn long_skip_can_remain_enabled() {
        let mut scheduler = test_scheduler();
        scheduler.config.skip.mini.enabled = false;
        let effects = scheduler.handle_command(&Command::Long, clock(0)).unwrap();
        let Effect::StartOverlay(spec) = &effects[0] else {
            panic!("expected an overlay");
        };
        assert!(spec.can_skip);
        assert!(scheduler.status(clock(20)).can_skip);
        assert!(scheduler.handle_command(&Command::Skip, clock(20)).is_ok());
    }

    #[test]
    fn long_postpone_can_remain_enabled() {
        let mut scheduler = test_scheduler();
        scheduler.config.postpone.mini.enabled = false;
        scheduler.handle_command(&Command::Long, clock(0)).unwrap();
        assert!(scheduler.status(clock(20)).can_postpone);
        assert!(
            scheduler
                .handle_command(&Command::Postpone, clock(20))
                .is_ok()
        );
    }

    #[test]
    fn pause_freezes_deadline() {
        let mut scheduler = test_scheduler();
        scheduler
            .handle_command(
                &Command::Pause {
                    duration: Some(DurationMs::from_millis(500)),
                },
                clock(100),
            )
            .unwrap();
        scheduler.handle_event(SchedulerEvent::Tick, clock(600));
        assert_eq!(scheduler.status(clock(600)).remaining_ms, Some(900));
    }

    #[test]
    fn paused_work_countdown_stays_frozen_until_resume() {
        let mut scheduler = test_scheduler();
        scheduler
            .handle_command(&Command::Pause { duration: None }, clock(100))
            .unwrap();

        assert_eq!(scheduler.status(clock(600)).remaining_ms, Some(900));
        scheduler
            .handle_command(&Command::Resume, clock(600))
            .unwrap();
        assert_eq!(scheduler.status(clock(600)).remaining_ms, Some(900));
    }

    #[test]
    fn paused_break_countdown_and_overlay_stay_paused() {
        let mut scheduler = test_scheduler();
        scheduler.config.completion.manual_resume = true;
        scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
        scheduler
            .handle_command(&Command::Pause { duration: None }, clock(20))
            .unwrap();

        assert!(scheduler.startup_effects(clock(500)).is_empty());
        let status = scheduler.status(clock(500));
        assert!(status.paused);
        assert!(!status.awaiting_resume);
        assert_eq!(status.remaining_ms, Some(80));

        let effects = scheduler
            .handle_command(&Command::Resume, clock(500))
            .unwrap();
        let [Effect::StartOverlay(spec)] = effects.as_slice() else {
            panic!("expected the paused break to resume");
        };
        assert_eq!(spec.duration, DurationMs::from_millis(80));
    }

    #[test]
    fn idle_does_not_cancel_indefinite_or_timed_pauses() {
        for duration in [None, Some(DurationMs::from_millis(5_000))] {
            let mut scheduler = test_scheduler();
            scheduler
                .handle_command(&Command::Pause { duration }, clock(100))
                .unwrap();
            assert!(
                scheduler
                    .handle_event(SchedulerEvent::IdleThresholdReached, clock(500))
                    .is_empty()
            );
            scheduler.handle_event(SchedulerEvent::ActivityResumed, clock(600));

            assert!(scheduler.status(clock(600)).paused);
            scheduler
                .handle_command(&Command::Resume, clock(700))
                .unwrap();
            assert_eq!(scheduler.status(clock(700)).remaining_ms, Some(900));
        }
    }

    #[test]
    fn long_lock_and_sleep_preserve_an_indefinite_pause() {
        for (started, ended) in [
            (SchedulerEvent::LockStarted, SchedulerEvent::LockEnded),
            (SchedulerEvent::SuspendStarted, SchedulerEvent::SuspendEnded),
        ] {
            let mut scheduler = test_scheduler();
            scheduler.config.idle.reset_after = DurationMs::from_millis(500);
            scheduler.config.recovery.wake_grace = DurationMs::from_millis(50);
            scheduler
                .handle_command(&Command::Pause { duration: None }, clock(100))
                .unwrap();
            scheduler.handle_event(started, clock(200));
            scheduler.handle_event(SchedulerEvent::IdleThresholdReached, clock(600));
            assert!(scheduler.handle_event(ended, clock(1_000)).is_empty());
            scheduler.handle_event(SchedulerEvent::Tick, clock(1_050));

            assert!(matches!(
                scheduler.state(),
                SchedulerState::PausedIndefinitely { .. }
            ));
            scheduler
                .handle_command(&Command::Resume, clock(1_100))
                .unwrap();
            assert_eq!(scheduler.status(clock(1_100)).remaining_ms, Some(900));
        }
    }

    #[test]
    fn lock_preserves_a_timed_pause_without_nesting_wake_grace() {
        let mut scheduler = test_scheduler();
        scheduler.config.idle.reset_after = DurationMs::from_millis(500);
        scheduler.config.recovery.wake_grace = DurationMs::from_millis(50);
        scheduler
            .handle_command(
                &Command::Pause {
                    duration: Some(DurationMs::from_millis(5_000)),
                },
                clock(100),
            )
            .unwrap();
        scheduler.handle_event(SchedulerEvent::LockStarted, clock(200));
        assert!(
            scheduler
                .handle_event(SchedulerEvent::LockEnded, clock(700))
                .is_empty()
        );
        scheduler.handle_event(SchedulerEvent::Tick, clock(5_100));

        assert!(matches!(scheduler.state(), SchedulerState::Running { .. }));
        assert_eq!(scheduler.status(clock(5_100)).remaining_ms, Some(900));
    }

    #[test]
    fn a_timed_break_pause_can_expire_while_locked() {
        let mut scheduler = test_scheduler();
        scheduler.config.recovery.wake_grace = DurationMs::from_millis(50);
        scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
        scheduler
            .handle_command(
                &Command::Pause {
                    duration: Some(DurationMs::from_millis(50)),
                },
                clock(20),
            )
            .unwrap();
        scheduler.handle_event(SchedulerEvent::LockStarted, clock(30));
        let effects = scheduler.handle_event(SchedulerEvent::LockEnded, clock(100));

        let [Effect::StartOverlay(spec)] = effects.as_slice() else {
            panic!("expected the timed pause to finish after unlocking");
        };
        assert_eq!(spec.duration, DurationMs::from_millis(80));
        assert!(!scheduler.status(clock(100)).paused);
    }

    #[test]
    fn overlapping_sleep_and_lock_wait_for_both_reasons_in_either_order() {
        for sleep_first in [false, true] {
            for wake_first in [false, true] {
                let mut scheduler = test_scheduler();
                scheduler.config.recovery.wake_grace = DurationMs::from_millis(0);
                scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
                let starts = if sleep_first {
                    [SchedulerEvent::SuspendStarted, SchedulerEvent::LockStarted]
                } else {
                    [SchedulerEvent::LockStarted, SchedulerEvent::SuspendStarted]
                };
                let ends = if wake_first {
                    [SchedulerEvent::SuspendEnded, SchedulerEvent::LockEnded]
                } else {
                    [SchedulerEvent::LockEnded, SchedulerEvent::SuspendEnded]
                };
                for (event, at) in starts.into_iter().zip([10, 20]) {
                    scheduler.handle_event(event, clock(at));
                }
                assert!(
                    scheduler
                        .handle_event(ends[0].clone(), clock(30))
                        .is_empty()
                );
                scheduler.handle_event(SchedulerEvent::Tick, clock(40));
                assert!(matches!(
                    scheduler.state(),
                    SchedulerState::Suspended { .. }
                ));
                assert!(scheduler.startup_effects(clock(40)).is_empty());

                let effects = scheduler.handle_event(ends[1].clone(), clock(50));
                assert!(matches!(effects.as_slice(), [Effect::StartOverlay(_)]));
                assert!(matches!(
                    scheduler.state(),
                    SchedulerState::MiniBreak { .. }
                ));
            }
        }
    }

    #[test]
    fn overlapping_sleep_and_lock_preserve_an_explicit_pause_in_either_order() {
        for sleep_first in [false, true] {
            for wake_first in [false, true] {
                let mut scheduler = test_scheduler();
                scheduler.config.recovery.wake_grace = DurationMs::from_millis(50);
                scheduler.config.idle.reset_after = DurationMs::from_millis(5);
                scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
                scheduler
                    .handle_command(&Command::Pause { duration: None }, clock(20))
                    .unwrap();
                let starts = if sleep_first {
                    [SchedulerEvent::SuspendStarted, SchedulerEvent::LockStarted]
                } else {
                    [SchedulerEvent::LockStarted, SchedulerEvent::SuspendStarted]
                };
                let ends = if wake_first {
                    [SchedulerEvent::SuspendEnded, SchedulerEvent::LockEnded]
                } else {
                    [SchedulerEvent::LockEnded, SchedulerEvent::SuspendEnded]
                };
                for (event, at) in starts.into_iter().zip([30, 40]) {
                    scheduler.handle_event(event, clock(at));
                }
                for (event, at) in ends.into_iter().zip([50, 60]) {
                    assert!(scheduler.handle_event(event, clock(at)).is_empty());
                    assert!(scheduler.status(clock(at)).paused);
                    assert_eq!(scheduler.status(clock(at)).remaining_ms, Some(80));
                    assert!(scheduler.startup_effects(clock(at)).is_empty());
                }
                assert!(matches!(
                    scheduler.state(),
                    SchedulerState::PausedIndefinitely { .. }
                ));

                let effects = scheduler
                    .handle_command(&Command::Resume, clock(70))
                    .unwrap();
                let [Effect::StartOverlay(spec)] = effects.as_slice() else {
                    panic!("expected the explicit pause to remain until resumed");
                };
                assert_eq!(spec.duration, DurationMs::from_millis(80));
            }
        }
    }

    #[test]
    fn overlapping_sleep_and_lock_apply_each_break_counting_policy() {
        for sleep_counts in [false, true] {
            for lock_counts in [false, true] {
                let mut scheduler = test_scheduler();
                scheduler.config.schedule.mini.duration = DurationMs::from_millis(1_000);
                scheduler.config.recovery.wake_grace = DurationMs::from_millis(0);
                scheduler.config.recovery.suspend_counts_as_break = sleep_counts;
                scheduler.config.recovery.lock_counts_as_break = lock_counts;
                scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
                scheduler.handle_event(SchedulerEvent::LockStarted, clock(100));
                scheduler.handle_event(SchedulerEvent::SuspendStarted, clock(200));
                scheduler.handle_event(SchedulerEvent::SuspendEnded, clock(700));
                scheduler.handle_event(SchedulerEvent::LockEnded, clock(800));

                let frozen_ms =
                    if sleep_counts { 0 } else { 500 } + if lock_counts { 0 } else { 200 };
                assert_eq!(
                    scheduler.status(clock(800)).remaining_ms,
                    Some(200 + frozen_ms)
                );
            }
        }
    }

    #[test]
    fn suspended_snapshots_support_missing_and_active_overlap_metadata() {
        let mut scheduler = test_scheduler();
        scheduler.config.recovery.wake_grace = DurationMs::from_millis(0);
        scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
        scheduler.handle_event(SchedulerEvent::SuspendStarted, clock(10));
        let legacy_snapshot = serde_json::to_value(scheduler.snapshot()).unwrap();
        assert!(legacy_snapshot["state"].get("overlap").is_none());
        let mut restored = Scheduler::restore(
            scheduler.config.clone(),
            "boot".into(),
            clock(15),
            "/tmp/breakd.sock".into(),
            serde_json::from_value(legacy_snapshot).unwrap(),
        );
        restored.handle_event(SchedulerEvent::LockStarted, clock(20));
        restored.handle_event(SchedulerEvent::SuspendEnded, clock(30));

        let snapshot = serde_json::to_value(restored.snapshot()).unwrap();
        let mut restored_again = Scheduler::restore(
            scheduler.config.clone(),
            "boot".into(),
            clock(40),
            "/tmp/breakd.sock".into(),
            serde_json::from_value(snapshot).unwrap(),
        );
        assert!(restored_again.startup_effects(clock(40)).is_empty());
        let effects = restored_again.handle_event(SchedulerEvent::LockEnded, clock(50));
        assert!(matches!(effects.as_slice(), [Effect::StartOverlay(_)]));
    }

    #[test]
    fn long_suspend_resets_schedule() {
        let mut scheduler = test_scheduler();
        scheduler.config.idle.reset_after = DurationMs::from_millis(500);
        scheduler.handle_event(SchedulerEvent::SuspendStarted, clock(100));
        scheduler.handle_event(SchedulerEvent::SuspendEnded, clock(700));
        assert!(matches!(scheduler.state(), SchedulerState::Running { .. }));
        assert_eq!(scheduler.status(clock(700)).remaining_ms, Some(1_000));
    }

    #[test]
    fn a_new_boot_starts_fresh() {
        let scheduler = test_scheduler();
        let restored = Scheduler::restore(
            scheduler.config.clone(),
            "new-boot".into(),
            clock(50),
            "/tmp/breakd.sock".into(),
            scheduler.snapshot(),
        );
        assert_eq!(restored.status(clock(50)).remaining_ms, Some(1_000));
    }

    #[test]
    fn timed_pause_recreates_an_active_overlay() {
        let mut scheduler = test_scheduler();
        scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
        scheduler
            .handle_command(
                &Command::Pause {
                    duration: Some(DurationMs::from_millis(50)),
                },
                clock(20),
            )
            .unwrap();
        let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(70));
        assert!(matches!(effects.as_slice(), [Effect::StartOverlay(_)]));
        assert!(matches!(
            scheduler.state(),
            SchedulerState::MiniBreak { .. }
        ));
    }

    #[test]
    fn wake_grace_delays_overlay_recreation() {
        let mut scheduler = test_scheduler();
        scheduler.config.recovery.wake_grace = DurationMs::from_millis(50);
        scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
        scheduler.handle_event(SchedulerEvent::SuspendStarted, clock(10));
        let effects = scheduler.handle_event(SchedulerEvent::SuspendEnded, clock(20));
        assert!(effects.is_empty());
        assert!(matches!(
            scheduler.state(),
            SchedulerState::PausedUntil { .. }
        ));

        let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(70));
        assert!(matches!(effects.as_slice(), [Effect::StartOverlay(_)]));
    }

    #[test]
    fn disabled_active_break_recovery_starts_fresh() {
        let mut active = test_scheduler();
        active.handle_command(&Command::Mini, clock(0)).unwrap();
        let mut config = active.config.clone();
        config.recovery.recover_active_break = false;
        let restored = Scheduler::restore(
            config,
            "boot".into(),
            clock(10),
            "/tmp/breakd.sock".into(),
            active.snapshot(),
        );
        assert!(matches!(restored.state(), SchedulerState::Running { .. }));
    }

    #[test]
    fn reset_missed_break_policy_discards_overdue_deadline() {
        let scheduler = test_scheduler();
        let mut config = scheduler.config.clone();
        config.recovery.missed_break = MissedBreakPolicy::Reset;
        let restored = Scheduler::restore(
            config,
            "boot".into(),
            clock(2_000),
            "/tmp/breakd.sock".into(),
            scheduler.snapshot(),
        );
        assert_eq!(restored.status(clock(2_000)).remaining_ms, Some(1_000));
    }

    #[test]
    fn stale_overlay_failure_does_not_affect_active_break() {
        let mut scheduler = test_scheduler();
        scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
        let effects = scheduler.handle_event(
            SchedulerEvent::OverlayFailed {
                session_id: BreakSessionId::new(),
                reason: "stale child".into(),
            },
            clock(1),
        );
        assert!(effects.is_empty());
        assert!(matches!(
            scheduler.state(),
            SchedulerState::MiniBreak { .. }
        ));
    }

    #[test]
    fn three_tier_cadence_reaches_a_rest_break() {
        let mut scheduler = test_scheduler();
        let mut starts = Vec::new();
        let mut last_session = None;
        for time in (0..=9_000).step_by(10) {
            scheduler.handle_event(SchedulerEvent::Tick, clock(time));
            let status = scheduler.status(clock(time));
            if status.active_session != last_session {
                if let (Some(_), Some(kind)) = (status.active_session, status.break_kind) {
                    starts.push((kind, status.longs_since_rest));
                }
                last_session = status.active_session;
            }
        }
        assert_eq!(
            starts,
            vec![
                (BreakKind::Mini, 0),
                (BreakKind::Mini, 0),
                (BreakKind::Long, 0),
                (BreakKind::Mini, 1),
                (BreakKind::Mini, 1),
                (BreakKind::Long, 1),
                (BreakKind::Rest, 2),
            ]
        );
        let status = scheduler.status(clock(9_000));
        assert_eq!(status.minis_since_long, 0);
        assert_eq!(status.longs_since_rest, 0);
    }

    #[test]
    fn long_requires_both_interval_and_mini_threshold() {
        let scheduler = test_scheduler();
        let context = ScheduleContext {
            cycle_started_mono_ms: 0,
            next_due_mono_ms: 0,
            minis_since_long: 2,
            longs_since_rest: 0,
            rest_cycle_started_mono_ms: 0,
            pending: None,
        };
        // Mini threshold met (2 >= 2), long interval (3s) not elapsed.
        assert_eq!(scheduler.next_kind(&context, 2_999), BreakKind::Mini);
        // Both conditions met.
        assert_eq!(scheduler.next_kind(&context, 3_000), BreakKind::Long);
        // Long interval elapsed, mini threshold not met.
        let mut context = context;
        context.minis_since_long = 1;
        assert_eq!(scheduler.next_kind(&context, 7_000), BreakKind::Mini);
    }

    #[test]
    fn rest_requires_both_interval_and_long_threshold() {
        let scheduler = test_scheduler();
        let context = ScheduleContext {
            cycle_started_mono_ms: 7_500,
            next_due_mono_ms: 0,
            minis_since_long: 0,
            longs_since_rest: 2,
            rest_cycle_started_mono_ms: 0,
            pending: None,
        };
        // Long threshold met (2 >= 2), rest interval (8s) not elapsed.
        assert_eq!(scheduler.next_kind(&context, 7_999), BreakKind::Mini);
        // Both conditions met.
        assert_eq!(scheduler.next_kind(&context, 8_000), BreakKind::Rest);
        // Rest interval elapsed, long threshold not met.
        let mut context = context;
        context.longs_since_rest = 1;
        assert_eq!(scheduler.next_kind(&context, 8_000), BreakKind::Mini);
    }

    #[test]
    fn scheduled_breaks_honour_both_conditions_over_a_full_cadence() {
        // Thresholds deliberately do not coincide with the interval boundaries:
        // three minis complete at 3.3s while the long interval elapses at 2.5s,
        // and the rest interval (9s) elapses between long-break completions.
        let mut scheduler = test_scheduler();
        scheduler.config.schedule.long.interval = DurationMs::from_millis(2_500);
        scheduler.config.schedule.long.after_minis = 3;
        scheduler.config.schedule.rest.interval = DurationMs::from_millis(9_000);
        scheduler.config.schedule.rest.after_longs = 1;

        let mut last_session = None;
        let mut active_kind = None;
        let mut minis_completed = 0_u32;
        let mut longs_completed = 0_u32;
        let mut long_cycle_started = 0_u64;
        let mut rest_cycle_started = 0_u64;
        for time in (0..=30_000).step_by(10) {
            scheduler.handle_event(SchedulerEvent::Tick, clock(time));
            let status = scheduler.status(clock(time));
            if status.active_session == last_session {
                continue;
            }
            match (active_kind.take(), status.break_kind) {
                (None, Some(kind)) if status.active_session.is_some() => {
                    match kind {
                        BreakKind::Mini => {}
                        BreakKind::Long => {
                            assert!(
                                time - long_cycle_started >= 2_500,
                                "long break at {time} before its interval elapsed"
                            );
                            assert!(
                                minis_completed >= 3,
                                "long break at {time} after {minis_completed} minis"
                            );
                        }
                        BreakKind::Rest => {
                            assert!(
                                time - rest_cycle_started >= 9_000,
                                "rest break at {time} before its interval elapsed"
                            );
                            assert!(
                                longs_completed >= 1,
                                "rest break at {time} after {longs_completed} longs"
                            );
                        }
                    }
                    active_kind = Some(kind);
                }
                (Some(kind), _) => match kind {
                    BreakKind::Mini => minis_completed += 1,
                    BreakKind::Long => {
                        minis_completed = 0;
                        longs_completed += 1;
                        long_cycle_started = time;
                    }
                    BreakKind::Rest => {
                        minis_completed = 0;
                        longs_completed = 0;
                        long_cycle_started = time;
                        rest_cycle_started = time;
                    }
                },
                _ => {}
            }
            last_session = status.active_session;
        }
        assert!(longs_completed > 0 || rest_cycle_started > 0);
    }

    #[test]
    fn rest_defers_to_the_cadence_until_its_interval_elapses() {
        let scheduler = test_scheduler();
        let context = ScheduleContext {
            cycle_started_mono_ms: 0,
            next_due_mono_ms: 5_000,
            minis_since_long: 2,
            longs_since_rest: 2,
            rest_cycle_started_mono_ms: 0,
            pending: None,
        };
        // Rest interval (8s) has not elapsed at 5s, so the long tier wins.
        assert_eq!(scheduler.next_kind(&context, 5_000), BreakKind::Long);
        // Once elapsed, rest supersedes an equally due long break.
        assert_eq!(scheduler.next_kind(&context, 8_000), BreakKind::Rest);
        let mut context = context;
        context.longs_since_rest = 1;
        assert_eq!(scheduler.next_kind(&context, 8_000), BreakKind::Long);
    }

    #[test]
    fn manual_rest_break_resets_all_counters() {
        let mut scheduler = test_scheduler();
        scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
        scheduler.handle_event(SchedulerEvent::Tick, clock(100));
        scheduler
            .handle_command(&Command::Long, clock(200))
            .unwrap();
        scheduler.handle_event(SchedulerEvent::Tick, clock(500));
        let status = scheduler.status(clock(500));
        assert_eq!(status.longs_since_rest, 1);

        let effects = scheduler
            .handle_command(&Command::Rest, clock(600))
            .unwrap();
        let [Effect::StartOverlay(spec)] = effects.as_slice() else {
            panic!("expected an overlay");
        };
        assert_eq!(spec.kind, BreakKind::Rest);
        assert!(matches!(
            scheduler.state(),
            SchedulerState::RestBreak { .. }
        ));

        scheduler.handle_event(SchedulerEvent::Tick, clock(1_100));
        assert!(matches!(scheduler.state(), SchedulerState::Running { .. }));
        let status = scheduler.status(clock(1_100));
        assert_eq!(status.minis_since_long, 0);
        assert_eq!(status.longs_since_rest, 0);
        assert_eq!(status.remaining_ms, Some(1_000));
    }

    #[test]
    fn rest_postpone_uses_its_own_rule_and_preserves_the_kind() {
        let mut scheduler = test_scheduler();
        scheduler.config.postpone.rest.duration = DurationMs::from_millis(700);
        scheduler.handle_command(&Command::Rest, clock(0)).unwrap();
        scheduler
            .handle_command(&Command::Postpone, clock(20))
            .unwrap();

        let due = match scheduler.state() {
            SchedulerState::Running { context } => context.next_due_mono_ms,
            state => panic!("unexpected state {state:?}"),
        };
        assert_eq!(due, 720);
        let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(due));
        let [Effect::StartOverlay(spec)] = effects.as_slice() else {
            panic!("expected the postponed overlay");
        };
        assert_eq!(spec.kind, BreakKind::Rest);
        assert_eq!(
            scheduler.handle_command(&Command::Postpone, clock(due + 20)),
            Err(SchedulerError::PostponeLimit)
        );
    }

    #[test]
    fn old_snapshot_without_rest_fields_restores() {
        let scheduler = test_scheduler();
        let encoded = r#"{
            "schema_version": 1,
            "boot_id": "boot",
            "state": {
                "state": "running",
                "context": {
                    "cycle_started_mono_ms": 0,
                    "next_due_mono_ms": 1000,
                    "minis_since_long": 1,
                    "pending": null
                }
            },
            "last_clock": {
                "monotonic_ms": 500,
                "boottime_ms": 500,
                "wall_unix_ms": 1700000000500
            }
        }"#;
        let snapshot: Snapshot = serde_json::from_str(encoded).unwrap();
        let restored = Scheduler::restore(
            scheduler.config.clone(),
            "boot".into(),
            clock(500),
            "/tmp/breakd.sock".into(),
            snapshot,
        );
        let status = restored.status(clock(500));
        assert_eq!(status.minis_since_long, 1);
        assert_eq!(status.longs_since_rest, 0);
        assert_eq!(status.remaining_ms, Some(500));
    }

    #[test]
    fn seven_simulated_days_preserve_scheduler_invariants() {
        let mut scheduler = test_scheduler();
        for time in (0..=7 * 24 * 60 * 60 * 1_000).step_by(10_000) {
            scheduler.handle_event(SchedulerEvent::Tick, clock(time));
            let status = scheduler.status(clock(time));
            assert!(status.active_session.is_none() || status.break_kind.is_some());
            assert!(status.postpone_count <= 1);
        }
    }

    proptest! {
        #[test]
        fn arbitrary_monotonic_ticks_keep_status_consistent(
            deltas in prop::collection::vec(0_u16..5_000, 1..500)
        ) {
            let mut scheduler = test_scheduler();
            let mut time = 0_u64;
            for delta in deltas {
                time = time.saturating_add(u64::from(delta));
                scheduler.handle_event(SchedulerEvent::Tick, clock(time));
                let status = scheduler.status(clock(time));
                prop_assert!(status.active_session.is_none() || status.break_kind.is_some());
                prop_assert!(status.postpone_count <= 1);
            }
        }
    }
}
