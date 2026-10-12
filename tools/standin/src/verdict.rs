//! Reading the mod's own account of what its hooks saw.
//!
//! The mod logs in the format of `mhw_core::log::format`:
//!
//! ```text
//! 2026-10-11T15:04:05.123 I [render] message key=value key="two words"
//! ```
//!
//! The standin passes when that log shows every [`Expectation`] for what it
//! did. These lines are the contract between the standin and `mhw-dlss`;
//! the mod's wiring must keep them (see `docs/DEVELOPMENT.md`).

/// One log line, taken apart. Lines that do not start with a timestamp
/// (continuation lines) are not records.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct Line<'a> {
    /// `E`, `W`, `I`, `D` or `T`.
    pub level: char,
    /// The subsystem in brackets.
    pub target: &'a str,
    /// Everything after `] `: the message and its fields.
    pub rest: &'a str,
}

impl<'a> Line<'a> {
    /// Parse one line; `None` if it is not a record.
    pub fn parse(line: &'a str) -> Option<Line<'a>> {
        let (stamp, rest) = line.split_once(' ')?;
        if stamp.len() != 23 || stamp.as_bytes().get(10) != Some(&b'T') {
            return None;
        }
        let mut chars = rest.chars();
        let level = chars.next()?;
        let rest = chars.as_str().strip_prefix(" [")?;
        let (target, rest) = rest.split_once("] ")?;
        Some(Line {
            level,
            target,
            rest,
        })
    }

    /// The value of the structured field `key`, unquoted.
    pub fn field(&self, key: &str) -> Option<String> {
        let pattern = format!(" {key}=");
        let start = self.rest.find(&pattern)? + pattern.len();
        let value = self.rest.get(start..)?;
        if let Some(quoted) = value.strip_prefix('"') {
            let mut out = String::new();
            let mut chars = quoted.chars();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => out.push(chars.next()?),
                    '"' => return Some(out),
                    other => out.push(other),
                }
            }
            None
        } else {
            Some(
                value
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
            )
        }
    }

    /// A numeric field.
    pub fn number(&self, key: &str) -> Option<u64> {
        self.field(key)?.parse().ok()
    }
}

/// What the standin did, which decides what the log must show.
#[derive(Debug, Default, Clone, Copy)]
pub struct Done {
    /// A pipeline that reflects like the engine's temporal pass was
    /// dispatched once and its list closed and executed.
    pub dispatched: bool,
    /// The mod ran a DLSS frame through its selftest export.
    pub dlss_frame: bool,
}

/// The verdict: every failed expectation, empty for a pass.
pub fn judge(log: &str, done: Done) -> Vec<String> {
    let lines: Vec<Line<'_>> = log.lines().filter_map(Line::parse).collect();
    let mut failures = Vec::new();
    if lines.is_empty() {
        failures.push("there is no mhw-dlss.log, or it is empty: the mod was not injected".into());
        return failures;
    }
    let has = |target: &str, start: &str| {
        lines
            .iter()
            .any(|line| line.target == target && line.rest.starts_with(start))
    };
    for (target, start, why) in EXPECTED {
        if !has(target, start) {
            failures.push(format!("no `[{target}] {start}` line: {why}"));
        }
    }

    // The last counters line: the mod's count of what its hooks saw.
    match lines
        .iter()
        .rev()
        .find(|line| line.field(COUNTER_DISPATCHES).is_some())
    {
        None => failures.push(format!(
            "the mod never logged its render counters (no line with {COUNTER_DISPATCHES}=)"
        )),
        Some(counters) => {
            let zero = |key: &str| counters.number(key) == Some(0);
            if done.dispatched && zero(COUNTER_RECOGNISED) {
                failures.push("the mod did not recognise the temporal-pass pipeline".into());
            }
            if done.dispatched && zero(COUNTER_DISPATCHES) {
                failures
                    .push("the mod's Dispatch hook did not see the temporal-pass dispatch".into());
            }
            if done.dispatched && zero(COUNTER_CLOSES) {
                failures.push("the mod's Close hook did not see the list close".into());
            }
        }
    }

    if done.dlss_frame {
        let created = lines.iter().any(|line| {
            line.target == "dlss"
                && line.rest.starts_with("CreateFeature")
                && line
                    .field("status")
                    .is_some_and(|status| status.starts_with(SUCCESS))
        });
        if !created {
            failures.push(format!(
                "no `[dlss] CreateFeature ... status={SUCCESS}` line"
            ));
        }
    }
    failures
}

