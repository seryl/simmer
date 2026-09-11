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
//!   `.simmer.toml`. `nix print-dev-env`'s script output applies
//!   variables, defines functions, and ends with `eval "${shellHook:-}"`.
//!
//! An explicit `simmer warm` probes native first and remembers the
//! answer in the cache; daemon warms reuse it, so a harness project never
//! pays a speculative `nix build` on every save.

use crate::cache::Cache;
use crate::configs;
use crate::envspec::{current_system, EnvSpec, Var, ENV_SPEC_FILE};
use crate::menu;
use crate::profile::{EnvProfile, Value, PROFILE_FORMAT, SKIP_VARS};
use crate::project::{hex, Project};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

const SHIM: &str = include_str!("shim.bash");

pub const MODE_NATIVE: &str = "native";
pub const MODE_HARNESS: &str = "harness";

/// The only caller variables visible to the warm harness. Warming must
/// produce the same profile whether it ran from a login shell, a cron
/// job, or inside another devshell — an inherited environment would both
/// leak session variables into the profile and mask environment variables
/// that happen to match it.
const HARNESS_KEEP: &[&str] = &[
    "HOME", "LANG", "LOGNAME", "PATH", "TERM", "TZ", "USER", "XDG_RUNTIME_DIR",
];

/// Build (or rebuild) the profile for `project`'s current generation,
/// probing for a native `env.json` shell first.
///
/// Trust is the caller's responsibility: this function executes project
/// code (.envrc, shellHooks, providers) unconditionally.
pub fn warm(project: &Project, cache: &Cache) -> Result<EnvProfile> {
    warm_with(project, cache, true)
}

