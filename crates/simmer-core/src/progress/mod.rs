//! Progress reporting for work simmer waits on.
//!
//! Callers describe work as *tasks* (`nix build`, `provider CUDA`, …) and
//! report what happens to them; a [`Progress`] turns that into one of four
//! presentations:
//!
//! - **live** (a terminal): a region redrawn in place at most ~15 times a
//!   second, with a header (status, elapsed, counts), one line per running
//!   task (spinner, phase, elapsed) and the last few lines of its output.
//!   A finished task leaves one permanent `✓`/`✗` line above the region.
//! - **plain** (CI, pipes, `--plain`): one stable line per state change,
//!   no escape codes.
//! - **json** (`--json`): one JSON object per event per line.
//! - **quiet**: nothing unless something fails.
//!
//! Whatever the presentation, every task's output is captured into a
//! bounded [`log::LogBuffer`]; when the run fails, the failing task's log
//! (bounded, or whole on request) and a summary table are printed, and
//! the full log is saved next to the project's cache.

pub mod log;
pub mod render;

pub use render::{Outcome, State};

use render::{Board, Style, Task};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Redraw interval of the live view (~15 Hz).
pub const FRAME_INTERVAL: Duration = Duration::from_millis(66);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Live,
    Plain,
    Json,
    Quiet,
}

impl Mode {
    /// Parse a `--progress` / `SIMMER_PROGRESS` value; `auto` (and
    /// `None`) picks live on a capable terminal and plain otherwise.
    pub fn resolve(requested: Option<&str>) -> Result<Mode, String> {
        match requested.unwrap_or("auto") {
            "auto" | "" => Ok(Self::auto()),
            "live" | "tty" => Ok(Mode::Live),
            "plain" => Ok(Mode::Plain),
            "json" => Ok(Mode::Json),
            "quiet" | "none" => Ok(Mode::Quiet),
            other => Err(format!("unknown progress mode '{other}' (use auto, live, plain, json, or quiet)")),
        }
    }

    pub fn auto() -> Mode {
        if crate::term::is_tty(2) && crate::term::can_redraw() {
            Mode::Live
        } else {
            Mode::Plain
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verbosity {
    Quiet,
    Normal,
    Verbose,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub mode: Mode,
    pub verbosity: Verbosity,
    pub color: bool,
    /// Lines of a failed task's log printed at the end; 0 prints it all.
    pub log_lines: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config { mode: Mode::auto(), verbosity: Verbosity::Normal, color: crate::term::color_allowed(), log_lines: 40 }
    }
}

/// How the whole run ended, for [`Progress::close`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    Success,
    Failed,
    Interrupted,
}

pub type Clock = Arc<dyn Fn() -> Duration + Send + Sync>;
/// Terminal `(columns, rows)`.
pub type SizeFn = Arc<dyn Fn() -> (usize, usize) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(usize);

/// Handle to a progress display; cheap to clone and safe to share with
/// the threads that pump a child's output.
#[derive(Clone)]
pub struct Progress {
    inner: Arc<Inner>,
}

struct Inner {
    shared: Mutex<Shared>,
    config: Config,
    clock: Clock,
    size: SizeFn,
    ticker: Mutex<Option<JoinHandle<()>>>,
}

struct Shared {
    board: Board,
    out: Box<dyn Write + Send>,
    /// Lines of the live region currently on screen.
    drawn: usize,
    cursor_hidden: bool,
    closed: bool,
    log_dir: Option<PathBuf>,
}

impl Progress {
    /// A display on stderr (`json_to_stdout` sends JSON events to stdout
    /// instead, for machine consumers of `simmer warm --json`).
    pub fn new(title: &str, config: Config, json_to_stdout: bool) -> Progress {
        let out: Box<dyn Write + Send> = if config.mode == Mode::Json && json_to_stdout {
            Box::new(std::io::stdout())
        } else {
            Box::new(std::io::stderr())
        };
        let start = Instant::now();
        let clock: Clock = Arc::new(move || start.elapsed());
        let size: SizeFn = Arc::new(|| {
            let cols = crate::term::width(2).unwrap_or(80);
            (cols, terminal_rows().unwrap_or(24))
        });
        let progress = Self::with_parts(title, config, out, clock, size);
        if progress.mode() == Mode::Live {
            progress.start_ticker();
        }
        progress
    }

