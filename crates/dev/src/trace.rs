//! `trace`: a timeline of the mod's events. Events are recorded only while
//! a `trace` request is collecting, so `trace_event` is one atomic load
//! otherwise.

use std::collections::{BTreeMap, VecDeque};
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use serde_json::{json, Value};

/// Events kept while collecting (a few dozen per frame).
pub const CAPACITY: usize = 16 * 1024;

/// One event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    /// What happened.
    pub name: &'static str,
    /// The frame it belongs to, as the caller numbered it.
    pub frame: u64,
    /// Performance-counter tick.
    pub qpc: u64,
    /// The recording thread's id (0 off Windows).
    pub thread: u32,
}

/// The event recorder.
#[derive(Debug, Default)]
pub struct Recorder {
    active: AtomicUsize,
    events: Mutex<VecDeque<Event>>,
}

/// Keeps the recorder collecting until dropped.
#[derive(Debug)]
pub struct Collecting<'a>(&'a Recorder);

impl Drop for Collecting<'_> {
    fn drop(&mut self) {
        if self.0.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.events.lock().clear();
        }
    }
}

impl Recorder {
    /// Start collecting (nested collections share the buffer).
    pub fn collect(&self) -> Collecting<'_> {
        self.active.fetch_add(1, Ordering::AcqRel);
        Collecting(self)
    }

    /// Whether anything is collecting.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed) != 0
    }

    /// Record an event if collecting; `qpc` is read only then.
    pub fn record(&self, name: &'static str, frame: u64, qpc: impl FnOnce() -> u64) {
        if !self.is_active() {
            return;
        }
        let event = Event {
            name,
            frame,
            qpc: qpc(),
            thread: clock::thread(),
        };
        let mut events = self.events.lock();
        if events.len() >= CAPACITY {
            events.pop_front();
        }
        events.push_back(event);
    }

    /// The events of `frames`, grouped by frame in frame order, each group
    /// in time order.
    pub fn timeline(&self, frames: Range<u64>, frequency: u64) -> Value {
        let mut groups: BTreeMap<u64, Vec<Event>> = BTreeMap::new();
        for e in self
            .events
            .lock()
            .iter()
            .filter(|e| frames.contains(&e.frame))
        {
            groups.entry(e.frame).or_default().push(*e);
        }
        let frames: Vec<Value> = groups
            .into_iter()
            .map(|(frame, mut events)| {
                events.sort_by_key(|e| e.qpc);
                let events: Vec<Value> = events
                    .iter()
                    .map(|e| json!({"what": e.name, "qpc": e.qpc, "thread": e.thread}))
                    .collect();
                json!({"frame": frame, "events": events})
            })
            .collect();
        json!({"qpc_frequency": frequency, "frames": frames})
    }
}

/// The performance counter: `QueryPerformanceCounter` on Windows, a
/// nanosecond monotonic clock elsewhere.
pub mod clock {
    /// Now, in ticks.
    #[cfg(windows)]
    pub fn now() -> u64 {
        let mut ticks = 0i64;
        // SAFETY: writes one integer; never fails on XP and later.
        let _ = unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&mut ticks) };
        ticks as u64
    }

    /// Ticks per second.
    #[cfg(windows)]
    pub fn frequency() -> u64 {
        let mut frequency = 0i64;
        // SAFETY: writes one integer; never fails on XP and later.
        let _ = unsafe {
            windows::Win32::System::Performance::QueryPerformanceFrequency(&mut frequency)
        };
        frequency as u64
    }

    /// The calling thread's id.
    #[cfg(windows)]
    pub fn thread() -> u32 {
        // SAFETY: no arguments, cannot fail.
        unsafe { windows::Win32::System::Threading::GetCurrentThreadId() }
    }

    /// No thread ids off Windows.
    #[cfg(not(windows))]
    pub fn thread() -> u32 {
        0
    }

    /// Now, in ticks.
    #[cfg(not(windows))]
    pub fn now() -> u64 {
        use std::sync::LazyLock;
        use std::time::Instant;
        static START: LazyLock<Instant> = LazyLock::new(Instant::now);
        START.elapsed().as_nanos() as u64
    }

    /// Ticks per second.
    #[cfg(not(windows))]
    pub fn frequency() -> u64 {
        1_000_000_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_recorded_unless_collecting() {
        let r = Recorder::default();
        r.record("camera", 1, || unreachable!("qpc read while idle"));
        let guard = r.collect();
        r.record("slot", 2, || 20);
        r.record("camera", 2, || 10);
        r.record("present", 3, || 30);
        r.record("camera", 9, || 90);
        let t = r.timeline(2..4, 100);
        assert_eq!(t["qpc_frequency"], 100);
        assert_eq!(
            t["frames"],
            json!([
                {"frame": 2, "events": [{"what": "camera", "qpc": 10, "thread": clock::thread()}, {"what": "slot", "qpc": 20, "thread": clock::thread()}]},
                {"frame": 3, "events": [{"what": "present", "qpc": 30, "thread": clock::thread()}]},
            ])
        );
        drop(guard);
        assert!(!r.is_active());
        assert_eq!(
            r.timeline(0..100, 1)["frames"],
            json!([]),
            "cleared when the last collector ends"
        );
    }

    #[test]
    fn the_buffer_is_bounded() {
        let r = Recorder::default();
        let _guard = r.collect();
        for i in 0..CAPACITY as u64 + 10 {
            r.record("x", i, || i);
        }
        let frames = r.timeline(0..u64::MAX, 1)["frames"]
            .as_array()
            .unwrap()
            .len();
        assert_eq!(frames, CAPACITY);
    }

    #[test]
    fn the_clock_moves_forward() {
        let a = clock::now();
        let b = clock::now();
        assert!(b >= a && clock::frequency() > 0);
    }
}
