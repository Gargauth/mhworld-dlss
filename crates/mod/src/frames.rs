//! Small per-frame bookkeeping. Pure.
//!
//! * [`CameraRing`]: the engine's camera and jitter by render frame. The
//!   camera setup runs on the main thread and the upscale slot on the
//!   render thread, and the engine counts frames (`sRender+0x1f208`) at the
//!   main thread's kick: a slot can carry the next frame's number while it
//!   records the frame before (seen in game, more often with frame
//!   generation's pacing). [`CameraRing::frame_with_projection`] finds the
//!   frame a temporal pass actually drew from the jitter in its camera
//!   buffer.
//! * [`KeepRing`]: what the mod holds for the GPU. Resources handed to NGX
//!   are raw pointers and the commands using them execute later; an engine
//!   release in between (scene change) left the driver reading freed
//!   memory in 0.2, so a reference is kept for a few frames. The GPU runs at
//!   most two or three frames behind.

use std::collections::VecDeque;

use mhw_core::frame::{CameraFrame, JitterSample, Size};

/// Frames a [`KeepRing`] holds references for.
pub const KEEP_FRAMES: usize = 4;

/// Cameras kept.
const CAMERAS: usize = 8;

/// How far a camera buffer's `fProj[8|9]` may be from a kept frame's jitter
/// and still be that frame's. Neighbouring phases of the sequence are at
/// least ~1e-5 apart at 4K; the engine's own rounding is ~1e-10.
const PROJECTION_TOLERANCE: f32 = 1e-6;

/// The last few cameras with their jitter, by frame.
#[derive(Clone, Debug, Default)]
pub struct CameraRing {
    entries: VecDeque<(CameraFrame, JitterSample)>,
}

impl CameraRing {
    /// Empty.
    pub const fn new() -> CameraRing {
        CameraRing {
            entries: VecDeque::new(),
        }
    }

    /// Keep `camera` (replacing an entry of the same frame).
    pub fn push(&mut self, camera: CameraFrame, jitter: JitterSample) {
        self.entries.retain(|(c, _)| c.frame != camera.frame);
        self.entries.push_back((camera, jitter));
        while self.entries.len() > CAMERAS {
            self.entries.pop_front();
        }
    }

    /// The camera of `frame`, if kept.
    pub fn get(&self, frame: u64) -> Option<(CameraFrame, JitterSample)> {
        self.entries
            .iter()
            .rev()
            .find(|(c, _)| c.frame == frame)
            .copied()
    }

    /// The newest camera.
    pub fn latest(&self) -> Option<(CameraFrame, JitterSample)> {
        self.entries.back().copied()
    }

    /// The jitter of exactly `frame`: the kept camera's, else `engine_last`
    /// (the engine's sample of the last main viewport, published with a
    /// camera or not) when it is `frame`'s. Never another frame's: an
    /// older Halton phase is the wrong offset for this image.
    pub fn jitter_of(&self, frame: u64, engine_last: Option<JitterSample>) -> Option<JitterSample> {
        self.get(frame)
            .map(|(_, jitter)| jitter)
            .or_else(|| engine_last.filter(|jitter| jitter.frame == frame))
    }

    /// The kept frame whose jitter the engine put into a camera buffer as
    /// `proj` = `(fProj[8], fProj[9])`: it adds `−units / display` per axis
    /// (`probe.jitter`: the divisor is the display). `None` without a match,
    /// or when two kept frames match (no jitter, or a repeated phase).
    pub fn frame_with_projection(&self, proj: [f32; 2], display: Size) -> Option<u64> {
        if display.is_empty() {
            return None;
        }
        let (w, h) = (display.w as f32, display.h as f32);
        let mut matching = self.entries.iter().rev().filter(|(_, jitter)| {
            (proj[0] + jitter.units.x / w).abs() <= PROJECTION_TOLERANCE
                && (proj[1] + jitter.units.y / h).abs() <= PROJECTION_TOLERANCE
        });
        let (camera, _) = matching.next()?;
        matching.next().is_none().then_some(camera.frame)
    }
}

