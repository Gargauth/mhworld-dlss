//! Logging: the `log` facade, one logger, one file.
//!
//! [`init`] installs the logger. Every crate logs through the `log` macros
//! with `target: "<subsystem>"`, one of [`TARGETS`]:
//!
//! ```text
//! log::info!(target: "render", w = 1920, h = 1080; "feature created");
//! 2026-10-11T15:04:05.123 I [render] feature created w=1920 h=1080
//! ```
//!
//! * Levels: a global level ([`LogConfig::level`], `[General] LogLevel`) and
//!   per-target overrides ([`LogConfig::targets`], `[Dev] LogTargets`), both
//!   changeable while running with [`set_filter`].
//! * Sinks: the file [`sink::FILE_NAME`] (previous run kept, size cap), and
//!   in dev builds the in-memory `ring`.
//! * Every line has the user profile path replaced by `%USERPROFILE%`
//!   ([`redact`]).
//! * Rate limiting: [`log_once!`](crate::log_once),
//!   [`log_every!`](crate::log_every), [`log_changed!`](crate::log_changed)
//!   (see [`rate`]).

pub mod format;
pub mod rate;
pub mod redact;
#[cfg(feature = "dev")]
pub mod ring;
pub mod sink;

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub use ::log::{Level, LevelFilter, SetLoggerError};
use ::log::{Log, Metadata, Record};
use parking_lot::{Mutex, RwLock};

use self::sink::FileSink;

#[doc(hidden)]
pub use ::log as __facade;

/// The subsystem names used as log targets.
pub const TARGETS: &[&str] = &[
    "boot", "gate", "engine", "jitter", "hijack", "tune", "render", "taa", "inputs", "dlss", "ngx",
    "fg", "sl", "reflex", "swap", "hudless", "dev", "perf", "config", "log", "crash",
];

/// A level for one target, overriding the global level.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetLevel {
    /// One of [`TARGETS`].
    pub target: String,
    /// Most verbose level logged for it.
    pub level: LevelFilter,
}

/// What [`init`] sets up.
#[derive(Clone, Debug)]
pub struct LogConfig {
    /// Folders tried in order for the log file; the first that accepts it
    /// wins. Empty: no file (the dev ring still works).
    pub dirs: Vec<PathBuf>,
    /// Global level.
    pub level: LevelFilter,
    /// Per-target overrides.
    pub targets: Vec<TargetLevel>,
    /// The user profile directory (`%USERPROFILE%`) to redact; empty for
    /// none.
    pub profile: String,
    /// Size cap of the file for this run.
    pub max_file_bytes: u64,
}

impl Default for LogConfig {
    fn default() -> LogConfig {
        LogConfig {
            dirs: Vec::new(),
            level: LevelFilter::Info,
            targets: Vec::new(),
            profile: String::new(),
            max_file_bytes: sink::MAX_FILE_BYTES,
        }
    }
}

/// Which records pass.
#[derive(Debug)]
struct Filter {
    level: LevelFilter,
    targets: Vec<TargetLevel>,
}

impl Filter {
    fn level_for(&self, target: &str) -> LevelFilter {
        self.targets
            .iter()
            .find(|entry| entry.target == target)
            .map_or(self.level, |entry| entry.level)
    }

    /// The most verbose level any target may log at, for
    /// `log::set_max_level`.
    fn max(&self) -> LevelFilter {
        self.targets
            .iter()
            .map(|entry| entry.level)
            .fold(self.level, Ord::max)
    }
}

/// The logger. Formatting happens outside any lock, into a per-thread
/// buffer; only the file write is serialised.
#[derive(Debug)]
struct Logger {
    filter: RwLock<Filter>,
    sink: Mutex<Option<FileSink>>,
    profile: RwLock<String>,
}

impl Logger {
    const fn new() -> Logger {
        Logger {
            filter: RwLock::new(Filter {
                level: LevelFilter::Info,
                targets: Vec::new(),
            }),
            sink: Mutex::new(None),
            profile: RwLock::new(String::new()),
        }
    }

    fn write(&self, line: &mut String, record: &Record<'_>) {
        line.clear();
        format::push_record(line, now_unix_ms(), record);
        redact::redact(line, &self.profile.read());
        if let Some(sink) = self.sink.lock().as_mut() {
            sink.write_line(line);
        }
        #[cfg(feature = "dev")]
        ring::push(record.level(), record.target(), line);
    }
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.filter.read().level_for(metadata.target())
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        thread_local! {
            static LINE: RefCell<String> = RefCell::new(String::with_capacity(512));
        }
        // A `Display` that itself logs re-enters here with the buffer
        // borrowed; it then gets a buffer of its own.
        let reused = LINE.with(|line| match line.try_borrow_mut() {
            Ok(mut line) => {
                self.write(&mut line, record);
                true
            }
            Err(_) => false,
        });
        if !reused {
            self.write(&mut String::new(), record);
        }
    }

    fn flush(&self) {}
}

