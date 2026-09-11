//! Compile an [`EnvProfile`] into environment *changes*, then render them
//! as native shell code or as data (direnv-compatible JSON).
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

/// An environment delta: `Some(value)` sets, `None` unsets.
pub type Changes = BTreeMap<String, Option<String>>;

/// The caller's environment with any active simmer session backed out —
/// the baseline both unload and fresh applies must be computed against.
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

/// Compute the delta that applies `profile` on top of `base` (an
/// [`effective_env`] result). If `prior` is an active session its
/// teardown is folded in first, so generation switches are atomic.
pub fn apply_changes(
    profile: &EnvProfile,
    base: &BTreeMap<String, String>,
    prior: Option<&SessionState>,
) -> Changes {
    let mut changes = Changes::new();
    if let Some(prior) = prior {
        unload_into(&mut changes, prior, base);
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
        changes.insert(name.clone(), Some(resolved));
    }
    changes.insert(GEN_VAR.into(), Some(profile.generation.clone()));
    changes.insert(
        STATE_VAR.into(),
        Some(serde_json::to_string(&state).unwrap_or_default()),
    );
    changes
}

/// Compute the delta that tears down `state`, restoring the pre-apply
/// world.
pub fn unload_changes(state: &SessionState, base: &BTreeMap<String, String>) -> Changes {
    let mut changes = Changes::new();
    unload_into(&mut changes, state, base);
    changes
}

fn unload_into(changes: &mut Changes, state: &SessionState, base: &BTreeMap<String, String>) {
    for name in &state.set {
        changes.insert(name.clone(), base.get(name).cloned());
    }
    changes.insert(GEN_VAR.into(), None);
    changes.insert(STATE_VAR.into(), None);
}

/// Render a delta as native shell code.
pub fn render(shell: Shell, changes: &Changes) -> String {
    let mut out = String::new();
    for (name, value) in changes {
        match value {
            Some(value) => emit_set(&mut out, shell, name, value),
            None => emit_erase(&mut out, shell, name),
        }
    }
    out
}

/// Emit code that applies `profile` on top of `base`, tearing down
/// `prior` first when present.
pub fn apply_script(
    shell: Shell,
    profile: &EnvProfile,
    base: &BTreeMap<String, String>,
    prior: Option<&SessionState>,
) -> String {
    render(shell, &apply_changes(profile, base, prior))
}

/// Emit code that tears down `state`, restoring the pre-apply world.
pub fn unload_script(shell: Shell, state: &SessionState, base: &BTreeMap<String, String>) -> String {
    render(shell, &unload_changes(state, base))
}

/// Code that shows `text` to a human: stderr only, and only when stderr
/// is a terminal, so captured environments (CI, editor shims, `nix
/// develop -c`) never see it. Emitted on entry, never on the fast path.
pub fn banner_script(shell: Shell, text: &str) -> String {
    match shell {
        Shell::Fish => format!(
            "if isatty stderr\n    printf '%s\\n' {} >&2\nend\n",
            quote_fish(text)
        ),
        Shell::Bash | Shell::Zsh => format!(
            "if [ -t 2 ]; then printf '%s\\n' {} >&2; fi\n",
            crate::warm::quote_single(text)
        ),
    }
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
            mode: String::new(),
            banner: None,
        }
    }

    #[test]
    fn banner_script_targets_a_terminal_on_stderr_only() {
        let fish = banner_script(Shell::Fish, "dev\n[code]\n  it's");
        assert_eq!(fish, "if isatty stderr\n    printf '%s\\n' 'dev\n[code]\n  it\\'s' >&2\nend\n");
        let bash = banner_script(Shell::Bash, "hi");
        assert_eq!(bash, "if [ -t 2 ]; then printf '%s\\n' 'hi' >&2; fi\n");
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
    fn apply_changes_returns_data_for_direnv_json() {
        let base = env(&[("PATH", "/usr/bin"), ("FOO", "old")]);
        let changes = apply_changes(&profile(), &base, None);
        assert_eq!(changes["PATH"], Some("/nix/bin:/usr/bin".to_string()));
        assert_eq!(changes["FOO"], Some("has 'quotes'\nand lines".to_string()));
        assert!(changes[STATE_VAR].is_some());
        // A state round trip carries the saved baseline.
        let state: SessionState =
            serde_json::from_str(changes[STATE_VAR].as_ref().unwrap()).unwrap();
        assert_eq!(state.saved["FOO"], "old");
    }

    #[test]
    fn unload_restores_saved_and_erases_new() {
        let base = env(&[("PATH", "/usr/bin"), ("FOO", "old")]);
        let changes = apply_changes(&profile(), &base, None);
        let state: SessionState =
            serde_json::from_str(changes[STATE_VAR].as_ref().unwrap()).unwrap();
        let unload = unload_changes(&state, &base);
        assert_eq!(unload["FOO"], Some("old".to_string()));
        assert_eq!(unload["PATH"], Some("/usr/bin".to_string()));
        assert_eq!(unload[GEN_VAR], None);
        assert_eq!(unload[STATE_VAR], None);
        let script = render(Shell::Fish, &unload);
        assert!(script.contains("set -gx FOO 'old'"));
        assert!(script.contains("set -e SIMMER_GEN"));
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