/// The frame recorded after `last` when the engine's count at the call is
/// `numbered`, for a call whose own frame is not known: the render thread
/// records frames in order, at most `lag` behind the count. `None` after a
/// gap (a pause, a load) or before the first known frame.
pub fn frame_after(last: Option<u64>, numbered: u64, lag: u64) -> Option<u64> {
    let next = last?.checked_add(1)?;
    (next <= numbered && numbered - next <= lag).then_some(next)
}

/// Supersampling calls kept for upscale points.
const SS_PENDING: usize = 4;

/// How far the engine's count at an upscale point may be ahead of the
/// count at its frame's supersampling call: one when the main thread
/// counted in between, one more when the next frame's call was recorded
/// first.
pub const UPSCALE_LAG: u64 = 2;

/// Pairs upscale points (EvalISP) with supersampling calls (EvalSS), one
/// each per frame and executed in that order on the GPU (the upscale
/// point reads the dense velocity the call's pass writes). The render
/// thread does not always record them in that order (seen in game: "no
/// temporal pass and no supersampling call" a few dozen times in ten
/// seconds at Performance):
///
/// * the next frame's call before this frame's upscale point: the oldest
///   pending call is this point's;
/// * the upscale point before its own call: the latest call's textures
///   (the engine reuses them every frame and the GPU fills them first),
///   numbered as the frame after the last one; the late call is dropped.
///
/// Calls counted more than [`UPSCALE_LAG`] before the point are stale
/// (frames without an upscale point: menus, DLAA) and are dropped. A
/// second call is pending at most once in a row when the order is only
/// swapped; pending at two points in a row means the pairing is a frame
/// behind, and the oldest call is dropped (seen in game: two frames
/// behind, with the jitter of the wrong frame, after starting at
/// Performance).
#[derive(Debug)]
pub struct SsPairing<T> {
    /// Calls not paired yet, oldest first: engine count, rendered frame.
    pending: VecDeque<(u64, Option<u64>, T)>,
    latest: Option<T>,
    last: Option<u64>,
    /// The previous upscale point found more than one call pending.
    backlog: bool,
}

impl<T: Copy> Default for SsPairing<T> {
    fn default() -> SsPairing<T> {
        SsPairing::new()
    }
}

impl<T: Copy> SsPairing<T> {
    /// Nothing seen.
    pub const fn new() -> SsPairing<T> {
        SsPairing {
            pending: VecDeque::new(),
            latest: None,
            last: None,
            backlog: false,
        }
    }

    /// A supersampling call at engine count `numbered`, rendered for
    /// `rendered` when known.
    pub fn call(&mut self, numbered: u64, rendered: Option<u64>, item: T) {
        if self.pending.len() == SS_PENDING {
            self.pending.pop_front();
        }
        self.pending.push_back((numbered, rendered, item));
        self.latest = Some(item);
    }

    /// The latest call.
    pub fn latest(&self) -> Option<T> {
        self.latest
    }

    /// The call for the upscale point at engine count `numbered` and the
    /// frame it evaluates (`None`: unknown, the caller numbers it).
    pub fn upscale(&mut self, numbered: u64) -> Option<(T, Option<u64>)> {
        let last = self.last;
        self.pending.retain(|&(count, rendered, _)| {
            let recent = numbered.saturating_sub(count) <= UPSCALE_LAG;
            let unseen = match (rendered, last) {
                (Some(rendered), Some(last)) => rendered > last,
                _ => true,
            };
            recent && unseen
        });
        let backlog = self.pending.len() > 1;
        if backlog && self.backlog {
            self.pending.pop_front();
        }
        self.backlog = backlog;
        let (item, frame) = match self.pending.pop_front() {
            Some((_, rendered, item)) => (item, rendered),
            None => (
                self.latest?,
                Some(frame_after(last, numbered, UPSCALE_LAG)?),
            ),
        };
        self.last = frame;
        Some((item, frame))
    }
}

/// Items kept alive for [`KEEP_FRAMES`] distinct frames.
#[derive(Debug)]
pub struct KeepRing<T> {
    frames: VecDeque<(u64, Vec<T>)>,
}

impl<T> Default for KeepRing<T> {
    fn default() -> KeepRing<T> {
        KeepRing::new()
    }
}

