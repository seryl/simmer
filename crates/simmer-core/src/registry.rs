//! Registry of every project simmer has warmed — the daemon's work list.

use crate::cache;
use anyhow::Result;
use std::path::{Path, PathBuf};

fn registry_path() -> Result<PathBuf> {
    Ok(cache::base_dir()?.join("registry.json"))
}

/// Projects known to simmer, sorted. Missing or unreadable registry is an
/// empty list, never an error — the registry is an optimization.
pub fn list() -> Vec<PathBuf> {
    let Ok(path) = registry_path() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    serde_json::from_str::<Vec<PathBuf>>(&text)
        .map(|mut roots| {
            roots.retain(|r| r.join("flake.nix").is_file());
            roots
        })
        .unwrap_or_default()
}

/// Idempotently add `root` to the registry.
pub fn register(root: &Path) -> Result<()> {
    let mut roots = list();
    if roots.iter().any(|r| r == root) {
        return Ok(());
    }
    roots.push(root.to_path_buf());
    roots.sort();
    let path = registry_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&roots)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}
