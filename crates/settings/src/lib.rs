use std::{cell::RefCell, process::Command, rc::Rc};

use breakd_core::{AppConfig, ContentSelector, DisplayMode, DurationMs, PointerMode, StrictMode};
use gtk::{gio, prelude::*};
use gtk4 as gtk;

const APPLICATION_ID: &str = "io.github.simonwinther.breakd.settings";

pub fn run() -> Result<(), String> {
    let initial = breakd_config::load().map_err(|error| error.to_string())?;
    let application = gtk::Application::builder()
        .application_id(APPLICATION_ID)
        .build();
    let window_holder = Rc::new(RefCell::new(None::<gtk::ApplicationWindow>));
    let holder_for_activate = window_holder.clone();
    application.connect_activate(move |application| {
        if let Some(window) = holder_for_activate.borrow().as_ref() {
            window.present();
            return;
        }
        install_css();
        let window = build_window(application, initial.clone());
        let holder_for_close = holder_for_activate.clone();
        window.connect_close_request(move |_| {
            holder_for_close.borrow_mut().take();
            glib::Propagation::Proceed
        });
        window.present();
        holder_for_activate.replace(Some(window));
    });
    let holder_for_shutdown = window_holder.clone();
    application.connect_shutdown(move |_| {
        holder_for_shutdown.borrow_mut().take();
    });
    application.run_with_args(&["breakd-settings"]);
    Ok(())
}

#[derive(Clone)]
struct SettingsWidgets {
    mini_interval: gtk::Entry,
    mini_duration: gtk::Entry,
    long_interval: gtk::Entry,
    long_duration: gtk::Entry,
    long_after_minis: gtk::SpinButton,
    notifications_enabled: gtk::Switch,
    mini_notification_lead: gtk::Entry,
    long_notification_lead: gtk::Entry,
    mini_skip: gtk::Switch,
    long_skip: gtk::Switch,
    mini_postpone: PostponeWidgets,
    long_postpone: PostponeWidgets,
    strict_mode: gtk::DropDown,
    strict_minimum: gtk::Entry,
    allow_postpone_during_lockout: gtk::Switch,
    inhibit_shortcuts: gtk::Switch,
    manual_resume: gtk::Switch,
    display_mode: gtk::DropDown,
    content_selector: gtk::DropDown,
    opacity: gtk::Scale,
    pointer_mode: gtk::DropDown,
    show_message: gtk::Switch,
    idle_enabled: gtk::Switch,
    idle_reset_after: gtk::Entry,
    tray_enabled: gtk::Switch,
}

#[derive(Clone)]
struct PostponeWidgets {
    enabled: gtk::Switch,
    duration: gtk::Entry,
    limit_enabled: gtk::Switch,
    maximum: gtk::SpinButton,
}

impl SettingsWidgets {
    fn collect(&self, base: &AppConfig) -> Result<AppConfig, String> {
        let mut config = base.clone();
        config.schedule.mini.interval = parse_duration(&self.mini_interval, "Mini interval")?;
        config.schedule.mini.duration = parse_duration(&self.mini_duration, "Mini duration")?;
        config.schedule.long.interval = parse_duration(&self.long_interval, "Long interval")?;
        config.schedule.long.duration = parse_duration(&self.long_duration, "Long duration")?;
        config.schedule.long.after_minis = self.long_after_minis.value_as_int() as u32;

        config.notifications.enabled = self.notifications_enabled.is_active();
        config.notifications.mini_lead =
            parse_duration(&self.mini_notification_lead, "Mini notification lead")?;
        config.notifications.long_lead =
            parse_duration(&self.long_notification_lead, "Long notification lead")?;

        config.skip.mini.enabled = self.mini_skip.is_active();
        config.skip.long.enabled = self.long_skip.is_active();
        update_postpone_rule(
            &mut config.postpone.mini,
            &self.mini_postpone,
            "Mini postpone duration",
        )?;
        update_postpone_rule(
            &mut config.postpone.long,
            &self.long_postpone,
            "Long postpone duration",
        )?;

        config.strict.mode = strict_mode_from_index(self.strict_mode.selected());
        config.strict.minimum_visible = parse_duration(&self.strict_minimum, "Strict-mode delay")?;
        config.strict.allow_postpone_during_lockout =
            self.allow_postpone_during_lockout.is_active();
        config.strict.inhibit_shortcuts = self.inhibit_shortcuts.is_active();
        config.completion.manual_resume = self.manual_resume.is_active();

        config.display.mode = display_mode_from_index(self.display_mode.selected());
        config.display.content_selector =
            content_selector_from_index(self.content_selector.selected());
        config.display.opacity = self.opacity.value();
        config.display.pointer_mode = pointer_mode_from_index(self.pointer_mode.selected());
        config.content.show_message = self.show_message.is_active();

        config.idle.enabled = self.idle_enabled.is_active();
        config.idle.reset_after = parse_duration(&self.idle_reset_after, "Idle reset threshold")?;
        config.tray.enabled = self.tray_enabled.is_active();

        breakd_config::validate(&config).map_err(|error| error.to_string())?;
        Ok(config)
    }
}

