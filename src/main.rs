mod app;
mod audio;
mod config;
mod env;
mod ipc;
mod openai;

use std::process::{Command as ProcessCommand, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "simple-openai-transcribe")]
#[command(about = "Simple GTK4 OpenAI realtime transcription app for Ubuntu Wayland")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, Copy, Subcommand)]
enum Command {
    Daemon,
    Show,
    Start,
    Finish,
    Quit,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "simple_openai_transcribe=info".into()),
        )
        .with_target(false)
        .compact()
        .init();

    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Daemon);

    env::load_dotenv()?;

    match command {
        Command::Daemon => app::run_daemon(),
        Command::Show => send_ipc(ipc::Command::Show),
        Command::Start => send_ipc(ipc::Command::Start),
        Command::Finish => send_ipc(ipc::Command::Finish),
        Command::Quit => send_ipc(ipc::Command::Quit),
    }
}

fn send_ipc(command: ipc::Command) -> Result<()> {
    match ipc::send_command(command) {
        Ok(()) => Ok(()),
        Err(first_error) => {
            if let Err(systemd_error) = ipc::start_systemd_user_service() {
                tracing::warn!("systemd auto-start failed: {systemd_error:#}");
                spawn_local_daemon()
                    .context("failed to spawn local daemon after systemd auto-start failed")?;
            }

            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(150));
                if ipc::send_command(command).is_ok() {
                    return Ok(());
                }
            }

            Err(first_error).context("could not reach daemon after trying to auto-start it")
        }
    }
}

fn spawn_local_daemon() -> Result<()> {
    let current_exe = std::env::current_exe().context("failed to locate current executable")?;
    let child = ProcessCommand::new(current_exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn daemon process")?;

    if child.id() == 0 {
        bail!("daemon process was not assigned a valid PID");
    }

    Ok(())
}
