use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum HistoryKind {
    Transcription,
    TranslateToEnglish,
    TranslateToJapanese,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: String,
    pub created_at_ms: i64,
    pub kind: HistoryKind,
    pub source_text: String,
    pub japanese_text: String,
    pub english_text: String,
}

#[derive(Debug, Clone)]
pub struct HistoryStore {
    dir: PathBuf,
}

impl HistoryStore {
    pub fn new() -> Result<Self> {
        let dirs = ProjectDirs::from("dev", "akirakono", "simple-openai-transcribe")
            .context("failed to derive data directory")?;
        Ok(Self {
            dir: dirs.data_local_dir().join("history"),
        })
    }

    pub fn load_all(&self) -> Result<Vec<HistoryEntry>> {
        if !self.dir.exists() {
            return Ok(Vec::new());
        }

        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.dir)
            .with_context(|| format!("failed to read {}", self.dir.display()))?
        {
            let entry =
                entry.with_context(|| format!("failed to iterate {}", self.dir.display()))?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }

            let body = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let item: HistoryEntry = serde_json::from_str(&body)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            entries.push(item);
        }

        entries.sort_by(|left, right| right.created_at_ms.cmp(&left.created_at_ms));
        Ok(entries)
    }

    pub fn save(&self, entry: &HistoryEntry) -> Result<()> {
        self.ensure_dir()?;
        let body =
            serde_json::to_string_pretty(entry).context("failed to serialize history entry")?;
        fs::write(self.entry_path(&entry.id), body)
            .with_context(|| format!("failed to write history entry {}", entry.id))
    }

    pub fn create_entry(
        &self,
        kind: HistoryKind,
        source_text: String,
        japanese_text: String,
        english_text: String,
    ) -> Result<HistoryEntry> {
        let entry = HistoryEntry {
            id: format!("{}-{}", now_unix_ms(), short_discriminator()),
            created_at_ms: now_unix_ms(),
            kind,
            source_text,
            japanese_text,
            english_text,
        };
        self.save(&entry)?;
        Ok(entry)
    }

    pub fn update(&self, entry: &HistoryEntry) -> Result<()> {
        self.save(entry)
    }
    fn ensure_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("failed to create {}", self.dir.display()))
    }

    fn entry_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn short_discriminator() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos() % 10_000)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_ids_are_non_empty() {
        let id = format!("{}-{}", now_unix_ms(), short_discriminator());
        assert!(!id.is_empty());
    }
}
