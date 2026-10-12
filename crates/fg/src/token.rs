//! Frame-token and marker discipline.
//!
//! Streamline keys every per-frame call on a `FrameToken` with an index we
//! choose. The rules this machine enforces, and why:
//!
//! * **The index advances only at the end of `Present`.** The frame routine
//!   can return early without presenting (`0x142298176`), but the frame is
//!   then only delayed: it stays pending and is presented in the next
//!   iteration (docs/re/ReFramePresent.md Q4), so the render index is 1:1
//!   with presents. An index that moved anywhere else would drift from what
//!   `slSetConstants` was told, and `ePresentStart`/`ePresentEnd` must carry
//!   the index the constants did.
//! * **It advances only if constants were filed under it.** A present that
//!   carried no rendered frame (loading screens: thousands per session)
//!   would otherwise leave an index with neither constants nor tags, and the
//!   plugin answers that with "Unable to find 'common' constants for frame
//!   N" and switches interpolation off. Reusing the token is what the plugin
//!   does itself ("using last set for frame"), with a complete set.
//! * **Constants once per index** ("do not set common constants multiple
//!   times in a single frame"); tags may be filed in several calls (the
//!   HUD-less image is captured after the upscale slot).
//! * **Each marker once per index**, in order: `SIMULATION_START`,
//!   `SIMULATION_END` + `RENDERSUBMIT_START` (first graphics-queue submit),
//!   `RENDERSUBMIT_END` + `PRESENT_START` (before Present), `PRESENT_END`.
//!   `PRESENT_START` follows the frame's constants by construction: the
//!   upscale slot runs on the render thread, which signals `done` before
//!   the main thread submits and presents (RE §2.3, CONFIRMED).
//! * **Reflex sleep once per present cycle**, even when the token is
//!   reused: `slReflexSleep` must be called every frame.
//!
//! Pure bookkeeping; the Windows glue makes the calls the returned plans
//! name, after the lock is released.

/// Counters over the session, for introspection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timeline {
    /// Index of the current token (0: none yet).
    pub index: u32,
    /// Index the last constants were filed under.
    pub constants_index: u32,
    /// Index the last tags were filed under.
    pub tags_index: u32,
    /// Tokens acquired.
    pub tokens: u64,
    /// Presents that kept the token because nothing was filed under it.
    pub reused: u64,
    /// Markers set, by [`Marker`](crate::abi::Marker) value.
    pub markers: [u64; 6],
    /// Reflex sleeps.
    pub sleeps: u64,
    /// Constants filed.
    pub constants: u64,
    /// Constants refused because the index already had them.
    pub duplicate_constants: u64,
    /// Tag calls filed.
    pub tags: u64,
    /// Presents seen.
    pub presents: u64,
}

/// What to do at the start of a main-loop iteration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameStart {
    /// Call `slReflexSleep`.
    pub sleep: bool,
    /// Set `SIMULATION_START`.
    pub simulation_start: bool,
}

/// What to do at the engine's first graphics-queue submit of a frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderSubmit {
    /// Set `SIMULATION_END`.
    pub simulation_end: bool,
    /// Set `RENDERSUBMIT_START`.
    pub render_submit_start: bool,
}

/// What to do just before `Present`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PresentStart {
    /// Set `RENDERSUBMIT_END`.
    pub render_submit_end: bool,
    /// Set `PRESENT_START`.
    pub present_start: bool,
}

/// What to do just after `Present` returned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PresentEnd {
    /// Set `PRESENT_END`.
    pub present_end: bool,
    /// Acquire a token with this index, then report it with
    /// [`FrameLoop::acquired`].
    pub advance_to: Option<u32>,
}

/// Why constants were not filed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// No token has been acquired yet.
    NoToken,
    /// This index already has constants.
    Duplicate(u32),
}

/// The token state of the one viewport this game renders.
#[derive(Clone, Debug, Default)]
pub struct FrameLoop {
    timeline: Timeline,
    /// Markers already set under the current index.
    sent: [bool; 6],
    constants_filed: bool,
    slept: bool,
}

/// The index after `index`, skipping 0 (which means "no token").
pub const fn next_index(index: u32) -> u32 {
    match index.checked_add(1) {
        Some(next) => next,
        None => 1,
    }
}

impl FrameLoop {
    /// No token yet.
    pub fn new() -> FrameLoop {
        FrameLoop::default()
    }

    /// The counters.
    pub fn timeline(&self) -> Timeline {
        self.timeline
    }

    /// The current index, 0 before the first token.
    pub fn index(&self) -> u32 {
        self.timeline.index
    }

    /// The index to acquire the first token with, if there is none yet.
    pub fn first_token(&self) -> Option<u32> {
        (self.timeline.index == 0).then_some(1)
    }