    /// A display that shows nothing and keeps only bounded logs.
    pub fn hidden() -> Progress {
        let config = Config { mode: Mode::Quiet, verbosity: Verbosity::Quiet, color: false, log_lines: 40 };
        let start = Instant::now();
        Self::with_parts(
            "",
            config,
            Box::new(std::io::sink()),
            Arc::new(move || start.elapsed()),
            Arc::new(|| (80, 24)),
        )
    }

    /// Full control over output, clock and terminal size — what tests use
    /// to make every byte deterministic. No ticker runs: the live region
    /// redraws on state changes and on [`Self::frame`].
    pub fn with_parts(title: &str, config: Config, out: Box<dyn Write + Send>, clock: Clock, size: SizeFn) -> Progress {
        let inner = Arc::new(Inner {
            shared: Mutex::new(Shared {
                board: Board { title: title.to_string(), tasks: Vec::new() },
                out,
                drawn: 0,
                cursor_hidden: false,
                closed: false,
                log_dir: None,
            }),
            config,
            clock,
            size,
            ticker: Mutex::new(None),
        });
        Progress { inner }
    }

    /// Redraw the live region every [`FRAME_INTERVAL`] while tasks run
    /// (spinners and elapsed times move even when nothing is printed).
    pub fn start_ticker(&self) {
        let weak = Arc::downgrade(&self.inner);
        let handle = std::thread::Builder::new().name("simmer-progress".into()).spawn(move || tick(weak)).ok();
        *lock(&self.inner.ticker) = handle;
    }

    /// Draw one live frame now (what the ticker does each interval).
    pub fn frame(&self) {
        if self.inner.config.mode != Mode::Live {
            return;
        }
        let now = self.now();
        let mut shared = lock(&self.inner.shared);
        if !shared.closed {
            self.redraw(&mut shared, now, &[]);
        }
    }

