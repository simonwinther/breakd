use breakd_core::{BreakKind, Command};
use ksni::{Status, ToolTip, TrayMethods, menu::StandardItem};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayAction {
    Command(Command),
    OpenSettings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayState {
    pub paused: bool,
    pub active_kind: Option<BreakKind>,
    pub remaining_seconds: Option<u64>,
    pub can_skip: bool,
    pub can_postpone: bool,
}

impl TrayState {
    pub fn status_text(&self) -> String {
        if self.paused {
            return "Schedule paused".into();
        }
        let remaining = self
            .remaining_seconds
            .map(format_duration)
            .unwrap_or_else(|| "unknown".into());
        match self.active_kind {
            Some(BreakKind::Mini) => format!("Mini break: {remaining}"),
            Some(BreakKind::Long) => format!("Long break: {remaining}"),
            Some(BreakKind::Rest) => format!("Rest break: {remaining}"),
            None => format!("Next break: {remaining}"),
        }
    }
}

pub struct TrayController {
    handle: Option<ksni::Handle<BreakdTray>>,
    sender: UnboundedSender<TrayAction>,
    name: String,
    last_state: Option<TrayState>,
}

impl TrayController {
    pub fn new(sender: UnboundedSender<TrayAction>, name: impl Into<String>) -> Self {
        Self {
            handle: None,
            sender,
            name: name.into(),
            last_state: None,
        }
    }

    pub async fn set_enabled(
        &mut self,
        enabled: bool,
        state: TrayState,
    ) -> Result<(), ksni::Error> {
        if !enabled {
            if let Some(handle) = self.handle.take() {
                handle.shutdown().await;
            }
            self.last_state = None;
            return Ok(());
        }

        if self.handle.is_none() {
            let tray = BreakdTray {
                state: state.clone(),
                sender: self.sender.clone(),
                name: self.name.clone(),
            };
            self.handle = Some(tray.assume_sni_available(true).spawn().await?);
            self.last_state = Some(state);
            return Ok(());
        }

        self.update(state).await;
        Ok(())
    }

    pub async fn update(&mut self, state: TrayState) {
        if self.last_state.as_ref() == Some(&state) {
            return;
        }
        if let Some(handle) = &self.handle {
            let next = state.clone();
            if handle
                .update(move |tray: &mut BreakdTray| tray.state = next)
                .await
                .is_none()
            {
                self.handle = None;
            }
        }
        self.last_state = Some(state);
    }

    pub fn available(&self) -> bool {
        self.handle
            .as_ref()
            .is_some_and(|handle| !handle.is_closed())
    }
}

struct BreakdTray {
    state: TrayState,
    sender: UnboundedSender<TrayAction>,
    name: String,
}

impl BreakdTray {
    fn command_item(&self, label: &str, command: Command, enabled: bool) -> ksni::MenuItem<Self> {
        let sender = self.sender.clone();
        StandardItem {
            label: label.into(),
            enabled,
            activate: Box::new(move |_| {
                let _ = sender.send(TrayAction::Command(command.clone()));
            }),
            ..Default::default()
        }
        .into()
    }
}

impl ksni::Tray for BreakdTray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        self.name.clone()
    }

    fn title(&self) -> String {
        format!("{}: {}", self.name, self.state.status_text())
    }

    fn icon_name(&self) -> String {
        if self.state.paused {
            "media-playback-pause-symbolic"
        } else if self.state.active_kind.is_some() {
            "appointment-soon-symbolic"
        } else {
            "alarm-symbolic"
        }
        .into()
    }

    fn status(&self) -> Status {
        if self.state.active_kind.is_some() && !self.state.paused {
            Status::NeedsAttention
        } else {
            Status::Active
        }
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            icon_name: self.icon_name(),
            title: self.name.clone(),
            description: self.state.status_text(),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let has_active_break = self.state.active_kind.is_some();
        let settings_sender = self.sender.clone();
        vec![
            StandardItem {
                label: format!("{}: {}", self.name, self.state.status_text()),
                enabled: false,
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            self.command_item(
                if self.state.paused {
                    "Resume schedule"
                } else {
                    "Pause schedule"
                },
                Command::Toggle,
                self.state.paused || !has_active_break || self.state.can_skip,
            ),
            self.command_item("Start mini break", Command::Mini, !has_active_break),
            self.command_item("Start long break", Command::Long, !has_active_break),
            self.command_item("Start rest break", Command::Rest, !has_active_break),
            self.command_item("Skip break", Command::Skip, self.state.can_skip),
            self.command_item("Postpone break", Command::Postpone, self.state.can_postpone),
            ksni::MenuItem::Separator,
            self.command_item(
                "Reset schedule",
                Command::Reset,
                !has_active_break || self.state.can_skip,
            ),
            self.command_item("Reload config", Command::Reload, true),
            StandardItem {
                label: "Settings...".into(),
                activate: Box::new(move |_| {
                    let _ = settings_sender.send(TrayAction::OpenSettings);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

fn format_duration(total_seconds: u64) -> String {
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_status_for_each_scheduler_mode() {
        let running = TrayState {
            paused: false,
            active_kind: None,
            remaining_seconds: Some(65),
            can_skip: false,
            can_postpone: false,
        };
        assert_eq!(running.status_text(), "Next break: 01:05");

        let active = TrayState {
            active_kind: Some(BreakKind::Long),
            remaining_seconds: Some(3_661),
            ..running.clone()
        };
        assert_eq!(active.status_text(), "Long break: 1:01:01");

        let resting = TrayState {
            active_kind: Some(BreakKind::Rest),
            remaining_seconds: Some(1_800),
            ..running.clone()
        };
        assert_eq!(resting.status_text(), "Rest break: 30:00");

        let paused = TrayState {
            paused: true,
            ..running
        };
        assert_eq!(paused.status_text(), "Schedule paused");
    }

    #[tokio::test]
    async fn pause_menu_item_sends_toggle_command() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut tray = BreakdTray {
            state: TrayState {
                paused: false,
                active_kind: None,
                remaining_seconds: Some(60),
                can_skip: false,
                can_postpone: false,
            },
            sender,
            name: "breakd-dev".into(),
        };
        let menu = ksni::Tray::menu(&tray);
        let ksni::MenuItem::Standard(item) = menu.into_iter().nth(2).unwrap() else {
            panic!("expected pause menu item");
        };
        (item.activate)(&mut tray);
        assert_eq!(
            receiver.recv().await,
            Some(TrayAction::Command(Command::Toggle))
        );
    }

    #[tokio::test]
    async fn settings_menu_item_sends_open_action() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut tray = BreakdTray {
            state: TrayState {
                paused: false,
                active_kind: None,
                remaining_seconds: Some(60),
                can_skip: false,
                can_postpone: false,
            },
            sender,
            name: "breakd-dev".into(),
        };
        let menu = ksni::Tray::menu(&tray);
        let ksni::MenuItem::Standard(item) = menu.into_iter().last().unwrap() else {
            panic!("expected settings menu item");
        };
        (item.activate)(&mut tray);
        assert_eq!(receiver.recv().await, Some(TrayAction::OpenSettings));
    }
}
