//! Run a child process and *wait* for it.
//!
//! Every external command simmer starts (nix, the bash harness,
//! providers) goes through [`run`], which guarantees:
//!
//! - the call returns only after the child has exited and been reaped;
//! - stderr is streamed line by line (bounded line length) to a sink while
//!   it runs, stdout is captured (bounded size);
//! - a non-zero exit is an error carrying the exit status, which maps to
//!   simmer's own exit code ([`exit_code_of`]);
//! - on an interrupt (SIGINT/SIGTERM/SIGHUP, once [`signals::install`]
//!   ran) or a deadline, the child's whole process group is sent SIGTERM,
//!   then SIGKILL after a grace period, and reaped — no orphans;
//! - grandchildren that keep the output pipes open after the child exits
//!   are killed too, so a stray background process cannot hang simmer.

use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// Longest stderr line delivered to a sink; longer runs are split.
pub const MAX_LINE: usize = 16 * 1024;
/// Most stdout bytes captured from one child.
pub const MAX_STDOUT: usize = 32 * 1024 * 1024;
/// SIGTERM → SIGKILL grace period.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(3);
/// How often the wait loop checks for an interrupt or a deadline.
const POLL: Duration = Duration::from_millis(25);

/// Process-wide interrupt handling. `install` replaces the default
/// "die immediately" behaviour of SIGINT/SIGTERM/SIGHUP with a flag that
/// [`run`] polls, so simmer can stop its children before it exits.
pub mod signals {
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

    static PENDING: AtomicI32 = AtomicI32::new(0);
    static INSTALLED: AtomicBool = AtomicBool::new(false);

    extern "C" fn record(signal: libc::c_int) {
        // Only an atomic store: async-signal-safe.
        PENDING.store(signal, Ordering::SeqCst);
    }

    /// Install the handlers. Children started afterwards run in their own
    /// process group (so a terminal Ctrl-C reaches simmer, which then
    /// tears the group down in order).
    pub fn install() -> std::io::Result<()> {
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            // SAFETY: a zeroed sigaction with a plain handler and an empty
            // mask is a valid argument; `record` is async-signal-safe.
            let rc = unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = record as extern "C" fn(libc::c_int) as libc::sighandler_t;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(signal, &action, std::ptr::null_mut())
            };
            if rc != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        INSTALLED.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub fn installed() -> bool {
        INSTALLED.load(Ordering::SeqCst)
    }

    /// The signal received since install, if any.
    pub fn pending() -> Option<i32> {
        match PENDING.load(Ordering::SeqCst) {
            0 => None,
            signal => Some(signal),
        }
    }
}

/// A cancellation flag local to one operation (tests, embedders);
/// checked alongside [`signals::pending`].
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Why a waited-on operation stopped early, if it did: the received
/// signal, a local cancel (reported as SIGINT), or nothing.
pub fn interrupted(cancel: Option<&Cancel>) -> Option<i32> {
    signals::pending().or_else(|| cancel.filter(|c| c.is_cancelled()).map(|_| libc::SIGINT))
}

#[derive(Debug, Clone)]
pub struct RunOpts {
    /// Give up (and stop the child) at this instant.
    pub deadline: Option<Instant>,
    pub cancel: Option<Cancel>,
    pub grace: Duration,
    /// Start the child in its own process group so the whole tree can be
    /// stopped. `None`: only when signal handling is installed (without
    /// it, a terminal Ctrl-C must still reach the child directly).
    pub own_group: Option<bool>,
}

impl Default for RunOpts {
    fn default() -> Self {
        RunOpts { deadline: None, cancel: None, grace: DEFAULT_GRACE, own_group: None }
    }
}