fn build_window(application: &gtk::Application, initial: AppConfig) -> gtk::ApplicationWindow {
    let state = Rc::new(RefCell::new(initial.clone()));
    let (schedule_page, schedule_widgets) = schedule_page(&initial);
    let (actions_page, action_widgets) = actions_page(&initial);
    let (desktop_page, desktop_widgets) = desktop_page(&initial);
    let widgets = SettingsWidgets {
        mini_interval: schedule_widgets.0,
        mini_duration: schedule_widgets.1,
        long_interval: schedule_widgets.2,
        long_duration: schedule_widgets.3,
        long_after_minis: schedule_widgets.4,
        notifications_enabled: schedule_widgets.5,
        mini_notification_lead: schedule_widgets.6,
        long_notification_lead: schedule_widgets.7,
        mini_skip: action_widgets.0,
        long_skip: action_widgets.1,
        mini_postpone: action_widgets.2,
        long_postpone: action_widgets.3,
        strict_mode: action_widgets.4,
        strict_minimum: action_widgets.5,
        allow_postpone_during_lockout: action_widgets.6,
        inhibit_shortcuts: action_widgets.7,
        manual_resume: action_widgets.8,
        display_mode: desktop_widgets.0,
        content_selector: desktop_widgets.1,
        opacity: desktop_widgets.2,
        pointer_mode: desktop_widgets.3,
        show_message: desktop_widgets.4,
        idle_enabled: desktop_widgets.5,
        idle_reset_after: desktop_widgets.6,
        tray_enabled: desktop_widgets.7,
    };

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .transition_duration(140)
        .vexpand(true)
        .build();
    stack.add_titled(&schedule_page, Some("schedule"), "Schedule");
    stack.add_titled(&actions_page, Some("actions"), "Actions");
    stack.add_titled(&desktop_page, Some("desktop"), "Desktop");
    stack.set_visible_child_name("schedule");

    let switcher = gtk::StackSwitcher::builder()
        .stack(&stack)
        .halign(gtk::Align::Center)
        .build();
    let introduction = gtk::Box::new(gtk::Orientation::Vertical, 4);
    introduction.set_margin_top(16);
    introduction.set_margin_start(24);
    introduction.set_margin_end(24);
    let title = gtk::Label::new(Some("Break preferences"));
    title.set_halign(gtk::Align::Start);
    title.add_css_class("settings-intro-title");
    let description = gtk::Label::new(Some(
        "Set your break cadence and what can happen while a break is active.",
    ));
    description.set_halign(gtk::Align::Start);
    description.set_wrap(true);
    description.add_css_class("settings-description");
    introduction.append(&title);
    introduction.append(&description);

    let status_text = format!(
        "Settings are stored in {}.",
        breakd_config::config_path().display()
    );
    let status = gtk::Label::new(Some(&status_text));
    status.set_halign(gtk::Align::Start);
    status.set_wrap(true);
    status.set_hexpand(true);
    status.add_css_class("settings-status");
    let status_bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    status_bar.add_css_class("settings-status-bar");
    status_bar.append(&status);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
    root.append(&introduction);
    root.append(&switcher);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    root.append(&stack);
    root.append(&status_bar);

    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    save.set_tooltip_text(Some("Validate, save, and reload the configuration"));
    let header = gtk::HeaderBar::new();
    header.set_title_widget(Some(&gtk::Label::new(Some("breakd Settings"))));
    header.pack_end(&save);

    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .title("breakd Settings")
        .default_width(760)
        .default_height(720)
        .child(&root)
        .build();
    window.set_titlebar(Some(&header));

    let state_for_save = state.clone();
    save.connect_clicked(move |button| {
        let current = state_for_save.borrow().clone();
        let config = match widgets.collect(&current) {
            Ok(config) => config,
            Err(error) => {
                set_status(&status, &error, true);
                return;
            }
        };
        let restart_required = current.idle != config.idle;
        button.set_sensitive(false);
        set_status(&status, "Saving configuration...", false);

        let button = button.clone();
        let status = status.clone();
        let state = state_for_save.clone();
        glib::spawn_future_local(async move {
            let saved_config = config.clone();
            let result =
                gio::spawn_blocking(move || save_and_reload(&config, restart_required)).await;
            button.set_sensitive(true);
            match result {
                Ok(Ok(message)) => {
                    state.replace(saved_config);
                    set_status(&status, &message, false);
                }
                Ok(Err(error)) => set_status(&status, &error, true),
                Err(_) => set_status(&status, "Saving failed unexpectedly.", true),
            }
        });
    });

    window
}

