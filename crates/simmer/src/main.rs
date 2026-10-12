//! simmer CLI — the only binary the shell ever talks to.
//!
//! `export` is the hot path: when the environment is current it must stay
//! allocation-light, spawn nothing, and print nothing. When the
//! environment is stale it *waits* for the rebuild (with live progress on
//! stderr) and then applies the fresh result; `--background` restores the
//! old serve-stale-and-rebuild-behind-the-prompt behaviour.

mod direnv_shim;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use simmer_core::{
    cache::Cache,
    compile::{self, Changes, Shell},
    profile::{EnvProfile, PROFILE_FORMAT},
    progress::{self, Progress},
    project::Project,
    registry, supervise, term, warm,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const AFTER_HELP: &str = "\
Getting started:
  1. Hook your shell (once):  fish:  simmer hook fish | source     (in config.fish)
                              bash:  eval \"$(simmer hook bash)\"    (in ~/.bashrc)
                              zsh:   eval \"$(simmer hook zsh)\"     (in ~/.zshrc)
  2. Allow a project:         cd my-project && simmer allow
  3. Use it:                  the environment loads at the next prompt and
                              follows every edit to flake.nix / .simmer.toml

When the environment is out of date, the prompt waits for the rebuild and
shows its progress. Ctrl-C stops it, and every process it started.

Exit status: 0 on success; a failed step's own exit status; 124 when
--timeout expired; 128+N when stopped by signal N (130 for Ctrl-C).

Environment:
  SIMMER_PROGRESS=MODE   default --progress (auto, live, plain, json, quiet)
  SIMMER_TIMEOUT=10m     default --timeout (0 = never give up)
  SIMMER_BACKGROUND=1    rebuild behind the prompt instead of waiting
  SIMMER_NO_MOTD=1       do not print a project's entry banner
  SIMMER_NO_HINTS=1      do not suggest `simmer allow` in new projects
  NO_COLOR=1             no colours in the progress display

Docs: https://github.com/seryl/simmer";

#[derive(Parser)]
#[command(
    name = "simmer",
    version,
    about = "Instant Nix dev environments: evaluate once, replay natively into fish, bash, or zsh",
    after_help = AFTER_HELP
)]
struct Cli {
    #[command(flatten)]
    global: Global,
    #[command(subcommand)]
    command: Cmd,
}

/// How commands that wait for work report it.
#[derive(Args, Clone)]
struct Global {
    /// Progress display: auto (live on a terminal, plain otherwise), live,
    /// plain, json, or quiet [env: SIMMER_PROGRESS]
    #[arg(long, global = true, value_name = "MODE")]
    progress: Option<String>,
    /// Line-per-event progress without colours or redraws (CI, logs)
    #[arg(long, global = true, conflicts_with = "json")]
    plain: bool,
    /// Progress as one JSON object per line (for `warm`: on stdout)
    #[arg(long, global = true)]
    json: bool,
    /// Only report failures
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    quiet: bool,
    /// Show every output line and nix activity, and a final summary
    #[arg(short, long, global = true)]
    verbose: bool,
    /// Stop waiting after this long, e.g. 90s, 10m, 1h; 0 means no limit
    /// [env: SIMMER_TIMEOUT]
    #[arg(long, global = true, value_name = "DURATION")]
    timeout: Option<String>,
    /// Lines of a failed step's log to print; 0 prints all of it (the full
    /// log is always saved in the project's cache)
    #[arg(long, global = true, value_name = "N", default_value_t = 40)]
    log_lines: usize,
}

#[derive(Clone, Copy, ValueEnum)]
enum ShellArg {
    Fish,
    Bash,
    Zsh,
}