    /// A token with `index` was acquired: it is now current and nothing is
    /// filed under it.
    pub fn acquired(&mut self, index: u32) {
        self.timeline.index = index;
        self.timeline.tokens += 1;
        self.sent = [false; 6];
        self.constants_filed = false;
    }

    fn mark(&mut self, marker: crate::abi::Marker) -> bool {
        let slot = marker as usize;
        if self.timeline.index == 0 || self.sent[slot] {
            return false;
        }
        self.sent[slot] = true;
        self.timeline.markers[slot] += 1;
        true
    }

    fn was_sent(&self, marker: crate::abi::Marker) -> bool {
        self.sent[marker as usize]
    }

    /// The main loop starts an iteration (the sleep point).
    pub fn frame_start(&mut self) -> FrameStart {
        if self.timeline.index == 0 {
            return FrameStart::default();
        }
        let sleep = !self.slept;
        if sleep {
            self.slept = true;
            self.timeline.sleeps += 1;
        }
        FrameStart {
            sleep,
            simulation_start: self.mark(crate::abi::Marker::SimulationStart),
        }
    }

    /// The engine submitted to its graphics queue.
    ///
    /// The first submit of a frame is the per-frame submit right before
    /// Present, except in the iteration of a resolution/DLSS mode change,
    /// where the DLSS feature recreation (`0x1423ac6c8`) submits first, at
    /// the start of the iteration. That frame's simulation/render split is
    /// then measured early; the markers do not optimise anything
    /// (`useMarkersToOptimize = false`), so it is accepted.
    pub fn render_submit(&mut self) -> RenderSubmit {
        RenderSubmit {
            simulation_end: self.mark(crate::abi::Marker::SimulationEnd),
            render_submit_start: self.mark(crate::abi::Marker::RenderSubmitStart),
        }
    }

    /// Constants are about to be filed: the index to file them under, or
    /// why not. Report success with [`FrameLoop::constants_filed`]; a call
    /// Streamline refused leaves the index free.
    pub fn constants(&mut self) -> Result<u32, Refused> {
        if self.timeline.index == 0 {
            return Err(Refused::NoToken);
        }
        if self.constants_filed {
            self.timeline.duplicate_constants += 1;
            return Err(Refused::Duplicate(self.timeline.index));
        }
        Ok(self.timeline.index)
    }

    /// Streamline accepted constants under `index`. Ignored unless `index`
    /// is current.
    pub fn constants_filed(&mut self, index: u32) {
        if index == 0 || index != self.timeline.index || self.constants_filed {
            return;
        }
        self.constants_filed = true;
        self.timeline.constants += 1;
        self.timeline.constants_index = index;
    }

    /// Whether constants were filed under the current index.
    pub fn has_constants(&self) -> bool {
        self.constants_filed
    }

    /// Tags are about to be filed: the index, if there is a token.
    pub fn tags(&mut self) -> Option<u32> {
        if self.timeline.index == 0 {
            return None;
        }
        self.timeline.tags += 1;
        self.timeline.tags_index = self.timeline.index;
        Some(self.timeline.index)
    }

    /// Just before `Present`.
    pub fn present_start(&mut self) -> PresentStart {
        self.timeline.presents += 1;
        let render_submit_end = self.was_sent(crate::abi::Marker::RenderSubmitStart)
            && self.mark(crate::abi::Marker::RenderSubmitEnd);
        PresentStart {
            render_submit_end,
            present_start: self.mark(crate::abi::Marker::PresentStart),
        }
    }

