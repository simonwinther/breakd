use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use breakd_core::{Command, DurationMs, OverlaySpec, Response};
use clap::{Parser, Subcommand};
use serde_json::Value;
use tokio::time::{Duration, Instant, sleep};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "breakd", version, about = "Wayland-native break reminder")]
struct Arguments {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Run the long-lived scheduler daemon.
    Daemon,
    /// Show scheduler status.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Pause indefinitely or for a duration such as 30m.
    Pause { duration: Option<String> },
    /// Resume a paused schedule.
    Resume,
    #[command(hide = true)]
    ResumeBreak,
    /// Reset cadence and counters.
    Reset,
    /// Skip the active break when policy allows it.
    Skip,
    /// Postpone the active break when policy allows it.
    Postpone,
    /// Start a mini break immediately.
    Mini,
    /// Start a long break immediately.
    Long,
    /// Start a rest break immediately.
    Rest,
    /// Toggle paused/running state.
    Toggle,
    /// Reload and validate the configuration.
    Reload,
    /// List outputs and stable identifiers.
    Outputs {
        #[arg(long)]
        json: bool,
    },
    /// Report protocols and desktop integrations.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Host, join, or inspect a synchronized co-op room.
    Coop {
        #[command(subcommand)]
        command: CoopCommand,
    },
    /// Open the graphical configuration window.
    Settings,
    #[command(hide = true)]
    Overlay,
    /// Print the default TOML configuration.
    ExampleConfig,
}

#[derive(Debug, Subcommand)]
enum CoopCommand {
    /// Create a room and print a secret invite.
    Host {
        /// Public ws:// or wss:// relay endpoint.
        #[arg(long)]
        relay: String,
    },
    /// Join a room using the invite printed by its host.
    Join { invite: String },
    /// Disconnect and return to a fresh local schedule.
    Leave,
    /// Show relay connection and room state.
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    init_logging();
    match execute(Arguments::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("breakd: {error:#}");
            ExitCode::from(2)
        }
    }
}

async fn execute(arguments: Arguments) -> Result<()> {
    match arguments.command {
        CliCommand::Daemon => breakd_daemon::run().await,
        CliCommand::Overlay => run_overlay(),
        CliCommand::ExampleConfig => {
            print!("{}", breakd_config::example_toml());
            Ok(())
        }
        CliCommand::Status { json } => send(Command::Status, json).await,
        CliCommand::Pause { duration } => {
            let duration = duration
                .map(|value| value.parse::<DurationMs>())
                .transpose()
                .context("invalid pause duration")?;
            send(Command::Pause { duration }, false).await
        }
        CliCommand::Resume => send(Command::Resume, false).await,
        CliCommand::ResumeBreak => send(Command::ResumeBreak, false).await,
        CliCommand::Reset => send(Command::Reset, false).await,
        CliCommand::Skip => send(Command::Skip, false).await,
        CliCommand::Postpone => send(Command::Postpone, false).await,
        CliCommand::Mini => send(Command::Mini, false).await,
        CliCommand::Long => send(Command::Long, false).await,
        CliCommand::Rest => send(Command::Rest, false).await,
        CliCommand::Toggle => send(Command::Toggle, false).await,
        CliCommand::Reload => send(Command::Reload, false).await,
        CliCommand::Outputs { json } => send(Command::Outputs, json).await,
        CliCommand::Doctor { json } => send(Command::Doctor, json).await,
        CliCommand::Coop { command } => match command {
            CoopCommand::Host { relay } => host_coop_room(relay).await,
            CoopCommand::Join { invite } => join_coop_room(invite).await,
            CoopCommand::Leave => send(Command::CoopLeave, false).await,
            CoopCommand::Status { json } => send(Command::CoopStatus, json).await,
        },
        CliCommand::Settings => breakd_settings::run().map_err(anyhow::Error::msg),
    }
}

async fn send(command: Command, json: bool) -> Result<()> {
    let response = request(command).await?;
    print_response(response, json)
}

async fn request(command: Command) -> Result<Response> {
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        breakd_ipc::request(breakd_config::socket_path(), command),
    )
    .await
    .context("daemon response timed out")??;
    if !response.ok {
        bail!(response.message);
    }
    Ok(response)
}

fn print_response(response: Response, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&response.data.unwrap_or(serde_json::Value::Null))?
        );
    } else if let Some(data) = response.data {
        println!("{}", response.message);
        println!("{}", serde_json::to_string_pretty(&data)?);
    } else {
        println!("{}", response.message);
    }
    Ok(())
}

async fn host_coop_room(relay_url: String) -> Result<()> {
    let response = request(Command::CoopHost { relay_url }).await?;
    let invite = response
        .data
        .as_ref()
        .and_then(|data| data.get("invite"))
        .and_then(Value::as_str)
        .context("daemon did not return a co-op invite")?
        .to_owned();
    let status = wait_for_coop(CoopReadiness::Host).await?;
    println!("co-op room created");
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "invite": invite,
            "status": status,
        }))?
    );
    Ok(())
}

async fn join_coop_room(invite: String) -> Result<()> {
    request(Command::CoopJoin { invite }).await?;
    let status = wait_for_coop(CoopReadiness::Guest).await?;
    println!("joined co-op room");
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum CoopReadiness {
    Host,
    Guest,
}

async fn wait_for_coop(expected: CoopReadiness) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let failed_status = loop {
        sleep(Duration::from_millis(100)).await;
        let response = request(Command::CoopStatus).await?;
        let status = response.data.unwrap_or(Value::Null);
        if coop_is_ready(&status, expected) {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            break status;
        }
    };
    let detail = failed_status
        .get("last_error")
        .and_then(Value::as_str)
        .map(|error| format!(": {error}"))
        .unwrap_or_default();
    bail!(
        "co-op connection did not become ready within 5 seconds{detail}; the daemon will keep retrying"
    )
}

fn coop_is_ready(status: &Value, expected: CoopReadiness) -> bool {
    let connected = status
        .get("connected")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let host_present = status
        .get("host_present")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let role_ready = match expected {
        CoopReadiness::Host => true,
        CoopReadiness::Guest => status
            .get("following_host")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    };
    connected && host_present && role_ready
}

fn run_overlay() -> Result<()> {
    let encoded =
        std::env::var("BREAKD_OVERLAY_SPEC").context("BREAKD_OVERLAY_SPEC is unavailable")?;
    let spec: OverlaySpec =
        serde_json::from_str(&encoded).context("invalid overlay specification")?;
    let config = breakd_config::load().context("load overlay configuration")?;
    breakd_wayland_overlay::run(spec, config).map_err(anyhow::Error::msg)
}

fn init_logging() {
    let logging = breakd_config::load().ok().map(|config| config.logging);
    let level = logging
        .as_ref()
        .map(|logging| logging.level.as_str())
        .unwrap_or("info");
    let format = logging
        .as_ref()
        .map(|logging| logging.format.as_str())
        .unwrap_or("compact");
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("breakd={level},warn")));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);
    if format == "json" {
        builder.json().init();
    } else {
        builder.compact().init();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coop_join_waits_for_the_first_host_snapshot() {
        let connecting = serde_json::json!({
            "connected": true,
            "host_present": true,
            "following_host": false,
        });
        let following = serde_json::json!({
            "connected": true,
            "host_present": true,
            "following_host": true,
        });

        assert!(!coop_is_ready(&connecting, CoopReadiness::Guest));
        assert!(coop_is_ready(&following, CoopReadiness::Guest));
        assert!(coop_is_ready(&connecting, CoopReadiness::Host));
    }
}
