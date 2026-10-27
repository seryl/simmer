//! The warm path: build the environment once and cache the result.
//!
//! Two ways to warm, chosen per project:
//!
//! - **Native** — the devshell derivation ships an `env.json`
//!   ([`crate::envspec::EnvSpec`]). `nix build` it (the profile symlink
//!   doubles as the GC root), read the spec, materialize variables in
//!   Rust, run declared providers, link generated config files. No bash
//!   is sourced; the whole warm is one nix invocation plus whatever the
//!   providers cost.
//! - **Harness** — everything else. Assemble the environment in a
//!   scrubbed bash harness: the shim (`shim.bash`) provides
//!   direnv-compatible functions; on top of it the script is either the
//!   project's `.envrc` (compat mode) or a few generated lines from
//!   `.simmer.toml`. A manifest outranks a `.envrc`
//!   ([`Project::sourced_envrc`]), so a project can keep a `.envrc` for
//!   direnv callers and still load natively here. `nix print-dev-env`'s script output applies
//!   variables, defines functions, and ends with `eval "${shellHook:-}"`.
//!
//! An explicit `simmer warm` probes native first and remembers the
//! answer in the cache; daemon warms reuse it, so a harness project never
//! pays a speculative `nix build` on every save.
//!
//! Every step runs through [`crate::supervise::run`]: the warm returns
//! only when all of its children have exited, reports each step to a
//! [`Progress`], stops cleanly on an interrupt or a deadline, and fails
//! with the failing child's exit status.

use crate::cache::Cache;
use crate::configs;
use crate::envspec::{current_system, EnvSpec, Var, ENV_SPEC_FILE};
use crate::menu;
use crate::nixlog;
use crate::profile::{EnvProfile, Value, PROFILE_FORMAT, SKIP_VARS};
use crate::progress::{Config, Ending, Progress, TaskHandle};
use crate::project::{hex, Project};
use crate::supervise::{self, Cancel, LineSink, RunError, RunOpts};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

const SHIM: &str = include_str!("shim.bash");

pub const MODE_NATIVE: &str = "native";
/// The harness, chosen because the native probe found no `env.json`:
/// remembered so daemon warms skip the speculative build.
pub const MODE_HARNESS: &str = "harness";
/// The harness, chosen because simmer sources the project's `.envrc`. No
/// probe ran, so this says nothing about native: once a manifest
/// outranks the `.envrc`, the next warm probes.
pub const MODE_ENVRC: &str = "envrc";

/// The only caller variables visible to the warm harness. Warming must
/// produce the same profile whether it ran from a login shell, a cron
/// job, or inside another devshell — an inherited environment would both
/// leak session variables into the profile and mask environment variables
/// that happen to match it.
const HARNESS_KEEP: &[&str] = &["HOME", "LANG", "LOGNAME", "PATH", "TERM", "TZ", "USER", "XDG_RUNTIME_DIR"];

/// How a warm runs: what to report to, and when to give up.
#[derive(Clone)]
pub struct WarmOptions {
    /// Probe for a native `env.json` shell first (an explicit warm);
    /// `false` reuses the mode the last warm recorded (the daemon).
    pub probe: bool,
    pub progress: Progress,
    /// Stop every child and fail at this instant.
    pub deadline: Option<Instant>,
    pub cancel: Option<Cancel>,
    /// GC-root the flake's inputs so a re-warm after `nix store gc` needs
    /// no network. Pointless for a cache that dies with its caller (a CI
    /// pod), where it costs a second fetch of every input.
    pub pin_inputs: bool,
}

impl WarmOptions {
    pub fn new(progress: Progress) -> Self {
        WarmOptions { probe: true, progress, deadline: None, cancel: None, pin_inputs: true }
    }

    fn run_opts(&self) -> RunOpts {
        RunOpts { deadline: self.deadline, cancel: self.cancel.clone(), ..RunOpts::default() }
    }
}

/// Build (or rebuild) the profile for `project`'s current generation,
/// probing for a native `env.json` shell first. Progress goes to stderr.
///
/// Trust is the caller's responsibility: this function executes project
/// code (.envrc, shellHooks, providers) unconditionally.
pub fn warm(project: &Project, cache: &Cache) -> Result<EnvProfile> {
    warm_with(project, cache, true)
}