impl<T> KeepRing<T> {
    /// Holding nothing.
    pub const fn new() -> KeepRing<T> {
        KeepRing {
            frames: VecDeque::new(),
        }
    }

    /// Hold `items` for `frame`; frames older than the last
    /// [`KEEP_FRAMES`] distinct ones are dropped.
    pub fn hold(&mut self, frame: u64, items: impl IntoIterator<Item = T>) {
        match self.frames.back_mut() {
            Some((last, held)) if *last == frame => held.extend(items),
            _ => self.frames.push_back((frame, items.into_iter().collect())),
        }
        while self.frames.len() > KEEP_FRAMES {
            self.frames.pop_front();
        }
    }

    /// Items held now.
    pub fn len(&self) -> usize {
        self.frames.iter().map(|(_, items)| items.len()).sum()
    }

    /// Whether nothing is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mhw_core::math::{Mat4, Vec2};
    use std::rc::Rc;

    #[test]
    fn upscale_points_pair_with_their_own_frame_in_order() {
        let mut p = SsPairing::new();
        // In order: each point takes its frame's call.
        p.call(11, Some(10), 'a');
        assert_eq!(p.upscale(11), Some(('a', Some(10))));
        p.call(12, Some(11), 'b');
        assert_eq!(p.upscale(12), Some(('b', Some(11))));
        // The next frame's call is recorded first: still frame 12, then 13.
        p.call(13, Some(12), 'c');
        p.call(14, Some(13), 'd');
        assert_eq!(p.upscale(14), Some(('c', Some(12))));
        assert_eq!(p.upscale(14), Some(('d', Some(13))));
        // A point before its own call: the latest textures, frame 14; the
        // late call is then dropped, not used for frame 15.
        assert_eq!(p.upscale(15), Some(('d', Some(14))));
        p.call(15, Some(14), 'e');
        p.call(16, Some(15), 'f');
        assert_eq!(p.upscale(16), Some(('f', Some(15))));
    }

    #[test]
    fn upscale_points_resync_after_a_gap() {
        let mut p = SsPairing::new();
        // Nothing known yet.
        assert_eq!(p.upscale(5), None);
        p.call(11, Some(10), 'a');
        assert_eq!(p.upscale(11), Some(('a', Some(10))));
        // A load: the count jumps; the call of the new frame is taken.
        p.call(41, Some(40), 'b');
        assert_eq!(p.upscale(41), Some(('b', Some(40))));
        // Out of step with no call close by: nothing.
        assert_eq!(p.upscale(90), None);
    }

    #[test]
    fn upscale_points_do_not_fall_behind() {
        let mut p = SsPairing::new();
        // Calls without upscale points (a menu, DLAA): stale by the count.
        for n in 1..=4 {
            p.call(n, Some(n - 1), 'x');
        }
        p.call(20, Some(19), 'a');
        assert_eq!(p.upscale(21), Some(('a', Some(19))));
        // A point that never came leaves a call behind; two points in a
        // row with a second call pending catch up.
        p.call(21, Some(20), 'b');
        p.call(22, Some(21), 'c');
        assert_eq!(p.upscale(22), Some(('b', Some(20))));
        p.call(23, Some(22), 'd');
        assert_eq!(p.upscale(23), Some(('d', Some(22))));
        p.call(24, Some(23), 'e');
        assert_eq!(p.upscale(24), Some(('e', Some(23))));
    }

    #[test]
    fn the_frame_after_continues_only_a_consecutive_run() {
        // One behind the count (render thread lagging) or level with it.
        assert_eq!(frame_after(Some(951), 953, 1), Some(952));
        assert_eq!(frame_after(Some(951), 952, 1), Some(952));
        // Further behind only within the lag allowed.
        assert_eq!(frame_after(Some(951), 954, 1), None);
        assert_eq!(frame_after(Some(951), 954, 2), Some(952));
        // Ahead of the count: that frame is not recorded yet.
        assert_eq!(frame_after(Some(951), 951, 1), None);
        // A gap: the last frame says nothing about this call.
        assert_eq!(frame_after(Some(951), 960, 2), None);
        assert_eq!(frame_after(None, 953, 1), None);
        assert_eq!(frame_after(Some(u64::MAX), 953, 1), None);
    }

