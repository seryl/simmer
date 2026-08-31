//! Project discovery and generation hashing.

use crate::config::{boot_id, Config};
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Files whose content defines the environment; `simmer.toml`'s `watch`
/// list adds project-specific ones.
pub const WATCHED_FILES: &[&str] =
    &["flake.nix", "flake.lock", "devshell.toml", ".simmer.toml", "simmer.toml"];

#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub config: Config,
    /// Watched files that exist right now, relative to `root`.
    pub watched: Vec<PathBuf>,
}

impl Project {
    /// Walk upward from `dir` to the first directory holding a flake.nix
    /// or a .envrc — either one roots a project.
    pub fn discover(dir: &Path) -> Option<Self> {
        let mut cur = Some(dir);
        while let Some(d) = cur {
            if d.join("flake.nix").is_file()
                || d.join(".envrc").is_file()
                || d.join(".simmer.toml").is_file()
                || d.join("simmer.toml").is_file()
            {
                return Self::at(d).ok();
            }
            cur = d.parent();
        }
        None
    }

    pub fn at(root: &Path) -> Result<Self> {
        let config = Config::load(root)?;
        let mut watched: Vec<PathBuf> = WATCHED_FILES
            .iter()
            .chain([".envrc"].iter())
            .map(PathBuf::from)
            .chain(config.watch.iter().map(PathBuf::from))
            .filter(|f| root.join(f).is_file())
            .collect();
        watched.sort();
        watched.dedup();
        Ok(Project { root: root.to_path_buf(), config, watched })
    }

    /// Fold in watches discovered by a previous warm (absolute paths from
    /// `watch_file` in .envrc). Must be applied before [`Self::generation`]
    /// for projects with dynamic watches.
    pub fn extend_watches(&mut self, extra: Vec<PathBuf>) {
        self.watched.extend(extra);
        self.watched.sort();
        self.watched.dedup();
    }

    pub fn envrc_path(&self) -> Option<PathBuf> {
        let path = self.root.join(".envrc");
        path.is_file().then_some(path)
    }

    /// Content hash of .envrc, the trust anchor for direnv-style projects.
    pub fn envrc_hash(&self) -> Option<String> {
        let content = std::fs::read(self.envrc_path()?).ok()?;
        Some(hex(&Sha256::digest(&content)))
    }

    /// Generation hash: project identity plus the content of every watched
    /// file (plus the boot id when a provider probes per-boot state). Any
    /// change is a new generation — this is the entire invalidation model,
    /// deliberately independent of Nix's own eval cache (which dirty git
    /// trees defeat).
    pub fn generation(&self) -> Result<String> {
        let mut hasher = Sha256::new();
        hasher.update(self.root.as_os_str().as_encoded_bytes());
        for file in &self.watched {
            let path = self.root.join(file);
            hasher.update([0]);
            hasher.update(file.as_os_str().as_encoded_bytes());
            hasher.update([0]);
            // A watched file that vanished still contributes (as absence):
            // dynamic watches may point at files a .envrc only sometimes
            // creates, and generation must stay total.
            match std::fs::read(&path) {
                Ok(content) => hasher.update(&content),
                Err(_) => hasher.update(b"<simmer:absent>"),
            }
        }
        if self.config.boot_scoped() {
            hasher.update([0]);
            hasher.update(boot_id().as_bytes());
        }
        let digest = hasher.finalize();
        Ok(hex(&digest[..16]))
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("simmer-test-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn generation_changes_with_content_and_is_stable() {
        let dir = scratch("gen");
        std::fs::write(dir.join("flake.nix"), "a").unwrap();
        let g1 = Project::at(&dir).unwrap().generation().unwrap();
        assert_eq!(g1, Project::at(&dir).unwrap().generation().unwrap());
        std::fs::write(dir.join("flake.nix"), "b").unwrap();
        assert_ne!(g1, Project::at(&dir).unwrap().generation().unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn config_watch_entries_join_watched_set() {
        let dir = scratch("watch");
        std::fs::create_dir_all(dir.join("nix")).unwrap();
        std::fs::write(dir.join("flake.nix"), "x").unwrap();
        std::fs::write(dir.join("nix/shell.nix"), "y").unwrap();
        std::fs::write(dir.join("simmer.toml"), "watch = [\"nix/shell.nix\"]\n").unwrap();
        let p = Project::at(&dir).unwrap();
        assert!(p.watched.contains(&PathBuf::from("nix/shell.nix")));
        // Editing an extra-watched file is a new generation.
        let g1 = p.generation().unwrap();
        std::fs::write(dir.join("nix/shell.nix"), "z").unwrap();
        assert_ne!(g1, Project::at(&dir).unwrap().generation().unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