/// Like [`warm`], but with `probe = false` reuse the mode the last warm
/// recorded instead of trying native first (the daemon's choice).
pub fn warm_with(project: &Project, cache: &Cache, probe: bool) -> Result<EnvProfile> {
    let progress = Progress::new(&format!("warming {}", project.root.display()), Config::default(), false);
    let result = warm_opts(project, cache, &WarmOptions { probe, ..WarmOptions::new(progress.clone()) });
    progress.close(ending_of(&result));
    result
}

/// How a result ends a progress display.
pub fn ending_of<T>(result: &Result<T>) -> Ending {
    match result {
        Ok(_) => Ending::Success,
        Err(e) if supervise::is_interrupt(e) => Ending::Interrupted,
        Err(_) => Ending::Failed,
    }
}

/// The warm itself, reporting to `opts.progress` (which the caller
/// closes). Returns only after every child it started has exited.
pub fn warm_opts(project: &Project, cache: &Cache, opts: &WarmOptions) -> Result<EnvProfile> {
    let started = Instant::now();
    cache.ensure_dir()?;
    opts.progress.set_log_dir(cache.logs_dir());
    let caller: BTreeMap<String, String> =
        std::env::vars().filter(|(k, _)| HARNESS_KEEP.contains(&k.as_str()) || k.starts_with("LC_")).collect();

    let try_native =
        project.sourced_envrc().is_none() && (opts.probe || cache.warm_mode().as_deref() != Some(MODE_HARNESS));
    let native = if try_native { build_native(project, cache, &caller, opts)? } else { None };

    let (vars, mut dynamic, boot_scoped, mode, banner) = match native {
        Some(spec) => {
            // Rooting flake inputs is independent of materializing the
            // environment: run both at once, wait for both.
            let (materialized, pinned) = std::thread::scope(|scope| {
                let pin = scope.spawn(|| match opts.pin_inputs {
                    true => pin_inputs(project, cache, &caller, opts),
                    false => Ok(()),
                });
                let materialized = materialize(project, &spec, &caller, opts);
                (materialized, pin.join())
            });
            match pinned {
                Ok(Ok(())) => {}
                Ok(Err(e)) if supervise::is_interrupt(&e) => return Err(e),
                // Non-fatal: the shell is pinned; only an offline re-warm
                // after `nix store gc` is affected. Reported on its task.
                Ok(Err(_)) => {}
                Err(_) => bail!("pinning flake inputs panicked"),
            }
            let (vars, dynamic) = materialized?;
            (vars, dynamic, spec.boot_scoped(), MODE_NATIVE, menu::banner(&spec))
        }
        None => {
            let (captured, dynamic) = run_harness(project, &caller, cache, opts)?;
            let vars = captured_vars(captured, &caller);
            let mode = if project.sourced_envrc().is_some() { MODE_ENVRC } else { MODE_HARNESS };
            (vars, dynamic, false, mode, None)
        }
    };
    // Wildcard watches are expanded here, once, into the dynamic set the
    // prompt hashes; the daemon watches their directory trees for files
    // that appear later.
    dynamic.extend(project.expand_globs());
    dynamic.sort();
    dynamic.dedup();
    cache.store_dynamic_watches(&dynamic)?;
    cache.store_boot_scoped(boot_scoped)?;
    cache.store_warm_mode(mode)?;

    // The generation must reflect what the warm actually depended on:
    // watches discovered while running it, and boot scoping declared by
    // the shell itself.
    // The static watch set is re-read too: the build itself may have
    // created a watched file (nix writes flake.lock on first evaluation),
    // and a generation computed without it would be stale at once.
    let mut resolved = Project::at(&project.root)
        .map(|p| p.with_invocation(project.invocation.clone()))
        .unwrap_or_else(|_| project.clone());
    resolved.extend_watches(dynamic);
    resolved.extend_boot_scoped(boot_scoped);
    let generation = resolved.generation()?;

    let profile = EnvProfile {
        format: PROFILE_FORMAT,
        generation,
        root: project.root.display().to_string(),
        vars,
        warm_ms: started.elapsed().as_millis() as u64,
        mode: mode.to_string(),
        banner,
    };
    cache.store(&profile)?;
    // Every warmed project joins the daemon's pre-warm work list.
    crate::registry::register(&project.root)?;
    Ok(profile)
}

fn captured_vars(captured: BTreeMap<String, String>, caller: &BTreeMap<String, String>) -> BTreeMap<String, Value> {
    EnvProfile::from_captured(String::new(), String::new(), captured, caller, 0).vars
}