type SchedulePageWidgets = (
    gtk::Entry,
    gtk::Entry,
    gtk::Entry,
    gtk::Entry,
    gtk::SpinButton,
    gtk::Switch,
    gtk::Entry,
    gtk::Entry,
);

fn schedule_page(config: &AppConfig) -> (gtk::ScrolledWindow, SchedulePageWidgets) {
    let mini_interval = duration_entry(config.schedule.mini.interval);
    let mini_duration = duration_entry(config.schedule.mini.duration);
    let long_interval = duration_entry(config.schedule.long.interval);
    let long_duration = duration_entry(config.schedule.long.duration);
    let long_after_minis = gtk::SpinButton::with_range(1.0, 100.0, 1.0);
    long_after_minis.set_value(f64::from(config.schedule.long.after_minis));

    let mini_group = settings_group(
        "Mini breaks",
        "Short, frequent pauses from keyboard and screen use.",
        &[
            settings_row("Interval", "Time between completed breaks.", &mini_interval),
            settings_row(
                "Duration",
                "How long the overlay remains visible.",
                &mini_duration,
            ),
        ],
    );
    let long_group = settings_group(
        "Long breaks",
        "A long break starts after both thresholds have been reached.",
        &[
            settings_row(
                "Minimum interval",
                "Time since the previous long break.",
                &long_interval,
            ),
            settings_row(
                "Duration",
                "How long the overlay remains visible.",
                &long_duration,
            ),
            settings_row(
                "Mini-break threshold",
                "Completed mini breaks required before a long break.",
                &long_after_minis,
            ),
        ],
    );

    let notifications_enabled = gtk::Switch::builder()
        .active(config.notifications.enabled)
        .valign(gtk::Align::Center)
        .build();
    let mini_notification_lead = duration_entry(config.notifications.mini_lead);
    let long_notification_lead = duration_entry(config.notifications.long_lead);
    mini_notification_lead.set_sensitive(config.notifications.enabled);
    long_notification_lead.set_sensitive(config.notifications.enabled);
    let mini_lead_for_toggle = mini_notification_lead.clone();
    let long_lead_for_toggle = long_notification_lead.clone();
    notifications_enabled.connect_active_notify(move |toggle| {
        mini_lead_for_toggle.set_sensitive(toggle.is_active());
        long_lead_for_toggle.set_sensitive(toggle.is_active());
    });
    let notification_group = settings_group(
        "Notifications",
        "Show a desktop notification shortly before an overlay appears.",
        &[
            settings_row(
                "Pre-break notifications",
                "Master switch for mini and long break notices.",
                &notifications_enabled,
            ),
            settings_row(
                "Mini-break notice",
                "How early to notify before a mini break.",
                &mini_notification_lead,
            ),
            settings_row(
                "Long-break notice",
                "How early to notify before a long break.",
                &long_notification_lead,
            ),
        ],
    );

    (
        settings_page(&[mini_group, long_group, notification_group]),
        (
            mini_interval,
            mini_duration,
            long_interval,
            long_duration,
            long_after_minis,
            notifications_enabled,
            mini_notification_lead,
            long_notification_lead,
        ),
    )
}

