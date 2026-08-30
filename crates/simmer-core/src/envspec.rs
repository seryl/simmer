//! Parser for `nix print-dev-env --json` output.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;

/// One variable from the dev environment, tagged the way Nix emits it.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", content = "value", rename_all = "lowercase")]
pub enum Var {
    /// `export NAME=...` — the only kind that reaches the user's shell.
    Exported(String),
    /// Plain bash variable (BASH, IFS, ...) — meaningful only inside the
    /// warm harness, never exported.
    Var(String),
    Array(Vec<String>),
    Associative(BTreeMap<String, String>),
}

/// Structured dev environment as reported by `nix print-dev-env --json`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EnvSpec {
    #[serde(default)]
    pub variables: BTreeMap<String, Var>,
    #[serde(default)]
    pub bash_functions: BTreeMap<String, String>,
}

impl EnvSpec {
    pub fn parse(json: &str) -> Result<Self> {
        serde_json::from_str(json).context("parsing `nix print-dev-env --json` output")
    }

    /// Exported string variables only — what a shell could actually adopt.
    pub fn exported(&self) -> impl Iterator<Item = (&str, &str)> {
        self.variables.iter().filter_map(|(k, v)| match v {
            Var::Exported(val) => Some((k.as_str(), val.as_str())),
            _ => None,
        })
    }

    /// The shellHook body, if the environment declares one.
    pub fn shell_hook(&self) -> Option<&str> {
        match self.variables.get("shellHook") {
            Some(Var::Exported(v) | Var::Var(v)) => Some(v),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_var_kinds() {
        let spec = EnvSpec::parse(
            r#"{
              "bashFunctions": {"runHook": " eval \"$shellHook\" "},
              "variables": {
                "PATH": {"type": "exported", "value": "/path-not-set"},
                "IFS": {"type": "var", "value": " \t\n"},
                "outputs": {"type": "array", "value": ["out"]},
                "attrs": {"type": "associative", "value": {"k": "v"}}
              }
            }"#,
        )
        .unwrap();
        assert_eq!(spec.exported().collect::<Vec<_>>(), vec![("PATH", "/path-not-set")]);
        assert_eq!(spec.bash_functions.len(), 1);
        assert_eq!(spec.variables.len(), 4);
    }

    #[test]
    fn shell_hook_found_when_exported() {
        let spec = EnvSpec::parse(
            r#"{"variables": {"shellHook": {"type": "exported", "value": "echo hi"}}}"#,
        )
        .unwrap();
        assert_eq!(spec.shell_hook(), Some("echo hi"));
    }
}
