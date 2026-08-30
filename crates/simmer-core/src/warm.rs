//! The warm path: evaluate the flake once, run the shellHook once, cache
//! the resulting environment.

use crate::cache::Cache;
use crate::envspec::EnvSpec;
use crate::profile::{EnvProfile, SENTINELS, SKIP_VARS};
use crate::project::Project;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::process::Command;
use std::time::Instant;

/// The only caller variables visible to the warm harness. Warming must
/// produce the same profile whether it ran from a login shell, a cron
/// job, or inside another devshell — an inherited environment would both
/// leak session variables into the profile and mask spec variables that
/// happen to match it.
const HARNESS_KEEP: &[&str] = &[
    "HOME", "LANG", "LOGNAME", "PATH", "TERM", "TZ", "USER", "XDG_RUNTIME_DIR",
];

/// Build (or rebuild) the profile for `project`'s current generation.
pub fn warm(project: &Project, cache: &Cache) -> Result<EnvProfile> {
    let started = Instant::now();
    let generation = project.generation()?;

    let output = Command::new("nix")
        .args(["print-dev-env", "--json", "--profile"])
        .arg(cache.gcroot_path())
        .current_dir(&project.root)
        .output()
        .context("running `nix print-dev-env` — is nix on PATH?")?;
    if !output.status.success() {
        bail!(
            "nix print-dev-env failed in {}:\n{}",
            project.root.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let spec = EnvSpec::parse(&String::from_utf8_lossy(&output.stdout))?;

    let caller: BTreeMap<String, String> = std::env::vars()
        .filter(|(k, _)| HARNESS_KEEP.contains(&k.as_str()) || k.starts_with("LC_"))
        .collect();
    let captured = run_hook_harness(project, &spec, &caller, cache)?;

    let profile = EnvProfile::from_captured(
        generation,
        project.root.display().to_string(),
        captured,
        &caller,
        started.elapsed().as_millis() as u64,
    );
    cache.store(&profile)?;
    // Every warmed project joins the daemon's pre-warm work list.
    crate::registry::register(&project.root)?;
    Ok(profile)
}

/// Apply the spec inside bash, run the shellHook, and capture the
/// environment it leaves behind. This is the only place bash ever runs.
fn run_hook_harness(
    project: &Project,
    spec: &EnvSpec,
    caller: &BTreeMap<String, String>,
    cache: &Cache,
) -> Result<BTreeMap<String, String>> {
    let mut script = String::from("set +e\n");
    for (name, body) in &spec.bash_functions {
        script.push_str(name);
        script.push_str("() {\n");
        script.push_str(body);
        script.push_str("\n}\n");
    }
    for (name, value) in spec.exported() {
        if SKIP_VARS.contains(&name) && name != "PATH" {
            continue;
        }
        if SENTINELS.contains(&(name, value)) {
            continue;
        }
        // The hook must be able to find both nix-provided and system
        // commands, so the caller's PATH rides behind the spec's.
        if name == "PATH" {
            let caller_path = caller.get("PATH").map(String::as_str).unwrap_or_default();
            script.push_str(&format!(
                "export PATH={}\n",
                quote_single(&format!("{value}:{caller_path}"))
            ));
            continue;
        }
        script.push_str(&format!("export {name}={}\n", quote_single(value)));
    }
    if let Some(hook) = spec.shell_hook() {
        script.push_str("eval \"$shellHook\"\n");
        let _ = hook; // the hook runs via $shellHook so `eval` semantics match nix develop
    }
    // Providers run after the hook so they see the finished environment;
    // their stdout lands in the captured env like any other export. A
    // failing or empty provider skips its variable rather than failing
    // the warm.
    for provider in &project.config.providers {
        script.push_str(&format!(
            "if __simmer_out=\"$({cmd})\" && [ -n \"$__simmer_out\" ]; then export {env}=\"$__simmer_out\"; else echo 'simmer: provider {env} produced nothing' >&2; fi\n",
            cmd = provider.command,
            env = provider.env,
        ));
    }
    script.push_str("command env -0\n");

    let script_path = cache.dir.join("warm-harness.bash");
    std::fs::write(&script_path, &script)?;

    let output = Command::new("bash")
        .args(["--noprofile", "--norc"])
        .arg(&script_path)
        .env_clear()
        .envs(caller)
        .current_dir(&project.root)
        .output()
        .context("running warm harness bash")?;
    if !output.status.success() {
        bail!(
            "shellHook harness failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let mut env = BTreeMap::new();
    for entry in output.stdout.split(|b| *b == 0) {
        if entry.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(entry);
        if let Some((name, value)) = text.split_once('=') {
            env.insert(name.to_string(), value.to_string());
        }
    }
    Ok(env)
}

/// POSIX single-quote escaping: safe for any byte sequence except NUL.
pub fn quote_single(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_quoting_survives_quotes_and_newlines() {
        assert_eq!(quote_single("a'b\nc"), "'a'\\''b\nc'");
    }
}
