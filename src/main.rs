use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use breakd_core::{Command, DurationMs, OverlaySpec};
use clap::{Parser, Subcommand};
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
    /// Open the graphical configuration window.
    Settings,
    #[command(hide = true)]
    Overlay,
    /// Print the default TOML configuration.
    ExampleConfig,
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
        CliCommand::Settings => breakd_settings::run().map_err(anyhow::Error::msg),
    }
}

async fn send(command: Command, json: bool) -> Result<()> {
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        breakd_ipc::request(breakd_config::socket_path(), command),
    )
    .await
    .context("daemon response timed out")??;
    if !response.ok {
        bail!(response.message);
    }
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
