use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub type AudioChunk = Vec<u8>;

pub struct AudioCapture {
    child: tokio::process::Child,
    pump_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
}

pub async fn start_capture(tx: mpsc::UnboundedSender<AudioChunk>) -> Result<AudioCapture> {
    let mut child = tokio::process::Command::new("pw-record")
        .arg("--rate=24000")
        .arg("--channels=1")
        .arg("--format=s16")
        .arg("--latency=40ms")
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start pw-record")?;

    let mut stdout = child
        .stdout
        .take()
        .context("failed to capture pw-record stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("failed to capture pw-record stderr")?;
    let pump_task = tokio::spawn(async move {
        const CHUNK_BYTES: usize = 960;
        let mut buffer = vec![0_u8; CHUNK_BYTES];

        loop {
            match stdout.read_exact(&mut buffer).await {
                Ok(_) => {
                    if tx.send(buffer.clone()).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    tracing::info!("audio capture ended: {error}");
                    break;
                }
            }
        }
    });
    let stderr_task = tokio::spawn(async move {
        let mut stderr_lines = BufReader::new(stderr).lines();
        loop {
            match stderr_lines.next_line().await {
                Ok(Some(line)) => tracing::warn!("pw-record: {line}"),
                Ok(None) => break,
                Err(error) => {
                    tracing::warn!("failed to read pw-record stderr: {error}");
                    break;
                }
            }
        }
    });

    Ok(AudioCapture {
        child,
        pump_task,
        stderr_task,
    })
}

impl AudioCapture {
    pub async fn stop(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        self.pump_task.abort();
        self.stderr_task.abort();
    }
}
