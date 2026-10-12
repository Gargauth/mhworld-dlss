//! Execution points (`docs/DEV-PROTOCOL.md` § Execution points): requests
//! that touch the game queue a task at a point; the mod's call for that
//! point runs it on its own thread. A point with nothing queued costs one
//! atomic load.

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};

use mhw_core::frame::{CameraFrame, JitterSample};
use parking_lot::Mutex;

use crate::proto::Point;
use crate::SlotContext;

/// What a task sees when its point comes.
#[derive(Clone, Copy, Debug)]
pub enum Ctx<'a> {
    /// Start of a main-loop iteration.
    Frame {
        /// The mod's frame number.
        frame: u64,
    },
    /// The camera detour, after the original.
    Camera {
        /// This frame's camera.
        camera: &'a CameraFrame,
        /// This frame's jitter.
        jitter: &'a JitterSample,
    },
    /// The upscale slot; `list` is the engine's open command list.
    Slot {
        /// `ID3D12GraphicsCommandList*`.
        list: *mut c_void,
        /// What the slot works on.
        slot: &'a SlotContext,
    },
    /// Before Present.
    Present {
        /// The mod's frame number.
        frame: u64,
    },
}

impl Ctx<'_> {
    /// The point this is.
    pub fn point(&self) -> Point {
        match self {
            Ctx::Frame { .. } => Point::Frame,
            Ctx::Camera { .. } => Point::Camera,
            Ctx::Slot { .. } => Point::Slot,
            Ctx::Present { .. } => Point::Present,
        }
    }

    /// The frame number the point carries.
    pub fn frame(&self) -> u64 {
        match self {
            Ctx::Frame { frame } | Ctx::Present { frame } => *frame,
            Ctx::Camera { camera, .. } => camera.frame,
            Ctx::Slot { slot, .. } => slot.frame.frame,
        }
    }
}

/// Whether a task wants to run again at the next occurrence of its point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Run again next time.
    More,
    /// Finished; dropped.
    Done,
}

/// A queued task.
pub type Task = Box<dyn FnMut(&Ctx<'_>) -> Step + Send>;

#[derive(Default)]
struct Slot {
    pending: AtomicUsize,
    tasks: Mutex<Vec<Task>>,
}

/// The four point queues.
#[derive(Default)]
pub struct Queue {
    slots: [Slot; 4],
}

impl std::fmt::Debug for Queue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pending: Vec<usize> = self
            .slots
            .iter()
            .map(|s| s.pending.load(Ordering::Relaxed))
            .collect();
        f.debug_struct("Queue").field("pending", &pending).finish()
    }
}

impl Queue {
    /// Queue `task` at `point`.
    pub fn push(&self, point: Point, task: Task) {
        let slot = &self.slots[point.index()];
        let mut tasks = slot.tasks.lock();
        tasks.push(task);
        slot.pending.store(tasks.len(), Ordering::Release);
    }

    /// Tasks waiting at `point`.
    pub fn pending(&self, point: Point) -> usize {
        self.slots[point.index()].pending.load(Ordering::Acquire)
    }

    /// Run every task queued at `ctx`'s point, in queue order, keeping
    /// those that return [`Step::More`]. A task that panics is dropped.
    /// Tasks queued while this runs wait for the next occurrence.
    pub fn run(&self, ctx: &Ctx<'_>) {
        let slot = &self.slots[ctx.point().index()];
        if slot.pending.load(Ordering::Acquire) == 0 {
            return;
        }
        let mut tasks = std::mem::take(&mut *slot.tasks.lock());
        tasks.retain_mut(|task| match catch_unwind(AssertUnwindSafe(|| task(ctx))) {
            Ok(step) => step == Step::More,
            Err(_) => {
                log::error!(target: "dev", "a dev task panicked at the {} point; dropped", ctx.point().as_str());
                false
            }
        });
        let mut queued = slot.tasks.lock();
        tasks.append(&mut queued);
        *queued = tasks;
        slot.pending.store(queued.len(), Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn counting(log: &Arc<Mutex<Vec<(u32, u64)>>>, id: u32, runs: u32) -> Task {
        let log = log.clone();
        let mut left = runs;
        Box::new(move |ctx| {
            log.lock().push((id, ctx.frame()));
            left -= 1;
            if left == 0 {
                Step::Done
            } else {
                Step::More
            }
        })
    }

    #[test]
    fn tasks_run_at_their_point_in_order_until_done() {
        let q = Queue::default();
        let log = Arc::new(Mutex::new(Vec::new()));
        q.push(Point::Frame, counting(&log, 1, 2));
        q.push(Point::Frame, counting(&log, 2, 1));
        q.push(Point::Present, counting(&log, 3, 1));
        assert_eq!(q.pending(Point::Frame), 2);
        q.run(&Ctx::Frame { frame: 10 });
        q.run(&Ctx::Frame { frame: 11 });
        q.run(&Ctx::Frame { frame: 12 });
        assert_eq!(*log.lock(), [(1, 10), (2, 10), (1, 11)]);
        assert_eq!(q.pending(Point::Frame), 0);
        assert_eq!(q.pending(Point::Present), 1);
        q.run(&Ctx::Present { frame: 13 });
        assert_eq!(log.lock().last(), Some(&(3, 13)));
    }

    #[test]
    fn a_task_queued_while_running_waits_for_the_next_occurrence() {
        let q = Arc::new(Queue::default());
        let log = Arc::new(Mutex::new(Vec::new()));
        let (inner_q, inner_log) = (q.clone(), log.clone());
        q.push(
            Point::Frame,
            Box::new(move |_| {
                inner_q.push(Point::Frame, counting(&inner_log, 9, 1));
                Step::Done
            }),
        );
        q.run(&Ctx::Frame { frame: 1 });
        assert!(log.lock().is_empty());
        q.run(&Ctx::Frame { frame: 2 });
        assert_eq!(*log.lock(), [(9, 2)]);
    }

    #[test]
    fn a_panicking_task_is_dropped_and_the_others_run() {
        let q = Queue::default();
        let log = Arc::new(Mutex::new(Vec::new()));
        q.push(Point::Frame, Box::new(|_| panic!("boom")));
        q.push(Point::Frame, counting(&log, 1, 1));
        q.run(&Ctx::Frame { frame: 5 });
        assert_eq!(*log.lock(), [(1, 5)]);
        assert_eq!(q.pending(Point::Frame), 0);
    }
}