    pub fn mode(&self) -> Mode {
        self.inner.config.mode
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    fn now(&self) -> Duration {
        (self.inner.clock)()
    }

    /// Where full logs of failed tasks are saved on close.
    pub fn set_log_dir(&self, dir: PathBuf) {
        lock(&self.inner.shared).log_dir = Some(dir);
    }

    pub fn set_title(&self, title: &str) {
        lock(&self.inner.shared).board.title = title.to_string();
    }

    /// Register a task that has not started yet (counted as queued).
    pub fn add(&self, name: &str) -> TaskId {
        let now = self.now();
        let mut shared = lock(&self.inner.shared);
        shared.board.tasks.push(Task::new(name));
        let id = TaskId(shared.board.tasks.len() - 1);
        if self.inner.config.mode == Mode::Json {
            let line = serde_json::json!({ "t_ms": now.as_millis() as u64, "event": "queued", "task": name });
            emit(&mut shared, &line.to_string());
        }
        id
    }

    pub fn start(&self, id: TaskId) {
        let now = self.now();
        let mut shared = lock(&self.inner.shared);
        let Some(task) = shared.board.tasks.get_mut(id.0) else { return };
        task.state = Some(State::Running { since: now });
        let name = task.name.clone();
        match self.inner.config.mode {
            Mode::Live => self.redraw(&mut shared, now, &[]),
            Mode::Plain if self.inner.config.verbosity >= Verbosity::Normal => {
                emit(&mut shared, &format!("simmer: start   {name}"));
            }
            Mode::Json => {
                let line = serde_json::json!({ "t_ms": now.as_millis() as u64, "event": "start", "task": name });
                emit(&mut shared, &line.to_string());
            }
            _ => {}
        }
    }

    /// Register and start a task in one step.
    pub fn begin(&self, name: &str) -> TaskHandle {
        let id = self.add(name);
        self.start(id);
        TaskHandle { progress: self.clone(), id, finished: false }
    }

    /// A handle for a task registered earlier with [`Self::add`].
    pub fn handle(&self, id: TaskId) -> TaskHandle {
        TaskHandle { progress: self.clone(), id, finished: false }
    }

    pub fn phase(&self, id: TaskId, phase: &str) {
        let now = self.now();
        let mut shared = lock(&self.inner.shared);
        let Some(task) = shared.board.tasks.get_mut(id.0) else { return };
        let phase = crate::term::sanitize(phase);
        if task.phase.as_deref() == Some(phase.as_str()) {
            return;
        }
        task.phase = Some(phase.clone());
        let name = task.name.clone();
        match self.inner.config.mode {
            Mode::Plain if self.inner.config.verbosity >= Verbosity::Verbose => {
                emit(&mut shared, &format!("simmer: phase   {name}: {phase}"));
            }
            Mode::Json => {
                let line = serde_json::json!({ "t_ms": now.as_millis() as u64, "event": "phase", "task": name, "phase": phase });
                emit(&mut shared, &line.to_string());
            }
            _ => {}
        }
    }

    /// One line of a task's output: always captured, shown according to
    /// the mode (live tail, verbose plain line, json event).
    pub fn output(&self, id: TaskId, line: &str) {
        let now = self.now();
        let mut shared = lock(&self.inner.shared);
        let Some(task) = shared.board.tasks.get_mut(id.0) else { return };
        task.push_output(line);
        let name = task.name.clone();
        match self.inner.config.mode {
            Mode::Plain if self.inner.config.verbosity >= Verbosity::Verbose => {
                let clean = crate::term::sanitize(line);
                emit(&mut shared, &format!("simmer: | {name}: {clean}"));
            }
            Mode::Json => {
                let clean = crate::term::sanitize(line);
                let line = serde_json::json!({ "t_ms": now.as_millis() as u64, "event": "output", "task": name, "line": clean });
                emit(&mut shared, &line.to_string());
            }
            _ => {}
        }
    }

    pub fn finish(&self, id: TaskId, outcome: Outcome, message: Option<String>) {
        let now = self.now();
        let mut shared = lock(&self.inner.shared);
        let Some(task) = shared.board.tasks.get_mut(id.0) else { return };
        let since = match task.state() {
            State::Running { since } => since,
            State::Queued => now,
            State::Done { .. } => return,
        };
        task.state = Some(State::Done { since, until: now, outcome });
        task.message = message.map(|m| crate::term::sanitize(&m));
        let config = &self.inner.config;
        match config.mode {
            Mode::Live => {
                // Committed lines scroll away above the live region; they
                // may wrap, so they are never cut.
                let line = render::done_line(task, now, None, config.color);
                let show = config.verbosity >= Verbosity::Normal || outcome.is_failure();
                self.redraw(&mut shared, now, if show { std::slice::from_ref(&line) } else { &[] });
            }
            Mode::Plain if config.verbosity >= Verbosity::Normal || outcome.is_failure() => {
                let word = match outcome {
                    Outcome::Failed => "FAILED",
                    Outcome::Interrupted => "STOPPED",
                    other => other.word(),
                };
                let mut line = format!("simmer: {word:<7} {} ({})", task.name, crate::term::fmt_duration(now - since));
                if let Some(message) = &task.message {
                    line.push_str(": ");
                    line.push_str(message);
                }
                emit(&mut shared, &line);
            }
            Mode::Json => {
                let line = serde_json::json!({
                    "t_ms": now.as_millis() as u64, "event": "finish", "task": task.name,
                    "status": outcome.word(), "duration_ms": (now - since).as_millis() as u64,
                    "message": task.message,
                });
                emit(&mut shared, &line.to_string());
            }
            _ => {}
        }
    }

    /// An informational line (hidden in quiet mode).
    pub fn info(&self, text: &str) {
        if self.inner.config.verbosity >= Verbosity::Normal {
            self.note("info", text);
        }
    }

    /// A warning line (shown at every verbosity).
    pub fn warn(&self, text: &str) {
        self.note("warning", text);
    }

    fn note(&self, level: &str, text: &str) {
        let now = self.now();
        let mut shared = lock(&self.inner.shared);
        let prefix = if level == "warning" { "simmer: warning: " } else { "simmer: " };
        match self.inner.config.mode {
            Mode::Live => {
                let lines: Vec<String> =
                    text.lines().map(|l| format!("{prefix}{}", crate::term::sanitize(l))).collect();
                self.redraw(&mut shared, now, &lines);
            }
            Mode::Json => {
                let line = serde_json::json!({ "t_ms": now.as_millis() as u64, "event": "note", "level": level, "text": text });
                emit(&mut shared, &line.to_string());
            }
            Mode::Plain | Mode::Quiet => {
                for l in text.lines() {
                    emit(&mut shared, &format!("{prefix}{l}"));
                }
            }
        }
    }

    /// The last `n` captured output lines of a task, oldest first.
    pub fn log_tail(&self, id: TaskId, n: usize) -> Vec<String> {
        let shared = lock(&self.inner.shared);
        shared.board.tasks.get(id.0).map(|t| t.log.tail(n).map(str::to_string).collect()).unwrap_or_default()
    }

    /// Outcome of the first failed task, with its name and message.
    pub fn first_failure(&self) -> Option<(String, Option<String>)> {
        let shared = lock(&self.inner.shared);
        shared
            .board
            .tasks
            .iter()
            .find(|t| matches!(t.state(), State::Done { outcome: Outcome::Failed, .. }))
            .map(|t| (t.name.clone(), t.message.clone()))
    }

    /// End the display. On failure or interrupt: each failed task's log
    /// (its last `log_lines` lines, or all of it) is printed and saved
    /// under the log dir, then a summary table of every task.
    pub fn close(&self, ending: Ending) {
        if let Some(handle) = lock(&self.inner.ticker).take() {
            // The ticker checks `closed` each frame; mark first, then join.
            lock(&self.inner.shared).closed = true;
            let _ = handle.join();
        }
        let now = self.now();
        let mut shared = lock(&self.inner.shared);
        shared.closed = true;
        let config = self.inner.config.clone();
        if config.mode == Mode::Live {
            clear_region(&mut shared);
        }
        // Tasks still running when the run ends were cut short.
        for task in &mut shared.board.tasks {
            if let State::Running { since } = task.state() {
                let outcome = if ending == Ending::Interrupted { Outcome::Interrupted } else { Outcome::Failed };
                task.state = Some(State::Done { since, until: now, outcome });
            }
        }
        if config.mode == Mode::Json {
            let tasks: Vec<serde_json::Value> = shared
                .board
                .tasks
                .iter()
                .map(|t| {
                    let failed = matches!(t.state(), State::Done { outcome, .. } if outcome.is_failure());
                    serde_json::json!({
                        "task": t.name,
                        "status": match t.state() { State::Done { outcome, .. } => outcome.word(), State::Queued => "not_started", State::Running { .. } => "running" },
                        "duration_ms": t.duration(now).map(|d| d.as_millis() as u64),
                        "message": t.message,
                        "log_tail": if failed { t.log.tail(config.log_lines.max(1)).collect::<Vec<_>>() } else { Vec::new() },
                    })
                })
                .collect();
            let status = match ending {
                Ending::Success => "ok",
                Ending::Failed => "failed",
                Ending::Interrupted => "interrupted",
            };
            let line = serde_json::json!({ "t_ms": now.as_millis() as u64, "event": "summary", "status": status, "tasks": tasks });
            emit(&mut shared, &line.to_string());
            save_failed_logs(&mut shared);
            return;
        }
        let failing = ending != Ending::Success;
        if failing {
            let saved = save_failed_logs(&mut shared);
            print_failure_logs(&mut shared, &config, &saved);
        }
        if config.verbosity >= Verbosity::Normal && config.mode != Mode::Quiet {
            print_warning_logs(&mut shared);
        }
        if failing || config.verbosity >= Verbosity::Verbose {
            let lines = render::summary(&shared.board, now);
            emit(&mut shared, "simmer: summary");
            for line in lines {
                emit(&mut shared, &line);
            }
        }
        let _ = shared.out.flush();
    }

    /// Redraw the live region with `commits` printed permanently above it.
    fn redraw(&self, shared: &mut Shared, now: Duration, commits: &[String]) {
        if shared.closed {
            for line in commits {
                emit(shared, line);
            }
            return;
        }
        let (width, height) = (self.inner.size)();
        let style = Style {
            color: self.inner.config.color,
            tail_lines: match self.inner.config.verbosity {
                Verbosity::Verbose => 8,
                _ => 3,
            },
        };
        let frame = render::live_frame(&shared.board, now, width, height, &style);
        let mut buf = String::new();
        if !shared.cursor_hidden {
            buf.push_str("\x1b[?25l");
            shared.cursor_hidden = true;
        }
        if shared.drawn > 0 {
            buf.push_str(&format!("\x1b[{}A", shared.drawn));
        }
        buf.push_str("\r\x1b[J");
        for line in commits.iter().chain(frame.iter()) {
            buf.push_str(line);
            buf.push('\n');
        }
        shared.drawn = frame.len();
        let _ = shared.out.write_all(buf.as_bytes());
        let _ = shared.out.flush();
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // A display dropped without close() must still give the terminal
        // its cursor back.
        if let Ok(shared) = self.shared.get_mut() {
            if shared.cursor_hidden {
                clear_region(shared);
            }
        }
    }
}

/// A started task; finishing it consumes the handle. Dropped unfinished
/// (an early `?` return) it is recorded as failed, so no task is left
/// spinning or silently forgotten.
pub struct TaskHandle {
    progress: Progress,
    id: TaskId,
    finished: bool,
}

impl TaskHandle {
    pub fn id(&self) -> TaskId {
        self.id
    }

