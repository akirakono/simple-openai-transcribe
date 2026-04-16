use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum Command {
    Show,
    Start,
    Finish,
    Quit,
    GetState,
}

#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    command: Command,
}

pub fn socket_path() -> Result<PathBuf> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .context("XDG_RUNTIME_DIR is not set; this app expects a user session")?;
    Ok(PathBuf::from(runtime_dir).join("simple-openai-transcribe.sock"))
}

pub fn send_command(command: Command) -> Result<()> {
    let path = socket_path()?;
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("failed to connect {}", path.display()))?;
    let payload =
        serde_json::to_vec(&Envelope { command }).context("failed to encode IPC command")?;
    stream
        .write_all(&payload)
        .context("failed to write IPC payload")?;
    stream
        .write_all(b"\n")
        .context("failed to terminate IPC payload")?;

    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .context("failed to read IPC response")?;

    if response.trim() != "ok" {
        tracing::error!(
            "daemon rejected IPC command {command:?}: {}",
            response.trim()
        );
        bail!("daemon rejected command: {}", response.trim());
    }

    Ok(())
}

pub fn spawn_server(tx: Sender<Command>) -> Result<thread::JoinHandle<()>> {
    let path = socket_path()?;
    if path.exists() {
        let _ = fs::remove_file(&path);
    }

    let listener =
        UnixListener::bind(&path).with_context(|| format!("failed to bind {}", path.display()))?;
    listener
        .set_nonblocking(true)
        .context("failed to set IPC listener nonblocking")?;

    let handle = thread::spawn(move || {
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut request = String::new();
                    let mut reader = match stream.try_clone() {
                        Ok(clone) => BufReader::new(clone),
                        Err(error) => {
                            tracing::error!("failed to clone IPC stream: {error}");
                            continue;
                        }
                    };

                    match reader.read_line(&mut request) {
                        Ok(_) => match serde_json::from_str::<Envelope>(&request) {
                            Ok(envelope) => match tx.send(envelope.command) {
                                Ok(()) => {
                                    let _ = stream.write_all(b"ok\n");
                                }
                                Err(error) => {
                                    tracing::error!(
                                        "failed to forward IPC command to UI thread: {error}"
                                    );
                                    let _ = stream.write_all(b"error\n");
                                }
                            },
                            Err(error) => {
                                tracing::error!("invalid IPC request: {error}");
                                let _ = stream.write_all(b"invalid\n");
                            }
                        },
                        Err(error) => {
                            tracing::error!("failed to read IPC request: {error}");
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(error) => {
                    tracing::error!("IPC listener error: {error}");
                    break;
                }
            }
        }
    });

    Ok(handle)
}

pub fn cleanup_socket() {
    if let Ok(path) = socket_path() {
        let _ = fs::remove_file(path);
    }
}

pub fn start_systemd_user_service() -> Result<()> {
    let status = ProcessCommand::new("systemctl")
        .arg("--user")
        .arg("start")
        .arg("simple-openai-transcribe.service")
        .status()
        .context("failed to invoke systemctl --user start")?;

    if !status.success() {
        bail!("systemctl --user start simple-openai-transcribe.service failed");
    }

    Ok(())
}
