//! The log file.
//!
//! Two properties are kept here so that no caller has to remember them:
//!
//! * the log of the previous run survives a restart as
//!   `mhw-dlss.previous.log`: the first thing a user does after a crash is
//!   start the game again, which would otherwise truncate the only copy of
//!   what had happened;
//! * the file cannot grow without bound: after [`MAX_FILE_BYTES`] one notice
//!   is written and the rest of the run is dropped.
//!
//! Every line reaches the system in one `write` with nothing buffered on our
//! side: after a crash the last line in the file is the last thing that
//! happened.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The log of this run.
pub const FILE_NAME: &str = "mhw-dlss.log";
/// The log of the previous run.
pub const PREVIOUS_FILE_NAME: &str = "mhw-dlss.previous.log";
/// Size cap per run. A dev session with trace logging for an evening stays
/// well below it; it is well short of filling a disk.
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// An open log file with its size accounting.
#[derive(Debug)]
pub struct FileSink {
    file: File,
    path: PathBuf,
    written: u64,
    cap: u64,
    full: bool,
    /// The line plus its newline, reused so that writing does not allocate.
    buffer: Vec<u8>,
}

impl FileSink {
    /// Open `mhw-dlss.log` in the first of `dirs` that accepts it, after
    /// moving that folder's existing log to `mhw-dlss.previous.log`.
    ///
    /// Several folders exist because the mod folder lives under Program
    /// Files, which may not be writable without elevation; callers pass the
    /// mod folder first and fall-backs after it. `None` when none works.
    pub fn open(dirs: &[PathBuf], cap: u64) -> Option<FileSink> {
        dirs.iter().find_map(|dir| Self::open_in(dir, cap).ok())
    }

    fn open_in(dir: &Path, cap: u64) -> std::io::Result<FileSink> {
        std::fs::create_dir_all(dir)?;
        keep_previous(dir);
        let path = dir.join(FILE_NAME);
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        Ok(FileSink {
            file,
            path,
            written: 0,
            cap,
            full: false,
            buffer: Vec::with_capacity(512),
        })
    }

    /// Where the file is.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the cap has been reached and lines are being dropped.
    pub fn is_full(&self) -> bool {
        self.full
    }

    /// Write one line (`line` has no trailing newline).
    pub fn write_line(&mut self, line: &str) {
        if self.full {
            return;
        }
        let length = line.len() as u64 + 1;
        if self.written.saturating_add(length) > self.cap {
            self.full = true;
            let _ = writeln!(
                self.file,
                "--- log truncated: this run reached {} bytes, later lines are not written ---",
                self.cap
            );
            return;
        }
        self.written += length;
        self.buffer.clear();
        self.buffer.extend_from_slice(line.as_bytes());
        self.buffer.push(b'\n');
        let _ = self.file.write_all(&self.buffer);
    }
}

/// Move the last run's log out of the way of this one's.
fn keep_previous(dir: &Path) {
    let current = dir.join(FILE_NAME);
    if !current.exists() {
        return;
    }
    let previous = dir.join(PREVIOUS_FILE_NAME);
    let _ = std::fs::remove_file(&previous);
    let _ = std::fs::rename(&current, &previous);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir =
                std::env::temp_dir().join(format!("mhw-core-sink-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn the_previous_run_is_kept_and_the_one_before_is_dropped() {
        let scratch = Scratch::new("rotate");
        std::fs::create_dir_all(&scratch.0).unwrap();
        std::fs::write(scratch.0.join(FILE_NAME), "last run").unwrap();
        std::fs::write(scratch.0.join(PREVIOUS_FILE_NAME), "the one before").unwrap();
        let mut sink = FileSink::open(std::slice::from_ref(&scratch.0), MAX_FILE_BYTES).unwrap();
        sink.write_line("this run");
        assert_eq!(read(&scratch.0.join(PREVIOUS_FILE_NAME)), "last run");
        assert_eq!(read(&scratch.0.join(FILE_NAME)), "this run\n");
    }

    #[test]
    fn a_missing_folder_is_created_and_nothing_to_keep_is_fine() {
        let scratch = Scratch::new("fresh");
        let sink = FileSink::open(std::slice::from_ref(&scratch.0), MAX_FILE_BYTES).unwrap();
        assert_eq!(sink.path(), scratch.0.join(FILE_NAME));
        assert!(!scratch.0.join(PREVIOUS_FILE_NAME).exists());
    }

    #[test]
    fn an_unusable_folder_falls_through_to_the_next() {
        let scratch = Scratch::new("fallback");
        std::fs::create_dir_all(&scratch.0).unwrap();
        // A regular file where a folder is wanted cannot hold the log.
        let blocked = scratch.0.join("blocked");
        std::fs::write(&blocked, "").unwrap();
        let good = scratch.0.join("good");
        let sink = FileSink::open(&[blocked, good.clone()], MAX_FILE_BYTES).unwrap();
        assert_eq!(sink.path(), good.join(FILE_NAME));
        assert!(FileSink::open(&[], MAX_FILE_BYTES).is_none());
    }

    #[test]
    fn the_cap_stops_writing_after_one_notice() {
        let scratch = Scratch::new("cap");
        let mut sink = FileSink::open(std::slice::from_ref(&scratch.0), 12).unwrap();
        sink.write_line("12345"); // 6 bytes
        sink.write_line("abcde"); // 12 bytes: exactly at the cap
        assert!(!sink.is_full());
        sink.write_line("x");
        assert!(sink.is_full());
        sink.write_line("never");
        let text = read(&scratch.0.join(FILE_NAME));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert_eq!(&lines[..2], &["12345", "abcde"]);
        assert!(lines[2].starts_with("--- log truncated"));
    }
}
