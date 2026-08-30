//! The cached artifact: a fully-resolved environment ready to replay.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Variables that must never be forced onto the user's shell. Everything
/// here is either shell machinery, session identity, or a build-sandbox
/// value that only makes sense inside `nix build`.
pub const SKIP_VARS: &[&str] = &[
    "_", "BASH", "BASHOPTS", "EUID", "HOME", "HOSTNAME", "HOSTTYPE", "IFS", "LINENO",
    "LOGNAME", "MACHTYPE", "NIX_BUILD_TOP", "NIX_ENFORCE_PURITY", "NIX_LOG_FD", "OLDPWD",
    "OPTERR", "OSTYPE", "PPID", "PS4", "PWD", "SHELL", "SHELLOPTS", "SHLVL", "TEMP",
    "TEMPDIR", "TERM", "TMP", "TMPDIR", "TZ", "UID", "USER",
];

/// Sentinel values Nix shells emit for variables they expect the hook or
/// the consumer to overwrite; a variable still carrying one after the hook
/// ran must not leak into a real shell.
pub const SENTINELS: &[(&str, &str)] = &[
    ("PATH", "/path-not-set"),
    ("HOME", "/homeless-shelter"),
    ("SHELL", "/noshell"),
];

/// Variables whose devshell value is merged with the caller's value at
/// export time (nix paths first), rather than replacing it. Mirrors
/// nix-direnv's PATH/XDG_DATA_DIRS handling.
pub const MERGE_VARS: &[&str] = &["PATH", "XDG_DATA_DIRS"];

/// A single environment value in the profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Value {
    Literal(String),
    /// `prefix` + the caller's current value of the variable (or nothing if
    /// unset) — how PATH keeps the user's entries behind the nix ones.
    WithCaller { prefix: String, sep: String },
}

/// The cached, fully-resolved environment for one project generation.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EnvProfile {
    /// Content-hash generation this profile was built from.
    pub generation: String,
    /// Project root it belongs to.
    pub root: String,
    pub vars: BTreeMap<String, Value>,
    /// Wall-clock milliseconds the warm run took — surfaced by `doctor`.
    pub warm_ms: u64,
}

impl EnvProfile {
    /// Fold a captured post-hook environment into a profile: drop skipped
    /// and sentinel-valued variables, and split merge-vars around the
    /// caller value they embedded during the warm run.
    pub fn from_captured(
        generation: String,
        root: String,
        captured: BTreeMap<String, String>,
        caller: &BTreeMap<String, String>,
        warm_ms: u64,
    ) -> Self {
        let mut vars = BTreeMap::new();
        for (name, value) in captured {
            if SKIP_VARS.contains(&name.as_str()) || name.starts_with("SIMMER") {
                continue;
            }
            // The harness inherits the caller's environment; a variable it
            // merely passed through is not part of the dev environment.
            if caller.get(&name) == Some(&value) {
                continue;
            }
            if SENTINELS.contains(&(name.as_str(), value.as_str())) {
                continue;
            }
            // Build-sandbox tmpdirs that the hook re-pointed at a warm-time
            // mktemp directory are per-session state, not environment.
            if value.starts_with("/build") || value.starts_with("/tmp/nix-shell.") {
                continue;
            }
            let entry = if MERGE_VARS.contains(&name.as_str()) {
                split_caller_suffix(&value, caller.get(&name).map(String::as_str))
            } else {
                Value::Literal(value)
            };
            vars.insert(name, entry);
        }
        EnvProfile { generation, root, vars, warm_ms }
    }
}

/// If `value` ends with the caller's warm-time value, strip it and record
/// that the caller's *current* value belongs there instead.
fn split_caller_suffix(value: &str, caller: Option<&str>) -> Value {
    if let Some(caller) = caller.filter(|c| !c.is_empty()) {
        for sep in [":", ""] {
            let tail = format!("{sep}{caller}");
            if let Some(prefix) = value.strip_suffix(tail.as_str()) {
                if !prefix.is_empty() || !sep.is_empty() {
                    return Value::WithCaller { prefix: prefix.to_string(), sep: sep.to_string() };
                }
            }
        }
    }
    Value::Literal(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn drops_unchanged_inherited_vars() {
        let caller = cap(&[("EDITOR", "vim"), ("CARGO_HOME", "/old")]);
        let profile = EnvProfile::from_captured(
            "g".into(),
            "/r".into(),
            cap(&[("EDITOR", "vim"), ("CARGO_HOME", "/new"), ("ADDED", "x")]),
            &caller,
            0,
        );
        assert!(!profile.vars.contains_key("EDITOR"));
        assert_eq!(profile.vars["CARGO_HOME"], Value::Literal("/new".into()));
        assert_eq!(profile.vars["ADDED"], Value::Literal("x".into()));
    }

    #[test]
    fn drops_skip_sentinel_and_sandbox_values() {
        let profile = EnvProfile::from_captured(
            "g".into(),
            "/r".into(),
            cap(&[
                ("SHLVL", "3"),
                ("HOME", "/homeless-shelter"),
                ("TMPDIR", "/build"),
                ("NIX_BUILD_TOP", "/tmp/nix-shell.abc123"),
                ("CARGO_HOME", "/real/cargo"),
            ]),
            &BTreeMap::new(),
            0,
        );
        assert_eq!(profile.vars.len(), 1);
        assert_eq!(profile.vars["CARGO_HOME"], Value::Literal("/real/cargo".into()));
    }

    #[test]
    fn path_splits_around_caller_value() {
        let caller = cap(&[("PATH", "/usr/bin:/bin")]);
        let profile = EnvProfile::from_captured(
            "g".into(),
            "/r".into(),
            cap(&[("PATH", "/nix/store/x/bin:/usr/bin:/bin")]),
            &caller,
            0,
        );
        assert_eq!(
            profile.vars["PATH"],
            Value::WithCaller { prefix: "/nix/store/x/bin".into(), sep: ":".into() }
        );
    }

    #[test]
    fn path_without_caller_tail_stays_literal() {
        let caller = cap(&[("PATH", "/usr/bin")]);
        let profile = EnvProfile::from_captured(
            "g".into(),
            "/r".into(),
            cap(&[("PATH", "/nix/store/x/bin")]),
            &caller,
            0,
        );
        assert_eq!(profile.vars["PATH"], Value::Literal("/nix/store/x/bin".into()));
    }
}
