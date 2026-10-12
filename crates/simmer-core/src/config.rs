//! `.simmer.toml` — the project manifest (preferred over `.envrc`).
//!
//! ```toml
//! # Which environment to load. Defaults to "." when flake.nix exists,
//! # so a bare flake project needs no manifest at all.
//! flake = ".#dev"
//!
//! # Files beyond flake.nix/flake.lock whose content defines the env —
//! # flakes importing their shell from another file are silently stale
//! # without this.
//! watch = ["nix/shells/dev.nix"]
//!
//! # Declarative equivalents of the common .envrc lines:
//! dotenv = [".env.local"]        # KEY=value files, watched automatically
//! path = ["bin", "scripts"]      # PATH prepends, relative to the root
//! [env]
//! DATABASE_URL = "postgres://localhost/dev"
//!
//! # Commands whose stdout becomes an exported variable, run once at warm
//! # time in the finished environment.
//! [[provider]]
//! env = "CUDA_COMPUTE_CAP"
//! command = "nvidia-smi --query-gpu=compute_cap --format=csv,noheader | head -1 | tr -d ."
//! invalidate = "boot"            # re-warm after reboot; default "generation"
//! ```
//!
//! `simmer.toml` (undotted) is accepted as a fallback spelling.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const MANIFEST_NAMES: &[&str] = &[".simmer.toml", "simmer.toml"];

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Flake ref to load (e.g. `.#dev`). Defaults to `.` when the
    /// project has a flake.nix.
    pub flake: Option<String>,
    #[serde(default)]
    pub watch: Vec<String>,
    #[serde(default)]
    pub dotenv: Vec<String>,
    /// Directories to prepend to PATH, relative to the project root.
    #[serde(default)]
    pub path: Vec<String>,
    /// Literal environment variables.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, rename = "provider")]
    pub providers: Vec<Provider>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    /// Exported variable the provider's stdout populates.
    pub env: String,
    /// Shell command run inside the warm harness after the environment
    /// is assembled.
    pub command: String,
    #[serde(default)]
    pub invalidate: Invalidate,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Invalidate {
    /// Re-run only when the generation (watched files) changes.
    #[default]
    Generation,
    /// Also re-run after a reboot — for hardware probes.
    Boot,
}

impl Config {
    /// Load the manifest from `root`; a missing file is an empty config,
    /// a malformed one is an error the user needs to see.
    pub fn load(root: &Path) -> Result<Self> {
        for name in MANIFEST_NAMES {
            let path = root.join(name);
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    return toml::from_str(&text).with_context(|| format!("parsing {}", path.display()));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
            }
        }
        Ok(Config::default())
    }

    pub fn boot_scoped(&self) -> bool {
        self.providers.iter().any(|p| p.invalidate == Invalidate::Boot)
    }
}

/// Kernel boot id — empty string where unavailable (non-Linux), which
/// simply disables boot-scoped invalidation there.
pub fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map(|s| s.trim().to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_manifest() {
        let config: Config = toml::from_str(
            r#"
            flake = ".#dev"
            watch = ["nix/shell.nix"]
            dotenv = [".env"]
            path = ["bin"]
            [env]
            FOO = "bar"
            [[provider]]
            env = "GPU_CAP"
            command = "echo 90"
            invalidate = "boot"
            "#,
        )
        .unwrap();
        assert_eq!(config.flake.as_deref(), Some(".#dev"));
        assert_eq!(config.watch, vec!["nix/shell.nix"]);
        assert_eq!(config.dotenv, vec![".env"]);
        assert_eq!(config.path, vec!["bin"]);
        assert_eq!(config.env["FOO"], "bar");
        assert!(config.boot_scoped());
    }

    #[test]
    fn the_documented_examples_parse() {
        let full: Config = toml::from_str(include_str!("../../../examples/simmer.toml")).unwrap();
        assert_eq!(full.providers.len(), 2);
        assert!(full.boot_scoped());
        let small: Config = toml::from_str(include_str!("../../../examples/flake/.simmer.toml")).unwrap();
        assert_eq!(small.path, vec!["bin"]);
    }

    #[test]
    fn unknown_keys_are_errors_not_silent_typos() {
        assert!(toml::from_str::<Config>("wach = []").is_err());
    }

    #[test]
    fn dotted_manifest_preferred_over_plain() {
        let dir = std::env::temp_dir().join(format!("simmer-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".simmer.toml"), "flake = \".#a\"").unwrap();
        std::fs::write(dir.join("simmer.toml"), "flake = \".#b\"").unwrap();
        assert_eq!(Config::load(&dir).unwrap().flake.as_deref(), Some(".#a"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
