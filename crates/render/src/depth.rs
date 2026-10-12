//! Choosing the scene depth among the depth targets bound this frame.
//!
//! The temporal pass binds a 1×1 dummy for `tDepthMap`, so the scene depth is
//! found where it is rendered: every depth-stencil view bound by
//! `OMSetRenderTargets` is a candidate. Six full-resolution depth buffers
//! exist and shadow maps are bound after the scene, on several threads, so
//! "the last one bound" once picked a 2048×2048 shadow cascade. The rule:
//!
//! * **exact size** of the rectangle being resolved, else exactly the size
//!   of the pass's output (the engine's own resolution scaling renders a
//!   sub-rectangle of full-size targets) — never "large enough", because a
//!   shadow atlas can be larger than the screen;
//! * bound within one frame of the temporal pass (the frame advances on the
//!   dispatching thread while others bind);
//! * among matches, the **most recently bound**, ties by address, so the
//!   choice is deterministic: a depth buffer that alternates between frames
//!   wrecks DLSS's disocclusion test.

use mhw_core::frame::Size;

use crate::addrmap::AddressMap;

/// Frames a binding is remembered for; older ones may have been released.
const KEEP_FRAMES: u64 = 4;

#[derive(Debug, Clone, Copy)]
struct Seen {
    frame: u64,
    order: u64,
    size: Size,
    serial: u64,
}

/// Which rule matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Match {
    /// The size of the rectangle being resolved.
    Render,
    /// The size of the pass's output.
    Output,
}

/// The chosen depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DepthPick {
    /// `ID3D12Resource*`.
    pub resource: usize,
    /// Its liveness serial.
    pub serial: u64,
    /// Its size.
    pub size: Size,
    /// Which rule chose it.
    pub matched: Match,
    /// How many targets matched that rule.
    pub candidates: usize,
}

/// Depth targets recently bound. Not synchronised.
#[derive(Debug, Default)]
pub struct DepthTargets {
    seen: AddressMap<usize, Seen>,
    order: u64,
}

impl DepthTargets {
    /// `resource` (of `size`, liveness `serial`) was bound as the depth
    /// target during `frame`.
    pub fn bound(&mut self, resource: usize, serial: u64, size: Size, frame: u64) {
        if resource == 0 {
            return;
        }
        self.order += 1;
        self.seen.insert(
            resource,
            Seen {
                frame,
                order: self.order,
                size,
                serial,
            },
        );
        self.seen
            .retain(|_, seen| frame.saturating_sub(seen.frame) <= KEEP_FRAMES);
    }

    /// The scene depth for a pass during `frame` resolving `render` into an
    /// output of `output`; `alive(resource, serial)` vets each candidate.
    pub fn pick(
        &self,
        frame: u64,
        render: Size,
        output: Size,
        alive: impl Fn(usize, u64) -> bool,
    ) -> Option<DepthPick> {
        let of_size = |wanted: Size| {
            let mut found: Vec<(usize, Seen)> = self
                .seen
                .iter()
                .filter(|(resource, seen)| {
                    frame.abs_diff(seen.frame) <= 1
                        && seen.size == wanted
                        && alive(**resource, seen.serial)
                })
                .map(|(resource, seen)| (*resource, *seen))
                .collect();
            found.sort_by(|a, b| b.1.order.cmp(&a.1.order).then(a.0.cmp(&b.0)));
            found
        };
        let (matches, matched) = match of_size(render) {
            found if !found.is_empty() => (found, Match::Render),
            _ if output != render && !output.is_empty() => (of_size(output), Match::Output),
            _ => return None,
        };
        let (resource, seen) = *matches.first()?;
        Some(DepthPick {
            resource,
            serial: seen.serial,
            size: seen.size,
            matched,
            candidates: matches.len(),
        })
    }

    /// Targets remembered.
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Whether none is remembered.
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCENE: usize = 0x100;
    const OTHER_SCENE: usize = 0x200;
    const SHADOW: usize = 0x300;
    const RENDER: Size = Size::new(2561, 1440);
    const OUTPUT: Size = Size::new(3840, 2160);

    fn alive(_: usize, _: u64) -> bool {
        true
    }

    #[test]
    fn exact_size_newest_first_never_a_larger_shadow_map() {
        let mut depth = DepthTargets::default();
        depth.bound(SCENE, 1, RENDER, 10);
        depth.bound(OTHER_SCENE, 2, RENDER, 10);
        // Shadow cascades bound afterwards, bigger than the screen.
        depth.bound(SHADOW, 3, Size::new(4096, 4096), 10);
        let pick = depth.pick(10, RENDER, OUTPUT, alive).unwrap();
        assert_eq!(pick.resource, OTHER_SCENE);
        assert_eq!((pick.matched, pick.candidates), (Match::Render, 2));
        // Rebinding the first makes it the newest.
        depth.bound(SCENE, 1, RENDER, 10);
        assert_eq!(
            depth.pick(10, RENDER, OUTPUT, alive).unwrap().resource,
            SCENE
        );
    }

    #[test]
    fn output_size_is_the_fallback_for_sub_rectangles() {
        let mut depth = DepthTargets::default();
        depth.bound(SCENE, 1, OUTPUT, 5);
        let pick = depth.pick(5, RENDER, OUTPUT, alive).unwrap();
        assert_eq!((pick.resource, pick.matched), (SCENE, Match::Output));
        assert_eq!(depth.pick(5, RENDER, RENDER, alive), None);
    }

    #[test]
    fn only_bindings_within_a_frame_and_alive_count() {
        let mut depth = DepthTargets::default();
        depth.bound(SCENE, 1, RENDER, 7);
        assert!(depth.pick(8, RENDER, OUTPUT, alive).is_some());
        assert!(depth.pick(6, RENDER, OUTPUT, alive).is_some());
        assert_eq!(depth.pick(9, RENDER, OUTPUT, alive), None);
        assert_eq!(depth.pick(7, RENDER, OUTPUT, |_, serial| serial != 1), None);
        // Old bindings are forgotten.
        depth.bound(OTHER_SCENE, 2, RENDER, 7 + KEEP_FRAMES + 1);
        assert_eq!(depth.len(), 1);
    }
}