type ActionPageWidgets = (
    gtk::Switch,
    gtk::Switch,
    PostponeWidgets,
    PostponeWidgets,
    gtk::DropDown,
    gtk::Entry,
    gtk::Switch,
    gtk::Switch,
    gtk::Switch,
);

fn actions_page(config: &AppConfig) -> (gtk::ScrolledWindow, ActionPageWidgets) {
    let mini_skip = gtk::Switch::builder()
        .active(config.skip.mini.enabled)
        .valign(gtk::Align::Center)
        .build();
    let long_skip = gtk::Switch::builder()
        .active(config.skip.long.enabled)
        .valign(gtk::Align::Center)
        .build();
    let (mini_postpone, mini_postpone_rows) = postpone_controls(&config.postpone.mini);
    let (long_postpone, long_postpone_rows) = postpone_controls(&config.postpone.long);

    let mut mini_rows = vec![settings_row(
        "Allow skip",
        "Show the Skip control for this break.",
        &mini_skip,
    )];
    mini_rows.extend(mini_postpone_rows);
    let mut long_rows = vec![settings_row(
        "Allow skip",
        "Show the Skip control for this break.",
        &long_skip,
    )];
    long_rows.extend(long_postpone_rows);

    let mini_group = settings_group(
        "Mini-break actions",
        "Controls available while a mini break is active.",
        &mini_rows,
    );
    let long_group = settings_group(
        "Long-break actions",
        "Controls available while a long break is active.",
        &long_rows,
    );

    let strict_mode = gtk::DropDown::from_strings(&["Off", "Delay controls", "Entire break"]);
    strict_mode.set_selected(strict_mode_index(config.strict.mode));
    let strict_minimum = duration_entry(config.strict.minimum_visible);
    strict_minimum.set_sensitive(config.strict.mode == StrictMode::Delay);
    let strict_minimum_for_mode = strict_minimum.clone();
    strict_mode.connect_selected_notify(move |dropdown| {
        strict_minimum_for_mode
            .set_sensitive(strict_mode_from_index(dropdown.selected()) == StrictMode::Delay);
    });
    let allow_postpone_during_lockout = gtk::Switch::builder()
        .active(config.strict.allow_postpone_during_lockout)
        .valign(gtk::Align::Center)
        .build();
    let inhibit_shortcuts = gtk::Switch::builder()
        .active(config.strict.inhibit_shortcuts)
        .valign(gtk::Align::Center)
        .build();
    let strict_group = settings_group(
        "Strict mode",
        "Control when a visible break may be dismissed.",
        &[
            settings_row(
                "Mode",
                "Delay controls briefly or lock them for the whole break.",
                &strict_mode,
            ),
            settings_row(
                "Minimum visible time",
                "Used when strict mode delays controls.",
                &strict_minimum,
            ),
            settings_row(
                "Postpone during lockout",
                "Allow postponement before strict mode unlocks other controls.",
                &allow_postpone_during_lockout,
            ),
            settings_row(
                "Block desktop shortcuts",
                "Temporarily inhibit compositor bindings during strict breaks.",
                &inhibit_shortcuts,
            ),
        ],
    );

    let manual_resume = gtk::Switch::builder()
        .active(config.completion.manual_resume)
        .valign(gtk::Align::Center)
        .build();
    let completion_group = settings_group(
        "Break completion",
        "Choose when a completed break returns to the work timer.",
        &[settings_row(
            "Manual resume",
            "Wait at zero until you press a key or click the overlay.",
            &manual_resume,
        )],
    );

    (
        settings_page(&[mini_group, long_group, completion_group, strict_group]),
        (
            mini_skip,
            long_skip,
            mini_postpone,
            long_postpone,
            strict_mode,
            strict_minimum,
            allow_postpone_during_lockout,
            inhibit_shortcuts,
            manual_resume,
        ),
    )
}

type DesktopPageWidgets = (
    gtk::DropDown,
    gtk::DropDown,
    gtk::Scale,
    gtk::DropDown,
    gtk::Switch,
    gtk::Switch,
    gtk::Entry,
    gtk::Switch,
);