/// A stderr sink that turns nix's JSON log into phases and output lines
/// of `task`.
fn nix_sink(task: &TaskHandle) -> LineSink {
    let progress = task.progress().clone();
    let id = task.id();
    Arc::new(move |line: &str| match nixlog::parse(line) {
        nixlog::Line::Phase(phase) => {
            progress.phase(id, &phase);
            progress.output(id, &phase);
        }
        // A nix message can span lines (an error with its source excerpt).
        nixlog::Line::Output(text) => {
            for part in text.lines() {
                progress.output(id, part);
            }
        }
        nixlog::Line::Ignore => {}
    })
}

/// Run `cmd` as `task`'s work, waiting for it; stdout is returned.
fn run_task(cmd: &mut Command, task: &TaskHandle, opts: &WarmOptions) -> Result<Vec<u8>, RunError> {
    supervise::run(cmd, &opts.run_opts(), Some(nix_sink(task)))
}

/// A nix command with simmer's scrubbed environment and JSON logging.
fn nix(caller: &BTreeMap<String, String>, cwd: &Path) -> Command {
    let mut cmd = Command::new("nix");
    cmd.args(nixlog::LOG_FLAGS).env_clear().envs(caller).current_dir(cwd);
    cmd
}

// ---------------------------------------------------------------- native

/// The installable to build for a native warm: `.simmer.toml`'s `flake`
/// (default `.`), with a bare fragment resolved under `devShells.<system>`.
pub fn native_installable(project: &Project) -> Option<String> {
    let flake = project.flake_ref()?;
    let (base, fragment) = match flake.split_once('#') {
        Some((b, f)) => (b.to_string(), f.to_string()),
        None => (flake.clone(), String::new()),
    };
    let fragment = if fragment.is_empty() { "default".to_string() } else { fragment };
    let attr = if fragment.contains('.') { fragment } else { format!("devShells.{}.{}", current_system(), fragment) };
    Some(format!("{base}#{attr}"))
}

/// `nix build` the devshell; `Some(spec)` when its output carries an
/// `env.json`, `None` when this is not a native shell (the harness takes
/// over). The build output is pinned as the project's GC root either way.
/// An interrupt, a timeout, or a missing `nix` is an error, never a
/// silent fallback.
fn build_native(
    project: &Project,
    cache: &Cache,
    caller: &BTreeMap<String, String>,
    opts: &WarmOptions,
) -> Result<Option<EnvSpec>> {
    let Some(installable) = native_installable(project) else { return Ok(None) };
    let task = opts.progress.begin("nix build");
    task.phase(&installable);
    let mut cmd = nix(caller, &project.root);
    cmd.args(["build", "--no-link", "--print-out-paths", "--profile"])
        .arg(cache.gcroot_path())
        .arg(&installable)
        .args(&project.invocation.nix_args);
    let stdout = match run_task(&mut cmd, &task, opts) {
        Ok(stdout) => stdout,
        // Not necessarily a native shell at all; the harness evaluates
        // the flake again and reports the real error if it is broken.
        Err(RunError::Failed { .. }) => {
            task.skip("not buildable as a native shell; using the harness");
            return Ok(None);
        }
        Err(e) => {
            let e = anyhow::Error::new(e).context("nix build");
            task.fail_err(&e);
            return Err(e);
        }
    };
    let out_path = String::from_utf8_lossy(&stdout).lines().next().unwrap_or("").to_string();
    let spec_path = Path::new(&out_path).join(ENV_SPEC_FILE);
    if !spec_path.is_file() {
        task.ok_with("no env.json; using the harness");
        return Ok(None);
    }
    match EnvSpec::load(&spec_path) {
        Ok(spec) => {
            task.ok();
            Ok(Some(spec))
        }
        Err(e) => {
            task.fail_err(&e);
            Err(e)
        }
    }
}