/// Receives each stderr line of a running child.
pub type LineSink = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Debug)]
pub enum RunError {
    Spawn { program: String, source: std::io::Error },
    Failed { program: String, status: ExitStatus },
    TimedOut { program: String, after: Duration },
    Interrupted { signal: i32 },
    OutputTooLarge { program: String },
    Io(std::io::Error),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Spawn { program, source } if source.kind() == std::io::ErrorKind::NotFound => {
                write!(f, "could not run `{program}`: not found on PATH")?;
                if program == "nix" {
                    write!(f, " (simmer needs Nix: https://nixos.org/download)")?;
                }
                Ok(())
            }
            RunError::Spawn { program, source } => write!(f, "could not run `{program}`: {source}"),
            RunError::Failed { program, status } => match (status.code(), status.signal()) {
                (Some(code), _) => write!(f, "`{program}` exited with status {code}"),
                (None, Some(signal)) => write!(f, "`{program}` was killed by signal {signal}"),
                (None, None) => write!(f, "`{program}` failed ({status})"),
            },
            RunError::TimedOut { program, after } => write!(
                f,
                "`{program}` did not finish within {} and was stopped (raise the limit with --timeout or SIMMER_TIMEOUT)",
                crate::term::fmt_duration(*after)
            ),
            RunError::Interrupted { signal } => write!(f, "interrupted by {}", signal_name(*signal)),
            RunError::OutputTooLarge { program } => {
                write!(f, "`{program}` printed more than {} MiB on stdout", MAX_STDOUT / (1024 * 1024))
            }
            RunError::Io(e) => write!(f, "waiting for child: {e}"),
        }
    }
}

impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RunError::Spawn { source, .. } | RunError::Io(source) => Some(source),
            _ => None,
        }
    }
}

impl RunError {
    /// The exit code simmer should end with: the child's own status, 128 +
    /// signal for a signal, 124 for a timeout (like `timeout(1)`), 127/126
    /// when the program could not be run.
    pub fn exit_code(&self) -> i32 {
        match self {
            RunError::Failed { status, .. } => match (status.code(), status.signal()) {
                (Some(code), _) if code != 0 => code,
                (None, Some(signal)) => 128 + signal,
                _ => 1,
            },
            RunError::TimedOut { .. } => 124,
            RunError::Interrupted { signal } => 128 + signal,
            RunError::Spawn { source, .. } if source.kind() == std::io::ErrorKind::NotFound => 127,
            RunError::Spawn { .. } => 126,
            RunError::OutputTooLarge { .. } | RunError::Io(_) => 1,
        }
    }
}

pub fn signal_name(signal: i32) -> String {
    match signal {
        libc::SIGINT => "SIGINT (Ctrl-C)".into(),
        libc::SIGTERM => "SIGTERM".into(),
        libc::SIGHUP => "SIGHUP".into(),
        other => format!("signal {other}"),
    }
}

/// Whether `err` (anywhere in its chain) is an interrupt.
pub fn is_interrupt(err: &anyhow::Error) -> bool {
    err.chain().any(|e| matches!(e.downcast_ref::<RunError>(), Some(RunError::Interrupted { .. })))
}

/// The exit code for a failed command: the first [`RunError`] in the
/// chain decides, anything else is 1.
pub fn exit_code_of(err: &anyhow::Error) -> i32 {
    err.chain().find_map(|e| e.downcast_ref::<RunError>()).map_or(1, RunError::exit_code)
}