    fn camera(frame: u64) -> (CameraFrame, JitterSample) {
        let proj = Mat4::from_rows([
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.7, 0.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
            [0.0, 0.0, 0.1, 0.0],
        ]);
        let c = CameraFrame::new(Mat4::IDENTITY, proj, Mat4::IDENTITY, proj, frame)
            .expect("a perspective camera");
        let j = JitterSample {
            px: Vec2::new(frame as f32, 0.0),
            units: Vec2::ZERO,
            phase: 0,
            frame,
        };
        (c, j)
    }

    #[test]
    fn cameras_are_found_by_frame_and_old_ones_drop_out() {
        let mut ring = CameraRing::default();
        assert!(ring.latest().is_none());
        for frame in 1..=20 {
            let (c, j) = camera(frame);
            ring.push(c, j);
        }
        assert_eq!(ring.latest().map(|(c, _)| c.frame), Some(20));
        assert_eq!(ring.get(15).map(|(_, j)| j.px.x), Some(15.0));
        assert!(ring.get(12).is_none(), "beyond the ring");
        let (c, mut j) = camera(20);
        j.px.x = -1.0;
        ring.push(c, j);
        assert_eq!(ring.get(20).map(|(_, j)| j.px.x), Some(-1.0), "replaced");
    }

    #[test]
    fn a_frame_gets_its_own_jitter_or_none() {
        let mut ring = CameraRing::default();
        let (c, j) = camera(10);
        ring.push(c, j);
        let engine = |frame| {
            Some(camera(frame).1).map(|mut j| {
                j.px.y = 99.0;
                j
            })
        };
        assert_eq!(ring.jitter_of(10, engine(10)), Some(j), "the ring first");
        // Frame 11's camera was not usable: the engine's sample of 11.
        assert_eq!(
            ring.jitter_of(11, engine(11)).map(|j| (j.frame, j.px.y)),
            Some((11, 99.0))
        );
        // Nothing of frame 12 anywhere: not the newest camera's (10).
        assert_eq!(ring.jitter_of(12, engine(11)), None);
        assert_eq!(ring.jitter_of(12, None), None);
    }

    #[test]
    fn a_camera_buffer_names_the_frame_its_jitter_belongs_to() {
        let display = Size::new(3840, 2160);
        let mut ring = CameraRing::default();
        for (frame, units) in [(10, (0.5, -1.0)), (11, (-0.25, 0.75)), (12, (1.0, 0.25))] {
            let (c, mut j) = camera(frame);
            j.units = Vec2::new(units.0, units.1);
            ring.push(c, j);
        }
        let proj = |x: f32, y: f32| [-x / 3840.0, -y / 2160.0];
        assert_eq!(
            ring.frame_with_projection(proj(-0.25, 0.75), display),
            Some(11)
        );
        assert_eq!(
            ring.frame_with_projection(proj(1.0, 0.25), display),
            Some(12)
        );
        // Another frame's phase, or nothing kept with it.
        assert_eq!(ring.frame_with_projection(proj(0.5, 0.75), display), None);
        assert_eq!(
            ring.frame_with_projection(proj(0.5, -1.0), Size::new(0, 0)),
            None
        );
        // No jitter: every frame matches, none is named.
        let mut still = CameraRing::default();
        for frame in 1..=3 {
            let (c, j) = camera(frame);
            still.push(c, j);
        }
        assert_eq!(still.frame_with_projection([0.0, 0.0], display), None);
    }

    #[test]
    fn references_are_released_after_keep_frames_distinct_frames() {
        let item = Rc::new(());
        let mut ring = KeepRing::default();
        ring.hold(1, [item.clone()]);
        ring.hold(1, [item.clone()]);
        assert_eq!(Rc::strong_count(&item), 3);
        for frame in 2..=KEEP_FRAMES as u64 {
            ring.hold(frame, []);
        }
        assert_eq!(Rc::strong_count(&item), 3, "frame 1 still among the last");
        ring.hold(KEEP_FRAMES as u64 + 1, []);
        assert_eq!(Rc::strong_count(&item), 1);
        assert!(ring.is_empty());
    }
}