/// Root every flake input next to the shell's GC root, once per
/// `flake.lock` content, so a warm after `nix store gc` re-evaluates from
/// the store instead of re-fetching sources. The flake's own source tree
/// is skipped: it is the working copy, and the shell closure is already
/// pinned.
fn pin_inputs(project: &Project, cache: &Cache, caller: &BTreeMap<String, String>, opts: &WarmOptions) -> Result<()> {
    let Ok(lock) = std::fs::read(project.root.join("flake.lock")) else { return Ok(()) };
    let lock_hash = hex(&Sha256::digest(&lock));
    let dir = cache.inputs_dir();
    let marker = dir.join("lock-hash");
    if std::fs::read_to_string(&marker).is_ok_and(|h| h == lock_hash) {
        return Ok(());
    }
    let task = opts.progress.begin("pin flake inputs");
    match pin_inputs_now(project, caller, opts, &task, &dir) {
        Ok(count) => {
            std::fs::write(&marker, lock_hash)?;
            task.ok_with(format!("{count} inputs rooted"));
            Ok(())
        }
        Err(e) if supervise::is_interrupt(&e) => {
            task.fail_err(&e);
            Err(e)
        }
        Err(e) => {
            task.warn(format!("{e:#}; offline re-warms after `nix store gc` may need the network"));
            Err(e)
        }
    }
}

fn pin_inputs_now(
    project: &Project,
    caller: &BTreeMap<String, String>,
    opts: &WarmOptions,
    task: &TaskHandle,
    dir: &Path,
) -> Result<usize> {
    let flake = native_installable(project)
        .and_then(|i| i.split_once('#').map(|(base, _)| base.to_string()))
        .unwrap_or_else(|| ".".to_string());
    let mut archive = nix(caller, &project.root);
    archive.args(["flake", "archive", "--json", "--no-write-lock-file"]).arg(&flake).args(&project.invocation.nix_args);
    let stdout = run_task(&mut archive, task, opts).context("nix flake archive")?;
    let tree: serde_json::Value = serde_json::from_slice(&stdout).context("parsing nix flake archive output")?;
    let paths = input_paths(&tree);
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir)?;
    if !paths.is_empty() {
        let mut build = nix(caller, &project.root);
        build
            .args(["build", "--no-write-lock-file", "--out-link"])
            .arg(dir.join("in"))
            .args(&paths)
            .args(&project.invocation.nix_args);
        run_task(&mut build, task, opts).with_context(|| format!("rooting {} inputs", paths.len()))?;
    }
    Ok(paths.len())
}

/// Store paths of every input in `nix flake archive --json` output,
/// recursively, without the top-level flake's own path.
fn input_paths(tree: &serde_json::Value) -> Vec<String> {
    fn collect(node: &serde_json::Value, top: bool, out: &mut Vec<String>) {
        if !top {
            if let Some(path) = node.get("path").and_then(|p| p.as_str()) {
                out.push(path.to_string());
            }
        }
        if let Some(inputs) = node.get("inputs").and_then(|i| i.as_object()) {
            for child in inputs.values() {
                collect(child, false, out);
            }
        }
    }
    let mut out = Vec::new();
    collect(tree, true, &mut out);
    out.sort();
    out.dedup();
    out
}