/// Run `cmd` to completion. stdin is `/dev/null`; stderr lines go to
/// `stderr_sink` (or to simmer's own stderr when `None`); stdout is
/// returned. See the module docs for the waiting guarantees.
pub fn run(cmd: &mut Command, opts: &RunOpts, stderr_sink: Option<LineSink>) -> Result<Vec<u8>, RunError> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    if let Some(signal) = interrupted(opts.cancel.as_ref()) {
        return Err(RunError::Interrupted { signal });
    }
    let own_group = opts.own_group.unwrap_or_else(signals::installed);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped());
    cmd.stderr(if stderr_sink.is_some() { Stdio::piped() } else { Stdio::inherit() });
    if own_group {
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|source| RunError::Spawn { program: program.clone(), source })?;
    let pid = child.id() as libc::pid_t;
    let target = if own_group { -pid } else { pid };

    // Readers are detached threads: if a grandchild we may not kill holds
    // a pipe open forever, simmer still returns.
    let (done_tx, done_rx) = mpsc::channel::<Stream>();
    let mut open = 0;
    let stdout = child.stdout.take();
    if let Some(mut pipe) = stdout {
        open += 1;
        let tx = done_tx.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let mut overflow = false;
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) if buf.len() + n <= MAX_STDOUT => buf.extend_from_slice(&chunk[..n]),
                    // Keep draining so the child never blocks on a full pipe.
                    Ok(_) => overflow = true,
                }
            }
            let _ = tx.send(Stream::Stdout { buf, overflow });
        });
    }
    if let (Some(pipe), Some(sink)) = (child.stderr.take(), stderr_sink) {
        open += 1;
        let tx = done_tx.clone();
        std::thread::spawn(move || {
            read_lines(pipe, MAX_LINE, |line| sink(line));
            let _ = tx.send(Stream::Stderr);
        });
    }
    drop(done_tx);

    let (exit_tx, exit_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = exit_tx.send(child.wait());
    });

    let started = Instant::now();
    let mut stopping: Option<(Instant, RunError)> = None;
    let status = loop {
        let wait = match (&stopping, opts.deadline) {
            (Some((at, _)), _) => (*at + opts.grace).saturating_duration_since(Instant::now()).min(POLL),
            (None, Some(deadline)) => deadline.saturating_duration_since(Instant::now()).min(POLL),
            (None, None) => POLL,
        };
        match exit_rx.recv_timeout(wait.max(Duration::from_millis(1))) {
            Ok(Ok(status)) => break status,
            Ok(Err(e)) => return Err(RunError::Io(e)),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(RunError::Io(std::io::Error::other("wait thread vanished")))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        match &stopping {
            None => {
                let reason = if let Some(signal) = interrupted(opts.cancel.as_ref()) {
                    Some(RunError::Interrupted { signal })
                } else if opts.deadline.is_some_and(|d| Instant::now() >= d) {
                    Some(RunError::TimedOut { program: program.clone(), after: started.elapsed() })
                } else {
                    None
                };
                if let Some(reason) = reason {
                    send(target, libc::SIGTERM);
                    stopping = Some((Instant::now(), reason));
                }
            }
            Some((at, _)) if at.elapsed() >= opts.grace => {
                send(target, libc::SIGKILL);
            }
            Some(_) => {}
        }
    };

    // The child is reaped. Collect its output; if the pipes stay open past
    // a grace period, a grandchild holds them — stop the group.
    let mut captured = Vec::new();
    let mut overflow = false;
    let mut grace_end = Instant::now() + opts.grace.min(Duration::from_millis(500));
    let mut killed = false;
    while open > 0 {
        match done_rx.recv_timeout(grace_end.saturating_duration_since(Instant::now())) {
            Ok(Stream::Stdout { buf, overflow: o }) => {
                captured = buf;
                overflow = o;
                open -= 1;
            }
            Ok(Stream::Stderr) => open -= 1,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) if own_group && !killed => {
                send(target, libc::SIGKILL);
                killed = true;
                grace_end = Instant::now() + Duration::from_secs(1);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
        }
    }

    if let Some((_, reason)) = stopping {
        return Err(reason);
    }
    if !status.success() {
        return Err(RunError::Failed { program, status });
    }
    if overflow {
        return Err(RunError::OutputTooLarge { program });
    }
    Ok(captured)
}

enum Stream {
    Stdout { buf: Vec<u8>, overflow: bool },
    Stderr,
}

fn send(target: libc::pid_t, signal: libc::c_int) {
    // SAFETY: kill only sends a signal; ESRCH (already gone) is fine.
    unsafe {
        libc::kill(target, signal);
    }
}

