//! Generated config files: materialize an [`EnvSpec`]'s `configs` into
//! the project root and audit what is there.
//!
//! No shell hook. Files are linked (or copied) at warm time, listed in
//! a marked block of the project's `.gitignore` (tracked, so every clone
//! and CI ignores them the same way), and the environment's GC root keeps
//! their store sources alive — a linked config can no longer dangle after
//! `nix store gc`.

use crate::envspec::{ConfigFile, Mode};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// State of one managed file relative to its declaration.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Status {
    Ok,
    /// Nothing at the output path.
    Missing,
    /// A symlink whose target no longer exists (the store path was
    /// garbage-collected).
    Dangling {
        target: String,
    },
    /// Present but not what the declaration says (other target, or
    /// different content for copies).
    Drifted {
        actual: String,
    },
    /// A regular file sits where a link is declared — someone's own
    /// file; never overwritten.
    Unmanaged,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Report {
    pub output: String,
    #[serde(flatten)]
    pub status: Status,
}

impl Report {
    pub fn is_ok(&self) -> bool {
        self.status == Status::Ok
    }
}

/// Audit every declared file. Never modifies anything.
pub fn check(root: &Path, configs: &[ConfigFile]) -> Vec<Report> {
    configs.iter().map(|cfg| Report { output: cfg.output.clone(), status: status_of(root, cfg, None) }).collect()
}

fn status_of(root: &Path, cfg: &ConfigFile, vars: Option<&BTreeMap<String, String>>) -> Status {
    let path = root.join(&cfg.output);
    let Ok(meta) = std::fs::symlink_metadata(&path) else { return Status::Missing };
    let is_link = meta.file_type().is_symlink();
    match cfg.mode {
        Mode::Link => {
            if !is_link {
                return Status::Unmanaged;
            }
            let target = std::fs::read_link(&path).unwrap_or_default();
            if !target.exists() {
                return Status::Dangling { target: target.display().to_string() };
            }
            if target == Path::new(&cfg.source) {
                Status::Ok
            } else {
                Status::Drifted { actual: target.display().to_string() }
            }
        }
        Mode::Copy | Mode::Template => {
            if is_link {
                // A leftover link from an earlier link-mode declaration
                // (or another tool): the declaration says a real file
                // belongs here, so this is drift, not someone's file.
                let target = std::fs::read_link(&path).unwrap_or_default();
                return Status::Drifted { actual: target.display().to_string() };
            }
            let expected = match rendered(cfg, vars) {
                Ok(text) => text,
                Err(e) => return Status::Drifted { actual: format!("source unreadable: {e}") },
            };
            match std::fs::read(&path) {
                Ok(actual) if actual == expected => Status::Ok,
                Ok(_) => Status::Drifted { actual: "content differs".into() },
                Err(e) => Status::Drifted { actual: format!("unreadable: {e}") },
            }
        }
    }
}

/// Materialize every declared file, then record them in
/// `.git/info/exclude`. `vars` feeds template substitution
/// (`@PRJ_ROOT@`, provider outputs, any finished-environment variable).
/// Returns the post-apply state; an `Unmanaged` entry is reported, not
/// clobbered.
pub fn apply(root: &Path, configs: &[ConfigFile], vars: &BTreeMap<String, String>) -> Result<Vec<Report>> {
    let mut reports = Vec::with_capacity(configs.len());
    for cfg in configs {
        let path = root.join(&cfg.output);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let status = match cfg.mode {
            Mode::Link => match status_of(root, cfg, None) {
                status @ (Status::Ok | Status::Unmanaged) => status,
                _ => {
                    remove_if_present(&path)?;
                    std::os::unix::fs::symlink(&cfg.source, &path)
                        .with_context(|| format!("linking {}", path.display()))?;
                    status_of(root, cfg, None)
                }
            },
            // Render once; rewrite only when the file differs. What was
            // just written is what was declared, so no re-read follows.
            Mode::Copy | Mode::Template => {
                let expected = rendered(cfg, Some(vars))?;
                let is_file = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_file());
                let current = if is_file { std::fs::read(&path).ok() } else { None };
                if current.as_deref() != Some(expected.as_slice()) {
                    remove_if_present(&path)?;
                    std::fs::write(&path, &expected).with_context(|| format!("writing {}", path.display()))?;
                }
                Status::Ok
            }
        };
        reports.push(Report { output: cfg.output.clone(), status });
    }
    let managed: Vec<String> = configs.iter().map(|c| c.output.clone()).collect();
    update_gitignore(root, &managed)?;
    remove_git_exclude_block(root)?;
    Ok(reports)
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

fn rendered(cfg: &ConfigFile, vars: Option<&BTreeMap<String, String>>) -> Result<Vec<u8>> {
    let bytes = std::fs::read(&cfg.source).with_context(|| format!("reading {}", cfg.source))?;
    Ok(match cfg.mode {
        Mode::Template => {
            let text = String::from_utf8(bytes).context("template is not UTF-8")?;
            substitute(&text, vars.unwrap_or(&BTreeMap::new())).into_bytes()
        }
        _ => bytes,
    })
}