static LOGGER: Logger = Logger::new();

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Install the logger and open the file.
///
/// Returns where the file landed (`None`: no folder accepted it, logging
/// continues to the other sinks). Fails only if a logger is already
/// installed in this process.
pub fn init(config: LogConfig) -> Result<Option<PathBuf>, SetLoggerError> {
    ::log::set_logger(&LOGGER)?;
    *LOGGER.profile.write() = config.profile;
    let sink = FileSink::open(&config.dirs, config.max_file_bytes);
    let path = sink.as_ref().map(|sink| sink.path().to_path_buf());
    *LOGGER.sink.lock() = sink;
    set_filter(config.level, config.targets);
    Ok(path)
}

/// Change the global level and the per-target overrides while running.
pub fn set_filter(level: LevelFilter, targets: Vec<TargetLevel>) {
    let mut filter = LOGGER.filter.write();
    filter.level = level;
    filter.targets = targets;
    ::log::set_max_level(filter.max());
}

/// Parse a level name, case-insensitively: `off, error, warn, info, debug,
/// trace`.
pub fn parse_level(text: &str) -> Option<LevelFilter> {
    let text = text.trim();
    [
        LevelFilter::Off,
        LevelFilter::Error,
        LevelFilter::Warn,
        LevelFilter::Info,
        LevelFilter::Debug,
        LevelFilter::Trace,
    ]
    .into_iter()
    .find(|level| level.as_str().eq_ignore_ascii_case(text))
}

/// Parse per-target overrides: `render=trace, fg=debug`. Empty text is no
/// overrides. Every target must be one of [`TARGETS`]; on error the entry
/// that failed is returned.
pub fn parse_targets(text: &str) -> Result<Vec<TargetLevel>, String> {
    let mut targets: Vec<TargetLevel> = Vec::new();
    for entry in text.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let parsed = entry.split_once('=').and_then(|(target, level)| {
            let target = target.trim().to_ascii_lowercase();
            let level = parse_level(level)?;
            TARGETS
                .contains(&target.as_str())
                .then_some(TargetLevel { target, level })
        });
        let Some(parsed) = parsed else {
            return Err(entry.to_owned());
        };
        // A later entry for the same target wins.
        targets.retain(|existing| existing.target != parsed.target);
        targets.push(parsed);
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(level: LevelFilter, targets: &str) -> Filter {
        Filter {
            level,
            targets: parse_targets(targets).unwrap(),
        }
    }

    #[test]
    fn overrides_apply_to_their_target_only() {
        let f = filter(LevelFilter::Info, "render=trace,fg=warn");
        assert_eq!(f.level_for("render"), LevelFilter::Trace);
        assert_eq!(f.level_for("fg"), LevelFilter::Warn);
        assert_eq!(f.level_for("dlss"), LevelFilter::Info);
        assert_eq!(f.max(), LevelFilter::Trace);
        assert_eq!(
            filter(LevelFilter::Debug, "fg=error").max(),
            LevelFilter::Debug
        );
    }

    #[test]
    fn target_lists_parse_leniently_and_reject_unknowns() {
        assert_eq!(parse_targets("").unwrap(), vec![]);
        assert_eq!(
            parse_targets(" Render = TRACE , fg=debug, render=info ,").unwrap(),
            vec![
                TargetLevel {
                    target: "fg".into(),
                    level: LevelFilter::Debug
                },
                TargetLevel {
                    target: "render".into(),
                    level: LevelFilter::Info
                },
            ]
        );
        assert_eq!(parse_targets("rendr=trace"), Err("rendr=trace".into()));
        assert_eq!(parse_targets("render=loud"), Err("render=loud".into()));
        assert_eq!(parse_targets("render"), Err("render".into()));
    }

    #[test]
    fn levels_parse_case_insensitively() {
        assert_eq!(parse_level("Trace"), Some(LevelFilter::Trace));
        assert_eq!(parse_level(" WARN "), Some(LevelFilter::Warn));
        assert_eq!(parse_level("off"), Some(LevelFilter::Off));
        assert_eq!(parse_level("verbose"), None);
    }

    #[test]
    fn the_logger_filters_formats_redacts_and_writes() {
        let dir = std::env::temp_dir().join(format!("mhw-core-logger-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let logger = Logger::new();
        *logger.profile.write() = r"C:\Users\Jane".into();
        *logger.sink.lock() = FileSink::open(std::slice::from_ref(&dir), sink::MAX_FILE_BYTES);
        logger.filter.write().targets = parse_targets("render=debug").unwrap();

        let log = |level, target, args: std::fmt::Arguments<'_>| {
            logger.log(
                &Record::builder()
                    .level(level)
                    .target(target)
                    .args(args)
                    .build(),
            );
        };
        log(
            Level::Debug,
            "render",
            format_args!(r"open C:\Users\Jane\x.dds"),
        );
        log(Level::Debug, "fg", format_args!("suppressed"));
        log(Level::Warn, "fg", format_args!("kept"));

        let text = std::fs::read_to_string(dir.join(sink::FILE_NAME)).unwrap();
        let lines: Vec<&str> = text.lines().map(|l| &l[24..]).collect();
        assert_eq!(
            lines,
            [r"D [render] open %USERPROFILE%\x.dds", "W [fg] kept"]
        );
        drop(logger);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
