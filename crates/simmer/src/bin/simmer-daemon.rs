//! `simmer-daemon` — the optional pre-warm daemon (`simmer daemon` runs it).
//!
//! A separate binary so that the file-watching machinery (and, on macOS,
//! the CoreServices framework it links) is loaded only by the daemon,
//! never by the `simmer export` that runs on every prompt.
//!
//! No IPC: the disk cache is the only interface — `simmer export` reads it
//! in well under a millisecond, so all the daemon has to do is keep it
//! fresh. It watches every registered project's watched files and re-warms
//! in the background the moment one changes, which means the multi-second
//! nix eval happens when you *save flake.nix*, not when you next open a
//! shell.

use anyhow::{Context, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use simmer_core::{
    cache::Cache,
    profile::PROFILE_FORMAT,
    project::{glob_base, glob_match, Project},
    registry, supervise, warm,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const DEBOUNCE: Duration = Duration::from_millis(500);
const REGISTRY_RESCAN: Duration = Duration::from_secs(30);

fn main() {
    // Rust ignores SIGPIPE; restore the default so a closed pipe ends us
    // quietly.
    // SAFETY: setting a signal disposition before any threads start.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    if std::env::args().nth(1).is_some_and(|a| a == "--help" || a == "-h") {
        println!("simmer-daemon: watch every allowed project simmer has warmed and re-warm it\nwhen a watched file changes. Usually started as `simmer daemon`.");
        return;
    }
    if let Err(e) = run() {
        eprintln!("simmer daemon: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    // SIGTERM/SIGINT stop a warm in progress (its whole process group)
    // before the daemon exits; nothing it started outlives it.
    supervise::signals::install().context("installing signal handlers")?;
    let (tx, rx) = mpsc::channel::<PathBuf>();
    let mut watcher = RecommendedWatcher::new(
        move |event: notify::Result<notify::Event>| {
            if let Ok(event) = event {
                for path in event.paths {
                    tx.send(path).ok();
                }
            }
        },
        notify::Config::default(),
    )
    .context("creating file watcher")?;

    // dir being watched -> projects interested in it; watched-file paths
    // are matched exactly against event paths.
    let mut watched_dirs: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
    let mut interesting: HashSet<PathBuf> = HashSet::new();
    // Wildcard watches: (project root, pattern) — the pattern's base
    // directory is watched recursively, so a file that did not exist at
    // the last warm still triggers one.
    let mut globs: Vec<(PathBuf, String)> = Vec::new();
    let mut known: HashSet<PathBuf> = HashSet::new();
    let mut pending: HashMap<PathBuf, Instant> = HashMap::new();
    let mut last_rescan = Instant::now() - REGISTRY_RESCAN;

    eprintln!("simmer daemon: watching registered projects (cache is the interface, no socket)");
    loop {
        if last_rescan.elapsed() >= REGISTRY_RESCAN {
            last_rescan = Instant::now();
            for root in registry::list() {
                if !known.insert(root.clone()) {
                    continue;
                }
                match Project::at(&root) {
                    Ok(mut project) => {
                        // The daemon executes project code; untrusted
                        // projects stay untracked until `simmer allow`.
                        let Ok(cache) = Cache::for_project(&project) else { continue };
                        if !cache.trusted(&project) {
                            known.remove(&root); // re-check next rescan
                            continue;
                        }
                        project.extend_watches(cache.dynamic_watches());
                        for file in &project.watched {
                            let path = root.join(file);
                            // Watch the parent directory: editors replace
                            // files atomically, and a watch on the old
                            // inode would go quiet after the first save.
                            let dir = path.parent().unwrap_or(&root).to_path_buf();
                            if !watched_dirs.contains_key(&dir) {
                                if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
                                    eprintln!("simmer daemon: cannot watch {}: {e}", dir.display());
                                    continue;
                                }
                            }
                            watched_dirs.entry(dir).or_default().insert(root.clone());
                            interesting.insert(path);
                        }
                        for pattern in project.watch_globs() {
                            let base = root.join(glob_base(pattern));
                            if let Err(e) = watcher.watch(&base, RecursiveMode::Recursive) {
                                eprintln!("simmer daemon: cannot watch {}: {e}", base.display());
                                continue;
                            }
                            globs.push((root.clone(), pattern.to_string()));
                        }
                        eprintln!("simmer daemon: tracking {}", root.display());
                        // Catch up on anything that changed while no
                        // daemon was watching (or a simmer upgrade).
                        pending.insert(root.clone(), Instant::now());
                    }
                    Err(e) => eprintln!("simmer daemon: skipping {}: {e:#}", root.display()),
                }
            }
        }

        if let Some(signal) = supervise::signals::pending() {
            eprintln!("simmer daemon: stopping ({})", supervise::signal_name(signal));
            std::process::exit(128 + signal);
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(path) => {
                if interesting.contains(&path) {
                    if let Some(dir) = path.parent() {
                        for root in watched_dirs.get(dir).into_iter().flatten() {
                            pending.insert(root.clone(), Instant::now() + DEBOUNCE);
                        }
                    }
                }
                for (root, pattern) in &globs {
                    let matched = path.strip_prefix(root).is_ok_and(|rel| glob_match(pattern, &rel.to_string_lossy()));
                    if matched {
                        pending.insert(root.clone(), Instant::now() + DEBOUNCE);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        let due: Vec<PathBuf> =
            pending.iter().filter(|(_, deadline)| Instant::now() >= **deadline).map(|(root, _)| root.clone()).collect();
        for root in due {
            pending.remove(&root);
            if let Err(e) = rewarm_if_stale(&root) {
                if supervise::is_interrupt(&e) {
                    break;
                }
                eprintln!("simmer daemon: warm failed for {}: {e:#}", root.display());
            }
        }
    }
    Ok(())
}

fn rewarm_if_stale(root: &Path) -> Result<()> {
    let mut project = Project::at(root)?;
    let cache = Cache::for_project(&project)?;
    if !cache.trusted(&project) {
        return Ok(()); // trust revoked (or .envrc edited) since tracking began
    }
    project.extend_watches(cache.dynamic_watches());
    // Expanded here (a tree walk is fine off the prompt) so a file that
    // appeared since the last warm counts as a change.
    project.extend_watches(project.expand_globs());
    project.extend_boot_scoped(cache.boot_scoped());
    let generation = project.generation()?;
    if cache.load().is_some_and(|p| p.generation == generation && p.format >= PROFILE_FORMAT) {
        return Ok(());
    }
    let Some(_lock) = cache.try_lock_warm()? else {
        return Ok(()); // a foreground warm already has it
    };
    eprintln!("simmer daemon: warming {}", root.display());
    let profile = warm::warm_with(&project, &cache, false)?;
    eprintln!(
        "simmer daemon: warmed {} ({} vars, {}ms, {})",
        root.display(),
        profile.vars.len(),
        profile.warm_ms,
        profile.mode
    );
    Ok(())
}
