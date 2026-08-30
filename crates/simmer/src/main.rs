//! simmer CLI — the only binary the shell ever talks to.
//!
//! `export` is the hot path: it must stay allocation-light, spawn nothing,
//! and emit nothing when the environment is already current.

mod daemon;

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
}

fn main() {
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

    let Some(project) = Project::discover(&dir) else {
        // Left the project: tear the session down against what the shell
        // held before we touched it.
        if let Some(state) = &state {
            let base = compile::effective_env(&current, Some(state));
            print!("{}", compile::unload_script(shell, state, &base));
        }
        return Ok(());
    };

    // Coexistence guard: a nix-flavored .envrc means direnv owns this
    // project — two systems applying and reverting the same variables
    // corrupts both. Switching a project to simmer = removing its .envrc.
    // Silent (this runs on every prompt); `simmer doctor` explains it.
    if let Ok(envrc) = std::fs::read_to_string(project.root.join(".envrc")) {
        if envrc.contains("use flake") || envrc.contains("use nix") {
            return Ok(());
        }
    }

    let cache = Cache::for_project(&project)?;
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
    let project = Project::discover(&dir)
        .with_context(|| format!("no flake.nix found above {}", dir.display()))?;
    let cache = Cache::for_project(&project)?;
    let Some(_lock) = cache.try_lock_warm()? else {
        eprintln!("simmer: another warm is already running for this project");
        return Ok(());
    };
    let profile = warm::warm(&project, &cache)?;
    eprintln!(
        "simmer: warmed {} ({} vars, {}ms)",
        project.root.display(),
        profile.vars.len(),
        profile.warm_ms
    );
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
        Some(project) => {
            let cache = Cache::for_project(&project)?;
            let generation = project.generation()?;
            let profile = cache.load();
            serde_json::json!({
                "project": project.root,
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

    let Some(project) = Project::discover(&dir) else {
        println!(
            "{}",
            serde_json::json!({ "project": null, "warnings": ["no flake.nix found upward"] })
        );
        return Ok(());
    };

    // direnv managing the same project would fight the hook: two systems
    // applying and reverting the same variables.
    let envrc = project.root.join(".envrc");
    if let Ok(text) = std::fs::read_to_string(&envrc) {
        if text.contains("use flake") || text.contains("use nix") {
            warnings.push(
                ".envrc uses direnv's nix integration — run either direnv or the simmer hook \
                 on this project, not both"
                    .to_string(),
            );
        }
    }
    if !registry::list().iter().any(|r| *r == project.root) {
        warnings.push("project not yet registered (run `simmer warm` once) — the daemon will not pre-warm it".to_string());
    }

    let cache = Cache::for_project(&project)?;
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
            "watched": project.watched,
            "providers": project.config.providers.iter().map(|p| &p.env).collect::<Vec<_>>(),
            "generation": generation,
            "fresh": profile.as_ref().map(|p| p.generation == generation).unwrap_or(false),
            "last_warm_ms": profile.as_ref().map(|p| p.warm_ms),
            "export_cost_us": export_us,
            "apply_script_bytes": compiled_bytes,
            "registered_projects": registry::list().len(),
            "warnings": warnings,
        }))?
    );
    Ok(())
}
