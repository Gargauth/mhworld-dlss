//! Dev builds only: the last log lines in memory, for the dev server's
//! `log.tail` and `log.subscribe`.
//!
//! Bounded: the oldest line is dropped when [`CAPACITY`] is reached. Every
//! line carries a sequence number so that a streaming client can ask for
//! "everything after what I already have" and notice what it missed.

use std::collections::VecDeque;

use ::log::Level;
use parking_lot::Mutex;

/// Lines kept.
pub const CAPACITY: usize = 4096;

/// One formatted log line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Position in the stream of all lines since start, from 0.
    pub seq: u64,
    /// Record level.
    pub level: Level,
    /// Record target (subsystem).
    pub target: String,
    /// The line exactly as written to the file (redacted, no newline).
    pub line: String,
}

/// A bounded ring of lines.
#[derive(Debug)]
pub struct Ring {
    entries: VecDeque<Entry>,
    capacity: usize,
    next: u64,
}

impl Ring {
    /// An empty ring holding at most `capacity` lines (at least one).
    pub const fn new(capacity: usize) -> Ring {
        Ring {
            entries: VecDeque::new(),
            capacity,
            next: 0,
        }
    }

    /// Append a line, dropping the oldest when full.
    pub fn push(&mut self, level: Level, target: &str, line: &str) {
        while self.entries.len() >= self.capacity.max(1) {
            self.entries.pop_front();
        }
        self.entries.push_back(Entry {
            seq: self.next,
            level,
            target: target.to_owned(),
            line: line.to_owned(),
        });
        self.next += 1;
    }

    /// The last `n` lines, oldest first.
    pub fn tail(&self, n: usize) -> Vec<Entry> {
        let skip = self.entries.len().saturating_sub(n);
        self.entries.iter().skip(skip).cloned().collect()
    }

    /// Every kept line with `seq >= from`, oldest first. If lines between
    /// `from` and the first returned one were already dropped, the first
    /// returned `seq` is greater than `from`.
    pub fn since(&self, from: u64) -> Vec<Entry> {
        self.entries
            .iter()
            .filter(|entry| entry.seq >= from)
            .cloned()
            .collect()
    }

    /// The sequence number the next line will get.
    pub fn next_seq(&self) -> u64 {
        self.next
    }
}

static RING: Mutex<Ring> = Mutex::new(Ring::new(CAPACITY));

/// Record a line in the process ring (called by the logger).
pub(crate) fn push(level: Level, target: &str, line: &str) {
    RING.lock().push(level, target, line);
}

/// The last `n` lines of the process ring.
pub fn tail(n: usize) -> Vec<Entry> {
    RING.lock().tail(n)
}

/// Lines of the process ring with `seq >= from`.
pub fn since(from: u64) -> Vec<Entry> {
    RING.lock().since(from)
}

/// The sequence number the next line of the process ring will get.
pub fn next_seq() -> u64 {
    RING.lock().next_seq()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled(capacity: usize, lines: u64) -> Ring {
        let mut ring = Ring::new(capacity);
        for index in 0..lines {
            ring.push(Level::Info, "test", &format!("line {index}"));
        }
        ring
    }

    #[test]
    fn the_ring_keeps_the_newest_lines() {
        let ring = filled(3, 5);
        let lines: Vec<String> = ring.tail(10).into_iter().map(|e| e.line).collect();
        assert_eq!(lines, ["line 2", "line 3", "line 4"]);
        assert_eq!(ring.tail(1)[0].seq, 4);
        assert_eq!(ring.next_seq(), 5);
        assert!(ring.tail(0).is_empty());
    }

    #[test]
    fn since_resumes_a_stream_and_shows_gaps() {
        let ring = filled(3, 5);
        let seqs = |from| ring.since(from).iter().map(|e| e.seq).collect::<Vec<_>>();
        assert_eq!(seqs(3), [3, 4]);
        assert_eq!(seqs(5), Vec::<u64>::new());
        // 0 and 1 were dropped: the reader sees the stream resume at 2.
        assert_eq!(seqs(0), [2, 3, 4]);
    }

    #[test]
    fn a_zero_capacity_ring_still_keeps_one_line() {
        let ring = filled(0, 2);
        assert_eq!(ring.tail(5).len(), 1);
    }
}
