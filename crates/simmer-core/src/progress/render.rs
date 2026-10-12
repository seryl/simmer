//! Pure rendering: a [`Board`] plus a time and a terminal size in, lines
//! of text out. Nothing here touches a terminal or a clock, so every
//! frame is reproducible in a test.

use super::log::LogBuffer;
use crate::term::{fmt_duration, sanitize, shorten_store_paths, str_width, truncate, truncate_start};
use std::time::Duration;

/// How a task ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// Finished, but something deserves attention (a provider produced
    /// nothing). Does not fail the run.
    Warned,
    Failed,
    /// Not needed this time (already up to date, not applicable).
    Skipped,
    Interrupted,
}

impl Outcome {
    pub fn word(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Warned => "warning",
            Outcome::Failed => "failed",
            Outcome::Skipped => "skipped",
            Outcome::Interrupted => "interrupted",
        }
    }

    pub fn is_failure(self) -> bool {
        matches!(self, Outcome::Failed | Outcome::Interrupted)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum State {
    Queued,
    Running { since: Duration },
    Done { since: Duration, until: Duration, outcome: Outcome },
}

#[derive(Debug, Default)]
pub struct Task {
    pub name: String,
    pub state: Option<State>,
    /// What the task is doing right now (e.g. the current nix activity).
    pub phase: Option<String>,
    /// One-line reason attached to the outcome.
    pub message: Option<String>,
    /// Captured output, sanitized, bounded.
    pub log: LogBuffer,
}

impl Task {
    pub fn new(name: &str) -> Self {
        Task { name: name.to_string(), state: Some(State::Queued), ..Default::default() }
    }

    pub fn state(&self) -> State {
        self.state.unwrap_or(State::Queued)
    }

    pub fn duration(&self, now: Duration) -> Option<Duration> {
        match self.state() {
            State::Queued => None,
            State::Running { since } => Some(now.saturating_sub(since)),
            State::Done { since, until, .. } => Some(until.saturating_sub(since)),
        }
    }

    pub fn push_output(&mut self, line: &str) {
        self.log.push(&sanitize(line));
    }
}

/// Everything the live view knows.
#[derive(Debug, Default)]
pub struct Board {
    pub title: String,
    pub tasks: Vec<Task>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub running: usize,
    pub done: usize,
    pub failed: usize,
    pub queued: usize,
}

impl Board {
    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for task in &self.tasks {
            match task.state() {
                State::Queued => c.queued += 1,
                State::Running { .. } => c.running += 1,
                State::Done { outcome, .. } if outcome.is_failure() => c.failed += 1,
                State::Done { .. } => c.done += 1,
            }
        }
        c
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub color: bool,
    /// Output lines shown under each running task.
    pub tail_lines: usize,
}

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const SPINNER_FRAME: u128 = 80;
const NAME_COLUMN_CAP: usize = 28;

pub fn spinner(now: Duration) -> char {
    SPINNER[((now.as_millis() / SPINNER_FRAME) % SPINNER.len() as u128) as usize]
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Paint {
    Plain,
    Bold,
    Dim,
    Green,
    Red,
    Yellow,
    Cyan,
}

impl Paint {
    fn code(self) -> Option<&'static str> {
        match self {
            Paint::Plain => None,
            Paint::Bold => Some("1"),
            Paint::Dim => Some("2"),
            Paint::Green => Some("32"),
            Paint::Red => Some("31"),
            Paint::Yellow => Some("33"),
            Paint::Cyan => Some("36"),
        }
    }
}

/// A line built from painted segments, truncated to a width over its
/// *visible* text so escape codes never count against the budget.
#[derive(Default)]
struct Line {
    segs: Vec<(String, Paint)>,
}

impl Line {
    fn push(mut self, text: impl Into<String>, paint: Paint) -> Self {
        self.segs.push((text.into(), paint));
        self
    }

    fn render(&self, max: usize, color: bool) -> String {
        let mut out = String::new();
        let mut left = max;
        for (text, paint) in &self.segs {
            if left == 0 {
                break;
            }
            let w = str_width(text);
            let shown = if w <= left { text.clone() } else { truncate(text, left) };
            left = left.saturating_sub(str_width(&shown));
            match paint.code().filter(|_| color && !shown.is_empty()) {
                Some(code) => {
                    out.push_str("\x1b[");
                    out.push_str(code);
                    out.push('m');
                    out.push_str(&shown);
                    out.push_str("\x1b[0m");
                }
                None => out.push_str(&shown),
            }
            if w > str_width(&shown) {
                break;
            }
        }
        out
    }
}

/// The in-place region: a header, then each running task with the tail
/// of its output. `width`/`height` are the terminal's; lines stay one
/// column short of the edge (so no terminal auto-wraps them) and the frame
/// never grows taller than the screen (so moving the cursor back up to
/// redraw stays exact).
pub fn live_frame(board: &Board, now: Duration, width: usize, height: usize, style: &Style) -> Vec<String> {
    let max = width.saturating_sub(1).max(1);
    let rows = height.saturating_sub(1).max(1);
    let mut frame = vec![header(board, now, max).render(max, style.color)];
    let running: Vec<&Task> = board.tasks.iter().filter(|t| matches!(t.state(), State::Running { .. })).collect();
    if running.is_empty() || rows < 2 {
        return frame;
    }
    // Rows left after the header. Every running task gets its line when it
    // fits; tails share what is left over.
    let budget = rows - 1;
    let (shown, hidden) = if running.len() > budget {
        (budget.saturating_sub(1), running.len() - budget.saturating_sub(1))
    } else {
        (running.len(), 0)
    };
    let spare = budget - shown - usize::from(hidden > 0);
    let tail = spare.checked_div(shown).map_or(0, |per_task| style.tail_lines.min(per_task));
    let name_col = running[..shown].iter().map(|t| str_width(&t.name)).max().unwrap_or(0).min(NAME_COLUMN_CAP);
    for task in &running[..shown] {
        frame.push(task_line(task, now, name_col, max).render(max, style.color));
        // The newest output often repeats the phase shown on the task line
        // (nix reports an activity as both); show it once.
        let phase = task.phase.as_deref();
        let lines: Vec<&str> = task.log.tail(tail + 1).filter(|l| Some(*l) != phase).collect();
        for line in &lines[lines.len().saturating_sub(tail)..] {
            let line = shorten_store_paths(line);
            frame.push(Line::default().push("      │ ", Paint::Dim).push(line, Paint::Dim).render(max, style.color));
        }
    }
    if hidden > 0 {
        frame.push(Line::default().push(format!("  … {hidden} more running"), Paint::Dim).render(max, style.color));
    }
    frame
}

fn header(board: &Board, now: Duration, max: usize) -> Line {
    let c = board.counts();
    let elapsed = format!("  {}  ", fmt_duration(now));
    let counts = format!("{} running · {} done · ", c.running, c.done);
    let failed = format!("{} failed", c.failed);
    let queued = format!(" · {} queued", c.queued);
    // The status (time and counts) matters more than the full title: on a
    // narrow terminal the title gives way first, keeping its end.
    let status = str_width(&elapsed) + str_width(&counts) + str_width(&failed) + str_width(&queued);
    let room = max.saturating_sub(2 + status).max(24);
    Line::default()
        .push(format!("{} ", spinner(now)), Paint::Cyan)
        .push(truncate_start(&board.title, room), Paint::Bold)
        .push(elapsed, Paint::Plain)
        .push(counts, Paint::Dim)
        .push(failed, if c.failed > 0 { Paint::Red } else { Paint::Dim })
        .push(queued, Paint::Dim)
}

fn task_line(task: &Task, now: Duration, name_col: usize, max: usize) -> Line {
    let elapsed = task.duration(now).map(fmt_duration).unwrap_or_default();
    // Align names into a column only when the aligned line still shows
    // the elapsed time; on a narrow terminal the time matters more.
    let fits = 4 + name_col + 2 + elapsed.len() <= max;
    let pad = if fits { name_col.saturating_sub(str_width(&task.name)) } else { 0 };
    let mut line = Line::default()
        .push(format!("  {} ", spinner(now)), Paint::Cyan)
        .push(task.name.clone(), Paint::Bold)
        .push(format!("{}  {elapsed}", " ".repeat(pad)), Paint::Plain);
    if let Some(phase) = task.phase.as_deref().filter(|p| !p.is_empty()) {
        line = line.push(format!("  {}", shorten_store_paths(phase)), Paint::Dim);
    }
    line
}

/// The permanent line a finished task leaves in the scrollback.
pub fn done_line(task: &Task, now: Duration, width: Option<usize>, color: bool) -> String {
    let State::Done { outcome, .. } = task.state() else { return String::new() };
    let (mark, paint) = match outcome {
        Outcome::Ok => ("✓", Paint::Green),
        Outcome::Warned => ("!", Paint::Yellow),
        Outcome::Failed => ("✗", Paint::Red),
        Outcome::Skipped => ("-", Paint::Dim),
        Outcome::Interrupted => ("✗", Paint::Red),
    };
    let elapsed = task.duration(now).map(fmt_duration).unwrap_or_default();
    let mut line = Line::default()
        .push("  ", Paint::Plain)
        .push(mark, paint)
        .push(" ", Paint::Plain)
        .push(task.name.clone(), if outcome.is_failure() { Paint::Red } else { Paint::Plain })
        .push(format!("  {elapsed}"), Paint::Dim);
    let detail = match (outcome, task.message.as_deref()) {
        (Outcome::Interrupted, None) => Some("interrupted"),
        (_, message) => message,
    };
    if let Some(message) = detail {
        line = line.push(format!("  {message}"), if outcome.is_failure() { Paint::Red } else { Paint::Dim });
    }
    line.render(width.map_or(usize::MAX, |w| w.saturating_sub(1).max(1)), color)
}

/// A table of every task: name, status, duration. Printed after a failure
/// or an interrupt (and on request), so the whole run is visible at once.
pub fn summary(board: &Board, now: Duration) -> Vec<String> {
    let name_col = board.tasks.iter().map(|t| str_width(&t.name)).max().unwrap_or(4).max(4);
    let mut out = vec![format!("  {:<name_col$}  {:<11}  time", "task", "status")];
    for task in &board.tasks {
        let status = match task.state() {
            State::Queued => "not started",
            State::Running { .. } => "running",
            State::Done { outcome, .. } => outcome.word(),
        };
        let time = task.duration(now).map(fmt_duration).unwrap_or_else(|| "—".to_string());
        let pad = name_col.saturating_sub(str_width(&task.name));
        out.push(format!("  {}{}  {status:<11}  {time}", task.name, " ".repeat(pad)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn task(name: &str, state: State) -> Task {
        Task { name: name.into(), state: Some(state), ..Default::default() }
    }

    fn board() -> Board {
        let mut nix = task("nix build", State::Running { since: ms(100) });
        nix.phase = Some(format!("building '/nix/store/{}-hello-2.12.drv'", "a".repeat(32)));
        for line in ["unpacking sources", "configuring", "\x1b[1mcompiling\x1b[0m hello.c"] {
            nix.push_output(line);
        }
        let mut pin = task("pin flake inputs", State::Running { since: ms(1_000) });
        pin.push_output("copying path '/nix/store/x-source'");
        Board {
            title: "warming ~/proj".into(),
            tasks: vec![
                nix,
                pin,
                task("provider CAP", State::Queued),
                task("resolve", State::Done { since: ms(0), until: ms(100), outcome: Outcome::Ok }),
            ],
        }
    }

    const PLAIN: Style = Style { color: false, tail_lines: 2 };

    #[test]
    fn live_frame_exact_at_a_fixed_time_and_width() {
        let frame = live_frame(&board(), ms(4_250), 80, 24, &PLAIN);
        assert_eq!(
            frame,
            vec![
                // 4250 ms / 80 ms = frame 53 → 53 % 10 = 3 → '⠸'
                "⠸ warming ~/proj  4.2s  2 running · 1 done · 0 failed · 1 queued",
                "  ⠸ nix build         4.1s  building 'hello-2.12.drv'",
                "      │ configuring",
                "      │ compiling hello.c",
                "  ⠸ pin flake inputs  3.2s",
                "      │ copying path '/nix/store/x-source'",
            ]
        );
    }

    #[test]
    fn narrow_terminals_truncate_every_line_with_a_marker() {
        let frame = live_frame(&board(), ms(4_250), 24, 24, &PLAIN);
        for line in &frame {
            assert!(str_width(line) <= 23, "{line:?} is wider than 23 columns");
        }
        assert_eq!(frame[0], "⠸ warming ~/proj  4.2s…");
        assert_eq!(frame[1], "  ⠸ nix build  4.1s  b…");
        // Degenerate width still produces a frame, never a panic.
        assert!(live_frame(&board(), ms(0), 1, 24, &PLAIN).iter().all(|l| str_width(l) <= 1));
    }

    #[test]
    fn long_titles_give_way_to_the_status() {
        let mut b = board();
        b.title = "warming /home/someone/src/a/deeply/nested/project/directory".into();
        let header = &live_frame(&b, ms(4_250), 80, 24, &PLAIN)[0];
        assert_eq!(header, "⠸ …ply/nested/project/directory  4.2s  2 running · 1 done · 0 failed · 1 queued");
        assert!(str_width(header) <= 79);
    }

    #[test]
    fn short_terminals_drop_tails_then_collapse_tasks() {
        // 4 rows → 3 usable: header + 2 task lines, no tails.
        let frame = live_frame(&board(), ms(4_250), 80, 4, &PLAIN);
        assert_eq!(frame.len(), 3);
        assert!(frame[1].contains("nix build") && frame[2].contains("pin flake inputs"));
        // 3 rows → 2 usable: header + "… more" summary line.
        let frame = live_frame(&board(), ms(4_250), 80, 3, &PLAIN);
        assert_eq!(frame, vec![frame[0].clone(), "  … 2 more running".to_string()]);
    }

    #[test]
    fn colour_codes_do_not_count_against_the_width() {
        let style = Style { color: true, tail_lines: 0 };
        let frame = live_frame(&board(), ms(4_250), 30, 24, &style);
        let visible = sanitize(&frame[0]);
        assert_eq!(visible, "⠸ warming ~/proj  4.2s  2 ru…");
        assert!(frame[0].contains("\x1b[1mwarming ~/proj\x1b[0m"));
    }

    #[test]
    fn done_lines_carry_mark_duration_and_reason() {
        let mut ok = task("nix build", State::Done { since: ms(0), until: ms(4_200), outcome: Outcome::Ok });
        assert_eq!(done_line(&ok, ms(9_999), None, false), "  ✓ nix build  4.2s");
        ok.state = Some(State::Done { since: ms(0), until: ms(120), outcome: Outcome::Failed });
        ok.message = Some("exit status 1".into());
        assert_eq!(done_line(&ok, ms(9_999), None, false), "  ✗ nix build  0.1s  exit status 1");
        ok.state = Some(State::Done { since: ms(0), until: ms(0), outcome: Outcome::Interrupted });
        ok.message = None;
        assert_eq!(done_line(&ok, ms(9_999), None, false), "  ✗ nix build  0.0s  interrupted");
        assert_eq!(
            done_line(&ok, ms(9_999), None, true),
            "  \x1b[31m✗\x1b[0m \x1b[31mnix build\x1b[0m\x1b[2m  0.0s\x1b[0m\x1b[31m  interrupted\x1b[0m"
        );
    }

    #[test]
    fn summary_lists_every_task() {
        assert_eq!(
            summary(&board(), ms(5_000)),
            vec![
                "  task              status       time",
                "  nix build         running      4.9s",
                "  pin flake inputs  running      4.0s",
                "  provider CAP      not started  —",
                "  resolve           ok           0.1s",
            ]
        );
    }
}