fn desktop_page(config: &AppConfig) -> (gtk::ScrolledWindow, DesktopPageWidgets) {
    let display_mode = gtk::DropDown::from_strings(&[
        "Every monitor",
        "Focused monitor",
        "Cursor monitor",
        "Primary monitor",
        "Configured monitor",
        "Dim all, controls on one",
    ]);
    display_mode.set_selected(display_mode_index(config.display.mode));
    let content_selector = gtk::DropDown::from_strings(&[
        "Focused monitor",
        "Cursor monitor",
        "Primary monitor",
        "Configured monitor",
    ]);
    content_selector.set_selected(content_selector_index(config.display.content_selector));
    content_selector.set_sensitive(config.display.mode == DisplayMode::DimAllContentOne);
    let content_selector_for_mode = content_selector.clone();
    display_mode.connect_selected_notify(move |dropdown| {
        content_selector_for_mode.set_sensitive(
            display_mode_from_index(dropdown.selected()) == DisplayMode::DimAllContentOne,
        );
    });

    let opacity = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.2, 1.0, 0.05);
    opacity.set_value(config.display.opacity);
    opacity.set_digits(2);
    opacity.set_value_pos(gtk::PositionType::Right);
    opacity.set_size_request(210, -1);
    let pointer_mode =
        gtk::DropDown::from_strings(&["Controls only", "Block all clicks", "Click through"]);
    pointer_mode.set_selected(pointer_mode_index(config.display.pointer_mode));
    let show_message = gtk::Switch::builder()
        .active(config.content.show_message)
        .valign(gtk::Align::Center)
        .build();
    let display_group = settings_group(
        "Overlay",
        "Choose where breaks appear and how the desktop behaves underneath.",
        &[
            settings_row("Display mode", "Monitor coverage policy.", &display_mode),
            settings_row(
                "Controls monitor",
                "Used when every monitor is dimmed.",
                &content_selector,
            ),
            settings_row(
                "Background opacity",
                "Strength of the screen dimming.",
                &opacity,
            ),
            settings_row(
                "Pointer input",
                "Whether clicks reach applications below.",
                &pointer_mode,
            ),
            settings_row(
                "Exercise suggestion",
                "Show one configured message during each break.",
                &show_message,
            ),
        ],
    );

    let idle_enabled = gtk::Switch::builder()
        .active(config.idle.enabled)
        .valign(gtk::Align::Center)
        .build();
    let idle_reset_after = duration_entry(config.idle.reset_after);
    idle_reset_after.set_sensitive(config.idle.enabled);
    let idle_reset_for_toggle = idle_reset_after.clone();
    idle_enabled.connect_active_notify(move |toggle| {
        idle_reset_for_toggle.set_sensitive(toggle.is_active());
    });
    let tray_enabled = gtk::Switch::builder()
        .active(config.tray.enabled)
        .valign(gtk::Align::Center)
        .build();
    let integration_group = settings_group(
        "Desktop integration",
        "Optional behavior outside the break overlay.",
        &[
            settings_row(
                "Natural-break detection",
                "Reset the schedule after a sufficiently long idle period.",
                &idle_enabled,
            ),
            settings_row(
                "Idle reset threshold",
                "Idle time that counts as a natural break.",
                &idle_reset_after,
            ),
            settings_row(
                "Tray indicator",
                "Show schedule status and quick actions in a compatible panel.",
                &tray_enabled,
            ),
        ],
    );

    let advanced = gtk::Label::new(Some(
        "Monitor identifiers, recovery policy, messages, and logging remain available in config.toml.",
    ));
    advanced.set_wrap(true);
    advanced.set_halign(gtk::Align::Start);
    advanced.add_css_class("settings-description");
    let advanced_group = gtk::Box::new(gtk::Orientation::Vertical, 0);
    advanced_group.append(&advanced);

    (
        settings_page(&[display_group, integration_group, advanced_group]),
        (
            display_mode,
            content_selector,
            opacity,
            pointer_mode,
            show_message,
            idle_enabled,
            idle_reset_after,
            tray_enabled,
        ),
    )
}

