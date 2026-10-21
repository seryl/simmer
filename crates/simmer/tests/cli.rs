//! End-to-end tests of the `simmer` binary against a stand-in `nix`
//! (`tests/support/nix`), in a scratch HOME and cache. They prove the
//! waiting contract: commands block until their work is done, a failing
//! step's exit status comes back out, failures are never swallowed, and
//! an interrupt leaves no process behind.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const SIMMER: &str = env!("CARGO_BIN_EXE_simmer");

/// Install the stand-in `nix` at `dest`, its interpreter pinned to the
/// `bash` found on this process's PATH. The script's own
/// `#!/usr/bin/env bash` needs `/usr/bin/env`, which a Nix build sandbox
/// does not have.
fn install_fake_nix(dest: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let support = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/nix");
    let script = std::fs::read_to_string(&support).unwrap();
    let body = script.split_once('\n').map_or("", |(_shebang, body)| body);
    std::fs::write(dest, format!("#!{}\n{body}", find_on_path("bash").display())).unwrap();
    std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn find_on_path(name: &str) -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("`{name}` not found on PATH; the fake nix needs it"))
}

struct World {
    root: PathBuf,
}

impl World {
    /// A scratch world: HOME, cache, a fake-nix bin dir, and a project
    /// directory holding a flake.
    fn new(name: &str) -> World {
        let root = std::env::temp_dir().join(format!("simmer-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["home", "cache", "bin", "proj", "out"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        install_fake_nix(&root.join("bin/nix"));
        std::fs::write(root.join("proj/flake.nix"), "{ }\n").unwrap();
        World { root }
    }

    fn proj(&self) -> PathBuf {
        self.root.join("proj")
    }

    /// Knobs for the fake nix (it cannot see simmer's scrubbed env).
    fn knobs(&self, lines: &[String]) {
        std::fs::write(self.root.join("bin/fake-nix.env"), lines.join("\n") + "\n").unwrap();
    }

    /// Make the fake build produce a native shell with this env.json.
    fn native_shell(&self, vars: &str) {
        let out = self.root.join("out");
        std::fs::write(
            out.join("env.json"),
            format!(r#"{{"schema":1,"name":"test","system":"x","profile":"{}","vars":{{{vars}}}}}"#, out.display()),
        )
        .unwrap();
        self.knobs(&[format!("FAKE_NIX_OUT={}", out.display())]);
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(SIMMER);
        let path = format!("{}:{}", self.root.join("bin").display(), std::env::var("PATH").unwrap_or_default());
        cmd.args(args)
            .current_dir(self.proj())
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("PATH", path)
            .env("TERM", "dumb");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks for existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Poll a condition another process will make true; fail, never hang.
fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(30);
    while !cond() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn warm_waits_and_export_applies_the_result() {
    let w = World::new("native");
    w.native_shell(r#""GREETING":{"kind":"set","value":"hello"}"#);
    let out = w.run(&["warm", "--plain"]);
    let err = text(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("simmer: start   nix build\n"), "{err}");
    assert!(err.contains("simmer: ok      nix build ("), "{err}");
    assert!(err.contains("simmer: warmed "), "{err}");
    // The work is done by the time warm returns: export applies it at once.
    let out = w.run(&["export", "bash", "--plain"]);
    assert!(out.status.success());
    assert!(text(&out.stdout).contains("export GREETING='hello'\n"), "{}", text(&out.stdout));
}

#[test]
fn a_file_the_build_creates_does_not_make_it_stale_at_once() {
    let w = World::new("lockfile");
    w.native_shell(r#""A":{"kind":"set","value":"1"}"#);
    let mut knobs = std::fs::read_to_string(w.root.join("bin/fake-nix.env")).unwrap();
    knobs.push_str("FAKE_NIX_WRITE_LOCK=1\n");
    std::fs::write(w.root.join("bin/fake-nix.env"), knobs).unwrap();
    assert!(w.run(&["warm", "--plain"]).status.success());
    assert!(w.proj().join("flake.lock").exists());
    let status: serde_json::Value = serde_json::from_slice(&w.run(&["status"]).stdout).unwrap();
    assert_eq!(status["fresh"], true, "{status}");
    assert!(status["watched"].as_array().unwrap().iter().any(|f| f == "flake.lock"));
}

#[test]
fn a_failing_build_propagates_its_exit_status_and_log() {
    let w = World::new("fail");
    w.knobs(&["FAKE_NIX_EXIT=3".into(), "FAKE_NIX_STDERR='error: flake does not evaluate'".into()]);
    let out = w.run(&["warm", "--plain"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(3), "{err}");
    // The harness step failed with nix's status; its log is printed.
    assert!(err.contains("simmer: FAILED  harness (print-dev-env)"), "{err}");
    assert!(err.contains("error: flake does not evaluate"), "{err}");
    assert!(err.contains("simmer: summary"), "{err}");
    assert!(err.contains("exited with status 3"), "{err}");
    // The full log was saved in the cache.
    let status: serde_json::Value = serde_json::from_slice(&w.run(&["status"]).stdout).unwrap();
    assert_eq!(status["last_failure"]["error"].as_str().map(|e| e.contains("status 3")), Some(true));
}

#[test]
fn export_waits_for_the_rebuild_and_does_not_swallow_failure() {
    let w = World::new("export");
    w.native_shell(r#""STAGE":{"kind":"set","value":"one"}"#);
    assert!(w.run(&["allow"]).status.success());
    // Stale (never built): export waits for the build, then applies it.
    let out = w.run(&["export", "bash", "--plain"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("export STAGE='one'"));
    assert!(text(&out.stderr).contains("simmer: ok      nix build"));

    // Edit the flake and break the build: export fails loudly, with the
    // build's exit status, and serves the previous environment.
    std::fs::write(w.proj().join("flake.nix"), "{ broken }\n").unwrap();
    w.knobs(&["FAKE_NIX_EXIT=5".into()]);
    let out = w.run(&["export", "bash", "--plain"]);
    assert_eq!(out.status.code(), Some(5), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("export STAGE='one'"), "previous environment still served");
    assert!(text(&out.stderr).contains("FAILED"));

    // The same failing version is not rebuilt on every prompt…
    let out = w.run(&["export", "bash", "--plain"]);
    assert!(out.status.success());
    assert!(!text(&out.stderr).contains("nix build"), "{}", text(&out.stderr));
    // …but an explicit warm retries it, and an edit builds again.
    w.native_shell(r#""STAGE":{"kind":"set","value":"two"}"#);
    std::fs::write(w.proj().join("flake.nix"), "{ fixed }\n").unwrap();
    let out = w.run(&["export", "bash", "--plain"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("export STAGE='two'"));
}

#[test]
fn interrupt_stops_every_child_and_reports() {
    for (signal, code) in [(libc::SIGTERM, 128 + libc::SIGTERM), (libc::SIGINT, 128 + libc::SIGINT)] {
        let w = World::new(&format!("interrupt-{signal}"));
        let pidfile = w.root.join("grandchild.pid");
        w.knobs(&[format!("FAKE_NIX_BLOCK={}", pidfile.display())]);
        let mut child = w.cmd(&["warm", "--plain"]).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
        eventually("nix to start its long-running child", || pidfile.exists());
        let grandchild: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        assert!(alive(grandchild));
        // SAFETY: signalling our own child process.
        unsafe { libc::kill(child.id() as i32, signal) };
        let mut err = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            std::io::Read::read_to_string(&mut pipe, &mut err).unwrap();
        }
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(code), "{err}");
        assert!(err.contains("STOPPED nix build") || err.contains("interrupted"), "{err}");
        assert!(err.contains("simmer: summary"), "{err}");
        eventually("the grandchild to be gone", || !alive(grandchild));
        // Stopping a build is not a request to restart it at the next
        // prompt: export leaves it alone until a file changes.
        w.knobs(&[]);
        let out = w.run(&["export", "bash", "--plain"]);
        assert!(out.status.success());
        assert!(!text(&out.stderr).contains("start"), "{}", text(&out.stderr));
    }
}

#[test]
fn timeout_stops_the_warm_with_a_clear_message() {
    let w = World::new("timeout");
    let pidfile = w.root.join("grandchild.pid");
    w.knobs(&[format!("FAKE_NIX_BLOCK={}", pidfile.display())]);
    let out = w.run(&["warm", "--plain", "--timeout", "1s"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(124), "{err}");
    assert!(err.contains("did not finish within"), "{err}");
    // Whatever the fake nix had started by then is gone too.
    if let Ok(pid) = std::fs::read_to_string(&pidfile) {
        let grandchild: i32 = pid.trim().parse().unwrap();
        eventually("the grandchild to be gone", || !alive(grandchild));
    }
}

#[test]
fn a_second_warm_waits_for_the_first_instead_of_returning() {
    let w = World::new("lockwait");
    w.native_shell(r#""A":{"kind":"set","value":"1"}"#);
    // Hold the project's warm lock as if another simmer were warming.
    assert!(w.run(&["warm", "--plain"]).status.success());
    let cache_dir = std::fs::read_dir(w.root.join("cache/simmer"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .unwrap();
    let lock = std::fs::OpenOptions::new().write(true).open(cache_dir.join("warm.flock")).unwrap();
    // SAFETY: flock on a descriptor we own.
    assert_eq!(unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) }, 0);
    let mut child = w.cmd(&["warm", "--plain"]).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let first = lines.next().unwrap().unwrap();
    assert!(first.starts_with("simmer: start   wait for the warm already running"), "{first}");
    // Still waiting: it has not finished while the lock is held.
    assert!(child.try_wait().unwrap().is_none());
    drop(lock);
    let rest: Vec<String> = lines.map_while(Result::ok).collect();
    assert!(child.wait().unwrap().success(), "{rest:?}");
    assert!(rest.iter().any(|l| l.starts_with("simmer: ok      wait for the warm")), "{rest:?}");
}

#[test]
fn warm_json_streams_events_on_stdout() {
    let w = World::new("json");
    w.native_shell(r#""A":{"kind":"set","value":"1"}"#);
    let out = w.run(&["warm", "--json"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let events: Vec<serde_json::Value> = text(&out.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let kinds: Vec<&str> = events.iter().filter_map(|e| e["event"].as_str()).collect();
    assert_eq!(kinds.first(), Some(&"queued"));
    assert!(kinds.contains(&"finish"));
    assert_eq!(events.last().unwrap()["event"], "summary");
    assert_eq!(events.last().unwrap()["status"], "ok");
}

#[test]
fn harness_projects_build_through_envrc() {
    let w = World::new("envrc");
    std::fs::write(w.proj().join(".envrc"), "use flake\nexport EXTRA=yes\n").unwrap();
    let out = w.run(&["warm", "--plain"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = w.run(&["export", "bash"]);
    let script = text(&out.stdout);
    assert!(script.contains("export FAKE_DEV_ENV='1'") && script.contains("export EXTRA='yes'"), "{script}");
}

#[test]
fn errors_explain_what_to_do() {
    let w = World::new("errors");
    std::fs::remove_file(w.proj().join("flake.nix")).unwrap();
    let out = w.run(&["warm"]);
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(err.contains("no project here") && err.contains(".simmer.toml"), "{err}");
    let out = w.run(&["export", "powershell"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("possible values: fish, bash, zsh"), "{}", text(&out.stderr));
    let out = w.run(&["warm", "--timeout", "soon"]);
    assert!(text(&out.stderr).contains("invalid duration"));
}

#[test]
fn a_broken_manifest_is_reported_and_the_session_kept() {
    let w = World::new("badtoml");
    w.native_shell(r#""KEEP":{"kind":"set","value":"me"}"#);
    assert!(w.run(&["warm", "--plain"]).status.success());
    // Load the environment into a "session", then break the manifest.
    let script = text(&w.run(&["export", "bash"]).stdout);
    let session: Vec<(String, String)> = script
        .lines()
        .filter_map(|l| l.strip_prefix("export "))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.trim_matches('\'').to_string()))
        .collect();
    std::fs::write(w.proj().join(".simmer.toml"), "[[provider]]\n").unwrap();
    let out = w.cmd(&["export", "bash"]).envs(session).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains(".simmer.toml"), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "", "nothing is unloaded while the manifest is broken");
}

#[test]
fn help_documents_every_command() {
    let out = Command::new(SIMMER).arg("--help").output().unwrap();
    let help = text(&out.stdout);
    for word in [
        "export",
        "warm",
        "hook",
        "status",
        "daemon",
        "doctor",
        "allow",
        "deny",
        "direnv",
        "--timeout",
        "--progress",
        "SIMMER_BACKGROUND",
        "Exit status",
    ] {
        assert!(help.contains(word), "--help lacks {word}:\n{help}");
    }
}

#[test]
fn direnv_shim_waits_and_answers_json() {
    let w = World::new("direnv");
    w.native_shell(r#""FROM_SHIM":{"kind":"set","value":"yes"}"#);
    assert!(w.run(&["allow"]).status.success());
    let out = w.run(&["direnv", "export", "json"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let changes: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(changes["FROM_SHIM"], "yes");
}
