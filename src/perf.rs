//! How much of a frame is spent inside this library.
//!
//! The game cannot be run under a profiler by everyone who reports a
//! problem, so the cost of the hooks is measured by the mod itself: with
//! `perf = 1` in the ini, every 600 rendered frames the log carries the time
//! spent in this library per rendered frame, broken down by hook family, with
//! call counts beside it.
//!
//! ## What is measured
//!
//! Only the part of a hook that is ours. A scope is opened on entry and closed
//! before the original D3D12 method is called, so the runtime's and the
//! driver's own time is not counted. Three things are worth knowing when
//! reading the numbers:
//!
//! * `list.dispatch` is the whole of the temporal-pass replacement, which
//!   includes `dlss.evaluate` -- NGX recording its work into the list. That
//!   one is printed on its own line as well and is not added twice.
//! * Our own recording goes through the patched vtable, so the handful of
//!   list calls the decode and the evaluation make are counted under their own
//!   families as well as inside `list.dispatch`. That is a few dozen calls a
//!   frame against tens of thousands.
//! * `present.after` contains `slReflexSleep`, which waits on purpose. It is
//!   printed and left out of the total.
//! * The two `wait:` lines are the time spent *acquiring* a lock, not holding
//!   it: the registry's, and the per-list state's. They are inside the
//!   families above and are not added again. Uncontended, an acquisition is
//!   some twenty nanoseconds plus the instrument; a figure well above that
//!   per call is threads queueing for each other, which is a different
//!   problem from a hook doing too much and wants a different cure.
//!
//! ## What the instrument costs
//!
//! Off, each hook pays one relaxed atomic load. On, every call bumps a
//! thread-local counter, and the families that run thousands of times a frame
//! time one call in `SAMPLE` with `rdtsc` and scale the result, so the
//! per-call cost is a counter rather than two timestamp reads and two shared
//! atomics. Per family and per thread, not one counter across families: a
//! shared one would lock onto whichever hook sat at a fixed position in a
//! repeating sequence of calls.
//!
//! When the switch is turned on the instrument times itself -- a timed pair
//! and an untimed one -- and each report states what that comes to for the
//! calls it saw, so the reader can subtract it.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// One call in this many is timed, for the high-frequency families.
const SAMPLE: u32 = 32;
/// Rendered frames between reports.
const PERIOD: u64 = 600;

#[derive(Clone, Copy)]
#[repr(usize)]
pub enum Family {
    /// `SetPipelineState`, the compute root binds, `SetDescriptorHeaps`.
    ListState = 0,
    /// Root constant buffer views, graphics and compute.
    ListCbv,
    /// `DrawInstanced`, `DrawIndexedInstanced`.
    ListDraw,
    /// `OMSetRenderTargets`.
    ListTargets,
    /// `ResourceBarrier`.
    ListBarrier,
    /// `Close`.
    ListClose,
    /// `Dispatch`, including the decode and the evaluation.
    ListDispatch,
    /// The call into NGX, inside `ListDispatch`.
    Evaluate,
    /// `CopyDescriptors`, `CopyDescriptorsSimple`.
    DeviceCopy,
    /// View, resource, root signature and pipeline creation; `Map`, `Unmap`.
    DeviceCreate,
    /// `ExecuteCommandLists`.
    Execute,
    /// The engine's camera setup, through the detour.
    Camera,
    /// The engine's upscale slot: the frame generation inputs.
    UpscaleSlot,
    /// Before `Present`: markers and the HUD-less tag.
    PresentBefore,
    /// After it: the next token, and `slReflexSleep`, which waits.
    PresentAfter,
    /// Waiting to acquire the descriptor registry's lock, which every
    /// descriptor copy and every mapping lookup takes. Inside the families
    /// that take it, so not part of the total.
    RegistryWait,
    /// Waiting to acquire the lock on per-list state. Likewise.
    ListsWait,
}

const FAMILIES: usize = 17;

const NAMES: [&str; FAMILIES] = [
    "list.state",
    "list.cbv",
    "list.draw",
    "list.targets",
    "list.barrier",
    "list.close",
    "list.dispatch",
    "  dlss.evaluate",
    "device.copy",
    "device.create",
    "queue.execute",
    "camera.detour",
    "upscale.slot",
    "present.before",
    "present.after",
    "wait: registry",
    "wait: lists",
];

