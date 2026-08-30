//! Compile an [`EnvProfile`] into native shell code, and back out again.
//!
//! Session state (which vars we set, what they held before) travels inside
//! the shell itself as `__SIMMER_STATE`, so unloading needs no cache
//! access and survives daemon restarts, cache GCs, and copied shells.

use crate::profile::{EnvProfile, Value};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shell {
    Fish,
    Bash,
    Zsh,
}

impl Shell {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "fish" => Some(Shell::Fish),
            "bash" => Some(Shell::Bash),
            "zsh" => Some(Shell::Zsh),
            _ => None,
        }
    }
}

/// What the active environment did to the shell, carried in
/// `__SIMMER_STATE` as JSON.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionState {
    pub generation: String,
    pub root: String,
    /// Pre-apply values of the variables we overwrote.
    pub saved: BTreeMap<String, String>,
    /// Every variable the apply script set.
    pub set: Vec<String>,
}

pub const STATE_VAR: &str = "__SIMMER_STATE";
pub const GEN_VAR: &str = "SIMMER_GEN";

/// The caller's environment with any active simmer session backed out —
/// the baseline both unload scripts and fresh applies must be computed
/// against.
pub fn effective_env(
    current: &BTreeMap<String, String>,
    state: Option<&SessionState>,
) -> BTreeMap<String, String> {
    let mut env = current.clone();
    if let Some(state) = state {
        for name in &state.set {
            env.remove(name);
        }
        for (name, value) in &state.saved {
            env.insert(name.clone(), value.clone());
        }
        env.remove(STATE_VAR);
        env.remove(GEN_VAR);
    }
    env
}

pub fn read_state(current: &BTreeMap<String, String>) -> Option<SessionState> {
    serde_json::from_str(current.get(STATE_VAR)?).ok()
}

/// Emit code that applies `profile` on top of `base` (an [`effective_env`]
/// result). If `prior` is an active session, its teardown is emitted first
/// so generation switches are atomic from the shell's point of view.
pub fn apply_script(
    shell: Shell,
    profile: &EnvProfile,
    base: &BTreeMap<String, String>,
    prior: Option<&SessionState>,
) -> String {
    let mut out = String::new();
    if let Some(prior) = prior {
        unload_into(&mut out, shell, prior, base);
    }

    let mut state = SessionState {
        generation: profile.generation.clone(),
        root: profile.root.clone(),
        ..Default::default()
    };
    for (name, value) in &profile.vars {
        if let Some(old) = base.get(name) {
            state.saved.insert(name.clone(), old.clone());
        }
        state.set.push(name.clone());
        let resolved = match value {
            Value::Literal(v) => v.clone(),
            Value::WithCaller { prefix, sep } => match base.get(name) {
                Some(cur) if !cur.is_empty() => format!("{prefix}{sep}{cur}"),
                _ => prefix.clone(),
            },
        };
        emit_set(&mut out, shell, name, &resolved);
    }
    emit_set(&mut out, shell, GEN_VAR, &profile.generation);
    emit_set(&mut out, shell, STATE_VAR, &serde_json::to_string(&state).unwrap_or_default());
    out
}

/// Emit code that tears down `state`, restoring the pre-apply world.
pub fn unload_script(shell: Shell, state: &SessionState, base: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    unload_into(&mut out, shell, state, base);
    out
}

fn unload_into(out: &mut String, shell: Shell, state: &SessionState, base: &BTreeMap<String, String>) {
    for name in &state.set {
        match base.get(name) {
            Some(value) => emit_set(out, shell, name, value),
            None => emit_erase(out, shell, name),
        }
    }
    emit_erase(out, shell, GEN_VAR);
    emit_erase(out, shell, STATE_VAR);
}

fn emit_set(out: &mut String, shell: Shell, name: &str, value: &str) {
    match shell {
        // PATH is a list in fish; splitting on ':' here keeps it one entry
        // per element instead of a single colon-joined blob.
        Shell::Fish if name == "PATH" => {
            let parts: Vec<String> =
                value.split(':').filter(|p| !p.is_empty()).map(quote_fish).collect();
            let _ = writeln!(out, "set -gx PATH {}", parts.join(" "));
        }
        Shell::Fish => {
            let _ = writeln!(out, "set -gx {name} {}", quote_fish(value));
        }
        Shell::Bash | Shell::Zsh => {
            let _ = writeln!(out, "export {name}={}", crate::warm::quote_single(value));
        }
    }
}

fn emit_erase(out: &mut String, shell: Shell, name: &str) {
    match shell {
        Shell::Fish => {
            let _ = writeln!(out, "set -e {name} 2>/dev/null");
        }
        Shell::Bash | Shell::Zsh => {
            let _ = writeln!(out, "unset {name} 2>/dev/null");
        }
    }
}

fn quote_fish(value: &str) -> String {
    format!("'{}'", value.replace('\\', r"\\").replace('\'', r"\'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Value;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn profile() -> EnvProfile {
        EnvProfile {
            generation: "gen1".into(),
            root: "/proj".into(),
            vars: BTreeMap::from([
                ("FOO".to_string(), Value::Literal("has 'quotes'\nand lines".into())),
                (
                    "PATH".to_string(),
                    Value::WithCaller { prefix: "/nix/bin".into(), sep: ":".into() },
                ),
            ]),
            warm_ms: 0,
        }
    }

    #[test]
    fn fish_apply_splits_path_and_quotes_values() {
        let base = env(&[("PATH", "/usr/bin"), ("FOO", "old")]);
        let script = apply_script(Shell::Fish, &profile(), &base, None);
        assert!(script.contains("set -gx PATH '/nix/bin' '/usr/bin'"));
        assert!(script.contains("set -gx FOO 'has \\'quotes\\'\nand lines'"));
        assert!(script.contains("set -gx SIMMER_GEN 'gen1'"));
    }

    #[test]
    fn bash_apply_merges_path_with_base() {
        let base = env(&[("PATH", "/usr/bin")]);
        let script = apply_script(Shell::Bash, &profile(), &base, None);
        assert!(script.contains("export PATH='/nix/bin:/usr/bin'"));
    }

    #[test]
    fn unload_restores_saved_and_erases_new() {
        let base = env(&[("PATH", "/usr/bin"), ("FOO", "old")]);
        let script = apply_script(Shell::Fish, &profile(), &base, None);
        let state: SessionState = serde_json::from_str(
            script.lines().last().unwrap().split_once("__SIMMER_STATE ").unwrap().1
                .trim_matches('\'').replace(r"\'", "'").replace(r"\\", "\\").as_str(),
        )
        .unwrap();
        let unload = unload_script(Shell::Fish, &state, &base);
        assert!(unload.contains("set -gx FOO 'old'"));
        assert!(unload.contains("set -gx PATH '/usr/bin'"));
        assert!(unload.contains("set -e SIMMER_GEN"));
    }

    #[test]
    fn effective_env_backs_out_session() {
        let state = SessionState {
            generation: "g".into(),
            root: "/p".into(),
            saved: BTreeMap::from([("FOO".to_string(), "old".to_string())]),
            set: vec!["FOO".into(), "NEW".into()],
        };
        let current = env(&[("FOO", "nix"), ("NEW", "x"), ("KEEP", "y")]);
        let eff = effective_env(&current, Some(&state));
        assert_eq!(eff.get("FOO").unwrap(), "old");
        assert!(!eff.contains_key("NEW"));
        assert_eq!(eff.get("KEEP").unwrap(), "y");
    }
}
