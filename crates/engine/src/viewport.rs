//! The camera-setup glue: a viewport read in one piece, which viewport is
//! the main one, once-per-frame bookkeeping and the frame's jitter.
//!
//! Pure; the detour reads the bytes and writes the result
//! (`docs/re/ReJitterCamera.md` §2.2–§2.7).
//!
//! The camera setup `0x14228f3b0` runs for every active viewport of the
//! pass being drawn, several times a frame. The engine reads the jitter
//! from `sCamera+0x38/+0x3c` inside it (`0x14228faaf`), after its frame start
//! cleared a value not stamped for this frame. So the jitter is computed once
//! per render frame and written just before the original runs for the main
//! viewport (later viewports and the engine's second, de-jittering reader
//! `0x1423c6511` see the same value); the camera is read just after, when
//! the rect is fresh.

use std::sync::atomic::{AtomicU64, Ordering};

use mhw_core::frame::{CameraFrame, JitterSample, Size};
use mhw_core::math::{JitterScale, JitterSequence, Mat4};

use crate::layout::viewport;

/// A viewport's fields, read in one piece.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewportSnapshot {
    /// `cCamera*`.
    pub camera: usize,
    /// Own render target; 0 draws to the screen.
    pub target: usize,
    /// Active.
    pub active: bool,
    /// Kind (`0xa` = the global rect).
    pub kind: u8,
    /// Pass (0 main, 1 sub display).
    pub pass: u8,
    /// `(left, top, right, bottom)`.
    pub rect: [i32; 4],
    /// View, unjittered.
    pub view: Mat4,
    /// Projection, unjittered, standard depth.
    pub proj: Mat4,
    /// Last frame's view.
    pub prev_view: Mat4,
    /// Last frame's projection.
    pub prev_proj: Mat4,
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

