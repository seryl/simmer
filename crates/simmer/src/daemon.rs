//! The pre-warm daemon.
//!
//! No IPC: the disk cache is the only interface — `simmer export` reads it
//! in well under a millisecond, so all the daemon has to do is keep it
//! fresh. It watches every registered project's watched files and re-warms
//! in the background the moment one changes, which means the multi-second
//! nix eval happens when you *save flake.nix*, not when you next open a
//! shell.

use anyhow::{Context, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use simmer_core::{cache::Cache, project::Project, registry, warm};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const DEBOUNCE: Duration = Duration::from_millis(500);
const REGISTRY_RESCAN: Duration = Duration::from_secs(30);

pub fn run() -> Result<()> {
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
                        eprintln!("simmer daemon: tracking {}", root.display());
                    }
                    Err(e) => eprintln!("simmer daemon: skipping {}: {e:#}", root.display()),
                }
            }
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
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        let due: Vec<PathBuf> = pending
            .iter()
            .filter(|(_, deadline)| Instant::now() >= **deadline)
            .map(|(root, _)| root.clone())
            .collect();
        for root in due {
            pending.remove(&root);
            if let Err(e) = rewarm_if_stale(&root) {
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
    project.extend_boot_scoped(cache.boot_scoped());
    let generation = project.generation()?;
    if cache.load().is_some_and(|p| p.generation == generation) {
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

/// systemd user unit for the daemon; `simmer daemon --print-systemd`.
pub fn systemd_unit() -> String {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "simmer".to_string());
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
