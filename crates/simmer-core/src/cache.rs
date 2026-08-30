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
