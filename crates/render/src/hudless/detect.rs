//! Finding the moment just before the first GUI draw.
//!
//! There is no UI-free image in this engine, only a moment
//! (`ReFramePresent` 5a-5c): `uGUI::draw` binds no target of its own and
//! draws into the scene target `primaryScene+0x4a0`, which the GUI viewport
//! binds again (`0x1423c3790`); the encode `System_CopyToBackBuffer` reads
//! that same target after the GUI, sMovie and the barriers. Its content just
//! before the first draw of that binding is post-tonemap, pre-encode and
//! without HUD.
//!
//! The old mod picked "the N-th binding of a window-sized target" with N
//! surveyed by hand (2), latched to the first resource that matched. That
//! picks the wrong binding whenever the frame's structure differs (loading
//! screens bind six targets before settling to three), and its mistakes are
//! silent. This detector **learns from each frame's own encode** and
//! **validates every capture after the fact**:
//!
//! 1. *Observe.* At the encode's draw (`System_CopyToBackBuffer*` pipeline on
//!    a list), the backbuffer is bound on that list. The scene target `S` is
//!    the most recent other target bound on that list before it that is
//!    window-sized and colour (`setRenderTarget(primaryScene+0x4a0, …)` runs
//!    right before the encode, `0x14229a3fe`; with `setRenderTarget`'s
//!    de-duplication `S` may simply still be bound from the GUI viewport).
//!    The GUI binding is the last binding of `S` this frame, before the
//!    backbuffer's, that received draws. The observation is `(S, N)` with
//!    `N` its occurrence (how many times `S` had been bound this frame,
//!    counted over all lists; `setRenderTarget` de-duplicates, so counts are
//!    counts of real changes and stable).
//! 2. *Learn.* `(S, N)` becomes the expectation once observed on
//!    [`STABLE_FRAMES`] consecutive frames; any other observation restarts
//!    the count.
//! 3. *Capture.* With a stable expectation, the first draw into binding `N`
//!    of `S` triggers the copy (on the list, before that draw is recorded).
//! 4. *Validate.* At the same frame's encode the capture is confirmed only if
//!    that frame's own observation equals what was captured. An unconfirmed
//!    capture is never handed out: a wrong HUD-less image is worse than none
//!    (frame generation masks the HUD by comparing it to the backbuffer).
//!
//! The frame is opened by `begin_frame` (the upscale slot: everything the
//! GUI does is later) and closed at `Present`.

use mhw_core::frame::Size;

/// Consecutive identical observations before captures start.
pub const STABLE_FRAMES: u32 = 3;
/// Bindings kept per frame (the steady state has a handful; a loading
/// screen a few more). Further bindings are ignored for the frame.
pub const MAX_BINDINGS: usize = 256;

/// Colour formats a scene target may have: `R11G11B10_FLOAT` (observed),
/// `R16G16B16A16_FLOAT`, `R32G32B32A32_FLOAT`, `R10G10B10A2_UNORM`,
/// `R8G8B8A8_UNORM`, `B8G8R8A8_UNORM`. Masks (`R8_UNORM`, which the loading
/// screen binds window-sized) and depth are not.
pub const SCENE_FORMATS: [u32; 6] = [26, 10, 2, 24, 28, 87];

/// The first render target a list bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bound {
    /// `ID3D12Resource*` (0: not resolvable).
    pub resource: usize,
    /// Liveness serial when bound (`None`: not watched).
    pub serial: Option<u64>,
    /// Size (empty when not described: swapchain buffers).
    pub size: Size,
    /// `DXGI_FORMAT` (0 when not described).
    pub format: u32,
}

/// The copy to make now, before the draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capture {
    /// The frame (as given to `begin_frame`).
    pub frame: u64,
    /// The scene target and what it was when bound.
    pub target: Bound,
    /// Its occurrence this frame.
    pub occurrence: u32,
}

/// What the detector has learned, for status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Learned {
    /// The scene target.
    pub scene: usize,
    /// The GUI binding's occurrence.
    pub occurrence: u32,
    /// Consecutive frames observed so.
    pub streak: u32,
}

#[derive(Debug, Clone, Copy)]
struct Binding {
    list: usize,
    target: Bound,
    occurrence: u32,
    draws: u32,
}

