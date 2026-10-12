//! `log.subscribe` / `log.tail`: turning the ring's formatted lines back
//! into the `{"event": "log", time, level, target, message, fields, line}`
//! records the client expects.
//!
//! Line format (`mhw_core::log::format`): `TIME L [target] message k=v
//! k="quoted value"`. Fields are the trailing `key=value` tokens; a message
//! that itself ends in `word=word` therefore loses those words to `fields`
//! (the `line` is always exact).

use mhw_core::log::Level;
use serde_json::{json, Map, Value};

/// A filter: most verbose level and targets (empty = all).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogFilter {
    /// Most verbose level passed.
    pub level: Level,
    /// Targets passed; empty passes every target.
    pub targets: Vec<String>,
}

impl LogFilter {
    /// Whether a record passes.
    pub fn passes(&self, level: Level, target: &str) -> bool {
        level <= self.level && (self.targets.is_empty() || self.targets.iter().any(|t| t == target))
    }
}

/// `error`..`trace`, case-insensitive.
pub fn parse_level(name: &str) -> Option<Level> {
    [
        Level::Error,
        Level::Warn,
        Level::Info,
        Level::Debug,
        Level::Trace,
    ]
    .into_iter()
    .find(|l| l.as_str().eq_ignore_ascii_case(name))
}

/// The wire name of a level.
pub fn level_name(level: Level) -> &'static str {
    match level {
        Level::Error => "error",
        Level::Warn => "warn",
        Level::Info => "info",
        Level::Debug => "debug",
        Level::Trace => "trace",
    }
}

/// Split into tokens at spaces outside double quotes, keeping each token's
/// start offset.
fn tokens(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = None;
    let mut quoted = false;
    let mut escaped = false;
    for (at, c) in text.char_indices() {
        if start.is_none() {
            if c == ' ' {
                continue;
            }
            start = Some(at);
        }
        if quoted {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => quoted = false,
                _ => {}
            }
        } else if c == '"' {
            quoted = true;
        } else if c == ' ' {
            if let Some(s) = start.take() {
                out.push((s, &text[s..at]));
            }
        }
    }
    if let Some(s) = start {
        out.push((s, &text[s..]));
    }
    out
}

/// `key=value` with a key made of `[A-Za-z0-9_.]`; quoted values are
/// unescaped (they were written with Rust's `{:?}`).
fn field(token: &str) -> Option<(&str, String)> {
    let (key, value) = token.split_once('=')?;
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    {
        return None;
    }
    if let Some(inner) = value.strip_prefix('"') {
        let inner = inner.strip_suffix('"')?;
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                '0' => out.push('\0'),
                other => out.push(other),
            }
        }
        return Some((key, out));
    }
    (!value.contains('"')).then(|| (key, value.to_owned()))
}

/// The event for one ring line.
pub fn event(level: Level, target: &str, line: &str, seq: u64) -> Value {
    let mut time = "";
    let mut message = line;
    let mut fields = Map::new();
    // `TIME L [target] rest`
    let head = line.split_once(' ').and_then(|(t, rest)| {
        let (_, rest) = rest.split_once(' ')?;
        let rest = rest.strip_prefix('[')?;
        let (_, rest) = rest.split_once("] ")?;
        Some((t, rest))
    });
    if let Some((t, rest)) = head {
        time = t;
        message = rest;
        // Fields can only be on the first line (continuations are message).
        let first_end = rest.find('\n').unwrap_or(rest.len());
        if first_end == rest.len() {
            let toks = tokens(rest);
            let mut cut = toks.len();
            while cut > 0 && field(toks[cut - 1].1).is_some() {
                cut -= 1;
            }
            if cut < toks.len() {
                for (_, tok) in &toks[cut..] {
                    if let Some((k, v)) = field(tok) {
                        fields.insert(k.to_owned(), Value::String(v));
                    }
                }
                message = toks.get(cut).map_or(rest, |&(at, _)| rest[..at].trim_end());
            }
        }
    }
    json!({
        "event": "log",
        "seq": seq,
        "time": time,
        "level": level_name(level),
        "target": target,
        "message": message,
        "fields": fields,
        "line": line,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_splits_into_its_parts() {
        let line = "2026-10-11T15:04:05.123 I [render] pass identified kind=temporal list=0x1f size=\"1920 x 1080\"";
        let e = event(Level::Info, "render", line, 9);
        assert_eq!(e["time"], "2026-10-11T15:04:05.123");
        assert_eq!(e["level"], "info");
        assert_eq!(e["message"], "pass identified");
        assert_eq!(
            e["fields"],
            json!({"kind": "temporal", "list": "0x1f", "size": "1920 x 1080"})
        );
        assert_eq!(e["line"], line);
        assert_eq!(e["seq"], 9);
    }

    #[test]
    fn messages_without_fields_and_odd_lines_stay_whole() {
        let e = event(
            Level::Warn,
            "dev",
            "2026-10-11T15:04:05.123 W [dev] client gone",
            0,
        );
        assert_eq!(
            (
                e["message"].as_str(),
                e["fields"].as_object().unwrap().len()
            ),
            (Some("client gone"), 0)
        );
        let e = event(Level::Error, "x", "not a formatted line", 0);
        assert_eq!(e["message"], "not a formatted line");
        assert_eq!(e["time"], "");
        let multi = "2026-10-11T15:04:05.123 E [crash] fault\n  at a=b";
        assert_eq!(
            event(Level::Error, "crash", multi, 0)["message"],
            "fault\n  at a=b"
        );
    }

    #[test]
    fn quoted_values_are_unescaped() {
        let line = r#"2026-10-11T15:04:05.123 D [fg] state why="a \"b\"\nc""#;
        assert_eq!(
            event(Level::Debug, "fg", line, 0)["fields"]["why"],
            "a \"b\"\nc"
        );
    }

    #[test]
    fn the_filter_passes_levels_up_to_its_own_and_listed_targets() {
        let f = LogFilter {
            level: Level::Info,
            targets: vec!["fg".into()],
        };
        assert!(f.passes(Level::Warn, "fg"));
        assert!(!f.passes(Level::Debug, "fg"));
        assert!(!f.passes(Level::Error, "render"));
        let all = LogFilter {
            level: Level::Trace,
            targets: vec![],
        };
        assert!(all.passes(Level::Trace, "render"));
        assert_eq!(parse_level("DEBUG"), Some(Level::Debug));
        assert_eq!(parse_level("verbose"), None);
    }
}
