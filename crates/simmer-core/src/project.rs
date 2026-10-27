//! Project discovery and generation hashing.

use crate::config::{boot_id, Config, MANIFEST_NAMES};
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Files whose content defines the environment; `simmer.toml`'s `watch`
/// list adds project-specific ones. A `watch` entry with a wildcard
/// (`nix/**/*.nix`) is a pattern: expanded at warm time into the dynamic
/// watch set, and watched as a directory tree by the daemon, so the
/// prompt never walks the tree.
pub const WATCHED_FILES: &[&str] = &["flake.nix", "flake.lock", "devshell.toml", ".simmer.toml", "simmer.toml"];

pub fn is_glob(pattern: &str) -> bool {
    pattern.contains(['*', '?'])
}

/// Match a `/`-separated pattern against a relative path: `**` spans any
/// number of components, `*` and `?` stay within one.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match_components(&pat, &parts)
}

fn match_components(pat: &[&str], parts: &[&str]) -> bool {
    match pat.split_first() {
        None => parts.is_empty(),
        Some((&"**", rest)) => (0..=parts.len()).any(|i| match_components(rest, &parts[i..])),
        Some((first, rest)) => match parts.split_first() {
            Some((part, tail)) => match_component(first, part) && match_components(rest, tail),
            None => false,
        },
    }
}

fn match_component(pat: &str, text: &str) -> bool {
    fn go(p: &[char], t: &[char]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some(('*', rest)) => (0..=t.len()).any(|i| go(rest, &t[i..])),
            Some(('?', rest)) => !t.is_empty() && go(rest, &t[1..]),
            Some((c, rest)) => t.first() == Some(c) && go(rest, &t[1..]),
        }
    }
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    go(&p, &t)
}

/// The literal directory a pattern starts in (`nix/**/*.nix` → `nix`).
pub fn glob_base(pattern: &str) -> PathBuf {
    pattern.split('/').take_while(|c| !is_glob(c)).collect()
}

fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            if entry.file_name() != ".git" {
                walk_files(&path, out);
            }
        } else if kind.is_file() {
            out.push(path);
        }
    }
}

/// Files whose presence makes a directory a project root.
pub const ROOT_MARKERS: &[&str] = &["flake.nix", ".envrc", ".simmer.toml", "simmer.toml"];

/// Whether `dir` holds any of the [`ROOT_MARKERS`].
pub fn is_project_root(dir: &Path) -> bool {
    ROOT_MARKERS.iter().any(|m| dir.join(m).is_file())
}

#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub config: Config,
    /// Watched files that exist right now, relative to `root`.
    pub watched: Vec<PathBuf>,
    /// Whether the generation also keys on the kernel boot id — true when
    /// a provider (manifest or shell-declared) probes per-boot state.
    pub boot_scoped: bool,
    /// Whether a `.simmer.toml` declares how simmer loads this project.
    /// A manifest outranks a `.envrc`: the `.envrc` is then left to the
    /// direnv-driven callers that still read it (a CI builder's
    /// `direnv exec`), and simmer never sources it.
    pub has_manifest: bool,
}

impl Project {
    /// Walk upward from `dir` to the first directory holding a flake.nix
    /// or a .envrc — either one roots a project.
    ///
    /// A root whose manifest does not parse is reported as `None` here;
    /// use [`Self::locate`] to see the error instead.
    pub fn discover(dir: &Path) -> Option<Self> {
        Self::locate(dir).ok().flatten()
    }

    /// Like [`Self::discover`], but a project root whose `.simmer.toml`
    /// cannot be read or parsed is an error (so the user hears about the
    /// typo) rather than "no project here".
    pub fn locate(dir: &Path) -> Result<Option<Self>> {
        let mut cur = Some(dir);
        while let Some(d) = cur {
            if is_project_root(d) {
                return Self::at(d).map(Some);
            }
            cur = d.parent();
        }
        Ok(None)
    }

