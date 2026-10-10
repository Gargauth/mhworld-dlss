//! The log file.
//!
//! It is written in the mod's own folder, `<game>\mhw-dlss`. The game lives
//! in Program Files, which may not be writable without elevation, so
//! %LOCALAPPDATA% and then %TEMP% are tried after it.
//!
//! Three properties are kept here, for every caller, so that none of them has
//! to remember:
//!
//! * the log of the session before this one survives a restart, as
//!   `mhw-dlss.previous.log`: the first thing a user does after a crash is
//!   start the game again, which would otherwise truncate the only copy of
//!   what had happened;
//! * the Windows profile path is written as `%USERPROFILE%`, because these
//!   files get pasted into public bug reports;
//! * the file cannot grow without bound. `Throttle` is for a line that some
//!   caller can make per frame or per call, and the size limit is the net
//!   under everything else.
//!
//! ## Tags
//!
//! Every line starts with a tag in square brackets naming the part of the
//! mod that wrote it. Short tags are padded to three characters.
//!
//! | tag | what writes it |
//! | --- | --- |
//! | `[bld]` | the build gate: is this the game build the mod knows (`gate.rs`) |
//! | `[cfg]` | the settings file (`config.rs`) |
//! | `[cmp]` | companions: the chain file's `renderer` entries, ReShade, the settings page |
//! | `[dls]` | DLSS: readiness, feature creation, evaluation (`dlss.rs`, `gpu.rs`) |
//! | `[hij]` | the takeover of the game's own DLSS path (`hijack.rs`) |
//! | `[eng]` | detours and reads in the game's code (`native.rs`) |
//! | `[jit]` | the camera jitter |
//! | `[tun]` | engine settings: mip bias, dither patterns (`tune.rs`) |
//! | `[taa]` | the engine's temporal pass, as recognised and as dispatched |
//! | `[pso]` | compute pipelines recognised at creation |
//! | `[mv ]` | the motion-vector decode (`mv.rs`) |
//! | `[ngx]` | NGX: the game's initialisation, capabilities, the serving module |
//! | `[sl ]` | calls into Streamline and what they returned (`streamline.rs`) |
//! | `[sl*]` | Streamline's and NGX's own messages, forwarded |
//! | `[fg ]` | frame generation, per frame (`fg.rs`) |
//! | `[hud]` | the capture of the frame before the interface (`hudless.rs`) |
//! | `[swap]` | swapchain creation and presents (`swap.rs`) |
//! | `[stk]` | which modules the presentation stack is made of (`reshade.rs`) |
//! | `[queue]` | which command queue is the engine's (`queue.rs`) |
//! | `[gpu]` | the render hooks' installation and periodic counts (`gpu.rs`) |
//! | `[iat]`, `[sym]`, `[load]` | the import-table hooks: installation, symbols the game resolves, libraries it loads (`hooks.rs`) |
//! | `[proxy]` | the forwarded `D3D12CreateDevice` and `CreateDXGIFactory2` (`proxy.rs`) |
//! | `[dx11]` | signs that the game is running on DirectX 11 |
//! | `[fatal]` | the engine's own fatal error, named before it aborts |
//! | `[dred]`, `[val]` | the two crash diagnostics: device-removed data and the Direct3D 12 debug layer |
//! | `[perf]`, `[path]`, `[key]` | diagnostics: time spent in hooks, the `path_dump` report, the F10 report |
//! | `[tst]` | the one frame run for `tools/standin` (`selftest.rs`) |
//! | `[log]` | the logger itself: lines left out, the size limit |
//! | `[alive]`, `[bind]`, `[map]`, `[heap]`, `[tex]`, `[dsv]`, `[cbuf]`, `[cam]`, `[rsig]`, `[list]` | resource tracking, mostly with `log_verbose = 1`: liveness, descriptor bindings, mapped memory, heaps, textures, depth views, constant buffers, the camera buffer, root signatures, command lists |

use parking_lot::Mutex;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// The log, and the one before it.
pub const NAME: &str = "mhw-dlss.log";
pub const PREVIOUS: &str = "mhw-dlss.previous.log";

/// Where writing stops. A session with the per-frame dumps on, played for an
/// evening, comes to tens of megabytes; this is well past that and well short
/// of filling a disk.
const LIMIT: u64 = 256 * 1024 * 1024;