fn i32_at(bytes: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn mat_at(bytes: &[u8], at: usize) -> Option<Mat4> {
    Mat4::from_le_bytes(bytes.get(at..)?)
}

impl ViewportSnapshot {
    /// From the viewport's `0x1a0` bytes; `None` if shorter.
    pub fn parse(bytes: &[u8]) -> Option<ViewportSnapshot> {
        let bytes = bytes.get(..viewport::SIZE)?;
        let byte = |at: usize| bytes.get(at).copied();
        Some(ViewportSnapshot {
            camera: u64_at(bytes, viewport::CAMERA)? as usize,
            target: u64_at(bytes, viewport::TARGET)? as usize,
            active: byte(viewport::ACTIVE)? != 0,
            kind: byte(viewport::KIND)?,
            pass: byte(viewport::PASS)?,
            rect: [
                i32_at(bytes, viewport::RECT)?,
                i32_at(bytes, viewport::RECT + 4)?,
                i32_at(bytes, viewport::RECT + 8)?,
                i32_at(bytes, viewport::RECT + 12)?,
            ],
            view: mat_at(bytes, viewport::VIEW)?,
            proj: mat_at(bytes, viewport::PROJ)?,
            prev_view: mat_at(bytes, viewport::PREV_VIEW)?,
            prev_proj: mat_at(bytes, viewport::PREV_PROJ)?,
        })
    }

    /// The main viewport: active, drawn in pass 0, to the screen (no own
    /// target). The first such in a frame is the one jittered and
    /// published.
    pub fn is_main(&self) -> bool {
        self.active && self.pass == 0 && self.target == 0
    }

    /// The rect's size (the engine's jitter divisor), empty if inverted.
    pub fn size(&self) -> Size {
        let [left, top, right, bottom] = self.rect;
        let side = |from: i32, to: i32| u32::try_from(to.saturating_sub(from)).unwrap_or(0);
        Size::new(side(left, right), side(top, bottom))
    }

    /// The camera as [`CameraFrame`]: `None` when the matrices are not a
    /// usable camera (not finite, view not invertible, projection not
    /// perspective).
    pub fn camera_frame(&self, frame: u64) -> Option<CameraFrame> {
        let finite = [&self.view, &self.proj, &self.prev_view, &self.prev_proj]
            .iter()
            .all(|m| m.is_finite());
        finite
            .then(|| CameraFrame::new(self.view, self.proj, self.prev_view, self.prev_proj, frame))
            .flatten()
    }
}

/// "Has this frame been seen?" across calls, lock-free.
#[derive(Debug, Default)]
pub struct OncePerFrame(AtomicU64);

impl OncePerFrame {
    /// Not yet any frame.
    pub const fn new() -> OncePerFrame {
        OncePerFrame(AtomicU64::new(0))
    }

    /// `true` the first time `frame` is claimed (and not again until another
    /// frame was claimed in between).
    pub fn claim(&self, frame: u64) -> bool {
        let key = frame.wrapping_add(1);
        self.0.swap(key, Ordering::AcqRel) != key
    }

    /// The last frame claimed.
    pub fn last(&self) -> Option<u64> {
        match self.0.load(Ordering::Acquire) {
            0 => None,
            key => Some(key.wrapping_sub(1)),
        }
    }
}

/// The render size the engine builds for `display` at the built ratio
/// (`cfg+0x204`): `trunc` per side (`0x142391f30`).
pub fn render_size(display: Size, built_ratio: f32) -> Size {
    if !built_ratio.is_finite() || built_ratio <= 0.0 {
        return Size::default();
    }
    display.scaled_trunc(built_ratio.min(1.0))
}

/// The jitter of `frame`: a `phases`-long Halton sequence converted by
/// `mode`. `Off` (or an empty size) is a zero sample.
pub fn jitter_for(
    frame: u64,
    render: Size,
    display: Size,
    mode: JitterScale,
    phases: u32,
) -> JitterSample {
    JitterSequence::new(phases).sample(frame, render, display, mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mhw_core::math::Vec2;

    fn perspective() -> Mat4 {
        // Row-vector perspective, standard depth, near 0.1 far 1000.
        let (n, f) = (0.1f32, 1000.0f32);
        Mat4::from_rows([
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.7, 0.0, 0.0],
            [0.0, 0.0, f / (n - f), -1.0],
            [0.0, 0.0, n * f / (n - f), 0.0],
        ])
    }

    fn bytes(active: bool, pass: u8, target: u64, rect: [i32; 4]) -> Vec<u8> {
        let mut b = vec![0u8; viewport::SIZE];
        b[viewport::TARGET..viewport::TARGET + 8].copy_from_slice(&target.to_le_bytes());
        b[viewport::ACTIVE] = u8::from(active);
        b[viewport::KIND] = 0xa;
        b[viewport::PASS] = pass;
        for (i, v) in rect.iter().enumerate() {
            let at = viewport::RECT + 4 * i;
            b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        }
        let mut view = Mat4::IDENTITY;
        view.rows[3] = [1.0, 2.0, 3.0, 1.0];
        for (at, m) in [
            (viewport::VIEW, view),
            (viewport::PROJ, perspective()),
            (viewport::PREV_VIEW, view),
            (viewport::PREV_PROJ, perspective()),
        ] {
            let flat: Vec<u8> = m.to_array().iter().flat_map(|v| v.to_le_bytes()).collect();
            b[at..at + 64].copy_from_slice(&flat);
        }
        b
    }

    #[test]
    fn a_viewport_parses_and_the_main_one_is_recognised() {
        let main = ViewportSnapshot::parse(&bytes(true, 0, 0, [0, 0, 3840, 2160])).unwrap();
        assert!(main.is_main());
        assert_eq!(main.size(), Size::new(3840, 2160));
        assert_eq!(main.kind, 0xa);
        assert!(!ViewportSnapshot::parse(&bytes(false, 0, 0, [0; 4]))
            .unwrap()
            .is_main());
        assert!(!ViewportSnapshot::parse(&bytes(true, 1, 0, [0; 4]))
            .unwrap()
            .is_main());
        assert!(!ViewportSnapshot::parse(&bytes(true, 0, 0x1234, [0; 4]))
            .unwrap()
            .is_main());
        assert_eq!(ViewportSnapshot::parse(&[0u8; 0x100]), None);
        assert_eq!(
            ViewportSnapshot::parse(&bytes(true, 0, 0, [10, 10, 5, 5]))
                .unwrap()
                .size(),
            Size::default()
        );
    }

    #[test]
    fn the_camera_comes_from_the_unjittered_matrices() {
        let main = ViewportSnapshot::parse(&bytes(true, 0, 0, [0, 0, 1920, 1080])).unwrap();
        let camera = main.camera_frame(7).unwrap();
        assert_eq!(camera.frame, 7);
        assert_eq!(camera.proj, perspective());
        assert!((camera.near - 0.1).abs() < 1e-3);
        let mut broken = main;
        broken.view.rows[0][0] = f32::NAN;
        assert_eq!(broken.camera_frame(7), None);
        let mut flat = main;
        flat.view = Mat4::from_rows([[0.0; 4]; 4]);
        assert_eq!(flat.camera_frame(7), None);
    }

    #[test]
    fn a_frame_is_claimed_once() {
        let once = OncePerFrame::new();
        assert_eq!(once.last(), None);
        assert!(once.claim(0));
        assert!(!once.claim(0));
        assert!(once.claim(1));
        assert!(!once.claim(1));
        assert!(once.claim(0));
        assert_eq!(once.last(), Some(0));
        // Frames are the engine's u32 counter; u64::MAX never occurs.
        assert!(once.claim(u64::from(u32::MAX)));
        assert_eq!(once.last(), Some(u64::from(u32::MAX)));
    }

    #[test]
    fn the_render_size_is_the_engines() {
        let display = Size::new(3840, 2160);
        assert_eq!(render_size(display, 0.667), Size::new(2561, 1440));
        assert_eq!(render_size(display, 0.0), Size::default());
        assert_eq!(render_size(display, f32::NAN), Size::default());
        assert_eq!(render_size(display, 1.0), display);
    }

    #[test]
    fn display_mode_moves_the_image_by_the_requested_render_pixels() {
        let (render, display) = (Size::new(1920, 1080), Size::new(3840, 2160));
        let sample = jitter_for(5, render, display, JitterScale::Display, 32);
        // units = 2·px·display/render = 4·px; px is what DLSS is told.
        assert!((sample.units.x - 4.0 * sample.px.x).abs() < 1e-5);
        assert!((sample.units.y - 4.0 * sample.px.y).abs() < 1e-5);
        assert_eq!(sample.phase, 5);
        assert_eq!(sample.frame, 5);
        let off = jitter_for(5, render, display, JitterScale::Off, 32);
        assert_eq!(off.units, Vec2::ZERO);
        assert_eq!(
            jitter_for(40, render, display, JitterScale::Display, 32).phase,
            8
        );
    }
}