    pub fn at(root: &Path) -> Result<Self> {
        let config = Config::load(root)?;
        let has_manifest = MANIFEST_NAMES.iter().any(|m| root.join(m).is_file());
        // A .envrc the manifest outranks does not define the environment;
        // editing it must not invalidate the cache.
        let envrc: &[&str] = if has_manifest { &[] } else { &[".envrc"] };
        let mut watched: Vec<PathBuf> = WATCHED_FILES
            .iter()
            .chain(envrc.iter())
            .map(PathBuf::from)
            .chain(config.watch.iter().filter(|w| !is_glob(w)).map(PathBuf::from))
            .filter(|f| root.join(f).is_file())
            .collect();
        watched.sort();
        watched.dedup();
        let boot_scoped = config.boot_scoped();
        Ok(Project { root: root.to_path_buf(), config, watched, boot_scoped, has_manifest })
    }

    /// The manifest's wildcard `watch` entries.
    pub fn watch_globs(&self) -> Vec<&str> {
        self.config.watch.iter().map(String::as_str).filter(|w| is_glob(w)).collect()
    }

    /// Every file under the root matching a wildcard `watch` entry,
    /// relative to the root. Walks the tree — warm and daemon only.
    pub fn expand_globs(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for pattern in self.watch_globs() {
            let mut files = Vec::new();
            walk_files(&self.root.join(glob_base(pattern)), &mut files);
            for file in files {
                let Ok(rel) = file.strip_prefix(&self.root) else { continue };
                if glob_match(pattern, &rel.to_string_lossy()) {
                    found.push(rel.to_path_buf());
                }
            }
        }
        found.sort();
        found.dedup();
        found
    }

    /// Fold in watches discovered by a previous warm (absolute paths from
    /// `watch_file` in .envrc). Must be applied before [`Self::generation`]
    /// for projects with dynamic watches.
    pub fn extend_watches(&mut self, extra: Vec<PathBuf>) {
        self.watched.extend(extra);
        self.watched.sort();
        self.watched.dedup();
    }

    /// Fold in boot scoping recorded by a previous warm (a native shell's
    /// own providers). Like [`Self::extend_watches`], apply before
    /// [`Self::generation`].
    pub fn extend_boot_scoped(&mut self, boot_scoped: bool) {
        self.boot_scoped |= boot_scoped;
    }

    /// The project's `.envrc`, whoever reads it. Use [`Self::sourced_envrc`]
    /// to decide how simmer itself loads the project.
    pub fn envrc_path(&self) -> Option<PathBuf> {
        let path = self.root.join(".envrc");
        path.is_file().then_some(path)
    }

    /// The `.envrc` simmer sources: present only when no manifest
    /// outranks it. With one, the project loads natively (or through the
    /// manifest's harness lines) whatever the `.envrc` says.
    pub fn sourced_envrc(&self) -> Option<PathBuf> {
        self.envrc_path().filter(|_| !self.has_manifest)
    }

    /// Content hash of the sourced .envrc, the trust anchor for
    /// direnv-style projects.
    pub fn envrc_hash(&self) -> Option<String> {
        let content = std::fs::read(self.sourced_envrc()?).ok()?;
        Some(hex(&Sha256::digest(&content)))
    }

