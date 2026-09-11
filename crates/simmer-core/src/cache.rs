//! On-disk profile cache: one directory per project under
//! `$XDG_CACHE_HOME/simmer/`.

use crate::profile::EnvProfile;
use crate::project::{hex, Project};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

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
    pub fn for_project(project: &Project) -> Result<Self> {
        let digest = Sha256::digest(project.root.as_os_str().as_encoded_bytes());
        let dir = base_dir()?.join(hex(&digest[..12]));
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating cache dir {}", dir.display()))?;
        Ok(Cache { dir })
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
        let tmp = self.dir.join("profile.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(profile)?)?;
        std::fs::rename(&tmp, self.profile_path())?;
        // Sidecar written after the profile so a reader never sees a
        // generation whose profile isn't there yet.
        let gen_tmp = self.dir.join("generation.tmp");
        std::fs::write(&gen_tmp, &profile.generation)?;
        std::fs::rename(&gen_tmp, self.dir.join("generation"))?;
        Ok(())
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
        let tmp = self.dir.join("statsig.tmp");
        std::fs::write(&tmp, format!("{sig}\n{generation}"))?;
        std::fs::rename(&tmp, self.dir.join("statsig"))?;
        Ok(())
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
        let tmp = self.dir.join("watches.tmp");
        let text: String =
            watches.iter().map(|w| format!("{}\n", w.display())).collect();
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, self.dir.join("watches"))?;
        Ok(())
    }

    /// Warm mode the last warm settled on (`native` / `harness`), so the
    /// daemon does not speculatively `nix build` a harness project on
    /// every save. Absent until the first warm.
    pub fn warm_mode(&self) -> Option<String> {
        let mode = std::fs::read_to_string(self.dir.join("mode")).ok()?;
        (!mode.is_empty()).then_some(mode)
    }

    pub fn store_warm_mode(&self, mode: &str) -> Result<()> {
        std::fs::write(self.dir.join("mode"), mode)?;
        Ok(())
    }

    /// Whether the shell itself declared a boot-scoped provider — part of
    /// the generation, so `export` must know it without reading env.json.
    pub fn boot_scoped(&self) -> bool {
        std::fs::read_to_string(self.dir.join("boot-scoped")).is_ok_and(|s| s == "1")
    }

    pub fn store_boot_scoped(&self, boot_scoped: bool) -> Result<()> {
        std::fs::write(self.dir.join("boot-scoped"), if boot_scoped { "1" } else { "0" })?;
        Ok(())
    }

    /// Trust model: nothing is warmed or applied for a project until a
    /// human allowed it (running code from a just-cloned repo is direnv's
    /// one non-negotiable lesson). For `.envrc` projects trust is bound to
    /// the file's content — editing it revokes trust, like direnv. For
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
        std::fs::write(self.dir.join("trust"), record)?;
        Ok(())
    }

    pub fn deny(&self) -> Result<()> {
        match std::fs::remove_file(self.dir.join("trust")) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).context("removing trust record"),
        }
    }

    /// Take the warm lock. Returns None while another warm holds a lock
    /// younger than 15 minutes; stale locks (crashed warms) are broken.
    pub fn try_lock_warm(&self) -> Result<Option<WarmLock>> {
        let path = self.dir.join("warm.lock");
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => Ok(Some(WarmLock { path })),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let age = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok());
                if age.is_some_and(|a| a.as_secs() > 900) {
                    std::fs::remove_file(&path).ok();
                    return self.try_lock_warm();
                }
                Ok(None)
            }
            Err(e) => Err(e).context("creating warm lock"),
        }
    }
}

pub struct WarmLock {
    path: PathBuf,
}

impl Drop for WarmLock {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).ok();
    }
}
