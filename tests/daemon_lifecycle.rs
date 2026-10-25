#![cfg(target_os = "linux")]

use std::{fs, path::PathBuf, process::Stdio, time::Duration};

use breakd_core::{AppConfig, BreakSessionId, Command, DurationMs, Response, StrictMode};
use serde_json::Value;
use tokio::{
    process::{Child, Command as ProcessCommand},
    time::{Instant, sleep, timeout},
};

struct TestEnvironment {
    root: PathBuf,
}

impl TestEnvironment {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("breakd-test-{}", BreakSessionId::new()));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("runtime")).unwrap();
        let mut config = breakd_config::defaults();
        config.idle.enabled = false;
        config.hyprland.enabled = false;
        config.tray.enabled = false;
        config.notifications.enabled = false;
        config.schedule.mini.duration = DurationMs::from_millis(60_000);
        let config_path = root.join("config/breakd/config.toml");
        breakd_config::save_to(&config_path, &config).unwrap();
        // Existing installations may retain this removed option. Default
        // startup must still reset the schedule when it is present and true.
        let legacy_config = fs::read_to_string(&config_path).unwrap().replace(
            "start_paused = false",
            "start_paused = false\nrecover_state = true",
        );
        fs::write(config_path, legacy_config).unwrap();
        Self { root }
    }

    fn socket(&self) -> PathBuf {
        self.root.join("runtime/breakd/control.sock")
    }

    fn state_path(&self) -> PathBuf {
        self.root.join("state/breakd/state.json")
    }

    fn config_path(&self) -> PathBuf {
        self.root.join("config/breakd/config.toml")
    }

    fn config(&self) -> AppConfig {
        breakd_config::load_from(self.config_path()).unwrap()
    }

    fn save_config(&self, config: &AppConfig) {
        breakd_config::save_to(&self.config_path(), config).unwrap();
    }

    fn snapshot(&self) -> Value {
        serde_json::from_slice(&fs::read(self.state_path()).unwrap()).unwrap()
    }

    fn binary_command(&self) -> ProcessCommand {
        let mut command = ProcessCommand::new(env!("CARGO_BIN_EXE_breakd"));
        command
            .env("BREAKD_INSTANCE", "production")
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!(
                    "unix:path={}",
                    self.root.join("missing-session-bus").display()
                ),
            )
            .env(
                "DBUS_SYSTEM_BUS_ADDRESS",
                format!(
                    "unix:path={}",
                    self.root.join("missing-system-bus").display()
                ),
            )
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("DISPLAY")
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            .env_remove("BREAKD_PREVIEW_CONFIG")
            .env_remove("BREAKD_OVERLAY_SPEC")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        command
    }

    fn daemon_command(&self) -> ProcessCommand {
        let mut command = self.binary_command();
        command.arg("daemon");
        command
    }

    async fn start(&self) -> Child {
        let mut child = self.daemon_command().spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "daemon exited before IPC became ready"
            );
            if let Ok(Ok(response)) = timeout(
                Duration::from_millis(200),
                breakd_ipc::request(self.socket(), Command::Status),
            )
            .await
                && response.ok
            {
                return child;
            }
            assert!(Instant::now() < deadline, "daemon did not become ready");
            sleep(Duration::from_millis(25)).await;
        }
    }

    async fn request_response(&self, command: Command) -> Response {
        timeout(
            Duration::from_secs(3),
            breakd_ipc::request(self.socket(), command),
        )
        .await
        .expect("daemon request timed out")
        .expect("daemon request failed")
    }

    async fn request(&self, command: Command) -> Value {
        let response = self.request_response(command).await;
        assert!(response.ok, "{}", response.message);
        response.data.unwrap_or(Value::Null)
    }

    async fn stop(&self, child: &mut Child) {
        let status = ProcessCommand::new("kill")
            .arg("-TERM")
            .arg(child.id().expect("daemon is running").to_string())
            .status()
            .await
            .unwrap();
        assert!(status.success());
        let status = timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("daemon did not handle SIGTERM")
            .unwrap();
        assert!(status.success(), "daemon failed during shutdown: {status}");
        assert!(
            !self.socket().exists(),
            "shutdown must remove the IPC socket"
        );
    }
}

impl Drop for TestEnvironment {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn restart_after_stopping_an_active_break_starts_a_fresh_work_interval() {
    let environment = TestEnvironment::new();
    let mut daemon = environment.start().await;
    environment.request(Command::Mini).await;
    let status = environment.request(Command::Status).await;
    assert_eq!(status["state"], "mini-break");
    assert!(status["active_session"].is_string());

