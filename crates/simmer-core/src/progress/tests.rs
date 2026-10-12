//! Deterministic tests: a fake clock, a fixed terminal size and an
//! in-memory writer, so every byte the display emits can be asserted.

use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        lock(&self.0).extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Capture {
    fn take(&self) -> String {
        String::from_utf8(std::mem::take(&mut *lock(&self.0))).unwrap()
    }
}

struct Fixture {
    progress: Progress,
    out: Capture,
    ms: Arc<AtomicU64>,
}

impl Fixture {
    fn new(mode: Mode, verbosity: Verbosity, width: usize, height: usize) -> Self {
        let out = Capture::default();
        let ms = Arc::new(AtomicU64::new(0));
        let clock_ms = ms.clone();
        let config = Config { mode, verbosity, color: false, log_lines: 3 };
        let progress = Progress::with_parts(
            "warming ~/proj",
            config,
            Box::new(out.clone()),
            Arc::new(move || Duration::from_millis(clock_ms.load(Ordering::SeqCst))),
            Arc::new(move || (width, height)),
        );
        Fixture { progress, out, ms }
    }

    fn at(&self, ms: u64) -> &Progress {
        self.ms.store(ms, Ordering::SeqCst);
        &self.progress
    }
}

/// The scripted run every presentation is checked against: a build that
/// succeeds while printing, and a provider that fails.
fn script(f: &Fixture) {
    let nix = f.at(0).add("nix build");
    let provider = f.at(0).add("provider CAP");
    f.at(0).start(nix);
    f.at(100).phase(nix, "building hello");
    f.at(150).output(nix, "unpacking sources");
    f.at(200).output(nix, "compiling hello.c");
    f.at(1_300).finish(nix, Outcome::Ok, None);
    f.at(1_300).start(provider);
    for i in 1..=5 {
        f.at(1_300 + i).output(provider, &format!("probe line {i}"));
    }
    f.at(1_400).finish(provider, Outcome::Failed, Some("exit status 3".into()));
    f.at(1_400).info("serving the previous environment");
}

#[test]
fn plain_output_is_one_stable_line_per_state_change() {
    let f = Fixture::new(Mode::Plain, Verbosity::Normal, 80, 24);
    script(&f);
    f.at(1_500).close(Ending::Failed);
    assert_eq!(
        f.out.take(),
        "simmer: start   nix build\n\
         simmer: ok      nix build (1.3s)\n\
         simmer: start   provider CAP\n\
         simmer: FAILED  provider CAP (0.1s): exit status 3\n\
         simmer: serving the previous environment\n\
         ── provider CAP failed: last 3 of 5 lines ──\n\
         probe line 3\n\
         probe line 4\n\
         probe line 5\n\
         simmer: summary\n  \
           task          status       time\n  \
           nix build     ok           1.3s\n  \
           provider CAP  failed       0.1s\n"
    );
}

#[test]
fn verbose_plain_output_includes_phases_and_output_lines() {
    let f = Fixture::new(Mode::Plain, Verbosity::Verbose, 80, 24);
    let nix = f.at(0).begin("nix build");
    f.at(10).phase(nix.id(), "building \x1b[1mhello\x1b[0m");
    nix.output("line\twith tab");
    f.at(20);
    nix.ok();
    f.progress.close(Ending::Success);
    assert_eq!(
        f.out.take(),
        "simmer: start   nix build\n\
         simmer: phase   nix build: building hello\n\
         simmer: | nix build: line    with tab\n\
         simmer: ok      nix build (0.0s)\n\
         simmer: summary\n  \
           task       status       time\n  \
           nix build  ok           0.0s\n"
    );
}

