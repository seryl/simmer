//! direnv-compatible CLI shim.
//!
//! Editors and tools shell out to `direnv` directly (VSCode/Zed/Emacs
//! plugins run `direnv export json`; launchers use `direnv exec`), which
//! would bypass simmer entirely. The install ships a `direnv`-named
//! symlink to the simmer binary (argv[0] routing in main — no wrapper,
//! no extra exec): trusted projects get simmer's cached environment,
//! everything else is transparently delegated to the real direnv
//! (`$SIMMER_DIRENV_FALLBACK`, else the first non-shim direnv on PATH) —
//! so unmigrated projects keep working in editors during the
//! transition, per-project. With no real direnv installed the shim is
//! the only loader: `export` for a project simmer does not manage is
//! quiet, and `exec` runs the command in the caller's environment.
//!
//! Like direnv itself, the shim *waits* when the environment needs
//! rebuilding (editors already wait on `direnv export`), reporting
//! progress on stderr; `SIMMER_BACKGROUND=1` opts into serving the
//! previous environment while a detached rebuild runs.

use anyhow::{bail, Context, Result};
use simmer_core::{
    cache::Cache,
    compile::{self, Changes, Shell},
    project::Project,
};
use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

/// direnv version the shim claims. Plugins gate features with
/// `direnv version <min>`; this must stay >= what common plugins require.
const SHIM_VERSION: &str = "2.37.0";

pub fn run(args: &[String], g: &crate::Global) -> Result<()> {
    let verb = args.first().map(String::as_str).unwrap_or("");
    match verb {
        "export" => export(args.get(1).map(String::as_str).unwrap_or("bash"), args, g),
        "exec" => exec_in_env(args.get(1..).unwrap_or_default(), args, g),
        "allow" | "permit" | "grant" => crate::cmd_allow(dir_arg(args), true),
        "deny" | "block" | "revoke" => crate::cmd_allow(dir_arg(args), false),
        "reload" => crate::cmd_warm(None, true, g),
        "status" => status(),
        "version" => version(args.get(1).map(String::as_str)),
        "hook" => match Shell::parse(args.get(1).map(String::as_str).unwrap_or("bash")) {
            Some(shell) => crate::cmd_hook(shell),
            None => delegate(args),
        },
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
    /// simmer owns it: emit this delta (possibly empty), then report the
    /// rebuild failure if there was one (the previous environment is
    /// still served).
    Changes(Changes, Option<anyhow::Error>),
    /// Not simmer's project — hand the whole invocation to real direnv.
    Delegate,
}

fn resolve(dir: &Path, g: &crate::Global) -> Result<Outcome> {
    let current: BTreeMap<String, String> = std::env::vars().collect();
    let state = compile::read_state(&current);

    let Some(mut project) = Project::locate(dir)? else {
        let changes = match &state {
            Some(state) => {
                let base = compile::effective_env(&current, Some(state));
                compile::unload_changes(state, &base)
            }
            None => Changes::new(),
        };
        return Ok(Outcome::Changes(changes, None));
    };

    let cache = Cache::for_project(&project)?;
    if !cache.trusted(&project) {
        return Ok(Outcome::Delegate);
    }

    project.extend_watches(cache.dynamic_watches());
    project.extend_boot_scoped(cache.boot_scoped());
    let generation = cache.generation_of(&project)?;
    let mut warm_error = None;
    if cache.cached_generation().as_deref() != Some(generation.as_str()) {
        let failed_before = cache.failure().is_some_and(|(g, _)| g == generation);
        if crate::env_flag("SIMMER_BACKGROUND") {
            crate::spawn_background_warm(&project)?;
        } else if !failed_before {
            match crate::foreground_warm(&project, &cache, g, false, false, false) {
                Ok(_) => {}
                Err(e) if simmer_core::supervise::is_interrupt(&e) => {
                    let _ = cache.record_failure(&generation, crate::INTERRUPTED);
                    return Err(e);
                }
                Err(e) => {
                    let _ = cache.record_failure(&generation, &crate::first_line(&e));
                    warm_error = Some(e);
                }
            }
        }
    }
    let cached_generation = cache.cached_generation();

    let already_applied = match (&state, &cached_generation) {
        (Some(state), Some(cached)) => state.generation == *cached,
        _ => false,
    };
    let changes = match cache.load() {
        Some(profile) if !already_applied => {
            let base = compile::effective_env(&current, state.as_ref());
            compile::apply_changes(&profile, &base, state.as_ref())
        }
        _ => Changes::new(),
    };
    Ok(Outcome::Changes(changes, warm_error))
}

fn export(format: &str, original_args: &[String], g: &crate::Global) -> Result<()> {
    let dir = std::env::current_dir().context("resolving current directory")?;
    let (changes, warm_error) = match resolve(&dir, g)? {
        Outcome::Delegate if real_direnv().is_none() => return Ok(()),
        Outcome::Delegate => return delegate(original_args),
        Outcome::Changes(changes, warm_error) => (changes, warm_error),
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
    warm_error.map_or(Ok(()), Err)
}

/// `direnv exec DIR CMD [ARGS…]` — run CMD with DIR's environment.
fn exec_in_env(rest: &[String], original_args: &[String], g: &crate::Global) -> Result<()> {
    let [dir, cmd, cmd_args @ ..] = rest else {
        bail!("usage: direnv exec DIR CMD [...ARGS]");
    };
    let dir = PathBuf::from(dir);
    let changes = match resolve(&dir, g)? {
        Outcome::Delegate if real_direnv().is_none() => Changes::new(),
        Outcome::Delegate => return delegate(original_args),
        // A failed rebuild still runs the command, in the previous
        // environment; the failure is reported on stderr.
        Outcome::Changes(changes, warm_error) => {
            if let Some(e) = warm_error {
                eprintln!("simmer: {e:#}");
            }
            changes
        }
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
                if cache.trusted(&project) {
                    "allowed (simmer)"
                } else {
                    "not allowed by simmer (delegating to direnv)"
                }
            );
        }
    }
    Ok(())
}

fn version(min: Option<&str>) -> Result<()> {
    println!("{SHIM_VERSION}");
    if let Some(min) = min {
        let parse = |v: &str| -> Vec<u64> { v.split('.').map(|p| p.trim().parse().unwrap_or(0)).collect() };
        if parse(SHIM_VERSION) < parse(min) {
            bail!("shim claims direnv {SHIM_VERSION}, older than required {min}");
        }
    }
    Ok(())
}

fn delegate(args: &[String]) -> Result<()> {
    match real_direnv() {
        Some(direnv) => Err(std::process::Command::new(direnv).args(args).exec()).context("delegating to real direnv"),
        None => bail!(
            "'direnv {}' is not supported by the simmer shim and no real direnv is \
             available (set SIMMER_DIRENV_FALLBACK or install direnv)",
            args.first().map(String::as_str).unwrap_or("")
        ),
    }
}

/// The genuine direnv to delegate to: $SIMMER_DIRENV_FALLBACK, or the
/// first `direnv` on PATH that is not this binary (the shim is a
/// `direnv`-named symlink, so a naive lookup would find ourselves).
fn real_direnv() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("SIMMER_DIRENV_FALLBACK") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
    }
    let own = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());
    for dir in std::env::split_paths(&std::env::var_os("PATH")?) {
        let candidate = dir.join("direnv");
        if !candidate.is_file() {
            continue;
        }
        if candidate.canonicalize().ok() == own {
            continue;
        }
        return Some(candidate);
    }
    None
}