    environment.stop(&mut daemon).await;
    let saved: Value =
        serde_json::from_slice(&fs::read(environment.state_path()).unwrap()).unwrap();
    assert_eq!(saved["state"]["state"], "mini-break");

    let mut restarted = environment.start().await;
    let status = environment.request(Command::Status).await;
    assert_eq!(status["state"], "running");
    assert_eq!(status["paused"], false);
    assert!(status["active_session"].is_null());
    let remaining = status["remaining_ms"].as_u64().unwrap();
    assert!(remaining > 9 * 60_000 && remaining <= 10 * 60_000);
    environment.stop(&mut restarted).await;
}

#[tokio::test]
async fn duplicate_start_preserves_the_running_daemons_saved_state() {
    let environment = TestEnvironment::new();
    let mut daemon = environment.start().await;
    environment.request(Command::Pause { duration: None }).await;
    let before = fs::read(environment.state_path()).unwrap();

    let duplicate = timeout(
        Duration::from_secs(5),
        environment.daemon_command().stderr(Stdio::piped()).output(),
    )
    .await
    .expect("duplicate daemon did not exit")
    .unwrap();
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already running"));
    assert_eq!(fs::read(environment.state_path()).unwrap(), before);
    let status = environment.request(Command::Status).await;
    assert_eq!(status["state"], "paused-indefinitely");
    assert_eq!(status["paused"], true);
    environment.stop(&mut daemon).await;
}

#[tokio::test]
async fn reload_updates_a_paused_interval_and_idle_monitor_without_restarting() {
    let environment = TestEnvironment::new();
    let mut config = environment.config();
    let mut daemon = environment.start().await;
    let process_id = daemon.id();
    environment.request(Command::Pause { duration: None }).await;
    let paused_at = environment.snapshot()["state"]["paused_at"].clone();

    config.schedule.mini.interval = DurationMs::from_millis(3 * 60_000);
    config.idle.enabled = true;
    config.idle.reset_after = DurationMs::from_millis(1_000);
    config.idle.respect_idle_inhibitors = true;
    environment.save_config(&config);
    let response = environment.request_response(Command::Reload).await;
    assert!(response.ok, "{}", response.message);
    assert!(
        response
            .message
            .contains("paused work interval now uses the new timing")
    );
    assert!(response.message.contains("Idle detection is unavailable"));

    assert!(daemon.try_wait().unwrap().is_none());
    assert_eq!(daemon.id(), process_id);
    let status = environment.request(Command::Status).await;
    assert_eq!(status["state"], "paused-indefinitely");
    assert_eq!(status["paused"], true);
    assert_eq!(status["remaining_ms"], 3 * 60_000);
    assert_eq!(environment.snapshot()["state"]["paused_at"], paused_at);

    // Disabling the unavailable monitor must also apply live and preserve the
    // existing pause. This exercises replacement after an ended idle worker.
    config.idle.enabled = false;
    environment.save_config(&config);
    let response = environment.request_response(Command::Reload).await;
    assert!(response.ok, "{}", response.message);
    assert!(!response.message.contains("Idle detection is unavailable"));
    assert_eq!(environment.request(Command::Status).await, status);
    assert_eq!(environment.snapshot()["state"]["paused_at"], paused_at);
    assert!(daemon.try_wait().unwrap().is_none());
    assert_eq!(daemon.id(), process_id);

    environment.request(Command::Resume).await;
    let status = environment.request(Command::Status).await;
    assert_eq!(status["state"], "running");
    let remaining = status["remaining_ms"].as_u64().unwrap();
    assert!(remaining > 2 * 60_000 && remaining <= 3 * 60_000);
    environment.stop(&mut daemon).await;
}

#[tokio::test]
async fn reload_preserves_an_active_break_session_and_its_original_duration() {
    let environment = TestEnvironment::new();
    let mut config = environment.config();
    let mut daemon = environment.start().await;
    environment.request(Command::Mini).await;
    let before = environment.snapshot()["state"]["active"].clone();
    assert!(before["session_id"].is_string());
    assert_eq!(
        before["ends_boot_ms"].as_u64().unwrap() - before["started_boot_ms"].as_u64().unwrap(),
        60_000
    );

    config.schedule.mini.interval = DurationMs::from_millis(3 * 60_000);
    config.schedule.mini.duration = DurationMs::from_millis(15_000);
    environment.save_config(&config);
    let response = environment.request_response(Command::Reload).await;
    assert!(response.ok, "{}", response.message);
    assert!(response.message.contains("current break is preserved"));
    assert_eq!(environment.snapshot()["state"]["active"], before);
    let status = environment.request(Command::Status).await;
    assert_eq!(status["state"], "mini-break");
    assert_eq!(status["active_session"], before["session_id"]);
    assert!(status["remaining_ms"].as_u64().unwrap() > 50_000);
    environment.stop(&mut daemon).await;
}

#[tokio::test]
async fn missing_overlay_retries_then_releases_a_strict_unskippable_break_without_credit() {
    let environment = TestEnvironment::new();
    let mut config = environment.config();
    config.strict.mode = StrictMode::Entire;
    config.skip.mini.enabled = false;
    config.postpone.mini.enabled = false;
    environment.save_config(&config);
    let mut daemon = environment.start().await;
    let started = Instant::now();
    environment.request(Command::Mini).await;
    let status = environment.request(Command::Status).await;
    assert_eq!(status["state"], "mini-break");
    assert_eq!(status["can_skip"], false);
    assert_eq!(status["can_postpone"], false);
    assert!(!environment.request_response(Command::Skip).await.ok);
    assert!(!environment.request_response(Command::Reset).await.ok);

    sleep(Duration::from_millis(1_200)).await;
    assert_eq!(
        environment.request(Command::Status).await["state"],
        "mini-break"
    );
    let deadline = started + Duration::from_secs(6);
    let status = loop {
        let status = environment.request(Command::Status).await;
        if status["state"] == "running" {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "overlay recovery did not finish: {status}"
        );
        sleep(Duration::from_millis(50)).await;
    };
    assert!(started.elapsed() >= Duration::from_secs(3));
    assert!(status["active_session"].is_null());
    assert_eq!(status["paused"], false);
    assert_eq!(status["minis_since_long"], 0);
    assert_eq!(status["longs_since_rest"], 0);
    let remaining = status["remaining_ms"].as_u64().unwrap();
    assert!(remaining > 9 * 60_000 && remaining <= 10 * 60_000);
    let context = &environment.snapshot()["state"]["context"];
    assert_eq!(context["minis_since_long"], 0);
    assert_eq!(context["longs_since_rest"], 0);
    assert!(context["pending"].is_null());
    environment.stop(&mut daemon).await;
}

#[tokio::test]
async fn failed_standalone_previews_leave_the_daemon_schedule_and_config_untouched() {
    let environment = TestEnvironment::new();
    let mut daemon = environment.start().await;
    let process_id = daemon.id();
    environment.request(Command::Pause { duration: None }).await;
    let status_before = environment.request(Command::Status).await;
    let state_before = fs::read(environment.state_path()).unwrap();
    let config_before = fs::read(environment.config_path()).unwrap();

    let preview = timeout(
        Duration::from_secs(5),
        environment
            .binary_command()
            .arg("preview")
            .env("GDK_BACKEND", "wayland")
            .env("XDG_SESSION_TYPE", "wayland")
            .env(
                "WAYLAND_DISPLAY",
                environment.root.join("missing-wayland-socket"),
            )
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .expect("standalone overlay preview did not exit without a compositor")
    .unwrap();
    assert!(!preview.status.success());
    assert!(
        preview.status.code().is_some(),
        "standalone preview crashed instead of reporting its startup failure"
    );
    assert!(
        !preview.stderr.is_empty(),
        "failed standalone preview should explain why it could not start"
    );
    assert_eq!(fs::read(environment.state_path()).unwrap(), state_before);
    assert_eq!(fs::read(environment.config_path()).unwrap(), config_before);
    assert_eq!(environment.request(Command::Status).await, status_before);

    let sound = timeout(
        Duration::from_secs(3),
        environment
            .binary_command()
            .args(["preview", "--sound", "invalid"])
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .expect("invalid sound preview did not exit")
    .unwrap();
    assert!(!sound.status.success());
    assert!(String::from_utf8_lossy(&sound.stderr).contains("choose warm-rise"));
    assert_eq!(fs::read(environment.state_path()).unwrap(), state_before);
    assert_eq!(fs::read(environment.config_path()).unwrap(), config_before);
    assert_eq!(environment.request(Command::Status).await, status_before);
    assert!(daemon.try_wait().unwrap().is_none());
    assert_eq!(daemon.id(), process_id);
    environment.stop(&mut daemon).await;
}