/// The detector. Not synchronised: the Windows side holds it in a mutex.
#[derive(Debug, Default)]
pub struct Detector {
    display: Size,
    frame: Option<u64>,
    bindings: Vec<Binding>,
    /// List -> index of its current binding (`None`: nothing bound).
    current: Vec<(usize, Option<usize>)>,
    learned: Option<Learned>,
    captured: Option<Capture>,
    encoded: bool,
    abandoned: bool,
    confirmed: Option<u64>,
    /// Captures refused at validation.
    pub refuted: u64,
    /// Captures confirmed.
    pub confirmations: u64,
}

impl Detector {
    /// An empty detector (no display size yet).
    pub const fn new() -> Detector {
        Detector {
            display: Size::new(0, 0),
            frame: None,
            bindings: Vec::new(),
            current: Vec::new(),
            learned: None,
            captured: None,
            encoded: false,
            abandoned: false,
            confirmed: None,
            refuted: 0,
            confirmations: 0,
        }
    }

    /// The window (display) size scene targets must have.
    pub fn set_display(&mut self, display: Size) {
        self.display = display;
    }

    /// The upscale slot of `frame`: candidates from here to `Present`. A
    /// repeated call for the open frame is ignored.
    pub fn begin_frame(&mut self, frame: u64) {
        if self.frame == Some(frame) {
            return;
        }
        self.frame = Some(frame);
        self.bindings.clear();
        self.current.clear();
        self.captured = None;
        self.encoded = false;
        self.abandoned = false;
    }

    /// `Present`: the frame is closed.
    pub fn end_frame(&mut self) {
        self.frame = None;
        self.bindings.clear();
        self.current.clear();
    }

    /// What is learned so far.
    pub fn learned(&self) -> Option<Learned> {
        self.learned
    }

    /// Whether `frame`'s capture was confirmed by its encode.
    pub fn confirmed(&self, frame: u64) -> bool {
        self.confirmed == Some(frame)
    }

    fn set_current(&mut self, list: usize, index: Option<usize>) {
        match self.current.iter_mut().find(|(l, _)| *l == list) {
            Some(entry) => entry.1 = index,
            None => self.current.push((list, index)),
        }
    }

    fn current_of(&self, list: usize) -> Option<usize> {
        self.current
            .iter()
            .find(|(l, _)| *l == list)
            .and_then(|(_, index)| *index)
    }

    /// `list` bound `target` as its first render target (`None`: none).
    pub fn bind(&mut self, list: usize, target: Option<Bound>) {
        if self.frame.is_none() {
            return;
        }
        let Some(target) = target.filter(|t| t.resource != 0) else {
            self.set_current(list, None);
            return;
        };
        if self.bindings.len() >= MAX_BINDINGS {
            self.set_current(list, None);
            return;
        }
        let occurrence = self
            .bindings
            .iter()
            .filter(|b| b.target.resource == target.resource)
            .count() as u32
            + 1;
        self.bindings.push(Binding {
            list,
            target,
            occurrence,
            draws: 0,
        });
        self.set_current(list, Some(self.bindings.len() - 1));
    }

    fn scene_like(&self, target: &Bound) -> bool {
        !self.display.is_empty()
            && target.size == self.display
            && SCENE_FORMATS.contains(&target.format)
    }

    /// A draw on `list`. Returns the capture to make before it, if this is
    /// the moment.
    pub fn draw(&mut self, list: usize) -> Option<Capture> {
        let frame = self.frame?;
        let index = self.current_of(list)?;
        let learned = self.learned;
        let binding = self.bindings.get_mut(index)?;
        binding.draws = binding.draws.saturating_add(1);
        let first = binding.draws == 1;
        let binding = *binding;
        let learned = learned?;
        if !first
            || self.captured.is_some()
            || self.encoded
            || learned.streak < STABLE_FRAMES
            || binding.target.resource != learned.scene
            || binding.occurrence != learned.occurrence
            || !self.scene_like(&binding.target)
        {
            return None;
        }
        let capture = Capture {
            frame,
            target: binding.target,
            occurrence: binding.occurrence,
        };
        self.captured = Some(capture);
        Some(capture)
    }

    /// The capture [`Detector::draw`] asked for could not be made: the frame
    /// is not confirmed, and no other binding is captured instead.
    pub fn abandon(&mut self) {
        self.abandoned = true;
    }

