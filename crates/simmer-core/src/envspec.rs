//! `env.json` — the environment a flake devshell exports as data.
//!
//! A devshell derivation that ships `env.json` next to its profile lets
//! the warm path skip bash entirely: build the derivation, read the
//! spec, materialize variables in Rust. Everything a shell hook used to
//! do imperatively (PATH assembly, generated config files, hardware
//! probes) is declared here and applied once at warm time.
//!
//! The schema is deliberately closed (`deny_unknown_fields`): a producer
//! emitting a field this version does not understand fails loudly rather
//! than silently dropping behaviour.

use crate::config::Provider;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const ENV_SPEC_SCHEMA: u32 = 1;
pub const ENV_SPEC_FILE: &str = "env.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EnvSpec {
    pub schema: u32,
    pub name: String,
    pub system: String,
    /// Store path of the profile whose `bin/` is prepended to PATH; the
    /// closure root the GC root pins.
    pub profile: String,
    /// Store paths of the declared packages, in declaration order.
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub vars: BTreeMap<String, Var>,
    #[serde(default)]
    pub commands: Vec<Command>,
    #[serde(default)]
    pub configs: Vec<ConfigFile>,
    #[serde(default)]
    pub providers: Vec<Provider>,
    #[serde(default)]
    pub motd: Option<String>,
    #[serde(default)]
    pub hooks: Hooks,
}

/// One environment variable. `Root` keeps the project root out of the
/// store: the consumer fills it in at materialization time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Var {
    Set {
        value: String,
    },
    /// `value` + `sep` + the caller's current value (PATH-style merge).
    Prefix {
        value: String,
        #[serde(default = "colon")]
        sep: String,
    },
    Unset,
    Root,
}

fn colon() -> String {
    ":".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub name: String,
    #[serde(default)]
    pub help: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    pub run: Run,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Run {
    /// A generated wrapper script at `source` (inside the profile).
    Script { source: String },
    /// A package's main program.
    Exec { program: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    /// Path relative to the project root.
    pub output: String,
    /// Store path holding the generated content.
    pub source: String,
    #[serde(default)]
    pub mode: Mode,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Symlink `output` to `source`.
    #[default]
    Link,
    /// Copy the content (for tools that refuse symlinks).
    Copy,
    /// Copy with `@NAME@` substituted from the finished environment.
    Template,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    /// Legacy escape hatch: bash run once at warm time for side effects
    /// the data model cannot express yet. Its stdout goes to stderr.
    #[serde(default)]
    pub enter: Option<String>,
}

impl EnvSpec {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let spec: EnvSpec = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if spec.schema != ENV_SPEC_SCHEMA {
            bail!(
                "{}: schema {} is not supported (this simmer speaks {})",
                path.display(),
                spec.schema,
                ENV_SPEC_SCHEMA
            );
        }
        Ok(spec)
    }

    pub fn boot_scoped(&self) -> bool {
        self.providers.iter().any(|p| p.invalidate == crate::config::Invalidate::Boot)
    }
}

/// The platform double nix would report for this binary — derived from
/// the compile target, never from a `nix eval`.
pub fn current_system() -> String {
    use std::env::consts::{ARCH, OS};
    let os = match OS {
        "macos" => "darwin",
        other => other,
    };
    format!("{ARCH}-{os}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_minimal_spec_and_defaults() {
        let spec: EnvSpec = serde_json::from_str(
            r#"{"schema":1,"name":"x","system":"x86_64-linux","profile":"/nix/store/p",
                "vars":{"PATH":{"kind":"prefix","value":"/nix/store/p/bin"},
                        "PRJ_ROOT":{"kind":"root"},"FOO":{"kind":"set","value":"1"},"BAR":{"kind":"unset"}},
                "commands":[{"name":"build","run":{"kind":"script","source":"/nix/store/p/bin/build"}}],
                "configs":[{"output":".rustfmt.toml","source":"/nix/store/c"}]}"#,
        )
        .unwrap();
        assert_eq!(spec.vars["PATH"], Var::Prefix { value: "/nix/store/p/bin".into(), sep: ":".into() });
        assert_eq!(spec.configs[0].mode, Mode::Link);
        assert!(spec.hooks.enter.is_none());
        assert!(!spec.boot_scoped());
    }

    #[test]
    fn unknown_fields_are_errors() {
        let bad = r#"{"schema":1,"name":"x","system":"s","profile":"p","bogus":1}"#;
        assert!(serde_json::from_str::<EnvSpec>(bad).is_err());
        let bad_var =
            r#"{"schema":1,"name":"x","system":"s","profile":"p","vars":{"A":{"kind":"set","value":"1","extra":2}}}"#;
        assert!(serde_json::from_str::<EnvSpec>(bad_var).is_err());
    }

    #[test]
    fn the_documented_example_parses() {
        let spec: EnvSpec = serde_json::from_str(include_str!("../../../examples/env.json")).unwrap();
        assert_eq!(spec.schema, ENV_SPEC_SCHEMA);
        assert_eq!(spec.configs.len(), 3);
        assert!(crate::menu::banner(&spec).is_some_and(|b| b.contains("hello")));
    }

    #[test]
    fn current_system_is_a_nix_double() {
        let s = current_system();
        assert!(s.ends_with("-linux") || s.ends_with("-darwin"), "{s}");
    }
}
