//! On-disk profile cache: one directory per project under
//! `$XDG_CACHE_HOME/simmer/`.

use crate::profile::EnvProfile;
use crate::project::{hex, Project};
use crate::supervise::{interrupted, Cancel, RunError};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::{Seek, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub struct Cache {
    pub dir: PathBuf,
}

/// `$XDG_CACHE_HOME/simmer` — shared by profiles and the registry.
pub fn base_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .context("neither XDG_CACHE_HOME nor HOME is set")?;
    Ok(base.join("simmer"))
}

impl Cache {
    /// The cache for `project`. Nothing is created on disk until the first
    /// write, so the per-prompt read path costs no `mkdir`.
    pub fn for_project(project: &Project) -> Result<Self> {
        let digest = Sha256::digest(project.root.as_os_str().as_encoded_bytes());
        let dir = base_dir()?.join(hex(&digest[..12]));
        Ok(Cache { dir })
    }

    /// Create the cache directory (idempotent).
    pub fn ensure_dir(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir).with_context(|| format!("creating cache dir {}", self.dir.display()))
    }

    /// Replace `name` atomically (write a temp file, rename over).
    fn write_file(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let path = self.dir.join(name);
        let tmp = self.dir.join(format!("{name}.tmp"));
        if let Err(e) = std::fs::write(&tmp, bytes) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(e).with_context(|| format!("writing {}", tmp.display()));
            }
            self.ensure_dir()?;
            std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
        }
        std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))
    }

    fn read_file(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.join(name)).ok()
    }

    /// Where full logs of failed warm steps are saved.
    pub fn logs_dir(&self) -> PathBuf {
        self.dir.join("logs")
    }

    pub fn profile_path(&self) -> PathBuf {
        self.dir.join("profile.json")
    }

    /// Nix profile path used as a GC root for the cached environment's
    /// closure — without it `nix store gc` could delete store paths the
    /// profile still references.
    pub fn gcroot_path(&self) -> PathBuf {
        self.dir.join("gcroot")
    }

    pub fn load(&self) -> Option<EnvProfile> {
        let text = std::fs::read_to_string(self.profile_path()).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Generation of the stored profile, from a tiny sidecar file — the
    /// export fast path compares this against the session without paying
    /// for a full profile parse.
    pub fn cached_generation(&self) -> Option<String> {
        let gen = std::fs::read_to_string(self.dir.join("generation")).ok()?;
        (!gen.is_empty()).then_some(gen)
    }

    pub fn store(&self, profile: &EnvProfile) -> Result<()> {
        self.write_file("profile.json", &serde_json::to_vec_pretty(profile)?)?;
        // Sidecar written after the profile so a reader never sees a
        // generation whose profile isn't there yet.
        self.write_file("generation", profile.generation.as_bytes())?;
        self.clear_failure();
        Ok(())
    }

    /// Remember that warming `generation` failed, so the prompt does not
    /// re-run (and re-wait for) the same failing build on every command;
    /// `simmer warm` retries explicitly, and any edit to a watched file is
    /// a new generation that is tried again automatically.
    pub fn record_failure(&self, generation: &str, message: &str) -> Result<()> {
        self.write_file("failed", format!("{generation}\n{message}").as_bytes())
    }

    /// The generation whose last warm failed, with the error message.
    pub fn failure(&self) -> Option<(String, String)> {
        let text = self.read_file("failed")?;
        let (generation, message) = text.split_once('\n').unwrap_or((text.as_str(), ""));
        (!generation.is_empty()).then(|| (generation.to_string(), message.to_string()))
    }

    pub fn clear_failure(&self) {
        let _ = std::fs::remove_file(self.dir.join("failed"));
    }

    /// The generation for the project's current state, hashing content
    /// only when the stat fingerprint changed since the last time — the
    /// hot-path reason a prompt costs microseconds with dozens of watched
    /// files. Falls back to [`Project::generation`] on a miss and records
    /// the answer.
    pub fn generation_of(&self, project: &Project) -> Result<String> {
        let sig = project.stat_signature();
        if let Some(known) = self.generation_for_stat(&sig) {
            return Ok(known);
        }
        let generation = project.generation()?;
        self.store_stat(&sig, &generation)?;
        Ok(generation)
    }

    fn generation_for_stat(&self, sig: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.dir.join("statsig")).ok()?;
        let (known_sig, generation) = text.split_once('\n')?;
        (known_sig == sig && !generation.is_empty()).then(|| generation.to_string())
    }

    fn store_stat(&self, sig: &str, generation: &str) -> Result<()> {
        self.write_file("statsig", format!("{sig}\n{generation}").as_bytes())
    }

    /// Directory of out-links pinning the flake's inputs, next to the
    /// shell's own GC root: after `nix store gc` a warm needs no network.
    pub fn inputs_dir(&self) -> PathBuf {
        self.dir.join("inputs")
    }

    /// Watches discovered during the last warm (`watch_file` calls in
    /// .envrc), absolute paths. Missing file = no dynamic watches.
    pub fn dynamic_watches(&self) -> Vec<PathBuf> {
        let Ok(text) = std::fs::read_to_string(self.dir.join("watches")) else {
            return Vec::new();
        };
        text.lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect()
    }

    pub fn store_dynamic_watches(&self, watches: &[PathBuf]) -> Result<()> {
        let text: String = watches.iter().map(|w| format!("{}\n", w.display())).collect();
        self.write_file("watches", text.as_bytes())
    }

    /// Warm mode the last warm settled on (`native` / `harness`), so the
    /// daemon does not speculatively `nix build` a harness project on
    /// every save. Absent until the first warm.
    pub fn warm_mode(&self) -> Option<String> {
        let mode = std::fs::read_to_string(self.dir.join("mode")).ok()?;
        (!mode.is_empty()).then_some(mode)
    }

    pub fn store_warm_mode(&self, mode: &str) -> Result<()> {
        self.write_file("mode", mode.as_bytes())
    }

    /// Whether the shell itself declared a boot-scoped provider — part of
    /// the generation, so `export` must know it without reading env.json.
    pub fn boot_scoped(&self) -> bool {
        std::fs::read_to_string(self.dir.join("boot-scoped")).is_ok_and(|s| s == "1")
    }

    pub fn store_boot_scoped(&self, boot_scoped: bool) -> Result<()> {
        self.write_file("boot-scoped", if boot_scoped { b"1" } else { b"0" })
    }

    /// Trust model: nothing is warmed or applied for a project until a
    /// human allowed it (running code from a just-cloned repo is direnv's
    /// one non-negotiable lesson). For `.envrc` projects trust is bound to
    /// the file's content — editing it revokes trust, like direnv. A
    /// `.envrc` a manifest outranks is never sourced, so it is no anchor. For
    /// flake-only projects trust sticks to the root: flake edits are
    /// routine and re-prompting would neuter the pre-warm daemon (the
    /// hook still runs through nix's evaluator rather than raw bash).
    pub fn trusted(&self, project: &Project) -> bool {
        let Ok(record) = std::fs::read_to_string(self.dir.join("trust")) else {
            return false;
        };
        match project.envrc_hash() {
            Some(hash) => record == format!("envrc:{hash}"),
            None => record == "root",
        }
    }

    pub fn allow(&self, project: &Project) -> Result<()> {
        let record = match project.envrc_hash() {
            Some(hash) => format!("envrc:{hash}"),
            None => "root".to_string(),
        };
        self.write_file("trust", record.as_bytes())
    }

    /// Revoke trust and remember the refusal, so simmer stops suggesting
    /// `simmer allow` for this project.
    pub fn deny(&self) -> Result<()> {
        self.write_file("trust", b"denied")
    }

    /// Whether the project was explicitly denied (as opposed to never
    /// allowed).
    pub fn denied(&self) -> bool {
        self.read_file("trust").is_some_and(|r| r == "denied")
    }

    /// Take the warm lock without waiting: `None` while another process
    /// holds it. The lock is an advisory `flock` the kernel releases when
    /// its holder exits, so a crashed warm never leaves a stale lock.
    pub fn try_lock_warm(&self) -> Result<Option<WarmLock>> {
        self.ensure_dir()?;
        WarmLock::try_acquire(&self.dir.join("warm.flock"))
    }

    /// Take the warm lock, *waiting* for a warm another process is running
    /// (a second shell, the daemon) to finish. `on_wait` is called once,
    /// with the holder's pid when known, before the first wait. Gives up
    /// on an interrupt or at `deadline`.
    pub fn lock_warm(
        &self,
        deadline: Option<Instant>,
        cancel: Option<&Cancel>,
        on_wait: impl FnOnce(Option<u32>),
    ) -> Result<WarmLock> {
        self.ensure_dir()?;
        let path = self.dir.join("warm.flock");
        let started = Instant::now();
        let mut on_wait = Some(on_wait);
        loop {
            if let Some(lock) = WarmLock::try_acquire(&path)? {
                return Ok(lock);
            }
            if let Some(notify) = on_wait.take() {
                notify(WarmLock::holder(&path));
            }
            if let Some(signal) = interrupted(cancel) {
                return Err(RunError::Interrupted { signal }.into());
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(RunError::TimedOut {
                    program: "waiting for another warm".into(),
                    after: started.elapsed(),
                }
                .into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// An exclusive `flock` on the project's warm lock file, released when
/// dropped (or by the kernel when the process dies).
pub struct WarmLock {
    _file: std::fs::File,
}

impl WarmLock {
    fn try_acquire(path: &Path) -> Result<Option<WarmLock>> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        // SAFETY: flock on a descriptor we own.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Ok(None);
            }
            return Err(err).with_context(|| format!("locking {}", path.display()));
        }
        // Record the holder for anyone who has to wait on us.
        let _ = file.set_len(0);
        let _ = file.rewind();
        let _ = write!(file, "{}", std::process::id());
        Ok(Some(WarmLock { _file: file }))
    }

    fn holder(path: &Path) -> Option<u32> {
        std::fs::read_to_string(path).ok()?.trim().parse().ok()
    }
}