    /// Just after `Present` returned.
    pub fn present_end(&mut self) -> PresentEnd {
        let present_end = self.was_sent(crate::abi::Marker::PresentStart)
            && self.mark(crate::abi::Marker::PresentEnd);
        self.slept = false;
        if self.timeline.index == 0 {
            return PresentEnd {
                present_end,
                advance_to: Some(1),
            };
        }
        let advance_to = if self.constants_filed {
            Some(next_index(self.timeline.index))
        } else {
            self.timeline.reused += 1;
            None
        };
        PresentEnd {
            present_end,
            advance_to,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::Marker;

    fn running() -> FrameLoop {
        let mut machine = FrameLoop::new();
        let first = machine.first_token().unwrap();
        machine.acquired(first);
        machine
    }

    /// Check, then report Streamline's acceptance, as the glue does.
    fn file(machine: &mut FrameLoop) -> Result<u32, Refused> {
        let index = machine.constants()?;
        machine.constants_filed(index);
        Ok(index)
    }

    #[test]
    fn nothing_happens_before_the_first_token() {
        let mut machine = FrameLoop::new();
        assert_eq!(machine.first_token(), Some(1));
        assert_eq!(machine.frame_start(), FrameStart::default());
        assert_eq!(machine.render_submit(), RenderSubmit::default());
        assert_eq!(machine.constants(), Err(Refused::NoToken));
        assert_eq!(machine.tags(), None);
        assert_eq!(machine.present_start(), PresentStart::default());
        assert_eq!(machine.timeline().markers, [0; 6]);
    }

    #[test]
    fn a_rendered_frame_runs_the_markers_in_order_and_advances() {
        let mut machine = running();
        assert_eq!(
            machine.frame_start(),
            FrameStart {
                sleep: true,
                simulation_start: true
            }
        );
        assert_eq!(
            machine.render_submit(),
            RenderSubmit {
                simulation_end: true,
                render_submit_start: true
            }
        );
        assert_eq!(file(&mut machine), Ok(1));
        assert_eq!(machine.tags(), Some(1));
        assert_eq!(
            machine.present_start(),
            PresentStart {
                render_submit_end: true,
                present_start: true
            }
        );
        assert_eq!(
            machine.present_end(),
            PresentEnd {
                present_end: true,
                advance_to: Some(2)
            }
        );
        machine.acquired(2);
        assert_eq!(machine.index(), 2);
        assert!(!machine.has_constants());
        assert_eq!(file(&mut machine), Ok(2));
        let t = machine.timeline();
        assert_eq!(t.markers, [1; 6]);
        assert_eq!((t.tokens, t.constants, t.constants_index), (2, 2, 2));
    }

    #[test]
    fn constants_are_filed_once_per_index() {
        let mut machine = running();
        assert_eq!(file(&mut machine), Ok(1));
        assert_eq!(machine.constants(), Err(Refused::Duplicate(1)));
        assert_eq!(machine.timeline().duplicate_constants, 1);
        // Tags may come in several calls.
        assert_eq!(machine.tags(), Some(1));
        assert_eq!(machine.tags(), Some(1));
    }

    #[test]
    fn constants_streamline_refused_leave_the_index_free() {
        let mut machine = running();
        assert_eq!(machine.constants(), Ok(1));
        // Not reported as filed: the call failed.
        assert_eq!(machine.constants(), Ok(1));
        machine.constants_filed(7);
        assert!(!machine.has_constants());
        assert_eq!(machine.present_end().advance_to, None);
    }

    #[test]
    fn a_present_without_constants_keeps_the_token() {
        let mut machine = running();
        machine.frame_start();
        machine.present_start();
        let end = machine.present_end();
        assert_eq!(end.advance_to, None);
        assert!(end.present_end);
        assert_eq!(machine.timeline().reused, 1);
        // Next present cycle on the same index: Reflex sleeps again, the
        // markers of that index are not repeated.
        assert_eq!(
            machine.frame_start(),
            FrameStart {
                sleep: true,
                simulation_start: false
            }
        );
        assert_eq!(machine.present_start(), PresentStart::default());
        assert!(!machine.present_end().present_end);
        // Then a rendered frame lands on the kept index.
        assert_eq!(file(&mut machine), Ok(1));
        assert_eq!(machine.present_end().advance_to, Some(2));
    }

    #[test]
    fn the_sleep_point_sleeps_once_per_present_cycle() {
        let mut machine = running();
        assert!(machine.frame_start().sleep);
        // A frame-skip iteration (no present in between) does not sleep twice.
        assert!(!machine.frame_start().sleep);
        machine.present_start();
        machine.present_end();
        assert!(machine.frame_start().sleep);
        assert_eq!(machine.timeline().sleeps, 2);
    }

    #[test]
    fn render_submit_end_needs_its_start() {
        let mut machine = running();
        let start = machine.present_start();
        assert!(!start.render_submit_end);
        assert!(start.present_start);
        assert_eq!(
            machine.timeline().markers[Marker::RenderSubmitEnd as usize],
            0
        );
        // Only the first submit of a frame counts.
        machine.acquired(2);
        assert!(machine.render_submit().render_submit_start);
        assert_eq!(machine.render_submit(), RenderSubmit::default());
    }

    #[test]
    fn indices_never_wrap_to_zero() {
        assert_eq!(next_index(1), 2);
        assert_eq!(next_index(u32::MAX), 1);
        let mut machine = FrameLoop::new();
        machine.acquired(u32::MAX);
        file(&mut machine).unwrap();
        assert_eq!(machine.present_end().advance_to, Some(1));
    }

    #[test]
    fn a_failed_acquisition_keeps_the_index() {
        let mut machine = running();
        file(&mut machine).unwrap();
        assert_eq!(machine.present_end().advance_to, Some(2));
        // The glue could not get token 2: nothing is reported, index 1 stays,
        // and its constants are already there.
        assert_eq!(machine.index(), 1);
        assert_eq!(machine.constants(), Err(Refused::Duplicate(1)));
        assert_eq!(machine.present_end().advance_to, Some(2));
    }
}
