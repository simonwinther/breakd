use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    ffi::OsStr,
    rc::Rc,
    time::{Duration, Instant},
};

use breakd_core::{
    AppConfig, ContentSelector, DisplayMode, KeyboardMode, Layer, OutputInfo, OverlaySpec,
    PointerMode,
};
use breakd_platform_linux::HyprlandClient;
use gtk::{gdk, gio, prelude::*};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode as LayerKeyboardMode, Layer as ShellLayer, LayerShell};

pub fn run(mut spec: OverlaySpec, config: AppConfig) -> Result<(), String> {
    sanitize_preview(&mut spec);
    if std::env::var("XDG_SESSION_TYPE").as_deref() != Ok("wayland") {
        return Err("breakd overlay requires a Wayland session".into());
    }

    let instance = breakd_config::RuntimeInstance::current();
    let application = gtk::Application::builder()
        .application_id(instance.overlay_application_id())
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let startup_error = Rc::new(RefCell::new(None::<String>));
    let error_for_activate = startup_error.clone();
    application.connect_activate(move |application| {
        if !gtk4_layer_shell::is_supported() {
            let message = "wlr-layer-shell is unavailable";
            tracing::error!("{message}");
            error_for_activate.replace(Some(message.into()));
            application.quit();
            return;
        }

        // Keep the overlay process alive for the whole break even if every
        // window is torn down. When the compositor removes or powers off the
        // outputs the overlay is anchored to (for example display power
        // management after the user steps away during a long break), GTK closes
        // those layer-shell windows; without an explicit hold the application
        // would quit as soon as the last window closes, leaving the break still
        // counting down in the daemon with nothing on screen. The hold is
        // released when the countdown ends (the timer below calls `quit`) or
        // when the daemon stops the overlay. Surfaces are recreated by
        // `reconcile` when the outputs come back.
        let hold = application.hold();

        install_css(&config);
        let manager = Rc::new(RefCell::new(OverlayManager::new(
            application.clone(),
            spec.clone(),
            config.clone(),
            instance,
        )));
        manager.borrow_mut().reconcile();

        let display = gdk::Display::default().expect("GTK activation has a display");
        let monitors = display.monitors();
        let manager_for_monitors = manager.clone();
        monitors.connect_items_changed(move |_, _, _, _| {
            manager_for_monitors.borrow_mut().reconcile();
        });

        let manager_for_timer = manager.clone();
        glib::timeout_add_local(Duration::from_millis(200), move || {
            // Own the application hold for the lifetime of the countdown so the
            // process survives losing all of its windows mid-break.
            let _hold = &hold;
            let mut manager = manager_for_timer.borrow_mut();
            if manager.update_countdown() {
                glib::ControlFlow::Continue
            } else {
                manager.application.quit();
                glib::ControlFlow::Break
            }
        });
    });
    application.run_with_args(&[instance.name()]);
    match startup_error.take() {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn sanitize_preview(spec: &mut OverlaySpec) {
    if spec.preview {
        spec.ready_token = None;
        spec.strict_remaining = breakd_core::DurationMs::from_millis(0);
        spec.can_skip = false;
        spec.can_postpone = false;
        spec.manual_resume = false;
        spec.allow_postpone_during_lockout = false;
        spec.inhibit_shortcuts = false;
    }
}

struct SurfaceWidgets {
    monitor: gdk::Monitor,
    content: bool,
    input_owner: bool,
    window: gtk::ApplicationWindow,
    countdown: Option<gtk::Label>,
    resume_prompt: Option<gtk::Label>,
    skip: Option<gtk::Button>,
    postpone: Option<gtk::Button>,
    resume_input_configured: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumePhase {
    Counting,
    Waiting,
    Acknowledging,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OverlayDeadlines {
    countdown: Instant,
    strict: Instant,
}

#[derive(Debug, Default)]
struct OverlayClock {
    deadlines: Cell<Option<OverlayDeadlines>>,
}

impl OverlayClock {
    fn start_once(&self, now: Instant, duration: Duration, strict_remaining: Duration) -> bool {
        if self.deadlines.get().is_some() {
            return false;
        }
        self.deadlines.set(Some(OverlayDeadlines {
            countdown: now + duration,
            strict: now + strict_remaining,
        }));
        true
    }
}

struct OverlayManager {
    application: gtk::Application,
    instance: breakd_config::RuntimeInstance,
    spec: OverlaySpec,
    config: AppConfig,
    clock: Rc<OverlayClock>,
    resume_phase: Rc<Cell<ResumePhase>>,
    surfaces: HashMap<String, SurfaceWidgets>,
}

impl OverlayManager {
    fn new(
        application: gtk::Application,
        spec: OverlaySpec,
        config: AppConfig,
        instance: breakd_config::RuntimeInstance,
    ) -> Self {
        let now = Instant::now();
        let clock = Rc::new(OverlayClock::default());
        if spec.preview {
            clock.start_once(
                now,
                spec.duration.as_duration(),
                spec.strict_remaining.as_duration(),
            );
        }
        let resume_phase = if spec.manual_resume && spec.duration.as_duration().is_zero() {
            ResumePhase::Waiting
        } else {
            ResumePhase::Counting
        };
        Self {
            application,
            instance,
            clock,
            spec,
            config,
            resume_phase: Rc::new(Cell::new(resume_phase)),
            surfaces: HashMap::new(),
        }
    }

    fn reconcile(&mut self) {
        let display = gdk::Display::default().expect("overlay has a GDK display");
        let monitors_model = display.monitors();
        let monitors: Vec<(String, gdk::Monitor)> = (0..monitors_model.n_items())
            .filter_map(|index| monitors_model.item(index))
            .filter_map(|item| item.downcast::<gdk::Monitor>().ok())
            .enumerate()
            .map(|(index, monitor)| (monitor_key(&monitor, index), monitor))
            .collect();
        if monitors.is_empty() {
            tracing::warn!("no GDK outputs are currently available");
            return;
        }

        let (metadata, cursor) = if self.config.hyprland.enabled {
            hyprland_snapshot()
        } else {
            (Vec::new(), None)
        };
        let decision = DisplayDecision::resolve(&self.config, &monitors, &metadata, cursor);
        let desired: HashSet<_> = decision.targets.iter().cloned().collect();
        let current_monitors: HashMap<_, _> = monitors
            .iter()
            .map(|(key, monitor)| (key.as_str(), monitor))
            .collect();

        self.surfaces.retain(|key, surface| {
            if desired.contains(key)
                && current_monitors
                    .get(key.as_str())
                    .is_some_and(|monitor| **monitor == surface.monitor)
                && surface.content == decision.content.contains(key)
                && surface.input_owner == (decision.input_owner.as_deref() == Some(key.as_str()))
            {
                true
            } else {
                surface.window.close();
                false
            }
        });

        for (key, monitor) in monitors {
            if !desired.contains(&key) || self.surfaces.contains_key(&key) {
                continue;
            }
            let content = decision.content.contains(&key);
            let input_owner = decision.input_owner.as_deref() == Some(key.as_str());
            let surface = self.create_surface(&monitor, content, input_owner);
            self.surfaces.insert(key, surface);
        }
    }

    fn create_surface(
        &self,
        monitor: &gdk::Monitor,
        content: bool,
        input_owner: bool,
    ) -> SurfaceWidgets {
        let window = gtk::ApplicationWindow::builder()
            .application(&self.application)
            .decorated(false)
            .build();
        window.set_default_size(0, 0);
        window.add_css_class("breakd-overlay");
        window.init_layer_shell();
        window.set_namespace(Some(self.instance.overlay_namespace()));
        window.set_monitor(Some(monitor));
        window.set_layer(match self.config.display.layer {
            Layer::Overlay => ShellLayer::Overlay,
            Layer::Top => ShellLayer::Top,
        });
        window.set_exclusive_zone(-1);
        for edge in [Edge::Top, Edge::Right, Edge::Bottom, Edge::Left] {
            window.set_anchor(edge, true);
        }
        let inhibit_shortcuts =
            content && input_owner && !self.spec.preview && self.spec.inhibit_shortcuts;
        window.set_keyboard_mode(if self.spec.preview {
            if content && input_owner {
                LayerKeyboardMode::OnDemand
            } else {
                LayerKeyboardMode::None
            }
        } else if inhibit_shortcuts {
            LayerKeyboardMode::Exclusive
        } else if content && input_owner {
            match self.config.display.keyboard_mode {
                KeyboardMode::None => LayerKeyboardMode::None,
                KeyboardMode::OnDemand => LayerKeyboardMode::OnDemand,
                KeyboardMode::Exclusive => LayerKeyboardMode::Exclusive,
            }
        } else {
            LayerKeyboardMode::None
        });

        let (panel, countdown, resume_prompt, skip, postpone) = if content {
            let geometry = monitor.geometry();
            let widgets = build_content(&window, &self.spec, geometry.width(), geometry.height());
            (
                Some(widgets.0),
                Some(widgets.1),
                Some(widgets.2),
                widgets.3,
                widgets.4,
            )
        } else {
            (None, None, None, None, None)
        };

        let strict_complete = self.spec.strict_remaining.as_duration().is_zero();
        let counting = self.resume_phase.get() == ResumePhase::Counting;
        if let Some(button) = &skip {
            button.set_sensitive(counting && strict_complete);
        }
        if let Some(button) = &postpone {
            button.set_sensitive(
                counting && (strict_complete || self.spec.allow_postpone_during_lockout),
            );
        }
        if content && input_owner && !self.spec.preview {
            configure_action_keys(
                &window,
                skip.as_ref(),
                postpone.as_ref(),
                self.resume_phase.clone(),
            );
        }

        if self.spec.preview {
            configure_preview_input(&window, &self.application, input_owner);
        } else {
            configure_input_region(
                &window,
                panel.as_ref(),
                self.config.display.pointer_mode,
                self.resume_phase.clone(),
            );
            if content {
                let clock = self.clock.clone();
                let duration = self.spec.duration.as_duration();
                let strict_remaining = self.spec.strict_remaining.as_duration();
                let session_id = self.spec.session_id.to_string();
                let ready_token = self.spec.ready_token;
                window.connect_map(move |_| {
                    // The daemon restores time spent launching or retrying
                    // the overlay. Count the supplied durations from the first
                    // visible content surface here as well, once per process.
                    if clock.start_once(Instant::now(), duration, strict_remaining)
                        && let Some(ready_token) = ready_token
                    {
                        invoke_cli_arguments_with_completion(
                            vec![
                                "overlay-ready".into(),
                                session_id.clone(),
                                ready_token.to_string(),
                            ],
                            |_| {},
                        );
                    }
                });
            }
        }
        window.present();
        configure_shortcut_inhibition(&window, inhibit_shortcuts);
        let mut surface = SurfaceWidgets {
            monitor: monitor.clone(),
            content,
            input_owner,
            window,
            countdown,
            resume_prompt,
            skip,
            postpone,
            resume_input_configured: false,
        };
        if self.resume_phase.get() != ResumePhase::Counting {
            surface.enter_manual_resume(self.resume_phase.clone());
        }
        surface
    }

    fn update_countdown(&mut self) -> bool {
        let Some(deadlines) = self.clock.deadlines.get() else {
            // A slow initial map must not consume a real break or make this
            // process exit before the daemon receives its ready signal.
            return true;
        };
        let now = Instant::now();
        let remaining = deadlines.countdown.saturating_duration_since(now);
        let text = format_countdown(remaining);
        let strict_complete = now >= deadlines.strict;
        let enter_manual_resume = remaining.is_zero()
            && self.spec.manual_resume
            && self.resume_phase.get() == ResumePhase::Counting;
        if enter_manual_resume {
            self.resume_phase.set(ResumePhase::Waiting);
        }
        let counting = self.resume_phase.get() == ResumePhase::Counting;
        for surface in self.surfaces.values_mut() {
            if let Some(label) = &surface.countdown {
                label.set_text(&text);
            }
            if let Some(button) = &surface.skip {
                button.set_sensitive(counting && strict_complete);
            }
            if let Some(button) = &surface.postpone {
                button.set_sensitive(
                    counting && (strict_complete || self.spec.allow_postpone_during_lockout),
                );
            }
            if enter_manual_resume {
                surface.enter_manual_resume(self.resume_phase.clone());
            }
        }
        self.spec.manual_resume || !remaining.is_zero()
    }
}

impl SurfaceWidgets {
    fn enter_manual_resume(&mut self, resume_phase: Rc<Cell<ResumePhase>>) {
        if self.resume_input_configured {
            return;
        }
        if let Some(countdown) = &self.countdown {
            countdown.set_text("00:00");
        }
        if let Some(prompt) = &self.resume_prompt {
            prompt.set_visible(true);
        }
        for button in [&self.skip, &self.postpone].into_iter().flatten() {
            button.set_sensitive(false);
            button.set_visible(false);
        }
        configure_manual_resume_input(&self.window, resume_phase, self.input_owner);
        self.resume_input_configured = true;
    }
}

fn build_content(
    window: &gtk::ApplicationWindow,
    spec: &OverlaySpec,
    monitor_width: i32,
    monitor_height: i32,
) -> (
    gtk::Box,
    gtk::Label,
    gtk::Label,
    Option<gtk::Button>,
    Option<gtk::Button>,
) {
    let layout = panel_layout(monitor_width, monitor_height);
    let panel = gtk::Box::new(gtk::Orientation::Vertical, 18);
    panel.set_halign(gtk::Align::Center);
    panel.set_valign(gtk::Align::Center);
    panel.set_width_request(layout.content_width);
    panel.add_css_class("breakd-panel");
    if layout.compact {
        panel.add_css_class("breakd-panel-compact");
    }

    let title = gtk::Label::new(Some(if spec.preview {
        "Overlay preview"
    } else {
        match spec.kind {
            breakd_core::BreakKind::Mini => "Mini break",
            breakd_core::BreakKind::Long => "Long break",
            breakd_core::BreakKind::Rest => "Rest break",
        }
    }));
    title.set_wrap(true);
    title.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    title.set_justify(gtk::Justification::Center);
    title.add_css_class("breakd-title");
    panel.append(&title);

    let countdown = gtk::Label::new(Some(&format_countdown(spec.duration.as_duration())));
    countdown.add_css_class("breakd-countdown");
    panel.append(&countdown);

    let resume_prompt = gtk::Label::new(Some(if spec.preview {
        "Closes in 8 seconds. Press Escape or click to close."
    } else {
        "Press any key or click to continue"
    }));
    resume_prompt.set_visible(spec.preview);
    resume_prompt.set_wrap(true);
    resume_prompt.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    resume_prompt.set_justify(gtk::Justification::Center);
    resume_prompt.add_css_class("breakd-resume");
    panel.append(&resume_prompt);

    if let Some(message) = &spec.message {
        let message_label = gtk::Label::new(Some(message));
        message_label.set_wrap(true);
        message_label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        message_label.set_max_width_chars(48);
        message_label.set_xalign(0.5);
        message_label.set_justify(gtk::Justification::Center);
        message_label.add_css_class("breakd-message");
        panel.append(&message_label);
    }

    let actions = gtk::Box::new(
        if layout.vertical_actions {
            gtk::Orientation::Vertical
        } else {
            gtk::Orientation::Horizontal
        },
        10,
    );
    actions.set_halign(gtk::Align::Center);
    actions.set_homogeneous(true);
    let skip = (spec.can_skip && !spec.preview).then(|| {
        let button = gtk::Button::with_label("Skip");
        button.add_css_class("breakd-action");
        button.connect_clicked(|_| invoke_cli("skip"));
        actions.append(&button);
        button
    });

    let postpone = (spec.can_postpone && !spec.preview).then(|| {
        let button = gtk::Button::with_label("Postpone");
        button.add_css_class("breakd-action");
        button.connect_clicked(|_| invoke_cli("postpone"));
        actions.append(&button);
        button
    });
    if skip.is_some() || postpone.is_some() {
        panel.append(&actions);
    }
    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.set_halign(gtk::Align::Center);
    frame.set_valign(gtk::Align::Center);
    frame.set_margin_top(16);
    frame.set_margin_bottom(16);
    frame.set_margin_start(16);
    frame.set_margin_end(16);
    frame.append(&panel);
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&frame)
        .build();
    window.set_child(Some(&scroller));
    (panel, countdown, resume_prompt, skip, postpone)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PanelLayout {
    content_width: i32,
    compact: bool,
    vertical_actions: bool,
}

fn panel_layout(monitor_width: i32, monitor_height: i32) -> PanelLayout {
    let compact = monitor_width < 600 || monitor_height < 540;
    // Leave room for the outer margins and CSS padding; width requests apply
    // to the panel content rather than its complete rendered box.
    let horizontal_reserve = if compact { 96 } else { 160 };
    PanelLayout {
        content_width: monitor_width
            .saturating_sub(horizontal_reserve)
            .clamp(120, 560),
        compact,
        vertical_actions: monitor_width < 480,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayAction {
    Skip,
    Postpone,
}

fn action_for_key(
    key: gdk::Key,
    modifiers: gdk::ModifierType,
    phase: ResumePhase,
) -> Option<OverlayAction> {
    if phase != ResumePhase::Counting {
        return None;
    }
    let command_modifiers = gdk::ModifierType::SHIFT_MASK
        | gdk::ModifierType::CONTROL_MASK
        | gdk::ModifierType::ALT_MASK
        | gdk::ModifierType::SUPER_MASK;
    if modifiers.intersects(command_modifiers) {
        return None;
    }

    match key
        .to_unicode()
        .map(|character| character.to_ascii_lowercase())
    {
        Some('s') => Some(OverlayAction::Skip),
        Some('p') => Some(OverlayAction::Postpone),
        _ => None,
    }
}

fn configure_action_keys(
    window: &gtk::ApplicationWindow,
    skip: Option<&gtk::Button>,
    postpone: Option<&gtk::Button>,
    resume_phase: Rc<Cell<ResumePhase>>,
) {
    let skip = skip.map(|button| button.downgrade());
    let postpone = postpone.map(|button| button.downgrade());
    let controller = gtk::EventControllerKey::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.connect_key_pressed(move |_, key, _, modifiers| {
        let Some(action) = action_for_key(key, modifiers, resume_phase.get()) else {
            return glib::Propagation::Proceed;
        };
        let enabled = match action {
            OverlayAction::Skip => skip
                .as_ref()
                .and_then(glib::WeakRef::upgrade)
                .is_some_and(|button| button.is_visible() && button.is_sensitive()),
            OverlayAction::Postpone => postpone
                .as_ref()
                .and_then(glib::WeakRef::upgrade)
                .is_some_and(|button| button.is_visible() && button.is_sensitive()),
        };
        if enabled {
            invoke_cli(match action {
                OverlayAction::Skip => "skip",
                OverlayAction::Postpone => "postpone",
            });
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(controller);
}

fn configure_preview_input(
    window: &gtk::ApplicationWindow,
    application: &gtk::Application,
    input_owner: bool,
) {
    capture_all_input(window);
    let application_for_click = application.clone();
    let click = gtk::GestureClick::new();
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    click.connect_pressed(move |gesture, _, _, _| {
        application_for_click.quit();
        gesture.set_state(gtk::EventSequenceState::Claimed);
    });
    window.add_controller(click);
    if input_owner {
        let application = application.clone();
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                application.quit();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        window.add_controller(keys);
    }
}

fn configure_manual_resume_input(
    window: &gtk::ApplicationWindow,
    resume_phase: Rc<Cell<ResumePhase>>,
    input_owner: bool,
) {
    capture_all_input(window);

    let phase_for_click = resume_phase.clone();
    let click = gtk::GestureClick::new();
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    click.connect_pressed(move |gesture, _, _, _| {
        request_manual_resume(phase_for_click.clone());
        gesture.set_state(gtk::EventSequenceState::Claimed);
    });
    window.add_controller(click);

    if !input_owner {
        return;
    }
    window.set_keyboard_mode(LayerKeyboardMode::Exclusive);
    let controller = gtk::EventControllerKey::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.connect_key_pressed(move |_, _, _, _| {
        request_manual_resume(resume_phase.clone());
        glib::Propagation::Stop
    });
    window.add_controller(controller);
}

fn capture_all_input(window: &gtk::ApplicationWindow) {
    let capture = |window: &gtk::ApplicationWindow| {
        if let Some(surface) = window.surface() {
            surface.set_input_region(None);
        }
    };
    if window.is_realized() {
        capture(window);
    } else {
        window.connect_realize(capture);
    }
}

fn begin_resume(phase: &Cell<ResumePhase>) -> bool {
    if phase.get() != ResumePhase::Waiting {
        return false;
    }
    phase.set(ResumePhase::Acknowledging);
    true
}

fn finish_resume(phase: &Cell<ResumePhase>, succeeded: bool) {
    if !succeeded && phase.get() == ResumePhase::Acknowledging {
        phase.set(ResumePhase::Waiting);
    }
}

fn request_manual_resume(phase: Rc<Cell<ResumePhase>>) {
    if begin_resume(&phase) {
        invoke_cli_with_completion("resume-break", move |succeeded| {
            finish_resume(&phase, succeeded);
        });
    }
}

fn configure_input_region(
    window: &gtk::ApplicationWindow,
    panel: Option<&gtk::Box>,
    mode: PointerMode,
    resume_phase: Rc<Cell<ResumePhase>>,
) {
    if mode == PointerMode::Block {
        return;
    }
    if mode == PointerMode::Controls
        && let Some(panel) = panel
    {
        let panel = panel.clone();
        window.connect_realize(move |window| {
            let Some(surface) = window.surface() else {
                return;
            };
            let window = window.downgrade();
            let panel = panel.downgrade();
            let resume_phase = resume_phase.clone();
            surface.connect_layout(move |surface, _, _| {
                // Showing the resume prompt changes the layout. Preserve full
                // click capture while waiting, even in controls-only mode.
                if resume_phase.get() != ResumePhase::Counting {
                    surface.set_input_region(None);
                    return;
                }
                let (Some(window), Some(panel)) = (window.upgrade(), panel.upgrade()) else {
                    return;
                };
                set_panel_input_region(&window, &panel);
            });
        });
        return;
    }
    window.connect_realize(|window| {
        if let Some(surface) = window.surface() {
            let region = gtk::cairo::Region::create();
            surface.set_input_region(Some(&region));
        }
    });
}

fn configure_shortcut_inhibition(window: &gtk::ApplicationWindow, enabled: bool) {
    if !enabled {
        return;
    }
    if window.is_realized() {
        request_shortcut_inhibition(window);
    } else {
        window.connect_realize(request_shortcut_inhibition);
    }
}

fn request_shortcut_inhibition(window: &gtk::ApplicationWindow) {
    let Some(surface) = window.surface() else {
        tracing::warn!("cannot inhibit shortcuts without a GDK surface");
        return;
    };
    let Ok(toplevel) = surface.dynamic_cast::<gdk::Toplevel>() else {
        tracing::warn!("GDK layer surface does not support shortcut inhibition");
        return;
    };
    toplevel.inhibit_system_shortcuts(None::<&gdk::Event>);
    tracing::info!("requested compositor shortcut inhibition");
}

fn set_panel_input_region(window: &gtk::ApplicationWindow, panel: &gtk::Box) {
    let (Some(surface), Some(bounds)) = (window.surface(), panel.compute_bounds(window)) else {
        return;
    };
    let rectangle = gtk::cairo::RectangleInt::new(
        bounds.x().floor() as i32,
        bounds.y().floor() as i32,
        bounds.width().ceil() as i32,
        bounds.height().ceil() as i32,
    );
    let region = gtk::cairo::Region::create_rectangle(&rectangle);
    surface.set_input_region(Some(&region));
}

fn invoke_cli(command: &str) {
    invoke_cli_with_completion(command, |_| {});
}

fn invoke_cli_with_completion(command: &str, completed: impl FnOnce(bool) + 'static) {
    invoke_cli_arguments_with_completion(vec![command.into()], completed);
}

fn invoke_cli_arguments_with_completion(
    arguments: Vec<String>,
    completed: impl FnOnce(bool) + 'static,
) {
    glib::spawn_future_local(async move {
        let result = match std::env::current_exe() {
            Ok(executable) => {
                let mut argv = vec![executable.as_os_str()];
                argv.extend(arguments.iter().map(OsStr::new));
                match gio::Subprocess::newv(
                    &argv,
                    gio::SubprocessFlags::STDOUT_SILENCE | gio::SubprocessFlags::STDERR_SILENCE,
                ) {
                    // Waiting asynchronously keeps GTK responsive and reaps the
                    // command process. A failed acknowledgement can then be retried.
                    Ok(process) => process
                        .wait_check_future()
                        .await
                        .map_err(|error| error.to_string()),
                    Err(error) => Err(error.to_string()),
                }
            }
            Err(error) => Err(error.to_string()),
        };
        let succeeded = result.is_ok();
        if let Err(error) = result {
            tracing::warn!(%error, ?arguments, "breakd command failed");
        }
        completed(succeeded);
    });
}

fn install_css(config: &AppConfig) {
    let (red, green, blue) = parse_hex_color(&config.display.dim_color).unwrap_or((16, 20, 24));
    let css = format!(
        r#"
        .breakd-overlay {{
            background-color: rgba({red}, {green}, {blue}, {opacity});
            color: #f4f7f8;
        }}
        .breakd-panel {{
            background-color: rgba(20, 24, 28, 0.94);
            border: 1px solid rgba(255, 255, 255, 0.18);
            border-radius: 8px;
            padding: 36px 42px;
        }}
        .breakd-panel-compact {{
            padding: 20px 24px;
        }}
        .breakd-title {{ font-size: 26px; font-weight: 600; }}
        .breakd-countdown {{ font-size: 64px; font-weight: 700; }}
        .breakd-resume {{ font-size: 18px; font-weight: 600; }}
        .breakd-message {{ font-size: 18px; }}
        .breakd-panel-compact .breakd-title {{ font-size: 21px; }}
        .breakd-panel-compact .breakd-countdown {{ font-size: 42px; }}
        .breakd-panel-compact .breakd-resume,
        .breakd-panel-compact .breakd-message {{ font-size: 15px; }}
        .breakd-action {{ min-width: 120px; min-height: 42px; font-size: 16px; }}
        "#,
        opacity = config.display.opacity,
    );
    let provider = gtk::CssProvider::new();
    provider.load_from_string(&css);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

#[derive(Default)]
struct DisplayDecision {
    targets: Vec<String>,
    content: HashSet<String>,
    input_owner: Option<String>,
}

impl DisplayDecision {
    fn resolve(
        config: &AppConfig,
        monitors: &[(String, gdk::Monitor)],
        metadata: &[OutputInfo],
        cursor: Option<(i32, i32)>,
    ) -> Self {
        let all: Vec<_> = monitors.iter().map(|(key, _)| key.clone()).collect();
        let selected =
            select_monitor(config, &all, metadata, cursor).or_else(|| all.first().cloned());
        match config.display.mode {
            DisplayMode::All => Self {
                targets: all.clone(),
                content: all.iter().cloned().collect(),
                input_owner: selected,
            },
            DisplayMode::DimAllContentOne => Self {
                targets: all,
                content: selected.iter().cloned().collect(),
                input_owner: selected,
            },
            _ => Self {
                targets: selected.iter().cloned().collect(),
                content: selected.iter().cloned().collect(),
                input_owner: selected,
            },
        }
    }
}

fn select_monitor(
    config: &AppConfig,
    available: &[String],
    metadata: &[OutputInfo],
    cursor: Option<(i32, i32)>,
) -> Option<String> {
    let selector = match config.display.mode {
        DisplayMode::Focused => ContentSelector::Focused,
        DisplayMode::Cursor => ContentSelector::Cursor,
        DisplayMode::Primary => ContentSelector::Primary,
        DisplayMode::Configured => ContentSelector::Configured,
        DisplayMode::All | DisplayMode::DimAllContentOne => config.display.content_selector,
    };
    resolve_selector(config, selector, available, metadata, cursor).or_else(|| {
        config
            .display
            .fallback
            .iter()
            .find_map(|fallback| resolve_selector(config, *fallback, available, metadata, cursor))
    })
}

fn resolve_selector(
    config: &AppConfig,
    selector: ContentSelector,
    available: &[String],
    metadata: &[OutputInfo],
    cursor: Option<(i32, i32)>,
) -> Option<String> {
    let connector = match selector {
        ContentSelector::Focused => metadata.iter().find(|output| output.focused),
        ContentSelector::Cursor => cursor.and_then(|(cursor_x, cursor_y)| {
            metadata
                .iter()
                .find(|output| output_contains(output, cursor_x, cursor_y))
        }),
        ContentSelector::Primary => config
            .display
            .primary_monitor
            .as_ref()
            .and_then(|selector| {
                metadata
                    .iter()
                    .find(|output| output.identity.matches_selector(selector))
            }),
        ContentSelector::Configured => {
            config
                .display
                .preferred_monitor
                .as_ref()
                .and_then(|selector| {
                    metadata
                        .iter()
                        .find(|output| output.identity.matches_selector(selector))
                })
        }
    }
    .and_then(|output| output.identity.connector.as_ref())?;
    available
        .iter()
        .find(|key| key.as_str() == connector)
        .cloned()
}

fn output_contains(output: &OutputInfo, x: i32, y: i32) -> bool {
    let (width, height) = if matches!(output.transform, 1 | 3 | 5 | 7) {
        (output.height, output.width)
    } else {
        (output.width, output.height)
    };
    let logical_width = (f64::from(width) / output.scale).round() as i32;
    let logical_height = (f64::from(height) / output.scale).round() as i32;
    x >= output.x && x < output.x + logical_width && y >= output.y && y < output.y + logical_height
}

fn hyprland_snapshot() -> (Vec<OutputInfo>, Option<(i32, i32)>) {
    std::thread::spawn(|| {
        let Ok(client) = HyprlandClient::from_env() else {
            return (Vec::new(), None);
        };
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
        else {
            return (Vec::new(), None);
        };
        runtime.block_on(async {
            let outputs = client.outputs().await.unwrap_or_default();
            let cursor = client.cursor_position().await.ok();
            (outputs, cursor)
        })
    })
    .join()
    .unwrap_or_default()
}

fn monitor_key(monitor: &gdk::Monitor, index: usize) -> String {
    monitor
        .connector()
        .map(|value| value.to_string())
        .unwrap_or_else(|| {
            monitor
                .description()
                .map(|value| format!("{}#{index}", value))
                .unwrap_or_else(|| format!("output#{index}"))
        })
}

fn parse_hex_color(value: &str) -> Option<(u8, u8, u8)> {
    let value = value.strip_prefix('#')?;
    (value.len() == 6).then_some((
        u8::from_str_radix(&value[0..2], 16).ok()?,
        u8::from_str_radix(&value[2..4], 16).ok()?,
        u8::from_str_radix(&value[4..6], 16).ok()?,
    ))
}

fn format_countdown(duration: Duration) -> String {
    let seconds = duration
        .as_secs()
        .saturating_add(u64::from(duration.subsec_nanos() > 0));
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use breakd_core::MonitorIdentity;

    use super::*;

    #[test]
    fn countdown_rounds_up() {
        assert_eq!(format_countdown(Duration::from_millis(1_001)), "00:02");
        assert_eq!(format_countdown(Duration::from_secs(65)), "01:05");
    }

    #[test]
    fn panel_layout_stays_inside_small_monitors() {
        let small = panel_layout(360, 480);
        assert_eq!(small.content_width, 264);
        assert!(small.compact);
        assert!(small.vertical_actions);

        let large = panel_layout(1_920, 1_080);
        assert_eq!(large.content_width, 560);
        assert!(!large.compact);
        assert!(!large.vertical_actions);
    }

    #[test]
    fn parses_rgb_color() {
        assert_eq!(parse_hex_color("#101418"), Some((16, 20, 24)));
        assert_eq!(parse_hex_color("invalid"), None);
    }

    #[test]
    fn slow_mapping_does_not_consume_break_or_strict_time() {
        let created = Instant::now();
        let clock = OverlayClock::default();
        assert!(clock.deadlines.get().is_none());

        let mapped = created + Duration::from_secs(120);
        assert!(clock.start_once(mapped, Duration::from_secs(8), Duration::from_secs(3)));
        let deadlines = clock.deadlines.get().unwrap();
        assert_eq!(
            deadlines.countdown.saturating_duration_since(mapped),
            Duration::from_secs(8)
        );
        assert_eq!(
            deadlines.strict.saturating_duration_since(mapped),
            Duration::from_secs(3)
        );

        // Another monitor mapping or remapping must not restart the timer or
        // produce a second daemon-ready acknowledgement.
        assert!(!clock.start_once(
            mapped + Duration::from_secs(2),
            Duration::from_secs(8),
            Duration::from_secs(3)
        ));
        assert_eq!(clock.deadlines.get(), Some(deadlines));
    }

    #[test]
    fn previews_never_enable_daemon_actions_or_inhibition() {
        let mut spec = OverlaySpec {
            session_id: breakd_core::BreakSessionId::new(),
            ready_token: Some(breakd_core::BreakSessionId::new()),
            kind: breakd_core::BreakKind::Mini,
            duration: breakd_core::DurationMs::from_millis(8_000),
            strict_remaining: breakd_core::DurationMs::from_millis(5_000),
            can_skip: true,
            can_postpone: true,
            manual_resume: true,
            allow_postpone_during_lockout: true,
            inhibit_shortcuts: true,
            preview: true,
            message: None,
            socket_path: String::new(),
        };
        sanitize_preview(&mut spec);
        assert!(spec.strict_remaining.as_duration().is_zero());
        assert!(!spec.can_skip && !spec.can_postpone && !spec.manual_resume);
        assert!(!spec.inhibit_shortcuts && !spec.allow_postpone_during_lockout);
        assert!(spec.preview);
        assert_eq!(spec.ready_token, None);
    }

    #[test]
    #[ignore = "requires a display server"]
    fn preview_content_has_no_scheduler_controls() {
        gtk::init().unwrap();
        let spec = OverlaySpec {
            session_id: breakd_core::BreakSessionId::new(),
            ready_token: None,
            kind: breakd_core::BreakKind::Mini,
            duration: breakd_core::DurationMs::from_millis(8_000),
            strict_remaining: breakd_core::DurationMs::from_millis(0),
            can_skip: true,
            can_postpone: true,
            manual_resume: false,
            allow_postpone_during_lockout: false,
            inhibit_shortcuts: false,
            preview: true,
            message: Some("Preview exercise suggestion".into()),
            socket_path: String::new(),
        };
        let window = gtk::ApplicationWindow::builder().build();
        let (panel, countdown, prompt, skip, postpone) =
            build_content(&window, &spec, 1_920, 1_080);
        assert!(skip.is_none() && postpone.is_none());
        assert_eq!(countdown.text(), "00:08");
        // is_visible includes every ancestor. Check the actual presented
        // preview tree so the dismissal hint must be visible on screen.
        window.present();
        assert!(prompt.is_visible());
        assert!(prompt.text().contains("click to close"));
        let title = panel
            .first_child()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        assert_eq!(title.text(), "Overlay preview");
        window.close();
    }

    #[test]
    fn plain_action_keys_are_recognized() {
        assert_eq!(
            action_for_key(
                gdk::Key::s,
                gdk::ModifierType::empty(),
                ResumePhase::Counting
            ),
            Some(OverlayAction::Skip)
        );
        assert_eq!(
            action_for_key(
                gdk::Key::P,
                gdk::ModifierType::LOCK_MASK,
                ResumePhase::Counting
            ),
            Some(OverlayAction::Postpone)
        );
        assert_eq!(
            action_for_key(
                gdk::Key::s,
                gdk::ModifierType::CONTROL_MASK,
                ResumePhase::Counting
            ),
            None
        );
        assert_eq!(
            action_for_key(
                gdk::Key::Escape,
                gdk::ModifierType::empty(),
                ResumePhase::Counting
            ),
            None
        );
    }

    #[test]
    fn action_keys_do_not_intercept_manual_resume() {
        for phase in [ResumePhase::Waiting, ResumePhase::Acknowledging] {
            for key in [gdk::Key::s, gdk::Key::p] {
                assert_eq!(action_for_key(key, gdk::ModifierType::empty(), phase), None);
            }
        }
    }

    #[test]
    fn manual_resume_can_only_be_acknowledged_once() {
        let phase = Cell::new(ResumePhase::Waiting);

        assert!(begin_resume(&phase));
        assert_eq!(phase.get(), ResumePhase::Acknowledging);
        assert!(!begin_resume(&phase));
    }

    #[test]
    fn failed_manual_resume_can_be_retried() {
        let phase = Cell::new(ResumePhase::Waiting);

        assert!(begin_resume(&phase));
        finish_resume(&phase, false);
        assert_eq!(phase.get(), ResumePhase::Waiting);
        assert!(begin_resume(&phase));

        finish_resume(&phase, true);
        assert_eq!(phase.get(), ResumePhase::Acknowledging);
        assert!(!begin_resume(&phase));
    }

    #[test]
    fn cursor_geometry_accounts_for_rotation_and_scale() {
        let output = OutputInfo {
            identity: MonitorIdentity {
                connector: Some("DP-1".into()),
                make: None,
                model: None,
                serial: None,
                description: None,
                physical_mm: None,
            },
            width: 1920,
            height: 1080,
            x: -720,
            y: -100,
            scale: 1.5,
            transform: 3,
            refresh_hz: 60.0,
            focused: false,
            enabled: true,
        };
        assert!(output_contains(&output, -1, 1_000));
        assert!(!output_contains(&output, 1, 1_000));
        assert!(!output_contains(&output, -1, 1_181));
    }

    #[test]
    fn stale_configured_monitor_falls_back_to_focused_output() {
        let mut config = breakd_config::defaults();
        config.display.mode = DisplayMode::Configured;
        config.display.preferred_monitor = Some("connector:missing".into());
        config.display.fallback = vec![ContentSelector::Focused];
        let metadata = vec![OutputInfo {
            identity: MonitorIdentity {
                connector: Some("DP-2".into()),
                make: Some("AOC".into()),
                model: Some("Panel".into()),
                serial: Some("123".into()),
                description: None,
                physical_mm: None,
            },
            width: 1920,
            height: 1080,
            x: 0,
            y: 0,
            scale: 1.0,
            transform: 0,
            refresh_hz: 165.0,
            focused: true,
            enabled: true,
        }];
        assert_eq!(
            select_monitor(&config, &["DP-2".into()], &metadata, None),
            Some("DP-2".into())
        );
    }
}
