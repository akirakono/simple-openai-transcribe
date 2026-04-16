use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub fn load_dotenv() -> Result<()> {
    let mut loaded = false;

    for candidate in dotenv_candidates() {
        if candidate.exists() {
            dotenvy::from_path_override(&candidate)
                .with_context(|| format!("failed to load {}", candidate.display()))?;
            loaded = true;
            break;
        }
    }

    if !loaded {
        tracing::warn!("no .env file found in expected locations");
    }

    require_api_key()?;
    Ok(())
}

pub fn require_api_key() -> Result<String> {
    let key = env::var("OPENAI_API_KEY").unwrap_or_default();
    if key.trim().is_empty() {
        bail!("OPENAI_API_KEY is not set. Add it to .env before starting the app.");
    }
    Ok(key)
}

fn dotenv_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Ok(current_dir) = env::current_dir() {
        candidates.push(current_dir.join(".env"));
    }

    if let Ok(exe_path) = env::current_exe() {
        for ancestor in exe_path.ancestors() {
            candidates.push(ancestor.join(".env"));
            if is_repo_root(ancestor) {
                break;
            }
        }
    }

    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".env"));
    dedup_paths(candidates)
}

fn is_repo_root(path: &Path) -> bool {
    path.join(".git").exists()
}

fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut unique = Vec::new();
    for path in paths {
        if !unique.iter().any(|existing| existing == &path) {
            unique.push(path);
        }
    }
    unique
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_dir_env_candidate_is_present() {
        let manifest_env = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".env");
        assert!(dotenv_candidates().contains(&manifest_env));
    }
}
