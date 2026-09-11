//! simmer CLI — the only binary the shell ever talks to.
//!
//! `export` is the hot path: it must stay allocation-light, spawn nothing,
//! and emit nothing when the environment is already current.

mod daemon;
mod direnv_shim;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use simmer_core::{
    cache::Cache,
    compile::{self, Shell},
    project::Project,
    registry, warm,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "simmer", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Emit shell code for the current directory's environment (hot path).
    /// Prints nothing when the shell is already up to date.
    Export {
        /// Target shell: fish, bash, or zsh
        shell: String,
        /// Directory to resolve (defaults to $PWD)
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Never spawn a background warm on cache miss
        #[arg(long)]
        no_warm: bool,
    },
    /// Evaluate the flake and build the cached environment profile now.
    Warm {
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Print the shell hook snippet to add to your shell config.
    Hook { shell: String },
    /// Machine-readable state for the current directory.
    Status {
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Watch every registered project and re-warm in the background the
    /// moment a watched file changes.
    Daemon {
        /// Print a systemd user unit instead of running
        #[arg(long)]
        print_systemd: bool,
    },
    /// Diagnose the load path: per-layer timings, cache state, and
    /// configuration hazards, as JSON.
    Doctor {
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Trust this project: simmer may execute its .envrc / shellHook /
    /// providers. For .envrc projects trust is bound to the file content
    /// (edits revoke it, like direnv).
    Allow {
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Revoke trust; simmer goes inert for this project.
    Deny {
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// direnv-compatible CLI shim (export json/bash/…, exec, allow,
    /// reload, version, hook) — install a `direnv` wrapper that execs
    /// `simmer direnv "$@"` so editors' direnv plugins get simmer envs.
    #[command(trailing_var_arg = true)]
    Direnv {
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

fn main() {
    // Rust ignores SIGPIPE; a consumer closing the pipe early (`head`,
    // an editor reading a prefix) must end us quietly like any CLI, not
    // panic mid-println.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    // Busybox pattern: a `direnv`-named symlink to this binary routes
    // straight into the shim — no shell wrapper, no extra exec.
    let invoked_as = std::env::args().next().unwrap_or_default();
    if std::path::Path::new(&invoked_as)
        .file_name()
        .is_some_and(|n| n == "direnv")
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if let Err(e) = direnv_shim::run(&args) {
            eprintln!("simmer(direnv): {e:#}");
            std::process::exit(1);
        }
        return;
    }

    let cli = Cli::parse();
    let result = match cli.command {
        Cmd::Export { shell, dir, no_warm } => cmd_export(&shell, dir, no_warm),
        Cmd::Warm { dir } => cmd_warm(dir),
        Cmd::Hook { shell } => cmd_hook(&shell),
        Cmd::Status { dir } => cmd_status(dir),
        Cmd::Daemon { print_systemd } if print_systemd => {
            print!("{}", daemon::systemd_unit());
            Ok(())
        }
        Cmd::Daemon { .. } => daemon::run(),
        Cmd::Doctor { dir } => cmd_doctor(dir),
        Cmd::Allow { dir } => cmd_allow(dir, true),
        Cmd::Deny { dir } => cmd_allow(dir, false),
        Cmd::Direnv { args } => direnv_shim::run(&args),
    };
    if let Err(e) = result {
        // The hook pipes stdout into `source`; errors must stay on stderr
        // and never poison the shell.
        eprintln!("simmer: {e:#}");
        std::process::exit(1);
    }
}

fn resolve_dir(dir: Option<PathBuf>) -> Result<PathBuf> {
    match dir {
        Some(d) => Ok(d),
        None => std::env::current_dir().context("resolving current directory"),
    }
}

fn cmd_export(shell: &str, dir: Option<PathBuf>, no_warm: bool) -> Result<()> {
    let shell = Shell::parse(shell).context("shell must be fish, bash, or zsh")?;
    let current: BTreeMap<String, String> = std::env::vars().collect();
    let state = compile::read_state(&current);
    let dir = resolve_dir(dir)?;

    let Some(mut project) = Project::discover(&dir) else {
        // Left the project: tear the session down against what the shell
        // held before we touched it.
        if let Some(state) = &state {
            let base = compile::effective_env(&current, Some(state));
            print!("{}", compile::unload_script(shell, state, &base));
        }
        return Ok(());
    };

    let cache = Cache::for_project(&project)?;
    // Trust gate: an untrusted project is not simmer's to manage — no
    // code execution, no application; any lingering session unloads.
    // Until `simmer allow`, direnv (or nothing) owns it, which is also
    // the coexistence story: allow is the per-project migration switch.
    // Silent (this runs on every prompt); `simmer doctor` explains it.
    if !cache.trusted(&project) {
        if let Some(state) = &state {
            let base = compile::effective_env(&current, Some(state));
            print!("{}", compile::unload_script(shell, state, &base));
        }
        return Ok(());
    }
    project.extend_watches(cache.dynamic_watches());
    project.extend_boot_scoped(cache.boot_scoped());
    let generation = project.generation()?;
    let cached_generation = cache.cached_generation();

    let profile_stale = cached_generation.as_deref() != Some(generation.as_str());
    if profile_stale && !no_warm {
        spawn_background_warm(&project)?;
    }

    // Fast path — the common case on every prompt: session already holds
    // the cached generation, so skip the profile parse entirely.
    let already_applied = match (&state, &cached_generation) {
        (Some(state), Some(cached)) => state.generation == *cached,
        _ => false,
    };
    if already_applied {
        return Ok(());
    }

    // Stale-while-revalidate: an out-of-date environment beats a missing
    // one, and beats blocking the prompt on a multi-second nix eval.
    let Some(profile) = cache.load() else { return Ok(()) };
    let base = compile::effective_env(&current, state.as_ref());
    print!("{}", compile::apply_script(shell, &profile, &base, state.as_ref()));
    Ok(())
}

fn spawn_background_warm(project: &Project) -> Result<()> {
    let exe = std::env::current_exe().context("locating simmer binary")?;
    let spawned = std::process::Command::new(exe)
        .args(["warm", "--dir"])
        .arg(&project.root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        Ok(_) => eprintln!(
            "simmer: building environment for {} in the background",
            project.root.display()
        ),
        Err(e) => eprintln!("simmer: could not start background warm: {e}"),
    }
    Ok(())
}

fn cmd_warm(dir: Option<PathBuf>) -> Result<()> {
    let dir = resolve_dir(dir)?;
    let project = Project::discover(&dir).with_context(|| {
        format!("no project root (flake.nix, .envrc, or .simmer.toml) above {}", dir.display())
    })?;
    let cache = Cache::for_project(&project)?;
    // Running `simmer warm` in a project is an explicit human action
    // there — it grants trust, like `direnv allow`.
    cache.allow(&project)?;
    let Some(_lock) = cache.try_lock_warm()? else {
        eprintln!("simmer: another warm is already running for this project");
        return Ok(());
    };
    let profile = warm::warm(&project, &cache)?;
    eprintln!(
        "simmer: warmed {} ({} vars, {}ms, {})",
        project.root.display(),
        profile.vars.len(),
        profile.warm_ms,
        profile.mode
    );
    Ok(())
}

fn cmd_allow(dir: Option<PathBuf>, allow: bool) -> Result<()> {
    let dir = resolve_dir(dir)?;
    let project = Project::discover(&dir).with_context(|| {
        format!("no project root (flake.nix, .envrc, or .simmer.toml) above {}", dir.display())
    })?;
    let cache = Cache::for_project(&project)?;
    if allow {
        cache.allow(&project)?;
        eprintln!("simmer: {} allowed", project.root.display());
    } else {
        cache.deny()?;
        eprintln!("simmer: {} denied — simmer is now inert here", project.root.display());
    }
    Ok(())
}

fn cmd_hook(shell: &str) -> Result<()> {
    let snippet = match Shell::parse(shell).context("shell must be fish, bash, or zsh")? {
        // fish_prompt as well as PWD: a daemon warm finishing while the
        // shell sits in the project gets picked up at the next prompt,
        // and the no-op costs well under a millisecond.
        Shell::Fish => concat!(
            "function __simmer_hook --on-variable PWD --on-event fish_prompt --description 'simmer environment switcher'\n",
            "    simmer export fish | source\n",
            "end\n",
            "__simmer_hook\n",
        ),
        Shell::Bash => concat!(
            "__simmer_hook() { eval \"$(simmer export bash)\"; }\n",
            "PROMPT_COMMAND=\"__simmer_hook${PROMPT_COMMAND:+;$PROMPT_COMMAND}\"\n",
        ),
        Shell::Zsh => concat!(
            "__simmer_hook() { eval \"$(simmer export zsh)\"; }\n",
            "autoload -U add-zsh-hook\n",
            "add-zsh-hook chpwd __simmer_hook\n",
            "__simmer_hook\n",
        ),
    };
    print!("{snippet}");
    Ok(())
}

fn cmd_status(dir: Option<PathBuf>) -> Result<()> {
    let dir = resolve_dir(dir)?;
    let current: BTreeMap<String, String> = std::env::vars().collect();
    let state = compile::read_state(&current);
    let status = match Project::discover(&dir) {
        None => serde_json::json!({ "project": null, "session": state.map(|s| s.generation) }),
        Some(mut project) => {
            let cache = Cache::for_project(&project)?;
            project.extend_watches(cache.dynamic_watches());
            project.extend_boot_scoped(cache.boot_scoped());
            let generation = project.generation()?;
            let profile = cache.load();
            serde_json::json!({
                "project": project.root,
                "trusted": cache.trusted(&project),
                "mode": profile.as_ref().map(|p| p.mode.clone()),
                "watched": project.watched,
                "generation": generation,
                "cached_generation": profile.as_ref().map(|p| p.generation.clone()),
                "fresh": profile.as_ref().map(|p| p.generation == generation).unwrap_or(false),
                "cached_vars": profile.as_ref().map(|p| p.vars.len()),
                "last_warm_ms": profile.as_ref().map(|p| p.warm_ms),
                "session": state.map(|s| s.generation),
            })
        }
    };
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
}

fn cmd_doctor(dir: Option<PathBuf>) -> Result<()> {
    let dir = resolve_dir(dir)?;
    let current: BTreeMap<String, String> = std::env::vars().collect();
    let mut warnings: Vec<String> = Vec::new();

    let Some(mut project) = Project::discover(&dir) else {
        println!(
            "{}",
            serde_json::json!({ "project": null, "warnings": ["no project root (flake.nix, .envrc, or .simmer.toml) found upward"] })
        );
        return Ok(());
    };

    let cache = Cache::for_project(&project)?;
    let trusted = cache.trusted(&project);
    if !trusted {
        warnings.push(
            "project is not trusted — simmer is inert here until `simmer allow` (or `simmer warm`); \
             if direnv manages this project, that is the correct state until you migrate"
                .to_string(),
        );
    }
    if trusted && project.envrc_path().is_some() && std::env::var_os("DIRENV_DIR").is_some() {
        warnings.push(
            "both simmer (allowed) and direnv appear active — pick one: `simmer deny` here, \
             or remove the direnv hook for this project"
                .to_string(),
        );
    }
    if !registry::list().contains(&project.root) {
        warnings.push("project not yet registered (run `simmer warm` once) — the daemon will not pre-warm it".to_string());
    }

    project.extend_watches(cache.dynamic_watches());
    project.extend_boot_scoped(cache.boot_scoped());
    let generation = project.generation()?;
    let profile = cache.load();

    // Hot-path cost, measured for real: hash watched files, load the
    // profile, compile the apply script.
    let t = Instant::now();
    let mut compiled_bytes = 0;
    for _ in 0..10 {
        let g = project.generation()?;
        if let Some(p) = cache.load() {
            let base = compile::effective_env(&current, None);
            compiled_bytes = compile::apply_script(Shell::Fish, &p, &base, None).len();
            let _ = g;
        }
    }
    let export_us = t.elapsed().as_micros() / 10;

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "project": project.root,
            "trusted": trusted,
            "watched": project.watched,
            "providers": project.config.providers.iter().map(|p| &p.env).collect::<Vec<_>>(),
            "generation": generation,
            "fresh": profile.as_ref().map(|p| p.generation == generation).unwrap_or(false),
            "mode": profile.as_ref().map(|p| p.mode.clone()),
            "native_installable": warm::native_installable(&project),
            "last_warm_ms": profile.as_ref().map(|p| p.warm_ms),
            "export_cost_us": export_us,
            "apply_script_bytes": compiled_bytes,
            "registered_projects": registry::list().len(),
            "warnings": warnings,
        }))?
    );
    Ok(())
}
