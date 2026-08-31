//! The warm path: assemble the environment once in a bash harness and
//! cache the captured result.
//!
//! One harness serves every source kind. The shim (`shim.bash`) provides
//! direnv-compatible functions; on top of it the script is either the
//! project's `.envrc` (compat mode) or a few generated lines from
//! `.simmer.toml` (native mode). `nix print-dev-env`'s script output
//! applies variables, defines functions, and ends with
//! `eval "${shellHook:-}"`, so `use_flake` is a single eval — no JSON
//! spec plumbing needed.

use crate::cache::Cache;
use crate::profile::EnvProfile;
use crate::project::Project;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

const SHIM: &str = include_str!("shim.bash");

/// The only caller variables visible to the warm harness. Warming must
/// produce the same profile whether it ran from a login shell, a cron
/// job, or inside another devshell — an inherited environment would both
/// leak session variables into the profile and mask environment variables
/// that happen to match it.
const HARNESS_KEEP: &[&str] = &[
    "HOME", "LANG", "LOGNAME", "PATH", "TERM", "TZ", "USER", "XDG_RUNTIME_DIR",
];

/// Build (or rebuild) the profile for `project`'s current generation.
///
/// Trust is the caller's responsibility: this function executes project
/// code (.envrc, shellHooks, providers) unconditionally.
pub fn warm(project: &Project, cache: &Cache) -> Result<EnvProfile> {
    let started = Instant::now();
    let caller: BTreeMap<String, String> = std::env::vars()
        .filter(|(k, _)| HARNESS_KEEP.contains(&k.as_str()) || k.starts_with("LC_"))
        .collect();

    let (captured, dynamic) = run_harness(project, &caller, cache)?;
    cache.store_dynamic_watches(&dynamic)?;

    // The generation must reflect what the warm actually depended on,
    // including watch_file calls discovered while running it.
    let mut resolved = project.clone();
    resolved.extend_watches(dynamic);
    let generation = resolved.generation()?;

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

fn build_script(project: &Project) -> String {
    let mut script = String::from("set +e\n");
    script.push_str(SHIM);
    script.push('\n');
    if project.envrc_path().is_some() {
        // direnv compat: the .envrc drives the environment via the shim.
        script.push_str(". ./.envrc\n");
    } else {
        let config = &project.config;
        let flake_ref = config
            .flake
            .clone()
            .or_else(|| project.root.join("flake.nix").is_file().then(|| ".".to_string()));
        if let Some(flake_ref) = flake_ref {
            script.push_str(&format!("use_flake {}\n", quote_single(&flake_ref)));
        }
        for (name, value) in &config.env {
            script.push_str(&format!("export {name}={}\n", quote_single(value)));
        }
        for dir in &config.path {
            script.push_str(&format!("PATH_add {}\n", quote_single(dir)));
        }
        for file in &config.dotenv {
            script.push_str(&format!("dotenv {}\n", quote_single(file)));
        }
    }
    // Providers run last so they see the finished environment; their
    // stdout lands in the captured env like any other export. A failing
    // or empty provider skips its variable rather than failing the warm.
    for provider in &project.config.providers {
        script.push_str(&format!(
            "if __simmer_out=\"$({cmd})\" && [ -n \"$__simmer_out\" ]; then export {env}=\"$__simmer_out\"; else echo 'simmer: provider {env} produced nothing' >&2; fi\n",
            cmd = provider.command,
            env = provider.env,
        ));
    }
    script.push_str("command env -0\n");
    script
}

/// Run the harness; capture the environment it leaves behind and the
/// watches it declared. This is the only place bash ever runs.
fn run_harness(
    project: &Project,
    caller: &BTreeMap<String, String>,
    cache: &Cache,
) -> Result<(BTreeMap<String, String>, Vec<PathBuf>)> {
    let script_path = cache.dir.join("warm-harness.bash");
    std::fs::write(&script_path, build_script(project))?;
    let watches_out = cache.dir.join("watches.collect");
    let _ = std::fs::remove_file(&watches_out);
    let layout_dir = cache.dir.join("layout");
    std::fs::create_dir_all(&layout_dir)?;

    let output = Command::new("bash")
        .args(["--noprofile", "--norc"])
        .arg(&script_path)
        .env_clear()
        .envs(caller)
        .env("SIMMER_WATCHES_OUT", &watches_out)
        .env("SIMMER_GCROOT", cache.gcroot_path())
        .env("SIMMER_LAYOUT_DIR", &layout_dir)
        .current_dir(&project.root)
        .output()
        .context("running warm harness bash")?;
    if !output.status.success() {
        bail!(
            "warm harness failed:\n{}",
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

    let mut dynamic: Vec<PathBuf> = std::fs::read_to_string(&watches_out)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        // Paths under the root are stored relative so they dedupe
        // against the static watch set.
        .map(|l| {
            let path = PathBuf::from(l);
            path.strip_prefix(&project.root).map(PathBuf::from).unwrap_or(path)
        })
        .collect();
    dynamic.sort();
    dynamic.dedup();
    // Directories can't be content-hashed into a generation; missing
    // files can (as absence), so only directories are dropped.
    dynamic.retain(|path| {
        let keep = !path.is_dir();
        if !keep {
            eprintln!("simmer: ignoring watch on directory {} (watch files instead)", path.display());
        }
        keep
    });
    Ok((env, dynamic))
}

/// POSIX single-quote escaping: safe for any byte sequence except NUL.
pub fn quote_single(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Project;

    #[test]
    fn single_quoting_survives_quotes_and_newlines() {
        assert_eq!(quote_single("a'b\nc"), "'a'\\''b\nc'");
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("simmer-warm-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn envrc_project_scripts_source_the_envrc() {
        let dir = scratch("envrc");
        std::fs::write(dir.join(".envrc"), "use flake\n").unwrap();
        let script = build_script(&Project::at(&dir).unwrap());
        assert!(script.contains(". ./.envrc"));
        assert!(!script.contains("use_flake '.'"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn manifest_project_scripts_are_generated() {
        let dir = scratch("manifest");
        std::fs::write(dir.join("flake.nix"), "{}").unwrap();
        std::fs::write(
            dir.join(".simmer.toml"),
            "flake = \".#dev\"\npath = [\"bin\"]\ndotenv = [\".env\"]\n[env]\nFOO = \"a'b\"\n",
        )
        .unwrap();
        let script = build_script(&Project::at(&dir).unwrap());
        assert!(script.contains("use_flake '.#dev'"));
        assert!(script.contains("export FOO='a'\\''b'"));
        assert!(script.contains("PATH_add 'bin'"));
        assert!(script.contains("dotenv '.env'"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