struct Sink {
    file: File,
    /// The line being built, kept so that building one does not allocate and
    /// so that it reaches the file in a single write.
    line: String,
    written: u64,
    full: bool,
    /// The user's profile directory, to be kept out of the file.
    profile: String,
}

static SINK: Mutex<Option<Sink>> = Mutex::new(None);
static PATH: Mutex<Option<PathBuf>> = Mutex::new(None);
/// Whether the per-frame dumps are wanted.
///
/// Kept here rather than read from the settings, so that deciding whether to
/// log never calls back into code that might itself want to log.
static VERBOSE: AtomicBool = AtomicBool::new(false);

fn candidates(dll_dir: Option<PathBuf>) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(d) = dll_dir {
        v.push(d);
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        v.push(PathBuf::from(local).join("mhw-dlss"));
    }
    if let Ok(tmp) = std::env::var("TEMP") {
        v.push(PathBuf::from(tmp));
    }
    v
}

/// Move the last session's log out of the way of this one's.
fn keep_previous(folder: &Path) {
    let current = folder.join(NAME);
    if !current.exists() {
        return;
    }
    let previous = folder.join(PREVIOUS);
    let _ = std::fs::remove_file(&previous);
    let _ = std::fs::rename(&current, &previous);
}

pub fn init(dll_dir: Option<PathBuf>) {
    for folder in candidates(dll_dir) {
        let _ = std::fs::create_dir_all(&folder);
        keep_previous(&folder);
        let path = folder.join(NAME);
        if let Ok(file) = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
        {
            *SINK.lock() = Some(Sink {
                file,
                line: String::with_capacity(512),
                written: 0,
                full: false,
                profile: std::env::var("USERPROFILE").unwrap_or_default(),
            });
            *PATH.lock() = Some(path);
            return;
        }
    }
}

/// Where the log actually landed, for reporting back to the user.
pub fn path() -> Option<PathBuf> {
    PATH.lock().clone()
}

/// Turn the per-frame dumps on or off.
pub fn set_verbose(on: bool) {
    VERBOSE.store(on, Ordering::Relaxed);
}

/// Whether the per-frame dumps are being written.
///
/// Worth checking before building an expensive message, not only before
/// writing one: several of the dumps resolve descriptor tables and read back
/// constant buffers to produce their text.
pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// Write one line.
///
/// The line is built in memory and handed to the system in one write, with
/// nothing held back in a buffer of ours: after a crash the last line in the
/// file is the last thing that happened.
pub fn write_line(args: std::fmt::Arguments) {
    let mut guard = SINK.lock();
    let Some(sink) = guard.as_mut() else {
        return;
    };
    if sink.full {
        return;
    }
    sink.line.clear();
    let _ = write!(sink.line, "{args}");
    crate::redact::redact(&mut sink.line, &sink.profile);
    sink.line.push('\n');
    if sink.written + sink.line.len() as u64 > LIMIT {
        sink.full = true;
        let _ = writeln!(
            sink.file,
            "[log] this file has reached {} MiB and nothing more is written to it this session",
            LIMIT / (1024 * 1024)
        );
        return;
    }
    sink.written += sink.line.len() as u64;
    let _ = sink.file.write_all(sink.line.as_bytes());
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::logger::write_line(format_args!($($arg)*)) };
}

/// Log only when the per-frame dumps are wanted.
///
/// The arguments are not evaluated otherwise, which matters because some of
/// these call `format_name` or resolve a descriptor to build their text.
#[macro_export]
macro_rules! logv {
    ($($arg:tt)*) => {
        if $crate::logger::verbose() {
            $crate::logger::write_line(format_args!($($arg)*))
        }
    };
}

/// Log through a `Throttle`: written while it allows, counted when it does
/// not, and the count said when writing resumes.
///
/// The arguments are not evaluated for a line that is not written.
#[macro_export]
macro_rules! log_limited {
    ($throttle:expr, $($arg:tt)*) => {
        if let Some(skipped) = $throttle.allow() {
            if skipped > 0 {
                $crate::logger::write_line(format_args!(
                    "[log] {skipped} lines like the next one were not written"
                ));
            }
            $crate::logger::write_line(format_args!($($arg)*))
        }
    };
}

