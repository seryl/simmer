//! Terminal helpers: detection, width, and making arbitrary tool output
//! safe to draw inside a fixed-width live region.
//!
//! Everything here is either a pure function (testable without a
//! terminal) or a single `libc` query.

/// Whether file descriptor `fd` is a terminal.
pub fn is_tty(fd: i32) -> bool {
    // SAFETY: isatty only inspects the descriptor.
    unsafe { libc::isatty(fd) == 1 }
}

/// Column count of the terminal on `fd`, or `None` when it is not a
/// terminal (or reports zero columns). Queried per frame, so a resize
/// takes effect on the next redraw without a SIGWINCH handler.
pub fn width(fd: i32) -> Option<usize> {
    let mut size = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCGWINSZ writes a winsize into the pointer we own.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) };
    (rc == 0 && size.ws_col > 0).then_some(size.ws_col as usize)
}

/// Whether colour output is allowed by the environment
/// (<https://no-color.org>: any non-empty `NO_COLOR` disables it).
pub fn color_allowed() -> bool {
    std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()) && std::env::var("TERM").map_or(true, |t| t != "dumb")
}

/// Whether the terminal can redraw in place. `TERM=dumb` cannot.
pub fn can_redraw() -> bool {
    std::env::var("TERM").map_or(true, |t| t != "dumb")
}

/// Display width of one character: 0 for combining marks and controls,
/// 2 for wide East Asian characters and most emoji, 1 otherwise. A
/// deliberately small table: exact enough to keep lines from wrapping.
pub fn char_width(c: char) -> usize {
    let cp = c as u32;
    if cp < 0x20 || (0x7f..0xa0).contains(&cp) {
        return 0;
    }
    const ZERO: &[(u32, u32)] = &[
        (0x0300, 0x036F),
        (0x0483, 0x0489),
        (0x0591, 0x05BD),
        (0x200B, 0x200F),
        (0x20D0, 0x20FF),
        (0xFE00, 0xFE0F),
        (0xFE20, 0xFE2F),
    ];
    const WIDE: &[(u32, u32)] = &[
        (0x1100, 0x115F),
        (0x2E80, 0x303E),
        (0x3041, 0x33FF),
        (0x3400, 0x4DBF),
        (0x4E00, 0x9FFF),
        (0xA000, 0xA4CF),
        (0xAC00, 0xD7A3),
        (0xF900, 0xFAFF),
        (0xFE30, 0xFE4F),
        (0xFF00, 0xFF60),
        (0xFFE0, 0xFFE6),
        (0x1F300, 0x1F64F),
        (0x1F900, 0x1F9FF),
        (0x20000, 0x3FFFD),
    ];
    let within = |table: &[(u32, u32)]| table.iter().any(|&(lo, hi)| (lo..=hi).contains(&cp));
    if within(ZERO) {
        0
    } else if within(WIDE) {
        2
    } else {
        1
    }
}

/// Display width of a string of already-sanitized text.
pub fn str_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Cut `s` to at most `max` columns. When something was cut and there is
/// room, the last column becomes `…` so the reader knows.
pub fn truncate(s: &str, max: usize) -> String {
    if str_width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let budget = max - 1;
    let mut out = String::with_capacity(max + 3);
    let mut used = 0;
    for c in s.chars() {
        let w = char_width(c);
        if used + w > budget {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// Cut `s` to at most `max` columns keeping its *end* (`…/to/project`):
/// for paths, the end is the informative part.
pub fn truncate_start(s: &str, max: usize) -> String {
    if str_width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut kept: Vec<char> = Vec::new();
    let mut used = 1;
    for c in s.chars().rev() {
        let w = char_width(c);
        if used + w > max {
            break;
        }
        used += w;
        kept.push(c);
    }
    std::iter::once('…').chain(kept.into_iter().rev()).collect()
}

/// Turn one line of arbitrary tool output into plain printable text:
/// ANSI escape sequences are removed, a carriage return keeps only what
/// was drawn after it (progress bars), tabs become spaces, and any other
/// control character is dropped.
pub fn sanitize(line: &str) -> String {
    let line = match line.rfind('\r') {
        // A trailing CR (CRLF line ending) is not a redraw.
        Some(i) if i + 1 < line.len() => &line[i + 1..],
        _ => line.trim_end_matches('\r'),
    };
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek() {
                // CSI: parameters and intermediates, then one final byte.
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL or ST (ESC \).
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // Two-byte escapes.
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            '\t' => out.push_str("    "),
            c if char_width(c) == 0 && (c as u32) < 0xa0 => {}
            c => out.push(c),
        }
    }
    out
}

/// Shorten Nix store paths for display: `/nix/store/<hash>-name` becomes
/// `name`. Logs keep the full text; only live phases use this.
pub fn shorten_store_paths(text: &str) -> String {
    const PREFIX: &str = "/nix/store/";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(PREFIX) {
        out.push_str(&rest[..i]);
        let after = &rest[i + PREFIX.len()..];
        let hash_len = after.bytes().take_while(|b| b.is_ascii_alphanumeric()).count();
        if hash_len == 32 && after.as_bytes().get(32) == Some(&b'-') {
            rest = &after[33..];
        } else {
            out.push_str(PREFIX);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Human duration: `0.4s`, `12.3s`, `1m05s`, `2h03m`.
pub fn fmt_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 60_000 {
        format!("{}.{}s", ms / 1000, (ms % 1000) / 100)
    } else if ms < 3_600_000 {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    } else {
        format!("{}h{:02}m", ms / 3_600_000, (ms % 3_600_000) / 60_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn sanitize_strips_escapes_and_redraws() {
        assert_eq!(sanitize("\x1b[1;31merror:\x1b[0m bad"), "error: bad");
        assert_eq!(sanitize("10%\r50%\r100% done"), "100% done");
        assert_eq!(sanitize("crlf line\r"), "crlf line");
        assert_eq!(sanitize("a\tb\x07c"), "a    bc");
        assert_eq!(sanitize("\x1b]0;title\x07after"), "after");
        assert_eq!(sanitize("\x1b]8;;http://x\x1b\\link"), "link");
    }

    #[test]
    fn truncate_respects_width_and_marks_cuts() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("hello", 1), "…");
        assert_eq!(truncate("hello", 0), "");
        // Wide characters count double and are never split.
        assert_eq!(truncate("日本語です", 5), "日本…");
        assert!(str_width(&truncate("日本語です", 4)) <= 4);
        assert_eq!(truncate_start("/home/me/src/project", 10), "…c/project");
        assert_eq!(truncate_start("short", 10), "short");
    }

    #[test]
    fn store_paths_are_shortened_only_when_well_formed() {
        let path = format!("copying '/nix/store/{}-hello-2.12' done", "a".repeat(32));
        assert_eq!(shorten_store_paths(&path), "copying 'hello-2.12' done");
        assert_eq!(shorten_store_paths("/nix/store/short-x"), "/nix/store/short-x");
    }

    #[test]
    fn durations_are_compact() {
        assert_eq!(fmt_duration(Duration::from_millis(400)), "0.4s");
        assert_eq!(fmt_duration(Duration::from_millis(12_345)), "12.3s");
        assert_eq!(fmt_duration(Duration::from_secs(65)), "1m05s");
        assert_eq!(fmt_duration(Duration::from_secs(7_380)), "2h03m");
    }
}
