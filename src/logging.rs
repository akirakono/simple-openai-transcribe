use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use tracing_subscriber::fmt::writer::MakeWriter;

pub fn init_logging() -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "simple_openai_transcribe=info".into());
    let file = open_log_file()?;
    let writer = TeeMakeWriter {
        file: Arc::new(Mutex::new(file)),
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .with_writer(writer)
        .init();

    Ok(())
}

pub fn log_file_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("dev", "akirakono", "simple-openai-transcribe")
        .context("failed to derive state directory")?;
    Ok(dirs
        .state_dir()
        .unwrap_or_else(|| dirs.data_local_dir())
        .join("simple-openai-transcribe.log"))
}

fn open_log_file() -> Result<File> {
    let path = log_file_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))
}

#[derive(Clone)]
struct TeeMakeWriter {
    file: Arc<Mutex<File>>,
}

struct TeeWriter {
    stderr: io::Stderr,
    file: Arc<Mutex<File>>,
}

impl<'a> MakeWriter<'a> for TeeMakeWriter {
    type Writer = TeeWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TeeWriter {
            stderr: io::stderr(),
            file: Arc::clone(&self.file),
        }
    }
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stderr.write_all(buf)?;
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("failed to lock log file"))?;
        file.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stderr.flush()?;
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("failed to lock log file"))?;
        file.flush()
    }
}