    /// A draw of the output encode on `list`: learn from this frame and
    /// validate its capture. Only the frame's first encode counts (later ones
    /// are secondary displays).
    pub fn encode(&mut self, list: usize) {
        let Some(frame) = self.frame else {
            return;
        };
        if self.encoded {
            return;
        }
        self.encoded = true;
        let observation = self.observe(list);
        match (observation, &mut self.learned) {
            (Some((scene, occurrence)), Some(learned))
                if learned.scene == scene && learned.occurrence == occurrence =>
            {
                learned.streak = learned.streak.saturating_add(1);
            }
            (Some((scene, occurrence)), learned) => {
                *learned = Some(Learned {
                    scene,
                    occurrence,
                    streak: 1,
                });
            }
            (None, _) => {}
        }
        if let Some(capture) = self.captured.filter(|_| !self.abandoned) {
            if observation == Some((capture.target.resource, capture.occurrence)) {
                self.confirmed = Some(frame);
                self.confirmations += 1;
            } else {
                self.refuted += 1;
            }
        }
    }

    /// `(S, N)` for this frame, at the encode on `list`.
    fn observe(&self, list: usize) -> Option<(usize, u32)> {
        let backbuffer_at = self.current_of(list)?;
        let backbuffer = self.bindings.get(backbuffer_at)?.target.resource;
        let before = self.bindings.get(..backbuffer_at)?;
        let scene = before
            .iter()
            .rev()
            .find(|b| {
                b.list == list && b.target.resource != backbuffer && self.scene_like(&b.target)
            })?
            .target
            .resource;
        before
            .iter()
            .rev()
            .find(|b| b.target.resource == scene && b.draws > 0)
            .map(|b| (scene, b.occurrence))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISPLAY: Size = Size::new(3840, 2160);
    const SCENE: usize = 0x5ce0;
    const MASK: usize = 0x3a5c;
    const BACK: usize = 0xbac0;
    const LIST: usize = 0x1157;

    fn target(resource: usize, format: u32) -> Option<Bound> {
        Some(Bound {
            resource,
            serial: Some(1),
            size: if resource == BACK {
                Size::default()
            } else {
                DISPLAY
            },
            format: if resource == BACK { 0 } else { format },
        })
    }

    /// The steady state surveyed in game: scene (post), mask, scene (GUI),
    /// backbuffer + encode. Returns captures made.
    fn frame(d: &mut Detector, number: u64, gui_draws: u32) -> Vec<Capture> {
        let mut captures = Vec::new();
        d.begin_frame(number);
        d.bind(LIST, target(SCENE, 26));
        for _ in 0..12 {
            captures.extend(d.draw(LIST));
        }
        d.bind(LIST, target(MASK, 61));
        captures.extend(d.draw(LIST));
        d.bind(LIST, target(SCENE, 26));
        for _ in 0..gui_draws {
            captures.extend(d.draw(LIST));
        }
        d.bind(LIST, target(BACK, 0));
        d.encode(LIST);
        captures.extend(d.draw(LIST));
        d.end_frame();
        captures
    }

    fn detector() -> Detector {
        let mut d = Detector::default();
        d.set_display(DISPLAY);
        d
    }

    #[test]
    fn learns_the_gui_binding_then_captures_and_confirms() {
        let mut d = detector();
        for number in 1..=STABLE_FRAMES as u64 {
            assert!(
                frame(&mut d, number, 20).is_empty(),
                "learning, frame {number}"
            );
        }
        assert_eq!(
            d.learned(),
            Some(Learned {
                scene: SCENE,
                occurrence: 2,
                streak: STABLE_FRAMES
            })
        );
        let captures = frame(&mut d, 10, 20);
        assert_eq!(captures.len(), 1, "once, at the first GUI draw");
        assert_eq!(
            (captures[0].target.resource, captures[0].occurrence),
            (SCENE, 2)
        );
        assert_eq!(captures[0].frame, 10);
        assert!(d.confirmed(10));
        assert!(!d.confirmed(9));
    }

    #[test]
    fn a_frame_shaped_differently_is_refuted_and_relearned() {
        let mut d = detector();
        for number in 1..=4 {
            frame(&mut d, number, 20);
        }
        // A loading-screen-like frame: the scene is bound a third time for
        // the GUI. The capture at binding 2 happens, but the encode shows the
        // GUI went into binding 3: refuted, never confirmed.
        d.begin_frame(5);
        d.bind(LIST, target(SCENE, 26));
        d.draw(LIST);
        d.bind(LIST, target(MASK, 61));
        d.draw(LIST);
        d.bind(LIST, target(SCENE, 26));
        assert!(d.draw(LIST).is_some());
        d.bind(LIST, target(MASK, 61));
        d.draw(LIST);
        d.bind(LIST, target(SCENE, 26));
        d.draw(LIST);
        d.bind(LIST, target(BACK, 0));
        d.encode(LIST);
        d.end_frame();
        assert!(!d.confirmed(5));
        assert_eq!(d.refuted, 1);
        assert_eq!(d.learned().map(|l| (l.occurrence, l.streak)), Some((3, 1)));
        // Back to the steady state: no captures until it is stable again.
        assert!(frame(&mut d, 6, 20).is_empty());
    }

    #[test]
    fn masks_and_wrong_sizes_are_never_the_scene() {
        let mut d = detector();
        for number in 1..=5 {
            d.begin_frame(number);
            d.bind(LIST, target(MASK, 61));
            d.draw(LIST);
            d.bind(LIST, target(BACK, 0));
            d.encode(LIST);
            d.end_frame();
        }
        assert_eq!(d.learned(), None);
        let mut small = detector();
        small.set_display(Size::new(1920, 1080));
        for number in 1..=5 {
            frame(&mut small, number, 5);
        }
        assert_eq!(small.learned(), None);
    }

    #[test]
    fn nothing_outside_a_frame_and_other_lists_do_not_count() {
        let mut d = detector();
        d.bind(LIST, target(SCENE, 26));
        assert_eq!(d.draw(LIST), None);
        for number in 1..=STABLE_FRAMES as u64 {
            frame(&mut d, number, 3);
        }
        d.begin_frame(9);
        d.bind(LIST, target(SCENE, 26));
        d.draw(LIST);
        d.bind(LIST, target(SCENE, 26));
        // A draw on another list (nothing bound there) is not the GUI's.
        assert_eq!(d.draw(LIST + 1), None);
        assert!(d.draw(LIST).is_some());
    }

    #[test]
    fn a_scene_binding_without_draws_is_not_the_gui() {
        // `setRenderTarget(S)` before the encode when the GUI viewport left
        // another target bound: a third binding without draws. The GUI one is
        // still the last that was drawn into.
        let mut d = detector();
        for number in 1..=4 {
            d.begin_frame(number);
            d.bind(LIST, target(SCENE, 26));
            d.draw(LIST);
            d.bind(LIST, target(SCENE, 26));
            d.draw(LIST);
            d.bind(LIST, target(MASK, 61));
            d.bind(LIST, target(SCENE, 26));
            d.bind(LIST, target(BACK, 0));
            d.encode(LIST);
            d.end_frame();
        }
        assert_eq!(d.learned().map(|l| l.occurrence), Some(2));
    }

    #[test]
    fn the_binding_cap_bounds_memory() {
        let mut d = detector();
        d.begin_frame(1);
        for _ in 0..MAX_BINDINGS + 10 {
            d.bind(LIST, target(MASK, 61));
        }
        assert_eq!(d.bindings.len(), MAX_BINDINGS);
        assert_eq!(d.draw(LIST), None, "past the cap nothing is current");
    }

    #[test]
    fn an_abandoned_capture_is_never_confirmed() {
        let mut d = detector();
        for number in 1..=STABLE_FRAMES as u64 {
            frame(&mut d, number, 4);
        }
        d.begin_frame(7);
        d.bind(LIST, target(SCENE, 26));
        d.draw(LIST);
        d.bind(LIST, target(SCENE, 26));
        assert!(d.draw(LIST).is_some());
        d.abandon();
        d.bind(LIST, target(BACK, 0));
        d.encode(LIST);
        assert!(!d.confirmed(7));
        assert_eq!(d.refuted, 0, "a failed copy is not a refutation");
        // The next frame captures normally again.
        assert_eq!(frame(&mut d, 8, 4).len(), 1);
        assert!(d.confirmed(8));
    }
}
