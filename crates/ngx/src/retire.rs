//! Deferred release: a feature (or a resource) that a command list may still
//! reference is released only after enough renderer frames have passed for
//! the GPU to be done with it.
//!
//! The clock is the renderer's frame number, passed in; only *distinct*
//! values count, so a frame reported twice (several evaluations, or a tick
//! and an evaluation in one frame) does not age anything, and a counter that
//! restarts still moves the queue forward.

use std::collections::VecDeque;

/// Distinct renderer frames a released item is held for. The engine keeps
/// at most three frames in flight; sixteen leaves room for a swapchain with
/// frame generation queueing more.
pub const RETIRE_FRAMES: u64 = 16;

/// Counts distinct frame numbers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameClock {
    last: Option<u64>,
    ticks: u64,
}

impl FrameClock {
    /// A clock that has seen no frame.
    pub const fn new() -> FrameClock {
        FrameClock {
            last: None,
            ticks: 0,
        }
    }

    /// Note that `frame` is being rendered; returns the tick (distinct frames
    /// seen so far, counting this one).
    pub fn observe(&mut self, frame: u64) -> u64 {
        if self.last != Some(frame) {
            self.last = Some(frame);
            self.ticks += 1;
        }
        self.ticks
    }

    /// Distinct frames seen.
    pub const fn ticks(&self) -> u64 {
        self.ticks
    }

    /// The last frame observed.
    pub const fn last(&self) -> Option<u64> {
        self.last
    }
}

/// Items waiting out [`RETIRE_FRAMES`] (or another delay) distinct frames.
#[derive(Debug)]
pub struct RetireQueue<T> {
    clock: FrameClock,
    delay: u64,
    items: VecDeque<(u64, T)>,
}

impl<T> RetireQueue<T> {
    /// A queue holding items for `delay` distinct frames.
    pub const fn new(delay: u64) -> RetireQueue<T> {
        RetireQueue {
            clock: FrameClock::new(),
            delay,
            items: VecDeque::new(),
        }
    }

    /// Hold `item`, last used in `frame`. It is handed back by the
    /// [`advance`](RetireQueue::advance) that observes the `delay`-th
    /// distinct frame after `frame`.
    pub fn retire(&mut self, item: T, frame: u64) {
        let tick = self.clock.observe(frame);
        self.items.push_back((tick, item));
    }

    /// Note that `frame` is being rendered and take every item whose time
    /// is up, oldest first.
    pub fn advance(&mut self, frame: u64) -> Vec<T> {
        let now = self.clock.observe(frame);
        let mut due = Vec::new();
        while let Some((tick, _)) = self.items.front() {
            if now.saturating_sub(*tick) < self.delay {
                break;
            }
            if let Some((_, item)) = self.items.pop_front() {
                due.push(item);
            }
        }
        due
    }

    /// Take every item regardless of age (NGX shutdown: the context the
    /// items belong to is going away).
    pub fn drain_all(&mut self) -> Vec<T> {
        self.items.drain(..).map(|(_, item)| item).collect()
    }

    /// Items waiting.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether nothing is waiting.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The queue's frame clock.
    pub const fn clock(&self) -> &FrameClock {
        &self.clock
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_counts_distinct_frames_only() {
        let mut clock = FrameClock::new();
        assert_eq!(clock.observe(10), 1);
        assert_eq!(clock.observe(10), 1);
        assert_eq!(clock.observe(11), 2);
        // A restarted counter is still a new frame.
        assert_eq!(clock.observe(0), 3);
        assert_eq!(clock.last(), Some(0));
        assert_eq!(clock.ticks(), 3);
    }

    #[test]
    fn an_item_is_held_for_sixteen_distinct_frames() {
        let mut queue = RetireQueue::new(RETIRE_FRAMES);
        queue.retire("feature", 100);
        // The retiring frame itself and fifteen more: still held.
        assert!(queue.advance(100).is_empty());
        for frame in 101..=115 {
            assert!(queue.advance(frame).is_empty(), "frame {frame}");
            // Repeats of a frame do not age it.
            assert!(queue.advance(frame).is_empty(), "frame {frame} again");
        }
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.advance(116), vec!["feature"]);
        assert!(queue.is_empty());
    }

    /// Evaluations and Present both tick the queue; fed the same render
    /// frame they age it once per frame. Fed two counters (the evaluation's
    /// frame and a Present stuck on 0 without frame generation), every
    /// alternation was a "frame" and the hold halved.
    #[test]
    fn evaluate_and_present_of_one_frame_age_the_queue_once() {
        let mut same = RetireQueue::new(RETIRE_FRAMES);
        same.retire("feature", 100);
        for frame in 100..116 {
            assert!(same.advance(frame).is_empty(), "evaluate {frame}");
            assert!(same.advance(frame).is_empty(), "present {frame}");
        }
        assert_eq!(same.advance(116), vec!["feature"]);

        let mut mixed = RetireQueue::new(RETIRE_FRAMES);
        mixed.retire("feature", 100);
        let freed_at = (100..116)
            .find(|&frame| !mixed.advance(frame).is_empty() || !mixed.advance(0).is_empty());
        assert_eq!(freed_at, Some(108), "two counters free it after 8 frames");
    }

    #[test]
    fn items_leave_in_order_each_after_its_own_delay() {
        let mut queue = RetireQueue::new(3);
        queue.retire(1, 10);
        queue.advance(11);
        queue.retire(2, 11);
        queue.retire(3, 11);
        assert!(queue.advance(12).is_empty());
        assert_eq!(queue.advance(13), vec![1]);
        assert_eq!(queue.advance(14), vec![2, 3]);
    }

    #[test]
    fn a_stalled_clock_holds_items_and_drain_takes_them_all() {
        let mut queue = RetireQueue::new(2);
        queue.retire('a', 5);
        queue.retire('b', 5);
        for _ in 0..100 {
            assert!(queue.advance(5).is_empty());
        }
        assert_eq!(queue.drain_all(), vec!['a', 'b']);
        assert!(queue.is_empty());
    }

    #[test]
    fn a_zero_delay_releases_on_the_next_advance() {
        let mut queue = RetireQueue::new(0);
        queue.retire(7, 1);
        assert_eq!(queue.advance(1), vec![7]);
    }
}