/// How many calls each timed one stands for.
const EVERY: [u32; FAMILIES] = [
    SAMPLE, SAMPLE, SAMPLE, SAMPLE, SAMPLE, SAMPLE, 1, 1, SAMPLE, SAMPLE, 1, 1, 1, 1, 1, SAMPLE,
    SAMPLE,
];

/// Whether a family's time is part of the total. `Evaluate` is inside
/// `ListDispatch`, and `PresentAfter` is a deliberate wait.
const IN_TOTAL: [bool; FAMILIES] = [
    true, true, true, true, true, true, true, false, true, true, true, true, true, true, false,
    false, false,
];

static ENABLED: AtomicBool = AtomicBool::new(false);
static CALIBRATED: AtomicBool = AtomicBool::new(false);
/// Timestamp counter ticks per second.
static TICKS_PER_SECOND: AtomicU64 = AtomicU64::new(0);
/// What one timed start/stop costs, and one untimed, in ticks.
static TIMED_COST: AtomicU64 = AtomicU64::new(0);
static UNTIMED_COST: AtomicU64 = AtomicU64::new(0);

/// Ticks inside our code, already scaled by the sampling rate.
static TICKS: [AtomicU64; FAMILIES] = [const { AtomicU64::new(0) }; FAMILIES];
/// Calls, already scaled.
static CALLS: [AtomicU64; FAMILIES] = [const { AtomicU64::new(0) }; FAMILIES];
/// Start/stop pairs actually timed, for the instrument's own cost.
static TIMED: [AtomicU64; FAMILIES] = [const { AtomicU64::new(0) }; FAMILIES];
static FRAMES: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static SEEN: [Cell<u32>; FAMILIES] = const { [const { Cell::new(0) }; FAMILIES] };
}