impl From<ShellArg> for Shell {
    fn from(s: ShellArg) -> Shell {
        match s {
            ShellArg::Fish => Shell::Fish,
            ShellArg::Bash => Shell::Bash,
            ShellArg::Zsh => Shell::Zsh,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Print shell code that loads the current directory's environment
    /// (what the hook runs at every prompt). Prints nothing when the shell
    /// is up to date; waits for a rebuild when the environment is stale.
    Export {
        #[arg(value_enum)]
        shell: ShellArg,
        /// Directory to resolve (defaults to the current directory)
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Never rebuild; serve whatever is cached (or nothing)
        #[arg(long)]
        no_warm: bool,
        /// Rebuild in a detached background process and serve the previous
        /// environment meanwhile, instead of waiting [env: SIMMER_BACKGROUND=1]
        #[arg(long, conflicts_with = "no_warm")]
        background: bool,
    },
    /// Build the environment now and wait for it (also allows the project).
    Warm {
        /// Project directory (defaults to the current directory)
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Rebuild even if another simmer finished building the current
        /// version while this one waited for it
        #[arg(long)]
        force: bool,
    },
    /// Print the snippet that hooks simmer into your shell's prompt.
    Hook {
        #[arg(value_enum)]
        shell: ShellArg,
    },
    /// Show the current directory's state as JSON.
    Status {
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Run the optional pre-warm daemon: re-warm allowed projects as soon
    /// as a watched file changes, so the prompt rarely has to wait.
    Daemon {
        /// Print a systemd user unit that runs the daemon, instead of
        /// running it
        #[arg(long)]
        print_systemd: bool,
    },
    /// Diagnose the load path: timings, cache state, and configuration
    /// hazards, as JSON.
    Doctor {
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Trust this project: simmer may run its .envrc / shellHook /
    /// providers. For .envrc projects trust is bound to the file content
    /// (edits revoke it, like direnv).
    Allow {
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Revoke trust; simmer goes inert for this project (and stops
    /// suggesting `simmer allow` for it).
    Deny {
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// direnv-compatible CLI (export json/bash/…, exec, allow, reload,
    /// version, hook). Symlink simmer as `direnv` to serve editor plugins.
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
    // SAFETY: setting a signal disposition before any threads start.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    // Busybox pattern: a `direnv`-named symlink to this binary routes
    // straight into the shim — no shell wrapper, no extra exec.
    let invoked_as = std::env::args_os().next().unwrap_or_default();
    if Path::new(&invoked_as).file_name().is_some_and(|n| n == "direnv") {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if let Err(e) = direnv_shim::run(&args, &Global::from_env()) {
            fail("simmer(direnv)", &e);
        }
        return;
    }

    let cli = Cli::parse();
    let g = cli.global;
    // Reject a malformed flag before doing any work.
    if let Err(e) = g.validate() {
        fail("simmer", &e);
    }
    let result = match cli.command {
        Cmd::Export { shell, dir, no_warm, background } => {
            let background = background || env_flag("SIMMER_BACKGROUND");
            cmd_export(shell.into(), dir, no_warm, background, &g)
        }
        Cmd::Warm { dir, force } => cmd_warm(dir, force, &g),
        Cmd::Hook { shell } => cmd_hook(shell.into()),
        Cmd::Status { dir } => cmd_status(dir),
        Cmd::Daemon { print_systemd: true } => {
            print!("{}", systemd_unit());
            Ok(())
        }
        Cmd::Daemon { print_systemd: false } => exec_daemon(),
        Cmd::Doctor { dir } => cmd_doctor(dir),
        Cmd::Allow { dir } => cmd_allow(dir, true),
        Cmd::Deny { dir } => cmd_allow(dir, false),
        Cmd::Direnv { args } => direnv_shim::run(&args, &g),
    };
    if let Err(e) = result {
        fail("simmer", &e);
    }
}

/// Report `e` on stderr (the hook pipes stdout into `source`; errors must
/// never poison the shell) and exit with the code it maps to: a failed
/// child's own status, 124 for a timeout, 128+N for signal N.
fn fail(prefix: &str, e: &anyhow::Error) -> ! {
    eprintln!("{prefix}: {e:#}");
    std::process::exit(supervise::exit_code_of(e));
}

fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty() && v != "0")
}

impl Global {
    /// Defaults with only the environment applied (the direnv shim has no
    /// simmer flags of its own).
    fn from_env() -> Global {
        Global { progress: None, plain: false, json: false, quiet: false, verbose: false, timeout: None, log_lines: 40 }
    }

    fn validate(&self) -> Result<()> {
        if let Some(timeout) = &self.timeout {
            parse_duration(timeout)?;
        }
        if let Some(mode) = &self.progress {
            progress::Mode::resolve(Some(mode)).map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }

    fn progress_config(&self) -> Result<progress::Config> {
        let requested = if self.json {
            Some("json".to_string())
        } else if self.plain {
            Some("plain".to_string())
        } else {
            self.progress.clone().or_else(|| std::env::var("SIMMER_PROGRESS").ok())
        };
        let mut mode = progress::Mode::resolve(requested.as_deref()).map_err(anyhow::Error::msg)?;
        let verbosity = if self.quiet {
            progress::Verbosity::Quiet
        } else if self.verbose {
            progress::Verbosity::Verbose
        } else {
            progress::Verbosity::Normal
        };
        if verbosity == progress::Verbosity::Quiet && mode == progress::Mode::Live {
            mode = progress::Mode::Quiet;
        }
        Ok(progress::Config { mode, verbosity, color: term::color_allowed(), log_lines: self.log_lines })
    }

    fn deadline(&self) -> Result<Option<Instant>> {
        let raw = self.timeout.clone().or_else(|| std::env::var("SIMMER_TIMEOUT").ok());
        Ok(match raw.as_deref().map(parse_duration).transpose()? {
            Some(d) if !d.is_zero() => Some(Instant::now() + d),
            _ => None,
        })
    }
}

/// `90`, `90s`, `10m`, `1h`, `500ms` → a duration.
fn parse_duration(text: &str) -> Result<Duration> {
    let text = text.trim();
    let split = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let n: u64 = number.parse().with_context(|| format!("invalid duration '{text}' (try 90s, 10m, 1h)"))?;
    Ok(match unit {
        "" | "s" => Duration::from_secs(n),
        "ms" => Duration::from_millis(n),
        "m" => Duration::from_secs(n * 60),
        "h" => Duration::from_secs(n * 3600),
        _ => bail!("invalid duration '{text}': the unit must be ms, s, m, or h"),
    })
}

fn resolve_dir(dir: Option<PathBuf>) -> Result<PathBuf> {
    match dir {
        Some(d) => Ok(d),
        None => std::env::current_dir().context("resolving current directory"),
    }
}

fn no_project(dir: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "no project here: none of flake.nix, .envrc, or .simmer.toml exists in {} or any parent \
         directory (add one to make this a project; see the README's quickstart)",
        dir.display()
    )
}

/// `~/x` for paths under $HOME — for human-facing messages.
fn pretty(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        if let Ok(rest) = path.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

/// Build the environment in the foreground: wait for any warm already
/// running for this project, then warm (unless that one just produced
/// the current generation and `force` is off). `explicit` marks a warm the
/// user asked for (`simmer warm`), as opposed to a prompt-time rebuild.
/// Progress is shown and closed here; the caller gets the profile or the
/// error.
fn foreground_warm(
    project: &Project,
    cache: &Cache,
    g: &Global,
    force: bool,
    explicit: bool,
    json_to_stdout: bool,
) -> Result<EnvProfile> {
    // From here on Ctrl-C / SIGTERM stop our children before we exit.
    supervise::signals::install().context("installing signal handlers")?;
    let config = g.progress_config()?;
    let deadline = g.deadline()?;
    let progress = Progress::new(&format!("warming {}", pretty(&project.root)), config, json_to_stdout);
    let result = (|| {
        let mut wait_task = None;
        let lock = cache.lock_warm(deadline, None, |pid| {
            let who = pid.map(|p| format!(" (pid {p})")).unwrap_or_default();
            wait_task = Some(progress.begin(&format!("wait for the warm already running{who}")));
        });
        let waited = wait_task.is_some();
        match (&lock, wait_task) {
            (Ok(_), Some(task)) => task.ok(),
            (Err(e), Some(task)) => task.fail_err(e),
            _ => {}
        }
        let _lock = lock?;
        if waited && !force {
            // Another simmer just finished; if it built what we need, done.
            let generation = cache.generation_of(project)?;
            if let Some(profile) = cache.load().filter(|p| p.generation == generation && p.format >= PROFILE_FORMAT) {
                return Ok(profile);
            }
        }
        // Probe for a native env.json shell only on an explicit `simmer
        // warm` (or the first build): a prompt-time rebuild reuses the mode
        // the last build found instead of paying for a speculative build.
        let probe = explicit || cache.warm_mode().is_none();
        let opts = warm::WarmOptions { deadline, probe, ..warm::WarmOptions::new(progress.clone()) };
        warm::warm_opts(project, cache, &opts)
    })();
    if let Ok(profile) = &result {
        progress.info(&format!(
            "warmed {} ({} vars, {}, {})",
            pretty(&project.root),
            profile.vars.len(),
            profile.mode,
            term::fmt_duration(Duration::from_millis(profile.warm_ms))
        ));
    }
    progress.close(warm::ending_of(&result));
    result
}

/// Show `text` once per shell session (keyed by `key`): appends the
/// marker assignment to `script` so later prompts stay quiet.
fn notice_once(shell: Shell, key: &str, text: &str, script: &mut String) {
    if std::env::var("__SIMMER_NOTICE").is_ok_and(|seen| seen == key) {
        return;
    }
    eprintln!("{text}");
    let marker = Changes::from([("__SIMMER_NOTICE".to_string(), Some(key.to_string()))]);
    script.push_str(&compile::render(shell, &marker));
}

/// The unload script for the session in the environment, if any.
fn unload_session(shell: Shell) -> String {
    let current: BTreeMap<String, String> = std::env::vars().collect();
    match compile::read_state(&current) {
        Some(state) => {
            let base = compile::effective_env(&current, Some(&state));
            compile::unload_script(shell, &state, &base)
        }
        None => String::new(),
    }
}

/// What a failure record says when the user stopped the build.
pub(crate) const INTERRUPTED: &str = "interrupted";

fn first_line(e: &anyhow::Error) -> String {
    format!("{e:#}").lines().next().unwrap_or_default().to_string()
}

fn cmd_export(shell: Shell, dir: Option<PathBuf>, no_warm: bool, background: bool, g: &Global) -> Result<()> {
    let dir = resolve_dir(dir)?;
    let in_session = std::env::var_os(compile::STATE_VAR).is_some();

    // A project whose manifest does not parse is an error, reported at
    // every prompt until fixed; the shell keeps whatever it has loaded.
    let Some(mut project) = Project::locate(&dir)? else {
        // Left the project: tear the session down against what the shell
        // held before we touched it.
        if in_session {
            print!("{}", unload_session(shell));
        }
        return Ok(());
    };

    let cache = Cache::for_project(&project)?;
    // Trust gate: an untrusted project is not simmer's to manage — no
    // code execution, no application; any lingering session unloads.
    // Until `simmer allow`, direnv (or nothing) owns it, which is also
    // the coexistence story: allow is the per-project migration switch.
    if !cache.trusted(&project) {
        let mut script = if in_session { unload_session(shell) } else { String::new() };
        let direnv_owns = project.envrc_path().is_some() && std::env::var_os("DIRENV_DIR").is_some();
        if !cache.denied() && !direnv_owns && !env_flag("SIMMER_NO_HINTS") && term::is_tty(2) {
            notice_once(
                shell,
                &format!("untrusted:{}", project.root.display()),
                &format!(
                    "simmer: {} has an environment, but simmer will not run it until you `simmer allow` \
                     (or `simmer deny` to silence this)",
                    pretty(&project.root)
                ),
                &mut script,
            );
        }
        print!("{script}");
        return Ok(());
    }
    project.extend_watches(cache.dynamic_watches());
    project.extend_boot_scoped(cache.boot_scoped());
    let generation = cache.generation_of(&project)?;
    let cached_generation = cache.cached_generation();
    let stale = cached_generation.as_deref() != Some(generation.as_str());

    // Fast path — the common case on every prompt: the cache is current
    // and the session already holds it. One env lookup, no parsing.
    if !stale && in_session && std::env::var(compile::GEN_VAR).ok() == cached_generation {
        return Ok(());
    }

    let mut script = String::new();
    let mut warm_error = None;
    if stale && !no_warm {
        let failed_before = cache.failure().filter(|(g, _)| *g == generation);
        if background {
            spawn_background_warm(&project)?;
        } else if let Some((_, message)) = failed_before {
            // Waiting again for the same failing (or deliberately stopped)
            // build on every prompt helps nobody; saving a watched file is a
            // new generation and retries automatically.
            if term::is_tty(2) {
                let text = if message == INTERRUPTED {
                    format!(
                        "simmer: the rebuild of {} was interrupted; save a watched file or run `simmer warm` \
                         to build it — the previous environment stays loaded",
                        pretty(&project.root)
                    )
                } else {
                    format!(
                        "simmer: building the environment for {} failed ({message}); fix it and save, \
                         or run `simmer warm` to retry — the previous environment stays loaded",
                        pretty(&project.root)
                    )
                };
                notice_once(shell, &format!("failed:{generation}"), &text, &mut script);
            }
        } else {
            match foreground_warm(&project, &cache, g, false, false, false) {
                Ok(_) => {}
                // Ctrl-C means "stop", not "try again at the next prompt".
                Err(e) if supervise::is_interrupt(&e) => {
                    let _ = cache.record_failure(&generation, INTERRUPTED);
                    return Err(e);
                }
                Err(e) => {
                    let _ = cache.record_failure(&generation, &first_line(&e));
                    warm_error = Some(e);
                }
            }
        }
    }

    let current: BTreeMap<String, String> = std::env::vars().collect();
    let state = compile::read_state(&current);
    let cached_generation = cache.cached_generation();
    let already_applied = match (&state, &cached_generation) {
        (Some(state), Some(cached)) => state.generation == *cached,
        _ => false,
    };
    if !already_applied {
        // An out-of-date environment (after a failed rebuild, or with
        // --background / --no-warm) beats a missing one.
        if let Some(mut profile) = cache.load() {
            // A profile written by an older simmer is rebuilt once, now.
            let failed_before = cache.failure().is_some_and(|(g, _)| g == generation);
            if profile.format < PROFILE_FORMAT && !no_warm && warm_error.is_none() && !failed_before {
                if background {
                    spawn_background_warm(&project)?;
                } else {
                    match foreground_warm(&project, &cache, g, true, false, false) {
                        Ok(fresh) => profile = fresh,
                        Err(e) if supervise::is_interrupt(&e) => {
                            let _ = cache.record_failure(&generation, INTERRUPTED);
                            return Err(e);
                        }
                        Err(e) => {
                            let _ = cache.record_failure(&generation, &first_line(&e));
                            warm_error = Some(e);
                        }
                    }
                }
            }
            let base = compile::effective_env(&current, state.as_ref());
            script.push_str(&compile::apply_script(shell, &profile, &base, state.as_ref()));
            // The banner greets a human entering the project — not a
            // generation switch while they sit in it, and never a captured
            // environment.
            let entering = state.as_ref().is_none_or(|s| s.root != profile.root);
            if let (true, false, Some(banner)) = (entering, env_flag("SIMMER_NO_MOTD"), &profile.banner) {
                script.push_str(&compile::banner_script(shell, banner));
            }
        }
    }
    print!("{script}");
    match warm_error {
        Some(e) => Err(e.context(format!("building the environment for {}", pretty(&project.root)))),
        None => Ok(()),
    }
}

/// The opt-in `--background` mode: a detached `simmer warm`.
fn spawn_background_warm(project: &Project) -> Result<()> {
    let exe = std::env::current_exe().context("locating simmer binary")?;
    let spawned = std::process::Command::new(exe)
        .args(["warm", "--progress", "quiet", "--dir"])
        .arg(&project.root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        Ok(_) => eprintln!("simmer: building the environment for {} in the background", pretty(&project.root)),
        Err(e) => eprintln!("simmer: could not start a background warm: {e}"),
    }
    Ok(())
}

fn cmd_warm(dir: Option<PathBuf>, force: bool, g: &Global) -> Result<()> {
    let dir = resolve_dir(dir)?;
    let project = Project::locate(&dir)?.ok_or_else(|| no_project(&dir))?;
    let cache = Cache::for_project(&project)?;
    // Running `simmer warm` in a project is an explicit human action
    // there — it grants trust, like `direnv allow`.
    cache.allow(&project)?;
    let json_to_stdout = g.json || g.progress.as_deref() == Some("json");
    let result = foreground_warm(&project, &cache, g, force, true, json_to_stdout);
    if let Err(e) = &result {
        if let Ok(generation) = cache.generation_of(&project) {
            let message = if supervise::is_interrupt(e) { INTERRUPTED.to_string() } else { first_line(e) };
            let _ = cache.record_failure(&generation, &message);
        }
    }
    result.map(|_| ())
}

fn cmd_allow(dir: Option<PathBuf>, allow: bool) -> Result<()> {
    let dir = resolve_dir(dir)?;
    let project = Project::locate(&dir)?.ok_or_else(|| no_project(&dir))?;
    let cache = Cache::for_project(&project)?;
    if allow {
        cache.allow(&project)?;
        eprintln!(
            "simmer: {} allowed — its environment loads at your next prompt (`simmer warm` builds it now)",
            pretty(&project.root)
        );
    } else {
        cache.deny()?;
        eprintln!("simmer: {} denied — simmer is now inert here", pretty(&project.root));
    }
    Ok(())
}

fn cmd_hook(shell: Shell) -> Result<()> {
    let snippet = match shell {
        // fish_prompt as well as PWD: an edit made while the shell sits in
        // the project is picked up at the next prompt, and the no-op costs
        // about a millisecond.
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
        // precmd as well as chpwd, matching fish and bash: edits are
        // picked up at the next prompt, not only on the next `cd`.
        Shell::Zsh => concat!(
            "__simmer_hook() { eval \"$(simmer export zsh)\"; }\n",
            "autoload -U add-zsh-hook\n",
            "add-zsh-hook chpwd __simmer_hook\n",
            "add-zsh-hook precmd __simmer_hook\n",
        ),
    };
    print!("{snippet}");
    Ok(())
}

fn cmd_status(dir: Option<PathBuf>) -> Result<()> {
    let dir = resolve_dir(dir)?;
    let current: BTreeMap<String, String> = std::env::vars().collect();
    let state = compile::read_state(&current);
    let status = match Project::locate(&dir)? {
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
                "last_failure": cache.failure().map(|(g, m)| serde_json::json!({ "generation": g, "error": m })),
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

    let mut project = match Project::locate(&dir) {
        Ok(Some(project)) => project,
        Ok(None) => {
            println!("{}", serde_json::json!({ "project": null, "warnings": [no_project(&dir).to_string()] }));
            return Ok(());
        }
        Err(e) => {
            println!("{}", serde_json::json!({ "project": null, "warnings": [format!("{e:#}")] }));
            return Ok(());
        }
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
        warnings
            .push("project not yet registered (run `simmer warm` once) — the daemon will not pre-warm it".to_string());
    }
    if let Some((_, message)) = cache.failure() {
        warnings.push(format!("the last warm failed: {message} (full logs in {})", cache.logs_dir().display()));
    }

    project.extend_watches(cache.dynamic_watches());
    project.extend_boot_scoped(cache.boot_scoped());
    let generation = project.generation()?;
    let profile = cache.load();

    // Hot-path cost, measured for real. `export_hot_us` is what every
    // prompt pays when the environment is current: fingerprint the
    // watched files and compare the generation sidecar. `export_apply_us`
    // is the entry path on top of that: load the profile and compile the
    // apply script.
    let t = Instant::now();
    for _ in 0..10 {
        let g = cache.generation_of(&project)?;
        let _ = cache.cached_generation().as_deref() == Some(g.as_str());
    }
    let export_hot_us = t.elapsed().as_micros() / 10;
    let t = Instant::now();
    let mut compiled_bytes = 0;
    for _ in 0..10 {
        if let Some(p) = cache.load() {
            let base = compile::effective_env(&current, None);
            compiled_bytes = compile::apply_script(Shell::Fish, &p, &base, None).len();
        }
    }
    let export_apply_us = export_hot_us + t.elapsed().as_micros() / 10;

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "project": project.root,
            "trusted": trusted,
            "watched": project.watched,
            "watch_globs": project.watch_globs(),
            "banner": profile.as_ref().is_some_and(|p| p.banner.is_some()),
            "input_roots": std::fs::read_dir(cache.inputs_dir()).map(|d| d.count().saturating_sub(1)).unwrap_or(0),
            "providers": project.config.providers.iter().map(|p| &p.env).collect::<Vec<_>>(),
            "generation": generation,
            "fresh": profile.as_ref().map(|p| p.generation == generation).unwrap_or(false),
            "mode": profile.as_ref().map(|p| p.mode.clone()),
            "native_installable": warm::native_installable(&project),
            "last_warm_ms": profile.as_ref().map(|p| p.warm_ms),
            "last_failure": cache.failure().map(|(g, m)| serde_json::json!({ "generation": g, "error": m })),
            "export_hot_us": export_hot_us,
            "export_apply_us": export_apply_us,
            "apply_script_bytes": compiled_bytes,
            "registered_projects": registry::list().len(),
            "warnings": warnings,
        }))?
    );
    Ok(())
}

/// Replace this process with `simmer-daemon` (installed next to simmer).
fn exec_daemon() -> Result<()> {
    use std::os::unix::process::CommandExt;
    let sibling = std::env::current_exe().ok().map(|exe| exe.with_file_name("simmer-daemon"));
    let program = match sibling.filter(|p| p.is_file()) {
        Some(path) => path,
        None => std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join("simmer-daemon"))
            .find(|p| p.is_file())
            .context("simmer-daemon is not installed next to simmer or on PATH (reinstall simmer)")?,
    };
    Err(std::process::Command::new(&program).exec()).with_context(|| format!("running {}", program.display()))
}

/// systemd user unit for the daemon; `simmer daemon --print-systemd`.
fn systemd_unit() -> String {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "simmer".to_string());
    format!(
        "[Unit]\n\
         Description=simmer devshell pre-warm daemon\n\n\
         [Service]\n\
         ExecStart={exe} daemon\n\
         Restart=on-failure\n\n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_with_units() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert!(parse_duration("ten").is_err());
        assert!(parse_duration("5d").is_err());
    }

    #[test]
    fn cli_definition_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
