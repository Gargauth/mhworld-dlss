//! The launcher's log: `mhw-dlss-launcher.log` in the mod's folder, and a
//! console under `--verbose`.
//!
//! The launcher is a GUI-subsystem program started by Steam: it has no
//! standard output, so everything goes to the file, which is what makes a
//! launch that went wrong explainable afterwards. Lines have the mod's
//! format ([`mhw_core::log::format`]) and the user profile path redacted
//! ([`mhw_core::log::redact`]), because both logs end up in bug reports.

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use log::{LevelFilter, Log, Metadata, Record, SetLoggerError};
use mhw_core::log::{format, redact};
use parking_lot::Mutex;

/// This run's log.
pub const LOG_FILE: &str = "mhw-dlss-launcher.log";
/// The previous run's log: a launch that went wrong is usually followed by
/// another before anybody looks.
pub const PREVIOUS_LOG_FILE: &str = "mhw-dlss-launcher.previous.log";

/// Where lines go.
#[derive(Debug, Default)]
struct Sinks {
    file: Option<File>,
    console: Option<File>,
}

/// The logger: both sinks behind one lock (the launcher logs a few dozen
/// lines from one thread; there is nothing to gain from more).
#[derive(Debug)]
pub struct Logger {
    sinks: Mutex<Sinks>,
    profile: Mutex<String>,
}

impl Logger {
    const fn new() -> Logger {
        Logger {
            sinks: Mutex::new(Sinks {
                file: None,
                console: None,
            }),
            profile: Mutex::new(String::new()),
        }
    }

    fn line(&self, record: &Record<'_>) -> String {
        let mut line = String::with_capacity(160);
        format::push_record(&mut line, now_unix_ms(), record);
        redact::redact(&mut line, &self.profile.lock());
        line
    }
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let mut line = self.line(record);
        let mut sinks = self.sinks.lock();
        line.push('\n');
        if let Some(file) = sinks.file.as_mut() {
            let _ = file.write_all(line.as_bytes());
        }
        if let Some(console) = sinks.console.as_mut() {
            line.pop();
            line.push_str("\r\n");
            let _ = console.write_all(line.as_bytes());
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

/// Install the logger at `level` and open the file in the first of `dirs`
/// that accepts it (the mod's folder first; a fallback under
/// `%LOCALAPPDATA%` for a folder that is not writable). `profile` is the
/// user profile path to redact. Returns where the file landed.
pub fn init(
    dirs: &[PathBuf],
    profile: String,
    level: LevelFilter,
) -> Result<Option<PathBuf>, SetLoggerError> {
    log::set_logger(&LOGGER)?;
    log::set_max_level(level);
    *LOGGER.profile.lock() = profile;
    let opened = dirs.iter().find_map(|dir| open_in(dir).ok());
    let path = opened.as_ref().map(|(_, path)| path.clone());
    LOGGER.sinks.lock().file = opened.map(|(file, _)| file);
    Ok(path)
}

/// Also print every line to `console` (`--verbose`).
pub fn attach_console(console: File) {
    LOGGER.sinks.lock().console = Some(console);
}

/// Create `LOG_FILE` in `dir`, after moving an existing one to
/// `PREVIOUS_LOG_FILE`.
pub fn open_in(dir: &Path) -> io::Result<(File, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(LOG_FILE);
    if path.exists() {
        let previous = dir.join(PREVIOUS_LOG_FILE);
        let _ = std::fs::remove_file(&previous);
        let _ = std::fs::rename(&path, &previous);
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)?;
    Ok((file, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir()
                .join(format!("mhw-launcher-log-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_previous_run_is_kept_once() {
        let scratch = Scratch::new("rotate");
        let write = |text: &str| {
            let (mut file, path) = open_in(&scratch.0).expect("opened");
            file.write_all(text.as_bytes()).expect("written");
            path
        };
        write("first");
        write("second");
        let path = write("third");
        assert_eq!(path, scratch.0.join(LOG_FILE));
        let read = |name: &str| std::fs::read_to_string(scratch.0.join(name)).unwrap_or_default();
        assert_eq!(read(LOG_FILE), "third");
        assert_eq!(read(PREVIOUS_LOG_FILE), "second");
    }

    #[test]
    fn lines_have_the_mods_format_and_no_profile_path() {
        let logger = Logger::new();
        *logger.profile.lock() = "C:\\Users\\someone".to_owned();
        let args = format_args!("chain file C:\\Users\\someone\\x.ini");
        let record = Record::builder()
            .args(args)
            .level(log::Level::Warn)
            .target(crate::TARGET)
            .build();
        let line = logger.line(&record);
        assert!(
            line.ends_with(" W [launcher] chain file %USERPROFILE%\\x.ini"),
            "{line}"
        );
    }
}
