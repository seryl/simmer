//! `simmer.toml` — per-project configuration.
//!
//! ```toml
//! # Files beyond flake.nix/flake.lock whose content defines the env.
//! # Flakes importing their shell from another file are silently stale
//! # without this.
//! watch = ["nix/shells/dev.nix"]
//!
//! # Declarative replacements for imperative shellHook probing: the
//! # command runs once at warm time (post-hook environment), its stdout
//! # becomes an exported variable in the cached profile.
//! [[provider]]
//! env = "CUDA_COMPUTE_CAP"
//! command = "nvidia-smi --query-gpu=compute_cap --format=csv,noheader | head -1 | tr -d ."
//! invalidate = "boot"   # re-warm after reboot; default is "generation"
//! ```

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub watch: Vec<String>,
    #[serde(default, rename = "provider")]
    pub providers: Vec<Provider>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    /// Exported variable the provider's stdout populates.
    pub env: String,
    /// Shell command run inside the warm harness after the shellHook.
    pub command: String,
    #[serde(default)]
    pub invalidate: Invalidate,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Invalidate {
    /// Re-run only when the generation (watched files) changes.
    #[default]
    Generation,
    /// Also re-run after a reboot — for hardware probes.
    Boot,
}

impl Config {
    /// Load `simmer.toml` from `root`; a missing file is an empty config,
    /// a malformed one is an error the user needs to see.
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join("simmer.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn boot_scoped(&self) -> bool {
        self.providers.iter().any(|p| p.invalidate == Invalidate::Boot)
    }
}

/// Kernel boot id — empty string where unavailable (non-Linux), which
/// simply disables boot-scoped invalidation there.
pub fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_watch_and_providers() {
        let config: Config = toml::from_str(
            r#"
            watch = ["nix/shell.nix"]
            [[provider]]
            env = "GPU_CAP"
            command = "echo 90"
            invalidate = "boot"
            [[provider]]
            env = "PLAIN"
            command = "echo x"
            "#,
        )
        .unwrap();
        assert_eq!(config.watch, vec!["nix/shell.nix"]);
        assert_eq!(config.providers.len(), 2);
        assert!(config.boot_scoped());
        assert_eq!(config.providers[1].invalidate, Invalidate::Generation);
    }

    #[test]
    fn unknown_keys_are_errors_not_silent_typos() {
        assert!(toml::from_str::<Config>("wach = []").is_err());
    }
}