/// Turn the spec plus the manifest's declarative additions into profile
/// values, running providers and the legacy enter hook in the finished
/// environment, then link the declared config files.
fn materialize(
    project: &Project,
    spec: &EnvSpec,
    caller: &BTreeMap<String, String>,
    opts: &WarmOptions,
) -> Result<(BTreeMap<String, Value>, Vec<PathBuf>)> {
    let root = project.root.display().to_string();
    let mut vars: BTreeMap<String, Value> = BTreeMap::new();
    for (name, var) in &spec.vars {
        if SKIP_VARS.contains(&name.as_str()) {
            continue;
        }
        let value = match var {
            Var::Set { value } => Value::Literal(value.clone()),
            Var::Prefix { value, sep } => Value::WithCaller { prefix: value.clone(), sep: sep.clone() },
            Var::Root => Value::Literal(root.clone()),
            Var::Unset => continue,
        };
        vars.insert(name.clone(), value);
    }
    vars.entry("PRJ_ROOT".into()).or_insert_with(|| Value::Literal(root.clone()));

    let config = &project.config;
    for (name, value) in &config.env {
        vars.insert(name.clone(), Value::Literal(value.clone()));
    }
    for dir in config.path.iter().rev() {
        let abs = project.root.join(dir).display().to_string();
        let merged = match vars.remove("PATH") {
            Some(Value::WithCaller { prefix, sep }) => Value::WithCaller { prefix: format!("{abs}:{prefix}"), sep },
            Some(Value::Literal(v)) => Value::Literal(format!("{abs}:{v}")),
            None => Value::WithCaller { prefix: abs, sep: ":".into() },
        };
        vars.insert("PATH".into(), merged);
    }
    let mut dynamic = Vec::new();
    for file in &config.dotenv {
        dynamic.push(PathBuf::from(file));
        if let Ok(text) = std::fs::read_to_string(project.root.join(file)) {
            for (name, value) in parse_dotenv(&text) {
                vars.insert(name, Value::Literal(value));
            }
        }
    }

    // Providers and the enter hook see the finished environment; their
    // outputs join it so later providers and templates can use them.
    let mut env = resolve(&vars, caller);
    let run_opts = opts.run_opts();
    for provider in spec.providers.iter().chain(config.providers.iter()) {
        let task = opts.progress.begin(&format!("provider {}", provider.env));
        let sink = plain_sink(&task);
        match run_in_env_opts(&provider.command, &env, &project.root, &run_opts, Some(sink)) {
            Ok(out) if !out.is_empty() => {
                env.insert(provider.env.clone(), out.clone());
                vars.insert(provider.env.clone(), Value::Literal(out));
                task.ok();
            }
            Ok(_) => task.warn(format!("produced nothing; {} left unset", provider.env)),
            Err(e) if supervise::is_interrupt(&e) || is_timeout(&e) => {
                task.fail_err(&e);
                return Err(e.context(format!("provider {}", provider.env)));
            }
            // A failing probe (no GPU here) skips its variable rather than
            // failing the warm — but visibly, with its output kept.
            Err(e) => task.warn(format!("{e:#}; {} left unset", provider.env)),
        }
    }
    if let Some(hook) = spec.hooks.enter.as_deref().filter(|h| !h.trim().is_empty()) {
        let task = opts.progress.begin("enter hook");
        let sink = plain_sink(&task);
        match run_in_env_opts(hook, &env, &project.root, &run_opts, Some(sink)) {
            Ok(out) => {
                for line in out.lines() {
                    task.output(line);
                    opts.progress.info(line);
                }
                task.ok();
            }
            Err(e) if supervise::is_interrupt(&e) || is_timeout(&e) => {
                task.fail_err(&e);
                return Err(e.context("enter hook"));
            }
            Err(e) => task.warn(format!("{e:#}")),
        }
    }

    if !spec.configs.is_empty() {
        let task = opts.progress.begin("config files");
        let reports = match configs::apply(&project.root, &spec.configs, &env) {
            Ok(reports) => reports,
            Err(e) => {
                task.fail_err(&e);
                return Err(e);
            }
        };
        let bad: Vec<String> =
            reports.iter().filter(|r| !r.is_ok()).map(|r| format!("{}: {:?}", r.output, r.status)).collect();
        for line in &bad {
            task.output(line);
        }
        if bad.is_empty() {
            task.ok_with(format!("{} linked", reports.len()));
        } else {
            task.warn(bad.join("; "));
        }
    }
    Ok((vars, dynamic))
}

fn is_timeout(err: &anyhow::Error) -> bool {
    err.chain().any(|e| matches!(e.downcast_ref::<RunError>(), Some(RunError::TimedOut { .. })))
}

/// A sink that records each stderr line as `task` output.
fn plain_sink(task: &TaskHandle) -> LineSink {
    let progress = task.progress().clone();
    let id = task.id();
    Arc::new(move |line: &str| progress.output(id, line))
}

/// The environment a provider runs in: profile values resolved against
/// the caller, on top of the caller's own variables.
fn resolve(vars: &BTreeMap<String, Value>, caller: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut env = caller.clone();
    for (name, value) in vars {
        let resolved = match value {
            Value::Literal(v) => v.clone(),
            Value::WithCaller { prefix, sep } => match caller.get(name) {
                Some(cur) if !cur.is_empty() => format!("{prefix}{sep}{cur}"),
                _ => prefix.clone(),
            },
        };
        env.insert(name.clone(), resolved);
    }
    env
}

/// Run `command` under bash with exactly `env`, in `cwd`; trimmed stdout.
/// How providers and the legacy enter hook execute. Waits for the command
/// (and, when simmer's signal handling is installed, stops its whole
/// process group on Ctrl-C); stderr goes to the caller's stderr.
pub fn run_in_env(command: &str, env: &BTreeMap<String, String>, cwd: &Path) -> Result<String> {
    run_in_env_opts(command, env, cwd, &RunOpts::default(), None)
}