/// Like [`warm`], but with `probe = false` reuse the mode the last warm
/// recorded instead of trying native first (the daemon's choice).
pub fn warm_with(project: &Project, cache: &Cache, probe: bool) -> Result<EnvProfile> {
    let started = Instant::now();
    let caller: BTreeMap<String, String> = std::env::vars()
        .filter(|(k, _)| HARNESS_KEEP.contains(&k.as_str()) || k.starts_with("LC_"))
        .collect();

    let try_native = project.envrc_path().is_none()
        && (probe || cache.warm_mode().as_deref() != Some(MODE_HARNESS));
    let native = if try_native { build_native(project, cache, &caller)? } else { None };

    let (vars, mut dynamic, boot_scoped, mode, banner) = match native {
        Some(spec) => {
            let (vars, dynamic) = materialize(project, &spec, &caller)?;
            (vars, dynamic, spec.boot_scoped(), MODE_NATIVE, menu::banner(&spec))
        }
        None => {
            let (captured, dynamic) = run_harness(project, &caller, cache)?;
            let vars = captured_vars(captured, &caller);
            (vars, dynamic, false, MODE_HARNESS, None)
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
    let mut resolved = project.clone();
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

fn captured_vars(
    captured: BTreeMap<String, String>,
    caller: &BTreeMap<String, String>,
) -> BTreeMap<String, Value> {
    EnvProfile::from_captured(String::new(), String::new(), captured, caller, 0).vars
}

// ---------------------------------------------------------------- native

/// The installable to build for a native warm: `.simmer.toml`'s `flake`
/// (default `.`), with a bare fragment resolved under `devShells.<system>`.
pub fn native_installable(project: &Project) -> Option<String> {
    let flake = project
        .config
        .flake
        .clone()
        .or_else(|| project.root.join("flake.nix").is_file().then(|| ".".to_string()))?;
    let (base, fragment) = match flake.split_once('#') {
        Some((b, f)) => (b.to_string(), f.to_string()),
        None => (flake.clone(), String::new()),
    };
    let fragment = if fragment.is_empty() { "default".to_string() } else { fragment };
    let attr = if fragment.contains('.') {
        fragment
    } else {
        format!("devShells.{}.{}", current_system(), fragment)
    };
    Some(format!("{base}#{attr}"))
}

/// `nix build` the devshell; `Some(spec)` when its output carries an
/// `env.json`, `None` when this is not a native shell (the harness takes
/// over). The build output is pinned as the project's GC root either way.
fn build_native(
    project: &Project,
    cache: &Cache,
    caller: &BTreeMap<String, String>,
) -> Result<Option<EnvSpec>> {
    let Some(installable) = native_installable(project) else { return Ok(None) };
    let output = Command::new("nix")
        .args(["build", "--no-link", "--print-out-paths", "--profile"])
        .arg(cache.gcroot_path())
        .arg(&installable)
        .env_clear()
        .envs(caller)
        .current_dir(&project.root)
        .stderr(Stdio::inherit())
        .output()
        .context("running nix build")?;
    if !output.status.success() {
        // Not necessarily a native shell at all; the harness will report
        // the real error if the flake is genuinely broken.
        return Ok(None);
    }
    let out_path = String::from_utf8_lossy(&output.stdout).lines().next().unwrap_or("").to_string();
    let spec_path = Path::new(&out_path).join(ENV_SPEC_FILE);
    if !spec_path.is_file() {
        return Ok(None);
    }
    if let Err(e) = pin_inputs(project, cache, caller) {
        eprintln!("simmer: could not pin flake inputs: {e:#}");
    }
    EnvSpec::load(&spec_path).map(Some)
}

/// Root every flake input next to the shell's GC root, once per
/// `flake.lock` content, so a warm after `nix store gc` re-evaluates from
/// the store instead of re-fetching sources. The flake's own source tree
/// is skipped: it is the working copy, and the shell closure is already
/// pinned.
fn pin_inputs(project: &Project, cache: &Cache, caller: &BTreeMap<String, String>) -> Result<()> {
    let Ok(lock) = std::fs::read(project.root.join("flake.lock")) else { return Ok(()) };
    let lock_hash = hex(&Sha256::digest(&lock));
    let dir = cache.inputs_dir();
    let marker = dir.join("lock-hash");
    if std::fs::read_to_string(&marker).is_ok_and(|h| h == lock_hash) {
        return Ok(());
    }
    let flake = native_installable(project)
        .and_then(|i| i.split_once('#').map(|(base, _)| base.to_string()))
        .unwrap_or_else(|| ".".to_string());
    let output = Command::new("nix")
        .args(["flake", "archive", "--json", "--no-write-lock-file"])
        .arg(&flake)
        .env_clear()
        .envs(caller)
        .current_dir(&project.root)
        .stderr(Stdio::inherit())
        .output()
        .context("running nix flake archive")?;
    if !output.status.success() {
        bail!("nix flake archive failed");
    }
    let tree: serde_json::Value = serde_json::from_slice(&output.stdout).context("parsing archive json")?;
    let paths = input_paths(&tree);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    if !paths.is_empty() {
        let status = Command::new("nix")
            .args(["build", "--no-write-lock-file", "--out-link"])
            .arg(dir.join("in"))
            .args(&paths)
            .env_clear()
            .envs(caller)
            .current_dir(&project.root)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()
            .context("running nix build for input roots")?;
        if !status.success() {
            bail!("nix build failed to root {} inputs", paths.len());
        }
    }
    std::fs::write(marker, lock_hash)?;
    Ok(())
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
            Some(Value::WithCaller { prefix, sep }) => {
                Value::WithCaller { prefix: format!("{abs}:{prefix}"), sep }
            }
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
    for provider in spec.providers.iter().chain(config.providers.iter()) {
        match run_in_env(&provider.command, &env, &project.root) {
            Ok(out) if !out.is_empty() => {
                env.insert(provider.env.clone(), out.clone());
                vars.insert(provider.env.clone(), Value::Literal(out));
            }
            Ok(_) => eprintln!("simmer: provider {} produced nothing", provider.env),
            Err(e) => eprintln!("simmer: provider {} failed: {e:#}", provider.env),
        }
    }
    if let Some(hook) = spec.hooks.enter.as_deref().filter(|h| !h.trim().is_empty()) {
        match run_in_env(hook, &env, &project.root) {
            Ok(out) if !out.is_empty() => eprintln!("{out}"),
            Ok(_) => {}
            Err(e) => eprintln!("simmer: enter hook failed: {e:#}"),
        }
    }

    for report in configs::apply(&project.root, &spec.configs, &env)? {
        if !report.is_ok() {
            eprintln!("simmer: config {}: {:?}", report.output, report.status);
        }
    }
    Ok((vars, dynamic))
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
/// How providers and the legacy enter hook execute.
pub fn run_in_env(command: &str, env: &BTreeMap<String, String>, cwd: &Path) -> Result<String> {
    let output = Command::new("bash")
        .args(["--noprofile", "--norc", "-c", command])
        .env_clear()
        .envs(env)
        .current_dir(cwd)
        .stderr(Stdio::inherit())
        .output()
        .context("spawning bash")?;
    if !output.status.success() {
        bail!("exit status {}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim_end().to_string())
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
/// watches it declared. This is the only place bash sources project code.
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
        assert_eq!(
            native_installable(&Project::at(&dir).unwrap()).unwrap(),
            format!(".#devShells.{sys}.default")
        );
        std::fs::write(dir.join(".simmer.toml"), "flake = \".#dev\"\n").unwrap();
        assert_eq!(native_installable(&Project::at(&dir).unwrap()).unwrap(), format!(".#devShells.{sys}.dev"));
        std::fs::write(dir.join(".simmer.toml"), "flake = \"path:/x#devShells.aarch64-linux.ci\"\n").unwrap();
        assert_eq!(
            native_installable(&Project::at(&dir).unwrap()).unwrap(),
            "path:/x#devShells.aarch64-linux.ci"
        );
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
        let (vars, dynamic) = materialize(&project, &spec, &caller).unwrap();
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
        assert_eq!(
            parsed,
            vec![("A".into(), "1".into()), ("B".into(), "two".into()), ("C".into(), "3".into())]
        );
    }
}