    /// Generation hash: project identity plus the content of every watched
    /// file (plus the boot id when a provider probes per-boot state). Any
    /// change is a new generation — this is the entire invalidation model,
    /// deliberately independent of Nix's own eval cache (which dirty git
    /// trees defeat).
    /// Cheap fingerprint of the watched set: identity, mtime, size, inode
    /// of every file — a few hundred nanoseconds per file against
    /// reading and hashing its content. Maps to a [`Self::generation`]
    /// through the cache so the prompt hashes content only after a save.
    pub fn stat_signature(&self) -> String {
        use std::os::unix::fs::MetadataExt;
        let mut hasher = Sha256::new();
        hasher.update(self.root.as_os_str().as_encoded_bytes());
        for file in &self.watched {
            hasher.update([0]);
            hasher.update(file.as_os_str().as_encoded_bytes());
            hasher.update([0]);
            match std::fs::metadata(self.root.join(file)) {
                Ok(meta) => {
                    hasher.update(meta.mtime().to_le_bytes());
                    hasher.update(meta.mtime_nsec().to_le_bytes());
                    hasher.update(meta.size().to_le_bytes());
                    hasher.update(meta.ino().to_le_bytes());
                }
                Err(_) => hasher.update(b"<simmer:absent>"),
            }
        }
        if self.boot_scoped {
            hasher.update([0]);
            hasher.update(boot_id().as_bytes());
        }
        hex(&hasher.finalize()[..16])
    }

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
        if self.boot_scoped {
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
    fn glob_matching_spans_components_only_with_double_star() {
        assert!(glob_match("nix/**/*.nix", "nix/repo/devshells.nix"));
        assert!(glob_match("nix/**/*.nix", "nix/top.nix"));
        assert!(!glob_match("nix/*.nix", "nix/repo/devshells.nix"));
        assert!(!glob_match("nix/**/*.nix", "flake.nix"));
        assert!(glob_match("**/*.toml", "a/b/c.toml"));
        assert!(glob_match("src/?.rs", "src/a.rs") && !glob_match("src/?.rs", "src/ab.rs"));
        assert_eq!(glob_base("nix/**/*.nix"), PathBuf::from("nix"));
        assert_eq!(glob_base("**/*.nix"), PathBuf::from(""));
    }

    #[test]
    fn glob_watches_expand_at_warm_not_at_discovery() {
        let dir = scratch("glob");
        std::fs::create_dir_all(dir.join("nix/deep")).unwrap();
        std::fs::write(dir.join("flake.nix"), "x").unwrap();
        std::fs::write(dir.join("nix/a.nix"), "a").unwrap();
        std::fs::write(dir.join("nix/deep/b.nix"), "b").unwrap();
        std::fs::write(dir.join("nix/deep/notes.md"), "m").unwrap();
        std::fs::write(dir.join(".simmer.toml"), "watch = [\"nix/**/*.nix\"]\n").unwrap();
        let p = Project::at(&dir).unwrap();
        // The prompt-side watched set carries no expansion.
        assert!(!p.watched.iter().any(|w| w.starts_with("nix")));
        assert_eq!(p.watch_globs(), vec!["nix/**/*.nix"]);
        assert_eq!(p.expand_globs(), vec![PathBuf::from("nix/a.nix"), PathBuf::from("nix/deep/b.nix")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stat_signature_tracks_edits_without_reading_content() {
        let dir = scratch("statsig");
        std::fs::write(dir.join("flake.nix"), "a").unwrap();
        let p = Project::at(&dir).unwrap();
        let s1 = p.stat_signature();
        assert_eq!(s1, p.stat_signature());
        std::fs::write(dir.join("flake.nix"), "bb").unwrap();
        assert_ne!(s1, Project::at(&dir).unwrap().stat_signature());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_broken_manifest_is_an_error_not_an_absent_project() {
        let dir = scratch("broken");
        std::fs::write(dir.join("flake.nix"), "x").unwrap();
        std::fs::write(dir.join(".simmer.toml"), "[[provider]]\n").unwrap();
        assert!(Project::discover(&dir).is_none());
        let err = Project::locate(&dir).unwrap_err();
        assert!(format!("{err:#}").contains(".simmer.toml"), "{err:#}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_manifest_outranks_the_envrc() {
        let dir = scratch("outrank");
        std::fs::write(dir.join("flake.nix"), "x").unwrap();
        std::fs::write(dir.join(".envrc"), "use flake\n").unwrap();
        let p = Project::at(&dir).unwrap();
        assert!(p.sourced_envrc().is_some() && p.envrc_hash().is_some());
        assert!(p.watched.contains(&PathBuf::from(".envrc")));

        std::fs::write(dir.join(".simmer.toml"), "watch = []\n").unwrap();
        let p = Project::at(&dir).unwrap();
        assert!(p.envrc_path().is_some(), "the .envrc stays for direnv callers");
        assert!(p.sourced_envrc().is_none() && p.envrc_hash().is_none());
        assert!(!p.watched.contains(&PathBuf::from(".envrc")));
        // Editing an outranked .envrc is not a new generation.
        let g1 = p.generation().unwrap();
        std::fs::write(dir.join(".envrc"), "use flake .#other\n").unwrap();
        assert_eq!(g1, Project::at(&dir).unwrap().generation().unwrap());
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
