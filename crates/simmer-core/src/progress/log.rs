//! Bounded per-task log storage.
//!
//! The newest lines live in memory in a ring bounded by both line count
//! and bytes; a line pushed out of the ring is appended to a spill file
//! instead of being dropped, so the full log can still be shown (or saved)
//! when a task fails. Memory use is constant per task no matter how much
//! a child prints.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Lines kept in memory per task.
pub const RING_LINES: usize = 256;
/// Bytes kept in memory per task.
pub const RING_BYTES: usize = 64 * 1024;
/// Longest single line stored; longer lines are cut (with a marker).
pub const MAX_LINE_BYTES: usize = 4 * 1024;

#[derive(Debug)]
pub struct LogBuffer {
    ring: VecDeque<String>,
    ring_bytes: usize,
    max_lines: usize,
    max_bytes: usize,
    spill: Option<Spill>,
    spilled: usize,
    /// Lines that could not be spilled (the temp file failed); counted so
    /// the gap is reported rather than silent.
    lost: usize,
}

#[derive(Debug)]
struct Spill {
    path: PathBuf,
    file: BufWriter<File>,
}

impl Drop for Spill {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::with_limits(RING_LINES, RING_BYTES)
    }
}

impl LogBuffer {
    pub fn with_limits(max_lines: usize, max_bytes: usize) -> Self {
        LogBuffer {
            ring: VecDeque::new(),
            ring_bytes: 0,
            max_lines: max_lines.max(1),
            max_bytes: max_bytes.max(1),
            spill: None,
            spilled: 0,
            lost: 0,
        }
    }

    pub fn push(&mut self, line: &str) {
        let line = clip(line);
        self.ring_bytes += line.len();
        self.ring.push_back(line);
        while self.ring.len() > self.max_lines || (self.ring_bytes > self.max_bytes && self.ring.len() > 1) {
            let Some(old) = self.ring.pop_front() else { break };
            self.ring_bytes -= old.len();
            self.spill_line(&old);
        }
    }

    fn spill_line(&mut self, line: &str) {
        if self.spill.is_none() {
            self.spill = open_spill();
        }
        let written = match &mut self.spill {
            Some(spill) => writeln!(spill.file, "{line}").is_ok(),
            None => false,
        };
        if written {
            self.spilled += 1;
        } else {
            self.lost += 1;
        }
    }

    /// Total lines ever pushed (in memory, spilled, or lost).
    pub fn total(&self) -> usize {
        self.ring.len() + self.spilled + self.lost
    }

    /// The newest `n` lines, oldest first.
    pub fn tail(&self, n: usize) -> impl Iterator<Item = &str> {
        let skip = self.ring.len().saturating_sub(n);
        self.ring.iter().skip(skip).map(String::as_str)
    }

    /// Write the whole log (spilled part, then the ring) to `out`.
    pub fn write_all(&mut self, out: &mut dyn Write) -> std::io::Result<()> {
        if self.lost > 0 {
            writeln!(out, "[simmer: {} earlier lines could not be kept]", self.lost)?;
        }
        if let Some(spill) = &mut self.spill {
            spill.file.flush()?;
            let mut file = File::open(&spill.path)?;
            std::io::copy(&mut file, out)?;
        }
        for line in &self.ring {
            writeln!(out, "{line}")?;
        }
        Ok(())
    }

    /// Save the whole log to `dest` (replacing it).
    pub fn save(&mut self, dest: &Path) -> std::io::Result<()> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = dest.with_extension("log.tmp");
        {
            let mut file = BufWriter::new(File::create(&tmp)?);
            self.write_all(&mut file)?;
            file.flush()?;
        }
        std::fs::rename(&tmp, dest)
    }
}

fn clip(line: &str) -> String {
    if line.len() <= MAX_LINE_BYTES {
        return line.to_string();
    }
    let mut end = MAX_LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} [… {} bytes cut]", &line[..end], line.len() - end)
}

fn open_spill() -> Option<Spill> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir();
    for _ in 0..4 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("simmer-log-{}-{n}.log", std::process::id()));
        if let Ok(file) = std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            return Some(Spill { path, file: BufWriter::new(file) });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_is_bounded_and_overflow_spills_in_order() {
        let mut log = LogBuffer::with_limits(3, 1024);
        for i in 0..10 {
            log.push(&format!("line {i}"));
        }
        assert_eq!(log.tail(10).collect::<Vec<_>>(), vec!["line 7", "line 8", "line 9"]);
        assert_eq!(log.tail(2).collect::<Vec<_>>(), vec!["line 8", "line 9"]);
        assert_eq!(log.total(), 10);
        let mut all = Vec::new();
        log.write_all(&mut all).unwrap();
        let text = String::from_utf8(all).unwrap();
        let expected: String = (0..10).map(|i| format!("line {i}\n")).collect();
        assert_eq!(text, expected);
    }

    #[test]
    fn byte_bound_applies_and_long_lines_are_clipped() {
        let mut log = LogBuffer::with_limits(100, 10);
        log.push("aaaaaaaa");
        log.push("bbbbbbbb");
        assert_eq!(log.tail(10).collect::<Vec<_>>(), vec!["bbbbbbbb"]);
        let long = "x".repeat(MAX_LINE_BYTES + 10);
        let mut log = LogBuffer::default();
        log.push(&long);
        let kept = log.tail(1).next().unwrap();
        assert!(kept.starts_with(&"x".repeat(MAX_LINE_BYTES)) && kept.ends_with(" [… 10 bytes cut]"));
        assert!(!kept.contains(&"x".repeat(MAX_LINE_BYTES + 1)));
    }

    #[test]
    fn spill_file_is_removed_on_drop() {
        let mut log = LogBuffer::with_limits(1, 1024);
        log.push("a");
        log.push("b");
        let path = log.spill.as_ref().unwrap().path.clone();
        assert!(path.exists());
        drop(log);
        assert!(!path.exists());
    }
}