/// A bound on how often one kind of line is written.
///
/// For any line whose rate is decided by somebody else: the engine calling a
/// hook, Streamline calling back, a failure that repeats every few frames.
/// The first `free` lines are always written, which is what lets a start-up
/// say everything it has to; after that at most `burst` per `window`.
pub struct Throttle {
    free: u32,
    burst: u32,
    window_ms: u64,
    written: AtomicU32,
    window_start: AtomicU64,
    in_window: AtomicU32,
    skipped: AtomicU64,
}

impl Throttle {
    pub const fn new(free: u32, burst: u32, window_seconds: u64) -> Throttle {
        Throttle {
            free,
            burst,
            window_ms: window_seconds * 1000,
            written: AtomicU32::new(0),
            window_start: AtomicU64::new(0),
            in_window: AtomicU32::new(0),
            skipped: AtomicU64::new(0),
        }
    }

    /// Whether to write a line now: `None` for no, and otherwise how many
    /// were not written since the last one that was.
    pub fn allow(&self) -> Option<u64> {
        self.allow_at(now_ms())
    }

    /// The same at a given time, in milliseconds on any steady clock.
    ///
    /// Two threads arriving together can each be allowed a line the bound
    /// would have refused one of. The bound is on the rate, not an invariant,
    /// and a lock here would be a lock in whichever hook is calling.
    pub fn allow_at(&self, now: u64) -> Option<u64> {
        if self.written.load(Ordering::Relaxed) < self.free {
            self.written.fetch_add(1, Ordering::Relaxed);
            return Some(0);
        }
        let start = self.window_start.load(Ordering::Relaxed);
        if now.saturating_sub(start) >= self.window_ms {
            self.window_start.store(now, Ordering::Relaxed);
            self.in_window.store(0, Ordering::Relaxed);
        }
        if self.in_window.fetch_add(1, Ordering::Relaxed) < self.burst {
            return Some(self.skipped.swap(0, Ordering::Relaxed));
        }
        self.skipped.fetch_add(1, Ordering::Relaxed);
        None
    }
}

/// Milliseconds since the first call, and never zero: zero is what a
/// `Throttle` that has not opened a window yet holds.
fn now_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
        + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!(
            "mhw-dlss-logger-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    #[test]
    fn the_last_log_is_kept_as_the_previous_one() {
        let folder = scratch("keep");
        std::fs::write(folder.join(NAME), "last session").unwrap();
        std::fs::write(folder.join(PREVIOUS), "the one before").unwrap();
        keep_previous(&folder);
        assert!(!folder.join(NAME).exists());
        assert_eq!(
            std::fs::read_to_string(folder.join(PREVIOUS)).unwrap(),
            "last session"
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn nothing_to_keep_is_not_an_error() {
        let folder = scratch("none");
        keep_previous(&folder);
        assert!(!folder.join(PREVIOUS).exists());
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn the_first_lines_are_free_and_the_rest_are_bounded() {
        let throttle = Throttle::new(3, 2, 10);
        // Three free ones, whenever they come.
        assert_eq!(throttle.allow_at(5), Some(0));
        assert_eq!(throttle.allow_at(5), Some(0));
        assert_eq!(throttle.allow_at(5), Some(0));
        // Then two in a window, and the rest of the window is counted.
        assert_eq!(throttle.allow_at(20_000), Some(0));
        assert_eq!(throttle.allow_at(20_001), Some(0));
        assert_eq!(throttle.allow_at(20_002), None);
        assert_eq!(throttle.allow_at(29_999), None);
        // The next window says how many were left out.
        assert_eq!(throttle.allow_at(30_000), Some(2));
        assert_eq!(throttle.allow_at(30_001), Some(0));
        assert_eq!(throttle.allow_at(30_002), None);
    }

    #[test]
    fn a_line_per_frame_for_an_hour_is_a_few_hundred_lines() {
        let throttle = Throttle::new(0, 5, 60);
        let mut written = 0;
        for frame in 0..(3600u64 * 240) {
            if throttle.allow_at(1 + frame * 1000 / 240).is_some() {
                written += 1;
            }
        }
        assert_eq!(written, 5 * 60);
    }
}