/// [`run_in_env`] with a deadline/cancel and an optional stderr sink.
pub fn run_in_env_opts(
    command: &str,
    env: &BTreeMap<String, String>,
    cwd: &Path,
    opts: &RunOpts,
    stderr: Option<LineSink>,
) -> Result<String> {
    let mut cmd = Command::new("bash");
    cmd.args(["--noprofile", "--norc", "-c", command]).env_clear().envs(env).current_dir(cwd);
    let stdout = supervise::run(&mut cmd, opts, stderr)?;
    Ok(String::from_utf8_lossy(&stdout).trim_end().to_string())
}

/// `KEY=value` lines, `export` prefix and matching quotes tolerated,
/// comments and blanks skipped. Enough for the `.env` files people
/// actually write; no interpolation.
pub fn parse_dotenv(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
            let (name, value) = line.split_once('=')?;
            let name = name.trim();
            let ident = !name.is_empty()
                && !name.as_bytes()[0].is_ascii_digit()
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
            if !ident {
                return None;
            }
            let value = value.trim();
            let value = match value.as_bytes() {
                [q, .., e] if (*q == b'"' || *q == b'\'') && q == e => &value[1..value.len() - 1],
                _ => value,
            };
            Some((name.to_string(), value.to_string()))
        })
        .collect()
}

// --------------------------------------------------------------- harness