/// Split a byte stream into lines without unbounded buffering: a line
/// longer than `max` bytes is delivered in `max`-sized pieces.
pub fn read_lines(mut reader: impl Read, max: usize, mut f: impl FnMut(&str)) {
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        let mut start = 0;
        while let Some(pos) = buf[start..].iter().position(|b| *b == b'\n') {
            for piece in buf[start..start + pos].chunks(max.max(1)) {
                f(&String::from_utf8_lossy(piece));
            }
            if pos == 0 {
                f("");
            }
            start += pos + 1;
        }
        buf.drain(..start);
        while buf.len() >= max {
            f(&String::from_utf8_lossy(&buf[..max]));
            buf.drain(..max);
        }
    }
    if !buf.is_empty() {
        f(&String::from_utf8_lossy(&buf));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }

    fn collect() -> (LineSink, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink_lines = lines.clone();
        let sink: LineSink = Arc::new(move |l: &str| sink_lines.lock().unwrap().push(l.to_string()));
        (sink, lines)
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only checks for existence.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Poll a condition that another process will make true, failing
    /// (never hanging) after a generous bound.
    fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(20);
        while !cond() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn success_returns_stdout_and_streams_stderr_in_order() {
        let (sink, lines) = collect();
        let out = run(&mut sh("echo out; echo one >&2; echo two >&2"), &RunOpts::default(), Some(sink)).unwrap();
        assert_eq!(out, b"out\n");
        assert_eq!(*lines.lock().unwrap(), vec!["one", "two"]);
    }

    #[test]
    fn exit_status_propagates_as_the_exit_code() {
        let err = run(&mut sh("echo boom >&2; exit 7"), &RunOpts::default(), Some(collect().0)).unwrap_err();
        assert!(matches!(err, RunError::Failed { .. }));
        assert_eq!(err.exit_code(), 7);
        assert_eq!(err.to_string(), "`sh` exited with status 7");
        let wrapped = anyhow::Error::new(err).context("warming");
        assert_eq!(exit_code_of(&wrapped), 7);
        assert!(!is_interrupt(&wrapped));
    }

    #[test]
    fn a_signal_death_is_128_plus_the_signal() {
        let err = run(&mut sh("kill -TERM $$"), &RunOpts::default(), None).unwrap_err();
        assert_eq!(err.exit_code(), 128 + libc::SIGTERM);
    }

    #[test]
    fn missing_programs_say_so_with_127() {
        let err = run(&mut Command::new("simmer-test-no-such-program"), &RunOpts::default(), None).unwrap_err();
        assert_eq!(err.exit_code(), 127);
        assert!(err.to_string().contains("not found on PATH"));
    }

    #[test]
    fn cancel_stops_the_whole_process_group_and_leaves_no_orphan() {
        let dir = std::env::temp_dir().join(format!("simmer-supervise-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("grandchild.pid");
        let script = format!("sleep 1000 & echo $! > '{}'.tmp; mv '{0}'.tmp '{0}'; wait", pidfile.display());
        let cancel = Cancel::default();
        let opts = RunOpts { cancel: Some(cancel.clone()), own_group: Some(true), ..RunOpts::default() };
        let handle = std::thread::spawn(move || run(&mut sh(&script), &opts, Some(collect().0)));
        eventually("the grandchild to start", || pidfile.exists());
        let grandchild: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        assert!(alive(grandchild));
        cancel.cancel();
        let err = handle.join().unwrap().unwrap_err();
        assert!(matches!(err, RunError::Interrupted { signal } if signal == libc::SIGINT));
        assert_eq!(err.exit_code(), 130);
        // The grandchild was in the child's process group: it is gone too
        // (reparented and reaped by init after SIGTERM).
        eventually("the grandchild to exit", || !alive(grandchild));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_already_cancelled_run_spawns_nothing() {
        let cancel = Cancel::default();
        cancel.cancel();
        let opts = RunOpts { cancel: Some(cancel), ..RunOpts::default() };
        assert!(matches!(run(&mut sh("exit 0"), &opts, None), Err(RunError::Interrupted { .. })));
    }

    #[test]
    fn deadline_stops_the_child_with_a_clear_message() {
        let opts = RunOpts {
            deadline: Some(Instant::now() + Duration::from_millis(100)),
            own_group: Some(true),
            grace: Duration::from_millis(100),
            ..RunOpts::default()
        };
        // `trap '' TERM` makes the shell ignore SIGTERM: only SIGKILL ends it.
        let err = run(&mut sh("trap '' TERM; while :; do sleep 1; done"), &opts, None).unwrap_err();
        assert!(matches!(err, RunError::TimedOut { .. }));
        assert_eq!(err.exit_code(), 124);
        assert!(err.to_string().contains("--timeout"));
    }

    #[test]
    fn a_grandchild_holding_the_pipe_does_not_hang_the_wait() {
        let dir = std::env::temp_dir().join(format!("simmer-supervise-stray-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("stray.pid");
        let opts = RunOpts { own_group: Some(true), grace: Duration::from_millis(100), ..RunOpts::default() };
        let script = format!("sleep 1000 & echo $! > '{}'; echo done", pidfile.display());
        let out = run(&mut sh(&script), &opts, Some(collect().0)).unwrap();
        assert_eq!(out, b"done\n");
        let stray: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        eventually("the stray grandchild to be killed", || !alive(stray));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn line_reader_bounds_long_lines() {
        let input = format!("short\n\n{}\ntail", "x".repeat(25));
        let mut lines = Vec::new();
        read_lines(input.as_bytes(), 10, |l| lines.push(l.to_string()));
        assert_eq!(lines, vec!["short", "", "xxxxxxxxxx", "xxxxxxxxxx", "xxxxx", "tail"]);
    }
}
