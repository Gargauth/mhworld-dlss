//! Rate limiting for lines whose frequency somebody else decides: per-frame
//! code, hooks the engine calls, callbacks from Streamline.
//!
//! Per-frame code never logs above `Debug` except through these:
//!
//! * [`log_once!`](crate::log_once) — once per `(target, key)` per run;
//! * [`log_every!`](crate::log_every) — the 1st, (n+1)-th, (2n+1)-th … pass
//!   through one call site;
//! * [`log_changed!`](crate::log_changed) — only when the value for
//!   `(target, key)` differs from the one last logged.
//!
//! None of them evaluates its format arguments for a line that is not
//! written, and none consumes its "once"/"changed" state while the level is
//! disabled, so raising the level later still shows the line.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

static ONCE: Mutex<Option<HashSet<u64>>> = Mutex::new(None);
static CHANGED: Mutex<Option<HashMap<u64, Box<dyn Any + Send>>>> = Mutex::new(None);

fn slot<K: Hash + ?Sized>(target: &str, key: &K) -> u64 {
    let mut hasher = DefaultHasher::new();
    target.hash(&mut hasher);
    key.hash(&mut hasher);
    hasher.finish()
}

/// `true` the first time `(target, key)` is seen in this process.
///
/// The set grows by one entry per distinct key: keys must come from a
/// bounded set (setting names, resource kinds), never from frame counters
/// or addresses.
pub fn once<K: Hash + ?Sized>(target: &str, key: &K) -> bool {
    let slot = slot(target, key);
    ONCE.lock().get_or_insert_with(HashSet::new).insert(slot)
}

/// `true` when `value` differs from the value last recorded for
/// `(target, key)` (or none was recorded); records it then.
///
/// A key reused with a different value type counts as changed.
pub fn changed<K, T>(target: &str, key: &K, value: &T) -> bool
where
    K: Hash + ?Sized,
    T: PartialEq + Clone + Send + 'static,
{
    let slot = slot(target, key);
    let mut guard = CHANGED.lock();
    let map = guard.get_or_insert_with(HashMap::new);
    match map.get_mut(&slot) {
        Some(last) => match last.downcast_mut::<T>() {
            Some(last) if last == value => false,
            Some(last) => {
                last.clone_from(value);
                true
            }
            None => {
                *last = Box::new(value.clone());
                true
            }
        },
        None => {
            map.insert(slot, Box::new(value.clone()));
            true
        }
    }
}

/// A call-site counter for [`log_every!`](crate::log_every).
#[derive(Debug, Default)]
pub struct Every {
    count: AtomicU64,
}

impl Every {
    /// A counter that has seen nothing.
    pub const fn new() -> Every {
        Every {
            count: AtomicU64::new(0),
        }
    }

    /// Count one pass; `true` for the 1st, (n+1)-th, (2n+1)-th … pass.
    /// `n == 0` is treated as 1.
    pub fn tick(&self, n: u64) -> bool {
        self.count
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(n.max(1))
    }

    /// Passes counted so far.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

/// Log once per `(target, key)` per run.
///
/// `log_once!(Warn, "config", key, "{key}: ignored, dev build only")`.
/// The level is one of `Error, Warn, Info, Debug, Trace`; everything after
/// the key is what `log::log!` takes after its level, structured fields
/// included.
#[macro_export]
macro_rules! log_once {
    ($level:ident, $target:expr, $key:expr, $($arg:tt)+) => {{
        let __level = $crate::log::Level::$level;
        if $crate::log::__facade::log_enabled!(target: $target, __level)
            && $crate::log::rate::once($target, &$key)
        {
            $crate::log::__facade::log!(target: $target, __level, $($arg)+);
        }
    }};
}

/// Log the 1st, (n+1)-th, (2n+1)-th … time this call site is reached.
///
/// `log_every!(600, Debug, "perf", "slot {} us", micros)`. Every call is
/// counted whether or not the level is enabled.
#[macro_export]
macro_rules! log_every {
    ($n:expr, $level:ident, $target:expr, $($arg:tt)+) => {{
        static __EVERY: $crate::log::rate::Every = $crate::log::rate::Every::new();
        let __level = $crate::log::Level::$level;
        if __EVERY.tick($n) && $crate::log::__facade::log_enabled!(target: $target, __level) {
            $crate::log::__facade::log!(target: $target, __level, $($arg)+);
        }
    }};
}

/// Log only when `value` differs from the value last logged for
/// `(target, key)`.
///
/// `log_changed!(Info, "render", "size", (w, h), "render size {w}x{h}")`.
/// `value` is evaluated once for the comparison; the message arguments are
/// evaluated only when the line is written.
#[macro_export]
macro_rules! log_changed {
    ($level:ident, $target:expr, $key:expr, $value:expr, $($arg:tt)+) => {{
        let __level = $crate::log::Level::$level;
        if $crate::log::__facade::log_enabled!(target: $target, __level)
            && $crate::log::rate::changed($target, &$key, &$value)
        {
            $crate::log::__facade::log!(target: $target, __level, $($arg)+);
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn once_is_per_target_and_key() {
        assert!(once("test-once", "a"));
        assert!(!once("test-once", "a"));
        assert!(once("test-once", "b"));
        assert!(once("test-once-other", "a"));
        assert!(once("test-once", &7u32));
        assert!(!once("test-once", &7u32));
    }

    #[test]
    fn changed_compares_with_the_last_recorded_value() {
        assert!(changed("test-changed", "size", &(1920u32, 1080u32)));
        assert!(!changed("test-changed", "size", &(1920u32, 1080u32)));
        assert!(changed("test-changed", "size", &(1280u32, 720u32)));
        assert!(!changed("test-changed", "size", &(1280u32, 720u32)));
        assert!(changed("test-changed", "size", &(1920u32, 1080u32)));
        // Another key is independent; floats work.
        assert!(changed("test-changed", "scale", &0.5f32));
        assert!(!changed("test-changed", "scale", &0.5f32));
        // A different type under the same key replaces the record.
        assert!(changed("test-changed", "scale", &"half"));
        assert!(!changed("test-changed", "scale", &"half"));
    }

    #[test]
    fn every_lets_through_the_first_of_each_n() {
        let every = Every::new();
        let passed: Vec<u64> = (0..10).filter(|_| every.tick(4)).collect();
        assert_eq!(passed, vec![0, 4, 8]);
        assert_eq!(every.count(), 10);
        let zero = Every::new();
        assert!((0..3).all(|_| zero.tick(0)));
    }

    #[test]
    fn the_macros_do_not_evaluate_arguments_of_suppressed_lines() {
        // No logger is installed in unit tests, so the max level is Off and
        // nothing is enabled: neither state nor arguments are touched.
        fn boom() -> u32 {
            panic!("evaluated")
        }
        crate::log_once!(Error, "test-macro", "k", "{}", boom());
        crate::log_every!(1, Error, "test-macro", "{}", boom());
        crate::log_changed!(Error, "test-macro", "k", 1u32, "{}", boom());
        assert!(once("test-macro", "k"));
        assert!(changed("test-macro", "k", &1u32));
    }
}