/// Lines the mod must log for any run of the standin: (target, message
/// start, what their absence means).
pub const EXPECTED: &[(&str, &str, &str)] = &[
    (
        "render",
        "render hooks installed",
        "the mod did not install its render hooks",
    ),
    (
        "render",
        "D3D12CreateDevice #",
        "the device was not created through the mod's replacement",
    ),
    (
        "swap",
        "swapchain ",
        "the mod did not see the swapchain being created",
    ),
];

/// Counter fields (`mhw_render::Counters`, logged by the mod).
pub const COUNTER_RECOGNISED: &str = "pipelines_recognised";
/// See [`COUNTER_RECOGNISED`].
pub const COUNTER_DISPATCHES: &str = "upsample_dispatches";
/// See [`COUNTER_RECOGNISED`].
pub const COUNTER_CLOSES: &str = "lists_closed";
/// `NVSDK_NGX_Result_Success` as `mhw_ngx::Status` displays it.
const SUCCESS: &str = "0x00000001";

#[cfg(test)]
mod tests {
    use super::*;

    const STAMP: &str = "2026-10-11T15:04:05.123";

    fn record(level: char, target: &str, rest: &str) -> String {
        format!("{STAMP} {level} [{target}] {rest}\n")
    }

    fn good_log() -> String {
        [
            record('I', "render", "render hooks installed"),
            record(
                'I',
                "render",
                "D3D12CreateDevice #1 (level 0xb000) -> device 0x1 on thread 4",
            ),
            record(
                'I',
                "swap",
                "swapchain 0x2: 640x360 format 87 buffers 3 flags 0x842, DXGI's own",
            ),
            record(
                'I',
                "perf",
                "render counters pipelines_recognised=1 upsample_dispatches=1 lists_closed=2",
            ),
            record(
                'I',
                "dlss",
                "CreateFeature render=\"1280x720\" status=\"0x00000001 Success\" frame=3",
            ),
        ]
        .concat()
    }

    #[test]
    fn a_line_is_taken_apart_and_fields_are_unquoted() {
        let text =
            format!("{STAMP} W [ngx] a message with=words in it key=\"two \\\"x\\\" words\" n=42");
        let line = Line::parse(&text).expect("a record");
        assert_eq!((line.level, line.target), ('W', "ngx"));
        assert_eq!(line.field("key").as_deref(), Some("two \"x\" words"));
        assert_eq!(line.number("n"), Some(42));
        assert_eq!(line.field("absent"), None);
        assert_eq!(Line::parse("  continuation of a message"), None);
        assert_eq!(Line::parse("[render] old 0.2 format"), None);
    }

    #[test]
    fn a_complete_log_passes() {
        let done = Done {
            dispatched: true,
            dlss_frame: true,
        };
        assert_eq!(judge(&good_log(), done), Vec::<String>::new());
    }

    #[test]
    fn an_empty_log_means_the_mod_was_not_there() {
        let failures = judge("", Done::default());
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("not injected"));
    }

    #[test]
    fn each_missing_piece_is_named() {
        let log = good_log().replace("render hooks installed", "render hooks failed");
        let failures = judge(&log, Done::default());
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("render hooks installed"));

        let log = good_log().replace("upsample_dispatches=1", "upsample_dispatches=0");
        let done = Done {
            dispatched: true,
            dlss_frame: false,
        };
        let failures = judge(&log, done);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("Dispatch hook"));
        // Not dispatched: the counter is not held against the mod.
        assert!(judge(&log, Done::default()).is_empty());

        let log = good_log().replace("0x00000001 Success", "0xbad00005 FeatureNotFound");
        let done = Done {
            dispatched: false,
            dlss_frame: true,
        };
        assert_eq!(judge(&log, done).len(), 1);
    }

    #[test]
    fn the_last_counters_line_counts() {
        let log = good_log()
            + &record(
                'I',
                "perf",
                "render counters pipelines_recognised=0 upsample_dispatches=0 lists_closed=0",
            );
        let done = Done {
            dispatched: true,
            dlss_frame: false,
        };
        assert_eq!(judge(&log, done).len(), 3);
    }
}
