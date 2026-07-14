use breakd_core::{
    AppConfig, BreakKind, BreakSessionId, ClockSample, Command, DueBreakId, DurationMs,
    MissedBreakPolicy, OverlaySpec, StrictMode,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingBreak {
    pub id: DueBreakId,
    pub kind: BreakKind,
    pub postpone_count: u32,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SuspendReason {
    Sleep,
    Lock,
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
}

#[derive(Debug, Clone)]
pub struct Scheduler {
    config: AppConfig,
    boot_id: String,
    socket_path: String,
    state: SchedulerState,
    last_clock: ClockSample,
}

impl Scheduler {
    pub fn new(config: AppConfig, boot_id: String, now: ClockSample, socket_path: String) -> Self {
        let mut scheduler = Self {
            state: Self::fresh_running(&config, now),
            config,
            boot_id,
            socket_path,
            last_clock: now,
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
        self.config = config;
    }

    pub fn startup_effects(&self, now: ClockSample) -> Vec<Effect> {
        self.active_break()
            .filter(|active| active.manual_resume || active.ends_boot_ms > now.boottime_ms)
            .map(|active| vec![Effect::StartOverlay(self.overlay_spec(active, now))])
            .unwrap_or_default()
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
                if self
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
            Command::Status | Command::Reload | Command::Outputs | Command::Doctor => {
                Err(SchedulerError::DaemonCommand)
            }
        }
    }

    pub fn status(&self, now: ClockSample) -> SchedulerStatus {
        let context = self.context();
        let active = self.active_break();
        let paused = matches!(
            self.state,
            SchedulerState::PausedIndefinitely { .. } | SchedulerState::PausedUntil { .. }
        );
        let awaiting_resume = active.is_some_and(|active| awaiting_manual_resume(active, now));
        let remaining_ms = if let Some(active) = active {
            Some(active.ends_boot_ms.saturating_sub(now.boottime_ms))
        } else {
            context.map(|context| context.next_due_mono_ms.saturating_sub(now.monotonic_ms))
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
                    self.skip_available(active.due.kind)
                        && now.boottime_ms >= active.strict_until_boot_ms
                }),
            can_postpone: !paused
                && !awaiting_resume
                && active.is_some_and(|active| self.postpone_allowed(active, now)),
        }
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
                let lead = self.notification_lead(kind);
                if now.monotonic_ms >= context.next_due_mono_ms.saturating_sub(lead) {
                    context.pending.get_or_insert_with(|| PendingBreak {
                        id: DueBreakId::new(),
                        kind,
                        postpone_count: 0,
                    });
                    if now.monotonic_ms >= context.next_due_mono_ms {
                        return self.begin_scheduled_break(kind, context, now);
                    }
                    self.state = match kind {
                        BreakKind::Mini => SchedulerState::PreMiniBreak { context },
                        BreakKind::Long => SchedulerState::PreLongBreak { context },
                        BreakKind::Rest => SchedulerState::PreRestBreak { context },
                    };
                    if self.config.notifications.enabled {
                        return vec![Effect::Notify {
                            summary: format!("{} break soon", title_kind(kind)),
                            body: format!("Starts in {}", DurationMs::from_millis(lead)),
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
        match kind {
            BreakKind::Mini => self.config.notifications.mini_lead.as_millis(),
            BreakKind::Long => self.config.notifications.long_lead.as_millis(),
            BreakKind::Rest => self.config.notifications.rest_lead.as_millis(),
        }
    }

    fn begin_scheduled_break(
        &mut self,
        kind: BreakKind,
        context: ScheduleContext,
        now: ClockSample,
    ) -> Vec<Effect> {
        let due = context.pending.clone().unwrap_or_else(|| PendingBreak {
            id: DueBreakId::new(),
            kind,
            postpone_count: 0,
        });
        self.begin_break(kind, context, due, now)
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
        let duration = self.break_duration(kind);
        let ends_boot_ms = now.boottime_ms.saturating_add(duration);
        let strict_until_boot_ms = match self.config.strict.mode {
            StrictMode::Off => now.boottime_ms,
            StrictMode::Delay => now
                .boottime_ms
                .saturating_add(self.config.strict.minimum_visible.as_millis())
                .min(ends_boot_ms),
            StrictMode::Entire => ends_boot_ms,
        };
        let active = ActiveBreak {
            context,
            due,
            session_id: BreakSessionId::new(),
            started_boot_ms: now.boottime_ms,
            ends_boot_ms,
            strict_until_boot_ms,
            manual_resume: self.config.completion.manual_resume,
            completion_sound_emitted: false,
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
        if !active.manual_resume {
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
        self.ensure_dismissal_allowed(now)?;
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
        if now.boottime_ms < active.strict_until_boot_ms
            && !self.config.strict.allow_postpone_during_lockout
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
        if matches!(self.state, SchedulerState::Suspended { .. }) {
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
        };
        effects
    }

    fn resume_suspended(&mut self, expected: SuspendReason, now: ClockSample) -> Vec<Effect> {
        let SchedulerState::Suspended {
            reason,
            inner,
            started_at,
        } = self.state.clone()
        else {
            return Vec::new();
        };
        if reason != expected {
            return Vec::new();
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
        let mono_delta = if reason == SuspendReason::Lock {
            now.monotonic_ms.saturating_sub(started_at.monotonic_ms)
        } else {
            0
        };
        let boot_delta = if counts_as_break { 0 } else { elapsed_boot };
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
            kind: active.due.kind,
            duration: DurationMs::from_millis(active.ends_boot_ms.saturating_sub(now.boottime_ms)),
            strict_remaining: DurationMs::from_millis(
                active.strict_until_boot_ms.saturating_sub(now.boottime_ms),
            ),
            can_skip: self.skip_available(active.due.kind),
            can_postpone: self.postpone_available(active),
            manual_resume: active.manual_resume,
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
            && (now.boottime_ms >= active.strict_until_boot_ms
                || self.config.strict.allow_postpone_during_lockout)
    }

    fn postpone_available(&self, active: &ActiveBreak) -> bool {
        let rule = match active.due.kind {
            BreakKind::Mini => &self.config.postpone.mini,
            BreakKind::Long => &self.config.postpone.long,
            BreakKind::Rest => &self.config.postpone.rest,
        };
        rule.enabled
            && rule
                .max_postponements
                .is_none_or(|maximum| active.due.postpone_count < maximum)
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

fn context_in(state: &SchedulerState) -> Option<&ScheduleContext> {
    match state {
        SchedulerState::Running { context }
        | SchedulerState::PreMiniBreak { context }
        | SchedulerState::PreLongBreak { context }
        | SchedulerState::PreRestBreak { context } => Some(context),
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
        | SchedulerState::PreRestBreak { context } => now.monotonic_ms >= context.next_due_mono_ms,
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
        assert!(matches!(effects.as_slice(), [Effect::Notify { .. }]));

        let effects = scheduler.handle_event(SchedulerEvent::Tick, clock(1_000));
        assert!(matches!(
            scheduler.state(),
            SchedulerState::MiniBreak { .. }
        ));
        assert!(matches!(effects.as_slice(), [Effect::StartOverlay(_)]));
    }

    #[test]
    fn strict_mode_rejects_early_skip() {
        let mut scheduler = test_scheduler();
        let effects = scheduler.handle_command(&Command::Mini, clock(0)).unwrap();
        let Effect::StartOverlay(spec) = &effects[0] else {
            panic!("expected an overlay");
        };
        assert!(spec.can_skip);
        assert!(spec.can_postpone);
        assert!(!scheduler.status(clock(10)).can_postpone);
        assert_eq!(
            scheduler.handle_command(&Command::Skip, clock(10)),
            Err(SchedulerError::StrictMode)
        );
        assert!(scheduler.status(clock(20)).can_postpone);
        assert!(scheduler.handle_command(&Command::Skip, clock(20)).is_ok());
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