/// Replace `@NAME@` with `vars[NAME]`, and `@NAME:-fallback@` with the
/// fallback when `NAME` is unset or empty. Unknown names without a
/// fallback are left verbatim so a template can carry literal `@` text
/// and so a missing provider output is visible in the file rather than
/// silently blank.
pub fn substitute(text: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('@') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('@') {
            Some(end) => {
                let token = &after[..end];
                let (name, fallback) = match token.split_once(":-") {
                    Some((n, f)) => (n, Some(f)),
                    None => (token, None),
                };
                let is_ident = !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
                let value = vars.get(name).filter(|v| !v.is_empty()).map(String::as_str);
                match (is_ident, value, fallback) {
                    (true, Some(value), _) => {
                        out.push_str(value);
                        rest = &after[end + 1..];
                    }
                    (true, None, Some(fallback)) => {
                        out.push_str(fallback);
                        rest = &after[end + 1..];
                    }
                    _ => {
                        out.push('@');
                        rest = after;
                    }
                }
            }
            None => {
                out.push('@');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

const EXCLUDE_BEGIN: &str = "# simmer:begin (generated config files; do not edit this block)";
const EXCLUDE_END: &str = "# simmer:end";

/// Where git reads per-repository excludes for `root`, following
/// worktree indirection (`.git` file → gitdir → commondir). None when
/// `root` is not a git checkout.
pub fn git_exclude_path(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    let meta = std::fs::metadata(&dot_git).ok()?;
    let git_dir = if meta.is_dir() {
        dot_git
    } else {
        let text = std::fs::read_to_string(&dot_git).ok()?;
        let rel = text.trim().strip_prefix("gitdir:")?.trim();
        let dir = root.join(rel);
        match std::fs::read_to_string(dir.join("commondir")) {
            Ok(common) => dir.join(common.trim()),
            Err(_) => dir,
        }
    };
    Some(git_dir.join("info").join("exclude"))
}

/// Everything outside the simmer block of `text`, plus a trailing newline
/// when non-empty.
fn without_block(text: &str) -> (String, bool) {
    let mut kept: Vec<&str> = Vec::new();
    let mut in_block = false;
    let mut found = false;
    for line in text.lines() {
        if line == EXCLUDE_BEGIN {
            in_block = true;
            found = true;
            continue;
        }
        if line == EXCLUDE_END {
            in_block = false;
            continue;
        }
        if !in_block {
            kept.push(line);
        }
    }
    let mut out = kept.join("\n");
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    (out, found)
}

/// Rewrite the simmer block in the project's `.gitignore` to list exactly
/// `outputs`. Leaves everything outside the block untouched, creates the
/// file when absent, never runs git.
pub fn update_gitignore(root: &Path, outputs: &[String]) -> Result<()> {
    let path = root.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let (mut text, _) = without_block(&existing);
    if !outputs.is_empty() {
        text.push_str(EXCLUDE_BEGIN);
        text.push('\n');
        for output in outputs {
            text.push('/');
            text.push_str(output);
            text.push('\n');
        }
        text.push_str(EXCLUDE_END);
        text.push('\n');
    }
    if text != existing {
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// Drop a simmer block that an earlier version wrote to `.git/info/exclude`
/// (the block now lives in `.gitignore`). No-op when there is none.
pub fn remove_git_exclude_block(root: &Path) -> Result<()> {
    let Some(path) = git_exclude_path(root) else { return Ok(()) };
    let Ok(existing) = std::fs::read_to_string(&path) else { return Ok(()) };
    let (text, found) = without_block(&existing);
    if found {
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("simmer-configs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("store")).unwrap();
        std::fs::create_dir_all(dir.join("root/.git/info")).unwrap();
        dir
    }

    fn cfg(dir: &Path, output: &str, mode: Mode) -> ConfigFile {
        ConfigFile {
            output: output.to_string(),
            source: dir.join("store").join(output.trim_start_matches('.')).display().to_string(),
            mode,
        }
    }

    #[test]
    fn dangling_drifted_missing_unmanaged_are_distinguished() {
        let dir = scratch("check");
        let root = dir.join("root");
        std::fs::write(dir.join("store/cog.toml"), "x").unwrap();
        let cog = cfg(&dir, "cog.toml", Mode::Link);
        // Missing.
        assert_eq!(check(&root, std::slice::from_ref(&cog))[0].status, Status::Missing);
        // Dangling: the store path was collected.
        std::os::unix::fs::symlink("/nix/store/xxxx-cog.toml", root.join("cog.toml")).unwrap();
        assert!(matches!(check(&root, std::slice::from_ref(&cog))[0].status, Status::Dangling { .. }));
        // Drifted: points at a different, existing path.
        std::fs::remove_file(root.join("cog.toml")).unwrap();
        std::fs::write(dir.join("store/other"), "y").unwrap();
        std::os::unix::fs::symlink(dir.join("store/other"), root.join("cog.toml")).unwrap();
        assert!(matches!(check(&root, std::slice::from_ref(&cog))[0].status, Status::Drifted { .. }));
        // Unmanaged: a real file of the user's.
        std::fs::remove_file(root.join("cog.toml")).unwrap();
        std::fs::write(root.join("cog.toml"), "mine").unwrap();
        assert_eq!(check(&root, std::slice::from_ref(&cog))[0].status, Status::Unmanaged);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn apply_links_copies_templates_and_repairs_but_keeps_unmanaged() {
        let dir = scratch("apply");
        let root = dir.join("root");
        std::fs::write(dir.join("store/cog.toml"), "x").unwrap();
        std::fs::write(dir.join("store/rustfmt.toml"), "edition").unwrap();
        std::fs::write(dir.join("store/buckconfig.local"), "root = @PRJ_ROOT@ cap=@CAP@ keep@me").unwrap();
        std::fs::write(root.join("mine.txt"), "mine").unwrap();
        std::fs::write(dir.join("store/mine.txt"), "generated").unwrap();
        // A dangling link gets repaired.
        std::os::unix::fs::symlink("/nix/store/gone", root.join("cog.toml")).unwrap();
        let configs = vec![
            cfg(&dir, "cog.toml", Mode::Link),
            cfg(&dir, ".rustfmt.toml", Mode::Copy),
            cfg(&dir, ".buckconfig.local", Mode::Template),
            cfg(&dir, "mine.txt", Mode::Link),
        ];
        let vars =
            BTreeMap::from([("PRJ_ROOT".to_string(), "/proj".to_string()), ("CAP".to_string(), "90".to_string())]);
        let reports = apply(&root, &configs, &vars).unwrap();
        assert_eq!(reports[0].status, Status::Ok);
        assert_eq!(std::fs::read_link(root.join("cog.toml")).unwrap(), Path::new(&configs[0].source));
        assert_eq!(reports[1].status, Status::Ok);
        assert!(!std::fs::symlink_metadata(root.join(".rustfmt.toml")).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(root.join(".buckconfig.local")).unwrap(), "root = /proj cap=90 keep@me");
        assert_eq!(reports[3].status, Status::Unmanaged);
        assert_eq!(std::fs::read_to_string(root.join("mine.txt")).unwrap(), "mine");
        // Idempotent.
        let again = apply(&root, &configs, &vars).unwrap();
        assert_eq!(again[..3].iter().filter(|r| r.is_ok()).count(), 3);
        // The .gitignore block lists every managed output, once.
        let ignore = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert_eq!(ignore.matches("/cog.toml\n").count(), 1);
        assert!(ignore.contains("/.buckconfig.local\n"));
        assert_eq!(ignore.matches(EXCLUDE_BEGIN).count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn gitignore_block_replaces_only_itself() {
        let dir = scratch("gitignore");
        let root = dir.join("root");
        std::fs::write(root.join(".gitignore"), "# user line\n/keep\n").unwrap();
        update_gitignore(&root, &["a".into()]).unwrap();
        update_gitignore(&root, &["b".into()]).unwrap();
        let text = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(text.starts_with("# user line\n/keep\n"));
        assert!(text.contains("/b\n") && !text.contains("/a\n"));
        assert_eq!(text.matches(EXCLUDE_BEGIN).count(), 1);
        // An empty managed set removes the block and leaves the rest.
        update_gitignore(&root, &[]).unwrap();
        assert_eq!(std::fs::read_to_string(root.join(".gitignore")).unwrap(), "# user line\n/keep\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stale_exclude_block_is_removed_by_apply() {
        let dir = scratch("exclude-cleanup");
        let root = dir.join("root");
        std::fs::write(root.join(".git/info/exclude"), format!("/mine\n{EXCLUDE_BEGIN}\n/old\n{EXCLUDE_END}\n"))
            .unwrap();
        apply(&root, &[], &BTreeMap::new()).unwrap();
        assert_eq!(std::fs::read_to_string(root.join(".git/info/exclude")).unwrap(), "/mine\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn worktree_git_file_is_followed() {
        let dir = scratch("worktree");
        let wt = dir.join("wt");
        std::fs::create_dir_all(dir.join("main/.git/worktrees/wt")).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", dir.join("main/.git/worktrees/wt").display())).unwrap();
        std::fs::write(dir.join("main/.git/worktrees/wt/commondir"), "../..\n").unwrap();
        let path = git_exclude_path(&wt).unwrap();
        assert!(path.ends_with("main/.git/worktrees/wt/../../info/exclude"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn substitution_leaves_unknown_and_literal_ats() {
        let vars = BTreeMap::from([("A".to_string(), "1".to_string()), ("E".to_string(), String::new())]);
        assert_eq!(substitute("@A@ @B@ a@b @@ @A", &vars), "1 @B@ a@b @@ @A");
        // Defaults apply when unset or empty; a set value wins.
        assert_eq!(substitute("@B:-90@ @E:-x@ @A:-0@ @:-z@", &vars), "90 x 1 @:-z@");
    }
}
