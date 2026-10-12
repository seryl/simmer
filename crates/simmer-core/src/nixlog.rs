//! Understand nix's machine-readable log (`--log-format internal-json`).
//!
//! Each such stderr line is `@nix {json}`. Activities ("building X",
//! "copying path Y", "downloading Z") become the task's *phase* in the
//! live view, messages and build-log lines become ordinary output, and
//! progress counters are dropped. Any other line (bash, a shell hook, an
//! older nix) passes through as output unchanged.

use serde_json::Value;

/// Flags that make nix emit the format this module parses.
pub const LOG_FLAGS: [&str; 2] = ["--log-format", "internal-json"];

#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    /// What nix is doing now.
    Phase(String),
    /// A line worth keeping in the task's log.
    Output(String),
    /// Bookkeeping (progress counters, activity stops).
    Ignore,
}

/// Activity type nix uses for a build's own log lines.
const RESULT_BUILD_LOG_LINE: u64 = 101;
/// Activity type nix uses for a builder's phase change (`buildPhase`).
const RESULT_SET_PHASE: u64 = 104;

pub fn parse(line: &str) -> Line {
    let Some(json) = line.strip_prefix("@nix ") else { return Line::Output(line.to_string()) };
    let Ok(value) = serde_json::from_str::<Value>(json) else { return Line::Output(line.to_string()) };
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    match value.get("action").and_then(Value::as_str) {
        Some("msg") => text("msg").map_or(Line::Ignore, Line::Output),
        Some("start") => match text("text") {
            Some(t) if !t.is_empty() => Line::Phase(t),
            _ => Line::Ignore,
        },
        Some("result") => {
            let kind = value.get("type").and_then(Value::as_u64);
            let first = value
                .get("fields")
                .and_then(Value::as_array)
                .and_then(|f| f.first())
                .and_then(Value::as_str)
                .map(str::to_string);
            match (kind, first) {
                (Some(RESULT_BUILD_LOG_LINE), Some(line)) => Line::Output(line),
                (Some(RESULT_SET_PHASE), Some(phase)) => Line::Phase(phase),
                _ => Line::Ignore,
            }
        }
        _ => Line::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activities_become_phases_and_messages_output() {
        assert_eq!(
            parse(r#"@nix {"action":"start","id":1,"level":3,"text":"building '/nix/store/x.drv'","type":105}"#),
            Line::Phase("building '/nix/store/x.drv'".into())
        );
        assert_eq!(
            parse(r#"@nix {"action":"msg","level":0,"msg":"error: attribute 'x' missing"}"#),
            Line::Output("error: attribute 'x' missing".into())
        );
        assert_eq!(
            parse(r#"@nix {"action":"result","id":1,"type":101,"fields":["checking for gcc... yes"]}"#),
            Line::Output("checking for gcc... yes".into())
        );
        assert_eq!(
            parse(r#"@nix {"action":"result","id":1,"type":104,"fields":["buildPhase"]}"#),
            Line::Phase("buildPhase".into())
        );
        assert_eq!(parse(r#"@nix {"action":"result","id":1,"type":105,"fields":[1,2,3,4]}"#), Line::Ignore);
        assert_eq!(parse(r#"@nix {"action":"stop","id":1}"#), Line::Ignore);
        assert_eq!(parse(r#"@nix {"action":"start","id":2,"text":""}"#), Line::Ignore);
    }

    #[test]
    fn other_lines_pass_through() {
        assert_eq!(
            parse("simmer: provider X produced nothing"),
            Line::Output("simmer: provider X produced nothing".into())
        );
        assert_eq!(parse("@nix not json"), Line::Output("@nix not json".into()));
    }
}
