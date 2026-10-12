//! The line format.
//!
//! `2026-10-11T15:04:05.123 I [render] message key=value key="two words"`
//!
//! UTC with milliseconds, one-letter level, the target in brackets, the
//! message, then the structured fields in the order they were given. A value
//! that is empty or contains whitespace, `"` or `=` is written quoted and
//! escaped so that a line can be split back into fields. A message spanning
//! several lines has its continuation lines indented by two spaces, so that
//! every record still starts with a timestamp at column 0.

use std::fmt::Write as _;

use ::log::kv::{Error as KvError, Key, Value, VisitSource};
use ::log::{Level, Record};

/// One letter per level.
pub fn level_letter(level: Level) -> char {
    match level {
        Level::Error => 'E',
        Level::Warn => 'W',
        Level::Info => 'I',
        Level::Debug => 'D',
        Level::Trace => 'T',
    }
}

/// Append `unix_ms` (milliseconds since 1970-01-01 UTC) as
/// `YYYY-MM-DDTHH:MM:SS.mmm`.
pub fn push_timestamp(out: &mut String, unix_ms: u64) {
    let millis = unix_ms % 1000;
    let seconds = unix_ms / 1000;
    let (days, of_day) = (seconds / 86_400, seconds % 86_400);
    let (year, month, day) = civil_from_days(days);
    let _ = write!(
        out,
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}",
        of_day / 3600,
        of_day / 60 % 60,
        of_day % 60
    );
}

/// Days since 1970-01-01 to a proleptic Gregorian date (H. Hinnant's
/// `civil_from_days`, restricted to non-negative days).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

/// Append one formatted record to `out`, without a trailing newline.
pub fn push_record(out: &mut String, unix_ms: u64, record: &Record<'_>) {
    push_timestamp(out, unix_ms);
    out.push(' ');
    out.push(level_letter(record.level()));
    out.push_str(" [");
    out.push_str(record.target());
    out.push_str("] ");
    let message_start = out.len();
    let _ = write!(out, "{}", record.args());
    indent_continuations(out, message_start);
    let mut fields = Fields { out };
    let _ = record.key_values().visit(&mut fields);
}

/// Indent every line after the first in `out[from..]` by two spaces.
fn indent_continuations(out: &mut String, from: usize) {
    if !out[from..].contains('\n') {
        return;
    }
    let tail = out.split_off(from);
    for (index, part) in tail.split('\n').enumerate() {
        if index > 0 {
            out.push_str("\n  ");
        }
        out.push_str(part.strip_suffix('\r').unwrap_or(part));
    }
}

struct Fields<'a> {
    out: &'a mut String,
}

impl<'kvs> VisitSource<'kvs> for Fields<'_> {
    fn visit_pair(&mut self, key: Key<'kvs>, value: Value<'kvs>) -> Result<(), KvError> {
        self.out.push(' ');
        self.out.push_str(key.as_str());
        self.out.push('=');
        let start = self.out.len();
        let _ = write!(self.out, "{value}");
        let written = &self.out[start..];
        let needs_quotes = written.is_empty()
            || written
                .chars()
                .any(|c| c.is_whitespace() || c == '"' || c == '=');
        if needs_quotes {
            let raw = self.out.split_off(start);
            let _ = write!(self.out, "{raw:?}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(unix_ms: u64) -> String {
        let mut text = String::new();
        push_timestamp(&mut text, unix_ms);
        text
    }

    #[test]
    fn timestamps_are_utc_with_milliseconds() {
        assert_eq!(stamp(0), "1970-01-01T00:00:00.000");
        // 2026-10-11T15:04:05.123Z
        assert_eq!(stamp(1_791_731_045_123), "2026-10-11T15:04:05.123");
        // A leap day and the last millisecond of a year.
        assert_eq!(stamp(951_782_400_000), "2000-02-29T00:00:00.000");
        assert_eq!(stamp(1_704_067_199_999), "2023-12-31T23:59:59.999");
    }

    fn line(record: &Record<'_>) -> String {
        let mut text = String::new();
        push_record(&mut text, 1_791_731_045_123, record);
        text
    }

    #[test]
    fn a_record_is_one_line_with_level_letter_and_target() {
        let args = format_args!("feature created {}x{}", 1920, 1080);
        let record = Record::builder()
            .level(Level::Info)
            .target("dlss")
            .args(args)
            .build();
        assert_eq!(
            line(&record),
            "2026-10-11T15:04:05.123 I [dlss] feature created 1920x1080"
        );
    }

    #[test]
    fn structured_fields_follow_the_message_and_are_quoted_when_needed() {
        let fields: [(&str, Value<'_>); 4] = [
            ("w", Value::from(1920u32)),
            ("scale", Value::from(0.5f32)),
            ("name", Value::from("two words")),
            ("empty", Value::from("")),
        ];
        let args = format_args!("resize");
        let record = Record::builder()
            .level(Level::Warn)
            .target("render")
            .args(args)
            .key_values(&fields)
            .build();
        assert_eq!(
            line(&record),
            "2026-10-11T15:04:05.123 W [render] resize w=1920 scale=0.5 name=\"two words\" empty=\"\""
        );
    }

    #[test]
    fn continuation_lines_are_indented() {
        let args = format_args!("first\r\nsecond\nthird");
        let record = Record::builder()
            .level(Level::Error)
            .target("boot")
            .args(args)
            .build();
        assert_eq!(
            line(&record),
            "2026-10-11T15:04:05.123 E [boot] first\n  second\n  third"
        );
    }

    #[test]
    fn every_level_has_its_letter() {
        let letters: String = [
            Level::Error,
            Level::Warn,
            Level::Info,
            Level::Debug,
            Level::Trace,
        ]
        .into_iter()
        .map(level_letter)
        .collect();
        assert_eq!(letters, "EWIDT");
    }
}