fn build_script(project: &Project) -> String {
    let mut script = String::from("set +e\n");
    script.push_str(SHIM);
    script.push('\n');
    if project.sourced_envrc().is_some() {
        // direnv compat: the .envrc drives the environment via the shim.
        script.push_str(". ./.envrc\n");
    } else {
        let config = &project.config;
        if let Some(flake_ref) = project.flake_ref() {
            script.push_str(&format!("use_flake {}", quote_single(&flake_ref)));
            for arg in &project.invocation.nix_args {
                script.push_str(&format!(" {}", quote_single(arg)));
            }
            script.push('\n');
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
/// watches it declared. This is the only place bash sources project code.
fn run_harness(
    project: &Project,
    caller: &BTreeMap<String, String>,
    cache: &Cache,
    opts: &WarmOptions,
) -> Result<(BTreeMap<String, String>, Vec<PathBuf>)> {
    let script_path = cache.dir.join("warm-harness.bash");
    std::fs::write(&script_path, build_script(project))?;
    let watches_out = cache.dir.join("watches.collect");
    let _ = std::fs::remove_file(&watches_out);
    let layout_dir = cache.dir.join("layout");
    std::fs::create_dir_all(&layout_dir)?;

    let label = if project.sourced_envrc().is_some() { "harness (.envrc)" } else { "harness (print-dev-env)" };
    let task = opts.progress.begin(label);
    let mut cmd = Command::new("bash");
    cmd.args(["--noprofile", "--norc"])
        .arg(&script_path)
        .env_clear()
        .envs(caller)
        .env("SIMMER_WATCHES_OUT", &watches_out)
        .env("SIMMER_GCROOT", cache.gcroot_path())
        .env("SIMMER_LAYOUT_DIR", &layout_dir)
        .env("SIMMER_NIX_FLAGS", nixlog::LOG_FLAGS.join(" "))
        .current_dir(&project.root);
    let stdout = match run_task(&mut cmd, &task, opts) {
        Ok(stdout) => stdout,
        Err(e) => {
            // Lead with what went wrong (nix's own error line), not just
            // which process exited.
            let context = match task.last_error_line() {
                Some(line) => format!("warm harness failed: {line}"),
                None => "warm harness failed".to_string(),
            };
            let e = anyhow::Error::new(e).context(context);
            task.fail_err(&e);
            return Err(e);
        }
    };
    task.ok();

    let mut env = BTreeMap::new();
    for entry in stdout.split(|b| *b == 0) {
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
        let keep = !project.root.join(path).is_dir();
        if !keep {
            opts.progress.warn(&format!("ignoring watch on directory {} (watch files instead)", path.display()));
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
        let _ = std::fs::remove_dir_all(&dir);
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
    fn the_harness_passes_the_invocation_to_use_flake() {
        let dir = scratch("invocation");
        std::fs::write(dir.join("flake.nix"), "{}").unwrap();
        let project = Project::at(&dir).unwrap().with_invocation(crate::project::Invocation {
            flake: Some("path:.#ci".into()),
            nix_args: vec!["--option".into(), "build-users-group".into(), String::new()],
        });
        let script = build_script(&project);
        assert!(script.contains("use_flake 'path:.#ci' '--option' 'build-users-group' ''\n"), "{script}");
        assert_eq!(native_installable(&project).unwrap(), format!("path:.#devShells.{}.ci", current_system()));
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

    #[test]
    fn native_installable_resolves_bare_fragments_under_devshells() {
        let dir = scratch("installable");
        std::fs::write(dir.join("flake.nix"), "{}").unwrap();
        let sys = current_system();
        assert_eq!(native_installable(&Project::at(&dir).unwrap()).unwrap(), format!(".#devShells.{sys}.default"));
        std::fs::write(dir.join(".simmer.toml"), "flake = \".#dev\"\n").unwrap();
        assert_eq!(native_installable(&Project::at(&dir).unwrap()).unwrap(), format!(".#devShells.{sys}.dev"));
        std::fs::write(dir.join(".simmer.toml"), "flake = \"path:/x#devShells.aarch64-linux.ci\"\n").unwrap();
        assert_eq!(native_installable(&Project::at(&dir).unwrap()).unwrap(), "path:/x#devShells.aarch64-linux.ci");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn materialize_maps_vars_and_applies_manifest_additions() {
        let dir = scratch("materialize");
        std::fs::write(dir.join("flake.nix"), "{}").unwrap();
        std::fs::write(dir.join(".env"), "# c\nexport DB=\"pg\"\nBAD LINE\n").unwrap();
        std::fs::write(dir.join(".simmer.toml"), "path = [\"bin\"]\ndotenv = [\".env\"]\n[env]\nX = \"1\"\n[[provider]]\nenv = \"CAP\"\ncommand = \"echo $X$X\"\n").unwrap();
        let project = Project::at(&dir).unwrap();
        let spec: EnvSpec = serde_json::from_str(&format!(
            r#"{{"schema":1,"name":"t","system":"s","profile":"{p}",
                "vars":{{"PATH":{{"kind":"prefix","value":"{p}/bin"}},"PRJ_ROOT":{{"kind":"root"}},
                        "HOME":{{"kind":"set","value":"/nope"}},"GONE":{{"kind":"unset"}},"FOO":{{"kind":"set","value":"f"}}}}}}"#,
            p = dir.display()
        ))
        .unwrap();
        // Providers run bash from the caller's PATH, so keep the real one.
        let caller = BTreeMap::from([("PATH".to_string(), std::env::var("PATH").unwrap_or_default())]);
        let opts = WarmOptions::new(Progress::hidden());
        let (vars, dynamic) = materialize(&project, &spec, &caller, &opts).unwrap();
        assert_eq!(
            vars["PATH"],
            Value::WithCaller { prefix: format!("{}/bin:{}/bin", dir.display(), dir.display()), sep: ":".into() }
        );
        assert_eq!(vars["PRJ_ROOT"], Value::Literal(dir.display().to_string()));
        assert!(!vars.contains_key("HOME") && !vars.contains_key("GONE"));
        assert_eq!(vars["X"], Value::Literal("1".into()));
        assert_eq!(vars["DB"], Value::Literal("pg".into()));
        assert_eq!(vars["CAP"], Value::Literal("11".into()));
        assert_eq!(dynamic, vec![PathBuf::from(".env")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn input_paths_skip_the_flake_itself_and_recurse() {
        let tree: serde_json::Value = serde_json::from_str(
            r#"{"path":"/nix/store/self","inputs":{
                "a":{"path":"/nix/store/a","inputs":{"nested":{"path":"/nix/store/n","inputs":{}}}},
                "b":{"path":"/nix/store/b","inputs":{"dup":{"path":"/nix/store/n"}}}}}"#,
        )
        .unwrap();
        assert_eq!(input_paths(&tree), vec!["/nix/store/a", "/nix/store/b", "/nix/store/n"]);
    }

    #[test]
    fn dotenv_parsing_tolerates_export_quotes_and_junk() {
        let parsed = parse_dotenv("A=1\nexport B='two'\n# x\n\nC = \"3\"\nnot a pair\n1BAD=x\n");
        assert_eq!(parsed, vec![("A".into(), "1".into()), ("B".into(), "two".into()), ("C".into(), "3".into())]);
    }
}
