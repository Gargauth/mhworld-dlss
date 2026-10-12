//! Data shared between crates about one frame.

use crate::math::{Mat4, Vec2, Vec3};

/// A size in pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Size {
    /// Width.
    pub w: u32,
    /// Height.
    pub h: u32,
}

impl Size {
    /// `w × h`.
    pub const fn new(w: u32, h: u32) -> Size {
        Size { w, h }
    }

    /// Whether either side is zero.
    pub const fn is_empty(self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// The render size the engine builds for this display size and render
    /// ratio: each side `trunc(side·ratio)` (`0x142391f30`).
    pub fn scaled_trunc(self, ratio: f32) -> Size {
        let side = |value: u32| (value as f32 * ratio) as u32;
        Size::new(side(self.w), side(self.h))
    }
}

/// The engine's main camera for one frame, **unjittered**, read in the
/// camera-setup detour after the original ran (`sCamera::Viewport`
/// `+0xa0` view, `+0xe0` proj, `+0x120` prev view, `+0x160` prev proj).
///
/// `proj` has standard depth (the engine applies the depth flip
/// [`DEPTH_FLIP`](crate::math::DEPTH_FLIP) only when it fills the constant
/// buffer). `prev_view`/`prev_proj` are the engine's own copies of last
/// frame's matrices, which its motion vectors are built from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraFrame {
    /// View matrix (world to view).
    pub view: Mat4,
    /// Projection matrix (view to clip), standard depth, no jitter.
    pub proj: Mat4,
    /// Last frame's view matrix, as the engine kept it.
    pub prev_view: Mat4,
    /// Last frame's projection matrix, as the engine kept it.
    pub prev_proj: Mat4,
    /// Inverse of `view` (view space to world).
    pub view_inv: Mat4,
    /// Camera position in world space (row 3 of `view_inv`).
    pub position: Vec3,
    /// Near plane distance.
    pub near: f32,
    /// Far plane distance (may be infinite).
    pub far: f32,
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Engine render frame counter (`sRender+0x1f208`).
    pub frame: u64,
}

impl CameraFrame {
    /// Build from the four matrices, deriving the rest. `None` when `view`
    /// does not invert or `proj` is not a perspective projection.
    pub fn new(
        view: Mat4,
        proj: Mat4,
        prev_view: Mat4,
        prev_proj: Mat4,
        frame: u64,
    ) -> Option<CameraFrame> {
        let view_inv = view.inverse()?;
        let lens = crate::math::lens(&proj)?;
        let [x, y, z, _] = view_inv.rows[3];
        Some(CameraFrame {
            view,
            proj,
            prev_view,
            prev_proj,
            view_inv,
            position: Vec3::new(x, y, z),
            near: lens.near,
            far: lens.far,
            fov_y: lens.fov_y,
            frame,
        })
    }

    /// The direction the camera looks in, world space (the engine looks
    /// down view-space −z: the negated row 2 of `view_inv`).
    pub fn forward(&self) -> Vec3 {
        let [x, y, z, _] = self.view_inv.rows[2];
        -Vec3::new(x, y, z)
    }
}

/// The jitter of one frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct JitterSample {
    /// The actual sub-pixel shift of the image, in render pixels, engine
    /// axes (+x right, +y up). Convert with
    /// [`dlss_jitter`](crate::math::dlss_jitter) /
    /// [`streamline_jitter`](crate::math::streamline_jitter).
    pub px: Vec2,
    /// What was written to `sCamera+0x38/+0x3c`.
    pub units: Vec2,
    /// Index in the jitter sequence.
    pub phase: u32,
    /// Engine render frame counter.
    pub frame: u64,
}

/// Why the temporal history is discarded this frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ResetReason {
    /// History is kept.
    #[default]
    None,
    /// The game's own reset flag (TAA history invalid).
    GameHistory,
    /// [`camera_cut`](crate::math::camera_cut) said so.
    CameraCut,
    /// A setting that invalidates history changed.
    Settings,
    /// Render or display size changed.
    Resize,
}

/// Per-frame summary published at the start of a frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameInfo {
    /// Engine render frame counter.
    pub frame: u64,
    /// Render (input) size.
    pub render: Size,
    /// Display (output) size.
    pub display: Size,
    /// `render / display`.
    pub scale: f32,
    /// Whether and why history is discarded.
    pub reset: ResetReason,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_size_truncates_like_the_engine() {
        let display = Size::new(3840, 2160);
        assert_eq!(display.scaled_trunc(0.5), Size::new(1920, 1080));
        // 2560·0.667 = 1707.52, 1440·0.667 = 960.48
        assert_eq!(
            Size::new(2560, 1440).scaled_trunc(0.667),
            Size::new(1707, 960)
        );
        assert!(Size::new(0, 10).is_empty());
        assert!(!display.is_empty());
    }
}