fn postpone_controls(rule: &breakd_core::PostponeRule) -> (PostponeWidgets, Vec<gtk::Box>) {
    let enabled = gtk::Switch::builder()
        .active(rule.enabled)
        .valign(gtk::Align::Center)
        .build();
    let duration = duration_entry(rule.duration);
    let limit_enabled = gtk::Switch::builder()
        .active(rule.max_postponements.is_some())
        .valign(gtk::Align::Center)
        .build();
    let maximum = gtk::SpinButton::with_range(1.0, 100.0, 1.0);
    maximum.set_value(f64::from(rule.max_postponements.unwrap_or(2)));
    let limit_controls = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    limit_controls.append(&maximum);
    limit_controls.append(&limit_enabled);

    duration.set_sensitive(rule.enabled);
    limit_enabled.set_sensitive(rule.enabled);
    maximum.set_sensitive(rule.enabled && rule.max_postponements.is_some());
    let duration_for_enabled = duration.clone();
    let limit_for_enabled = limit_enabled.clone();
    let maximum_for_enabled = maximum.clone();
    enabled.connect_active_notify(move |toggle| {
        let active = toggle.is_active();
        duration_for_enabled.set_sensitive(active);
        limit_for_enabled.set_sensitive(active);
        maximum_for_enabled.set_sensitive(active && limit_for_enabled.is_active());
    });
    let enabled_for_limit = enabled.clone();
    let maximum_for_limit = maximum.clone();
    limit_enabled.connect_active_notify(move |toggle| {
        maximum_for_limit.set_sensitive(enabled_for_limit.is_active() && toggle.is_active());
    });

    let widgets = PostponeWidgets {
        enabled: enabled.clone(),
        duration: duration.clone(),
        limit_enabled: limit_enabled.clone(),
        maximum: maximum.clone(),
    };
    let rows = vec![
        settings_row(
            "Allow postpone",
            "Allow this break to be delayed.",
            &enabled,
        ),
        settings_row(
            "Postpone by",
            "Delay before showing this break again.",
            &duration,
        ),
        settings_row(
            "Limit postponements",
            "Leave off for unlimited postponements.",
            &limit_controls,
        ),
    ];
    (widgets, rows)
}

fn update_postpone_rule(
    rule: &mut breakd_core::PostponeRule,
    widgets: &PostponeWidgets,
    field_name: &str,
) -> Result<(), String> {
    rule.enabled = widgets.enabled.is_active();
    rule.duration = parse_duration(&widgets.duration, field_name)?;
    rule.max_postponements = widgets
        .limit_enabled
        .is_active()
        .then(|| widgets.maximum.value_as_int() as u32);
    Ok(())
}

fn settings_page(groups: &[gtk::Box]) -> gtk::ScrolledWindow {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 24);
    content.set_margin_top(20);
    content.set_margin_bottom(24);
    content.set_margin_start(28);
    content.set_margin_end(28);
    content.set_width_request(680);
    content.set_halign(gtk::Align::Center);
    for group in groups {
        content.append(group);
    }
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&content)
        .build()
}

fn settings_group(title: &str, description: &str, rows: &[gtk::Box]) -> gtk::Box {
    let group = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let heading = gtk::Label::new(Some(title));
    heading.set_halign(gtk::Align::Start);
    heading.add_css_class("settings-section-title");
    let detail = gtk::Label::new(Some(description));
    detail.set_halign(gtk::Align::Start);
    detail.set_wrap(true);
    detail.add_css_class("settings-description");
    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    list.add_css_class("settings-list");
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        }
        list.append(row);
    }
    group.append(&heading);
    group.append(&detail);
    group.append(&list);
    group
}

fn settings_row(title: &str, description: &str, control: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 20);
    row.add_css_class("settings-row");
    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);
    let title = gtk::Label::new(Some(title));
    title.set_halign(gtk::Align::Start);
    title.set_wrap(true);
    let description = gtk::Label::new(Some(description));
    description.set_halign(gtk::Align::Start);
    description.set_wrap(true);
    description.add_css_class("settings-description");
    text.append(&title);
    text.append(&description);
    control.set_halign(gtk::Align::End);
    control.set_valign(gtk::Align::Center);
    row.append(&text);
    row.append(control);
    row
}

fn duration_entry(value: DurationMs) -> gtk::Entry {
    let entry = gtk::Entry::builder()
        .text(value.to_string())
        .width_chars(10)
        .max_width_chars(16)
        .placeholder_text("10m")
        .build();
    entry.set_input_purpose(gtk::InputPurpose::FreeForm);
    entry
}

fn parse_duration(entry: &gtk::Entry, field_name: &str) -> Result<DurationMs, String> {
    entry
        .text()
        .trim()
        .parse()
        .map_err(|_| format!("{field_name} must be a duration such as 20s, 10m, or 1h 15m."))
}