#[inline(always)]
fn now() -> u64 {
    // SAFETY: `rdtsc` is unprivileged and present on every x86-64 processor.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// An open measurement. Inert when the switch is off or the call was not
/// sampled.
#[must_use]
pub struct Scope {
    start: u64,
    family: u8,
}

/// Open a scope. Close it with `stop` before calling the original method.
#[inline(always)]
pub fn start(family: Family) -> Scope {
    if !ENABLED.load(Ordering::Relaxed) {
        return Scope {
            start: 0,
            family: 0,
        };
    }
    begin(family as usize)
}

#[inline(never)]
fn begin(family: usize) -> Scope {
    let every = EVERY[family];
    if every > 1 {
        let seen = SEEN.with(|seen| {
            let cell = &seen[family];
            let next = cell.get().wrapping_add(1);
            cell.set(next);
            next
        });
        if !seen.is_multiple_of(every) {
            return Scope {
                start: 0,
                family: 0,
            };
        }
    }
    CALLS[family].fetch_add(every as u64, Ordering::Relaxed);
    // Never zero, which is the mark of an inert scope.
    Scope {
        start: now() | 1,
        family: family as u8,
    }
}

impl Scope {
    #[inline(always)]
    pub fn stop(self) {
        if self.start != 0 {
            finish(self);
        }
    }
}

#[inline(never)]
fn finish(scope: Scope) {
    let elapsed = now().wrapping_sub(scope.start);
    let family = scope.family as usize;
    TIMED[family].fetch_add(1, Ordering::Relaxed);
    // A hook body that takes nanoseconds, timed on a thread the scheduler may
    // take away between the two readings. One such interruption is a
    // millisecond or more, and one in ten thousand samples of it adds a
    // hundred nanoseconds to the mean of a body that takes five -- which is
    // how a draw hook that does one atomic load came to read 0.19 us a call.
    // So a sample of a frequent hook longer than any such body can be is set
    // aside and reported separately rather than averaged in. The lock waits
    // are exempt: a long wait is exactly what they are there to show. So are
    // the families timed on every call, whose bodies really can be long.
    if SET_ASIDE[family] && elapsed > LONGEST.load(Ordering::Relaxed) {
        ASIDE[family].fetch_add(1, Ordering::Relaxed);
        ASIDE_TICKS.fetch_add(elapsed, Ordering::Relaxed);
        return;
    }
    // Less what the two readings cost with nothing between them.
    let net = elapsed.saturating_sub(EMPTY.load(Ordering::Relaxed));
    TICKS[family].fetch_add(net.wrapping_mul(EVERY[family] as u64), Ordering::Relaxed);
}

/// Which families have over-long samples set aside: the frequent, short ones.
const SET_ASIDE: [bool; FAMILIES] = [
    true, true, true, true, true, true, false, false, true, true, false, false, false, false,
    false, false, false,
];
/// What counts as over-long, in microseconds. No body of a frequent hook
/// comes near it; a context switch always exceeds it.
const LONGEST_MICROSECONDS: u64 = 20;
/// The same in ticks, once calibrated. Until then nothing is set aside.
static LONGEST: AtomicU64 = AtomicU64::new(u64::MAX);
/// What a scope reads with nothing inside it, in ticks.
static EMPTY: AtomicU64 = AtomicU64::new(0);
/// Samples set aside, per family, and the ticks they held between them.
static ASIDE: [AtomicU64; FAMILIES] = [const { AtomicU64::new(0) }; FAMILIES];
static ASIDE_TICKS: AtomicU64 = AtomicU64::new(0);

/// Follow the configuration. Called from the settings watcher, never from a
/// hook, so the calibration's fifty milliseconds are nobody's frame time.
pub fn follow(wanted: bool) {
    if wanted == ENABLED.load(Ordering::Relaxed) {
        return;
    }
    if !wanted {
        ENABLED.store(false, Ordering::Relaxed);
        crate::log!("[perf] off");
        return;
    }
    if !CALIBRATED.swap(true, Ordering::Relaxed) {
        calibrate();
    }
    reset();
    ENABLED.store(true, Ordering::Relaxed);
    let per_second = TICKS_PER_SECOND.load(Ordering::Relaxed).max(1) as f64;
    crate::log!(
        "[perf] on: a report every {PERIOD} rendered frames. One call in {SAMPLE} of the \
         frequent hooks is timed. The instrument costs about {:.0} ns for a timed call and \
         {:.1} ns for an untimed one, measured just now",
        TIMED_COST.load(Ordering::Relaxed) as f64 * 1e9 / per_second,
        UNTIMED_COST.load(Ordering::Relaxed) as f64 / 16.0 * 1e9 / per_second
    );
}

fn reset() {
    for family in 0..FAMILIES {
        TICKS[family].store(0, Ordering::Relaxed);
        CALLS[family].store(0, Ordering::Relaxed);
        TIMED[family].store(0, Ordering::Relaxed);
        ASIDE[family].store(0, Ordering::Relaxed);
    }
    ASIDE_TICKS.store(0, Ordering::Relaxed);
    FRAMES.store(0, Ordering::Relaxed);
}

/// The counter's rate against the OS clock, and what the instrument itself
/// costs, measured rather than assumed.
fn calibrate() {
    let wall = std::time::Instant::now();
    let first = now();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let ticks = now().wrapping_sub(first);
    let seconds = wall.elapsed().as_secs_f64().max(1e-6);
    TICKS_PER_SECOND.store((ticks as f64 / seconds) as u64, Ordering::Relaxed);

    // A timed pair: a family that is timed on every call.
    const ROUNDS: u64 = 20_000;
    let before = now();
    for _ in 0..ROUNDS {
        begin(Family::Execute as usize).stop();
    }
    TIMED_COST.store(now().wrapping_sub(before) / ROUNDS, Ordering::Relaxed);
    // And what those empty scopes read from the inside, which is what every
    // real reading carries on top of the body it times. The smallest of a few
    // batches, since anything that disturbs a batch only makes it larger.
    let mut empty = u64::MAX;
    for _ in 0..8 {
        TICKS[Family::Execute as usize].store(0, Ordering::Relaxed);
        for _ in 0..1000 {
            begin(Family::Execute as usize).stop();
        }
        empty = empty.min(TICKS[Family::Execute as usize].load(Ordering::Relaxed) / 1000);
    }
    EMPTY.store(empty, Ordering::Relaxed);
    LONGEST.store(
        TICKS_PER_SECOND.load(Ordering::Relaxed) / 1_000_000 * LONGEST_MICROSECONDS,
        Ordering::Relaxed,
    );

    // An untimed one: a sampled family, so all but one in `SAMPLE` take the
    // counter-only path. Kept in sixteenths of a tick, because the whole
    // answer is a tick or two.
    let before = now();
    for _ in 0..ROUNDS {
        begin(Family::ListState as usize).stop();
    }
    let all = now().wrapping_sub(before);
    let timed = ROUNDS / SAMPLE as u64 * TIMED_COST.load(Ordering::Relaxed);
    UNTIMED_COST.store(all.saturating_sub(timed) * 16 / ROUNDS, Ordering::Relaxed);
}

/// A rendered frame has gone by: the engine's temporal pass was dispatched.
///
/// The denominator is rendered frames and not Presents: with frame
/// generation on, Presents count frames nobody rendered.
pub fn frame() {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let frames = FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
    if frames < PERIOD {
        return;
    }
    // Another thread's dispatch may land here too; whoever takes the count
    // reports, and the other finds it already taken.
    if FRAMES.swap(0, Ordering::Relaxed) < PERIOD {
        return;
    }
    report(frames as f64);
}

fn report(frames: f64) {
    let per_second = TICKS_PER_SECOND.load(Ordering::Relaxed);
    if per_second == 0 {
        return;
    }
    let to_ms = 1000.0 / per_second as f64;
    let timed_cost = TIMED_COST.load(Ordering::Relaxed) as f64;
    let untimed_cost = UNTIMED_COST.load(Ordering::Relaxed) as f64 / 16.0;

    let mut total = 0.0;
    let mut overhead = 0.0;
    let mut aside = 0.0;
    let mut doubt = 0.0;
    let mut lines = Vec::with_capacity(FAMILIES);
    // How far the subtraction of the empty reading can be off per timed call:
    // a few ticks either way. It is the floor under what a line can show.
    let slack = 8.0;
    for family in 0..FAMILIES {
        let ticks = TICKS[family].swap(0, Ordering::Relaxed) as f64;
        let calls = CALLS[family].swap(0, Ordering::Relaxed) as f64;
        let timed = TIMED[family].swap(0, Ordering::Relaxed) as f64;
        let set_aside = ASIDE[family].swap(0, Ordering::Relaxed) as f64;
        aside += set_aside;
        // The samples kept stand for all the calls, the ones set aside
        // included, so the mean is scaled back up over them.
        let kept = (timed - set_aside).max(1.0);
        let ms = ticks * (timed.max(1.0) / kept) * to_ms / frames;
        if IN_TOTAL[family] {
            doubt += calls * slack * to_ms / frames;
        }
        if IN_TOTAL[family] {
            total += ms;
        }
        overhead += (timed * timed_cost + (calls - timed).max(0.0) * untimed_cost) * to_ms / frames;
        lines.push((family, ms, calls / frames));
    }
    crate::log!(
        "[perf] {frames:.0} rendered frames: {total:.3} ms per frame inside mhw-dlss \
         (present.after excluded: it holds slReflexSleep, which waits on purpose). \
         The instrument itself added about {overhead:.3} ms per frame on top"
    );
    crate::log!(
        "[perf]   each reading is net of the {:.0} ns an empty scope reads; a difference \
         under about {doubt:.3} ms in the total is not to be trusted. {aside:.0} timed calls \
         of the frequent hooks ran over {LONGEST_MICROSECONDS} us and were set aside as \
         interruptions; averaged in, they would have added {:.3} ms per frame",
        EMPTY.load(Ordering::Relaxed) as f64 * to_ms * 1e6,
        ASIDE_TICKS.swap(0, Ordering::Relaxed) as f64 * SAMPLE as f64 * to_ms / frames
    );
    for (family, ms, calls) in lines {
        crate::log!(
            "[perf]   {:<16} {ms:>8.4} ms/frame {calls:>10.1} calls/frame  {}",
            NAMES[family],
            if EVERY[family] > 1 {
                "1 in 32 timed"
            } else {
                "every call timed"
            }
        );
    }
}
