//! direnv-compatible CLI shim.
//!
//! Editors and tools shell out to `direnv` directly (VSCode/Zed/Emacs
//! plugins run `direnv export json`; launchers use `direnv exec`), which
//! would bypass simmer entirely. The install ships a `direnv`-named
//! wrapper that execs `simmer direnv "$@"`: trusted projects get simmer's
//! cached environment, everything else is transparently delegated to the
//! real direnv named by `$SIMMER_DIRENV_FALLBACK` — so unmigrated
//! projects keep working in editors during the transition, per-project.

use anyhow::{bail, Context, Result};
use simmer_core::{
    cache::Cache,
    compile::{self, Changes, Shell},
    project::Project,
};
use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;

/// direnv version the shim claims. Plugins gate features with
/// `direnv version <min>`; this must stay >= what common plugins require.
const SHIM_VERSION: &str = "2.37.0";

pub fn run(args: &[String]) -> Result<()> {
    let verb = args.first().map(String::as_str).unwrap_or("");
    match verb {
        "export" => export(args.get(1).map(String::as_str).unwrap_or("bash"), args),
        "exec" => exec_in_env(&args[1..], args),
        "allow" | "permit" | "grant" => crate::cmd_allow(dir_arg(args), true),
        "deny" | "block" | "revoke" => crate::cmd_allow(dir_arg(args), false),
        "reload" => crate::cmd_warm(None),
        "status" => status(),
        "version" => version(args.get(1).map(String::as_str)),
        "hook" => {
            crate::cmd_hook(args.get(1).map(String::as_str).unwrap_or("bash"))
        }
        // Anything simmer doesn't model (edit, prune, fetchurl, stdlib…)
        // belongs to the real direnv.
        _ => delegate(args),
    }
}

fn dir_arg(args: &[String]) -> Option<PathBuf> {
    args.get(1).map(PathBuf::from)
}

/// What the shim decided for the current directory.
enum Outcome {
    /// simmer owns it: emit this delta (possibly empty).
    Changes(Changes),
    /// Not simmer's project — hand the whole invocation to real direnv.
    Delegate,
}

fn resolve(dir: &PathBuf) -> Result<Outcome> {
    let current: BTreeMap<String, String> = std::env::vars().collect();
    let state = compile::read_state(&current);

    let Some(mut project) = Project::discover(dir) else {
        let changes = match &state {
            Some(state) => {
                let base = compile::effective_env(&current, Some(state));
                compile::unload_changes(state, &base)
            }
            None => Changes::new(),
        };
        return Ok(Outcome::Changes(changes));
    };

    let cache = Cache::for_project(&project)?;
    if !cache.trusted(&project) {
        return Ok(Outcome::Delegate);
    }

    project.extend_watches(cache.dynamic_watches());
    let generation = project.generation()?;
    let cached_generation = cache.cached_generation();
    if cached_generation.as_deref() != Some(generation.as_str()) {
        crate::spawn_background_warm(&project)?;
    }

    let already_applied = match (&state, &cached_generation) {
        (Some(state), Some(cached)) => state.generation == *cached,
        _ => false,
    };
    if already_applied {
        return Ok(Outcome::Changes(Changes::new()));
    }
    let Some(profile) = cache.load() else {
        return Ok(Outcome::Changes(Changes::new()));
    };
    let base = compile::effective_env(&current, state.as_ref());
    Ok(Outcome::Changes(compile::apply_changes(&profile, &base, state.as_ref())))
}

fn export(format: &str, original_args: &[String]) -> Result<()> {
    let dir = std::env::current_dir().context("resolving current directory")?;
    let changes = match resolve(&dir)? {
        Outcome::Delegate => return delegate(original_args),
        Outcome::Changes(changes) => changes,
    };
    match format {
        // direnv's JSON contract: object of changed vars, null = unset.
        "json" => {
            if !changes.is_empty() {
                println!("{}", serde_json::to_string(&changes)?);
            }
        }
        _ => match Shell::parse(format) {
            Some(shell) => print!("{}", compile::render(shell, &changes)),
            None => return delegate(original_args),
        },
    }
    Ok(())
}

/// `direnv exec DIR CMD [ARGS…]` — run CMD with DIR's environment.
fn exec_in_env(rest: &[String], original_args: &[String]) -> Result<()> {
    let [dir, cmd, cmd_args @ ..] = rest else {
        bail!("usage: direnv exec DIR CMD [...ARGS]");
    };
    let dir = PathBuf::from(dir);
    let changes = match resolve(&dir)? {
        Outcome::Delegate => return delegate(original_args),
        Outcome::Changes(changes) => changes,
    };
    let mut command = std::process::Command::new(cmd);
    command.args(cmd_args).current_dir(&dir);
    for (name, value) in &changes {
        match value {
            Some(value) => {
                command.env(name, value);
            }
            None => {
                command.env_remove(name);
            }
        }
    }
    Err(command.exec()).context(format!("executing {cmd}"))
}

fn status() -> Result<()> {
    let dir = std::env::current_dir()?;
    println!("simmer direnv shim {SHIM_VERSION}");
    match Project::discover(&dir) {
        None => println!("No .envrc or project found"),
        Some(project) => {
            let cache = Cache::for_project(&project)?;
            println!("Found project {}", project.root.display());
            println!(
                "Project is {}",
                if cache.trusted(&project) { "allowed (simmer)" } else { "not allowed by simmer (delegating to direnv)" }
            );
        }
    }
    Ok(())
}

fn version(min: Option<&str>) -> Result<()> {
    println!("{SHIM_VERSION}");
    if let Some(min) = min {
        let parse = |v: &str| -> Vec<u64> {
            v.split('.').map(|p| p.trim().parse().unwrap_or(0)).collect()
        };
        if parse(SHIM_VERSION) < parse(min) {
            bail!("shim claims direnv {SHIM_VERSION}, older than required {min}");
        }
    }
    Ok(())
}

fn delegate(args: &[String]) -> Result<()> {
    let fallback = std::env::var("SIMMER_DIRENV_FALLBACK").ok().filter(|f| {
        // Never re-exec ourselves if the wrapper is misconfigured.
        std::fs::metadata(f).is_ok()
    });
    match fallback {
        Some(direnv) => {
            Err(std::process::Command::new(direnv).args(args).exec())
                .context("delegating to real direnv")
        }
        None => bail!(
            "'direnv {}' is not supported by the simmer shim and no real direnv is \
             available (set SIMMER_DIRENV_FALLBACK)",
            args.first().map(String::as_str).unwrap_or("")
        ),
    }
}