fn save_and_reload(config: &AppConfig, restart_required: bool) -> Result<String, String> {
    breakd_config::save(config).map_err(|error| error.to_string())?;
    let executable = std::env::current_exe().map_err(|error| {
        format!("Configuration was saved, but breakd could not reload: {error}")
    })?;
    let output = Command::new(executable)
        .arg("reload")
        .output()
        .map_err(|error| {
            format!("Configuration was saved, but breakd could not reload: {error}")
        })?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        return Ok(if detail.is_empty() {
            "Saved. Start the breakd daemon to apply the changes.".into()
        } else {
            format!("Saved, but the daemon did not reload: {detail}")
        });
    }
    if restart_required {
        Ok("Saved and reloaded. Restart the daemon to apply idle-detection changes.".into())
    } else {
        Ok("Saved and applied.".into())
    }
}

fn set_status(label: &gtk::Label, message: &str, error: bool) {
    label.set_text(message);
    if error {
        label.add_css_class("settings-error");
    } else {
        label.remove_css_class("settings-error");
    }
}

fn strict_mode_index(value: StrictMode) -> u32 {
    match value {
        StrictMode::Off => 0,
        StrictMode::Delay => 1,
        StrictMode::Entire => 2,
    }
}

fn strict_mode_from_index(value: u32) -> StrictMode {
    match value {
        0 => StrictMode::Off,
        2 => StrictMode::Entire,
        _ => StrictMode::Delay,
    }
}

fn display_mode_index(value: DisplayMode) -> u32 {
    match value {
        DisplayMode::All => 0,
        DisplayMode::Focused => 1,
        DisplayMode::Cursor => 2,
        DisplayMode::Primary => 3,
        DisplayMode::Configured => 4,
        DisplayMode::DimAllContentOne => 5,
    }
}

fn display_mode_from_index(value: u32) -> DisplayMode {
    match value {
        0 => DisplayMode::All,
        1 => DisplayMode::Focused,
        2 => DisplayMode::Cursor,
        3 => DisplayMode::Primary,
        4 => DisplayMode::Configured,
        _ => DisplayMode::DimAllContentOne,
    }
}

fn content_selector_index(value: ContentSelector) -> u32 {
    match value {
        ContentSelector::Focused => 0,
        ContentSelector::Cursor => 1,
        ContentSelector::Primary => 2,
        ContentSelector::Configured => 3,
    }
}

fn content_selector_from_index(value: u32) -> ContentSelector {
    match value {
        1 => ContentSelector::Cursor,
        2 => ContentSelector::Primary,
        3 => ContentSelector::Configured,
        _ => ContentSelector::Focused,
    }
}

fn pointer_mode_index(value: PointerMode) -> u32 {
    match value {
        PointerMode::Controls => 0,
        PointerMode::Block => 1,
        PointerMode::None => 2,
    }
}

fn pointer_mode_from_index(value: u32) -> PointerMode {
    match value {
        0 => PointerMode::Controls,
        2 => PointerMode::None,
        _ => PointerMode::Block,
    }
}

fn install_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        r#"
        .settings-intro-title {
            font-size: 1.35em;
            font-weight: 700;
        }
        .settings-section-title {
            font-size: 1.08em;
            font-weight: 700;
        }
        .settings-description {
            opacity: 0.72;
        }
        .settings-list {
            background-color: alpha(@theme_fg_color, 0.045);
            border: 1px solid alpha(@theme_fg_color, 0.14);
            border-radius: 8px;
        }
        .settings-row {
            padding: 13px 16px;
            min-height: 42px;
        }
        .settings-status-bar {
            border-top: 1px solid alpha(@theme_fg_color, 0.12);
        }
        .settings-status {
            padding: 10px 16px;
            opacity: 0.78;
        }
        .settings-error {
            color: @error_color;
            opacity: 1;
        }
        "#,
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_indexes_round_trip() {
        for value in [
            DisplayMode::All,
            DisplayMode::Focused,
            DisplayMode::Cursor,
            DisplayMode::Primary,
            DisplayMode::Configured,
            DisplayMode::DimAllContentOne,
        ] {
            assert_eq!(display_mode_from_index(display_mode_index(value)), value);
        }
        for value in [StrictMode::Off, StrictMode::Delay, StrictMode::Entire] {
            assert_eq!(strict_mode_from_index(strict_mode_index(value)), value);
        }
    }
}
