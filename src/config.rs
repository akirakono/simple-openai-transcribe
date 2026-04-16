use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub terms: Vec<String>,
    pub vad_threshold: f32,
    pub vad_silence_ms: u32,
    pub copy_on_finish: bool,
    pub auto_show_on_start: bool,
    pub window_width: i32,
    pub window_height: i32,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            terms: Vec::new(),
            vad_threshold: 0.45,
            vad_silence_ms: 900,
            copy_on_finish: true,
            auto_show_on_start: true,
            window_width: 1100,
            window_height: 760,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn new() -> Result<Self> {
        let dirs = ProjectDirs::from("dev", "akirakono", "simple-openai-transcribe")
            .context("failed to derive config directory")?;
        Ok(Self {
            path: dirs.config_dir().join("config.toml"),
        })
    }

    pub fn load(&self) -> Result<AppConfig> {
        if !self.path.exists() {
            return Ok(AppConfig::default());
        }

        let body = fs::read_to_string(&self.path)
            .with_context(|| format!("failed to read {}", self.path.display()))?;
        toml::from_str(&body).context("failed to parse config.toml")
    }

    pub fn save(&self, config: &AppConfig) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let body = toml::to_string_pretty(config).context("failed to serialize config")?;
        fs::write(&self.path, body)
            .with_context(|| format!("failed to write {}", self.path.display()))
    }
}

pub fn parse_terms(raw: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for line in raw.lines() {
        let normalized = line.trim();
        if normalized.is_empty() {
            continue;
        }
        if !terms.iter().any(|existing| existing == normalized) {
            terms.push(normalized.to_string());
        }
    }
    terms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_terms_deduplicates_and_skips_blanks() {
        let terms = parse_terms("  OpenAI\n\nfoo\nOpenAI\n bar ");
        assert_eq!(terms, vec!["OpenAI", "foo", "bar"]);
    }
}