#[test]
fn live_view_redraws_in_place_and_commits_finished_tasks() {
    let f = Fixture::new(Mode::Live, Verbosity::Normal, 60, 20);
    let task = f.at(0).begin("nix build");
    assert_eq!(
        f.out.take(),
        "\x1b[?25l\r\x1b[J⠋ warming ~/proj  0.0s  1 running · 0 done · 0 failed · 0 …\n  ⠋ nix build  0.0s\n"
    );
    task.output("fetching source");
    // Output alone never draws: the ticker (here: frame()) rate-limits.
    assert_eq!(f.out.take(), "");
    f.at(250).frame();
    assert_eq!(
        f.out.take(),
        "\x1b[2A\r\x1b[J⠸ warming ~/proj  0.2s  1 running · 0 done · 0 failed · 0 …\n  ⠸ nix build  0.2s\n      │ fetching source\n"
    );
    f.at(1_200);
    task.ok();
    assert_eq!(
        f.out.take(),
        "\x1b[3A\r\x1b[J  ✓ nix build  1.2s\n⠴ warming ~/proj  1.2s  0 running · 1 done · 0 failed · 0 …\n"
    );
    f.progress.close(Ending::Success);
    assert_eq!(f.out.take(), "\x1b[1A\r\x1b[J\x1b[?25h");
}

#[test]
fn live_failure_prints_log_and_summary_after_clearing_the_region() {
    let f = Fixture::new(Mode::Live, Verbosity::Normal, 60, 20);
    script(&f);
    f.out.take();
    f.at(1_500).close(Ending::Failed);
    let text = f.out.take();
    assert!(text.starts_with("\x1b[1A\r\x1b[J\x1b[?25h── provider CAP failed: last 3 of 5 lines ──\n"), "{text:?}");
    assert!(text.ends_with("  provider CAP  failed       0.1s\n"), "{text:?}");
}

#[test]
fn json_mode_emits_one_parseable_event_per_line() {
    let f = Fixture::new(Mode::Json, Verbosity::Normal, 80, 24);
    script(&f);
    f.at(1_500).close(Ending::Failed);
    let events: Vec<serde_json::Value> = f.out.take().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let kinds: Vec<&str> = events.iter().map(|e| e["event"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        [
            "queued", "queued", "start", "phase", "output", "output", "finish", "start", "output", "output", "output",
            "output", "output", "finish", "note", "summary"
        ]
    );
    let summary = events.last().unwrap();
    assert_eq!(summary["status"], "failed");
    assert_eq!(summary["tasks"][1]["status"], "failed");
    assert_eq!(summary["tasks"][1]["log_tail"], serde_json::json!(["probe line 3", "probe line 4", "probe line 5"]));
    assert_eq!(events[6]["duration_ms"], 1300);
}

#[test]
fn quiet_mode_prints_only_the_failure() {
    let f = Fixture::new(Mode::Quiet, Verbosity::Quiet, 80, 24);
    script(&f);
    f.at(1_500).close(Ending::Failed);
    let text = f.out.take();
    assert!(text.starts_with("── provider CAP failed: last 3 of 5 lines ──\n"), "{text:?}");
    assert!(!text.contains("start"));
}

#[test]
fn interrupt_marks_running_tasks_and_prints_summary() {
    let f = Fixture::new(Mode::Plain, Verbosity::Normal, 80, 24);
    let task = f.at(0).begin("nix build");
    task.output("downloading");
    f.at(700).close(Ending::Interrupted);
    std::mem::forget(task);
    assert_eq!(
        f.out.take(),
        "simmer: start   nix build\n\
         ── nix build interrupted: 1 lines ──\n\
         downloading\n\
         simmer: summary\n  \
           task       status       time\n  \
           nix build  interrupted  0.7s\n"
    );
}

#[test]
fn dropped_task_handles_are_recorded_as_failed() {
    let f = Fixture::new(Mode::Plain, Verbosity::Normal, 80, 24);
    drop(f.at(0).begin("orphan"));
    assert_eq!(f.progress.first_failure(), Some(("orphan".into(), Some("did not finish".into()))));
}

#[test]
fn failed_logs_are_saved_in_full_to_the_log_dir() {
    let dir = std::env::temp_dir().join(format!("simmer-progress-logs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let f = Fixture::new(Mode::Quiet, Verbosity::Quiet, 80, 24);
    f.progress.set_log_dir(dir.clone());
    script(&f);
    f.at(1_500).close(Ending::Failed);
    let saved = std::fs::read_to_string(dir.join("provider-CAP.log")).unwrap();
    assert_eq!(saved.lines().count(), 5);
    assert!(f.out.take().contains(&format!("── full log: {} ──", dir.join("provider-CAP.log").display())));
    std::fs::remove_dir_all(&dir).unwrap();
}