    pub fn progress(&self) -> &Progress {
        &self.progress
    }

    pub fn phase(&self, phase: &str) {
        self.progress.phase(self.id, phase);
    }

    pub fn output(&self, line: &str) {
        self.progress.output(self.id, line);
    }

    /// The most recent captured line that looks like an error (`error:`,
    /// as nix and most tools print them), for a one-line failure summary.
    pub fn last_error_line(&self) -> Option<String> {
        self.progress
            .log_tail(self.id, log::RING_LINES)
            .into_iter()
            .rev()
            .find(|l| l.trim_start().to_ascii_lowercase().starts_with("error"))
            .map(|l| l.trim().to_string())
    }

    pub fn ok(self) {
        self.end(Outcome::Ok, None);
    }

    pub fn ok_with(self, message: impl Into<String>) {
        self.end(Outcome::Ok, Some(message.into()));
    }

    pub fn warn(self, message: impl Into<String>) {
        self.end(Outcome::Warned, Some(message.into()));
    }

    pub fn skip(self, message: impl Into<String>) {
        self.end(Outcome::Skipped, Some(message.into()));
    }

    pub fn fail(self, message: impl Into<String>) {
        self.end(Outcome::Failed, Some(message.into()));
    }

    /// Finish from an error: interrupted when the error is an interrupt,
    /// failed otherwise, with the error's first line as the reason.
    pub fn fail_err(self, err: &anyhow::Error) {
        let outcome = if crate::supervise::is_interrupt(err) { Outcome::Interrupted } else { Outcome::Failed };
        let message = format!("{err:#}").lines().next().unwrap_or_default().to_string();
        self.end(outcome, Some(message));
    }

    fn end(mut self, outcome: Outcome, message: Option<String>) {
        self.finished = true;
        self.progress.finish(self.id, outcome, message);
    }
}

impl Drop for TaskHandle {
    fn drop(&mut self) {
        if !self.finished {
            self.progress.finish(self.id, Outcome::Failed, Some("did not finish".into()));
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while drawing must not take reporting down with it.
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn emit(shared: &mut Shared, line: &str) {
    let _ = writeln!(shared.out, "{line}");
}

fn clear_region(shared: &mut Shared) {
    let mut buf = String::new();
    if shared.drawn > 0 {
        buf.push_str(&format!("\x1b[{}A\r\x1b[J", shared.drawn));
        shared.drawn = 0;
    }
    if shared.cursor_hidden {
        buf.push_str("\x1b[?25h");
        shared.cursor_hidden = false;
    }
    let _ = shared.out.write_all(buf.as_bytes());
    let _ = shared.out.flush();
}

fn tick(inner: Weak<Inner>) {
    loop {
        std::thread::sleep(FRAME_INTERVAL);
        let Some(inner) = inner.upgrade() else { return };
        let progress = Progress { inner };
        if lock(&progress.inner.shared).closed {
            return;
        }
        progress.frame();
    }
}

/// Save the full log of every failed task; returns `(task, path)`.
fn save_failed_logs(shared: &mut Shared) -> Vec<(usize, PathBuf)> {
    let Some(dir) = shared.log_dir.clone() else { return Vec::new() };
    let mut saved = Vec::new();
    for (i, task) in shared.board.tasks.iter_mut().enumerate() {
        let failed = matches!(task.state(), State::Done { outcome, .. } if outcome.is_failure());
        if !failed || task.log.total() == 0 {
            continue;
        }
        let file: String =
            task.name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect();
        let path = dir.join(format!("{file}.log"));
        if task.log.save(&path).is_ok() {
            saved.push((i, path));
        }
    }
    saved
}

fn print_failure_logs(shared: &mut Shared, config: &Config, saved: &[(usize, PathBuf)]) {
    let mut out: Vec<String> = Vec::new();
    for (i, task) in shared.board.tasks.iter_mut().enumerate() {
        let State::Done { outcome, .. } = task.state() else { continue };
        if !outcome.is_failure() || task.log.total() == 0 {
            continue;
        }
        let total = task.log.total();
        let word = if outcome == Outcome::Interrupted { "interrupted" } else { "failed" };
        if config.log_lines == 0 {
            out.push(format!("── {} {word}: full log ({total} lines) ──", task.name));
            let mut all = Vec::new();
            let _ = task.log.write_all(&mut all);
            out.extend(String::from_utf8_lossy(&all).lines().map(str::to_string));
        } else {
            let shown = config.log_lines.min(total);
            let label = if shown < total { format!("last {shown} of {total} lines") } else { format!("{total} lines") };
            out.push(format!("── {} {word}: {label} ──", task.name));
            out.extend(task.log.tail(shown).map(str::to_string));
        }
        if let Some((_, path)) = saved.iter().find(|(j, _)| *j == i) {
            out.push(format!("── full log: {} ──", path.display()));
        }
    }
    for line in out {
        emit(shared, &line);
    }
}

fn print_warning_logs(shared: &mut Shared) {
    let mut out: Vec<String> = Vec::new();
    for task in &shared.board.tasks {
        if !matches!(task.state(), State::Done { outcome: Outcome::Warned, .. }) || task.log.total() == 0 {
            continue;
        }
        out.push(format!("── {}: warning, last output ──", task.name));
        out.extend(task.log.tail(10).map(str::to_string));
    }
    for line in out {
        emit(shared, &line);
    }
}

fn terminal_rows() -> Option<usize> {
    let mut size = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCGWINSZ writes a winsize into the pointer we own.
    let rc = unsafe { libc::ioctl(2, libc::TIOCGWINSZ, &mut size) };
    (rc == 0 && size.ws_row > 0).then_some(size.ws_row as usize)
}

#[cfg(test)]
mod tests;
