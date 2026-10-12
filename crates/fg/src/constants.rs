//! `sl::Constants` for one frame, assembled from the engine's camera.
//!
//! Every convention comes from `mhw_core::math` and nothing here repeats
//! one: the jitter is [`streamline_jitter`] of the image shift the engine
//! write actually produced ([`JitterSample::px`], render pixels incl. the
//! display/render factor), `clipToPrevClip` is [`clip_to_prev_clip`] of the
//! camera's own **unjittered** matrices, and the projection handed over is
//! `P·R` ([`DEPTH_FLIP`]), the clip space the depth buffer was rendered in
//! (`fNoJitterProj`).
//!
//! Fixed facts of this engine (docs/re/ReJitterCamera.md §2.3):
//!
//! * depth is inverted (`R`: `z' = w − z`);
//! * the motion vectors include camera motion and carry no jitter, because
//!   the engine builds its previous-frame matrix with the *current* jitter;
//! * the tagged vectors (render's decode) are one vector per pixel, not
//!   dilated.

use mhw_core::frame::{CameraFrame, JitterSample, Size};
use mhw_core::math::{clip_to_prev_clip, lens, streamline_jitter, Mat4, Vec2, Vec3, DEPTH_FLIP};

use crate::abi;

/// How the tagged motion-vector texture is encoded.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionVectors {
    /// Multiplying a texel by this gives render pixels in NVIDIA's axes
    /// (+y down): the scale DLSS-SR is given for the same texture.
    pub pixel_scale: Vec2,
    /// Size of the tagged texture, pixels.
    pub size: Size,
}

/// Why no constants could be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConstantsError {
    /// The jitter belongs to another frame than the camera.
    FrameMismatch {
        /// [`CameraFrame::frame`].
        camera: u64,
        /// [`JitterSample::frame`].
        jitter: u64,
    },
    /// `P·R` does not invert.
    ProjectionNotInvertible,
    /// `V·P·R` (current or previous) does not invert.
    ReprojectionNotInvertible,
    /// The motion-vector texture has no size.
    EmptyMotionVectors,
    /// A matrix or the jitter is not finite.
    NotFinite,
}

impl std::fmt::Display for ConstantsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConstantsError::FrameMismatch { camera, jitter } => {
                write!(
                    f,
                    "camera of frame {camera} paired with jitter of frame {jitter}"
                )
            }
            ConstantsError::ProjectionNotInvertible => f.write_str("projection does not invert"),
            ConstantsError::ReprojectionNotInvertible => {
                f.write_str("view-projection does not invert")
            }
            ConstantsError::EmptyMotionVectors => f.write_str("motion-vector texture is empty"),
            ConstantsError::NotFinite => f.write_str("non-finite camera or jitter"),
        }
    }
}

impl std::error::Error for ConstantsError {}

/// `sl::Constants::mvecScale`: Streamline normalises vectors to a fraction
/// of the screen (its samples pass `{1/width, 1/height}` for pixel-space
/// vectors), so the scale is the pixel scale over the texture size.
/// `None` for an empty texture.
pub fn streamline_mvec_scale(motion: MotionVectors) -> Option<Vec2> {
    if motion.size.is_empty() {
        return None;
    }
    Some(Vec2::new(
        motion.pixel_scale.x / motion.size.w as f32,
        motion.pixel_scale.y / motion.size.h as f32,
    ))
}

/// The camera basis Streamline asks for, world space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Basis {
    /// View-space +x.
    pub right: Vec3,
    /// View-space +y.
    pub up: Vec3,
    /// The viewing direction ([`CameraFrame::forward`]).
    pub forward: Vec3,
}

/// The basis out of `view_inv`, if its rotation part is orthonormal to
/// within a thousandth; anything else is not the matrix it is taken for.
pub fn basis(camera: &CameraFrame) -> Option<Basis> {
    let row = |i: usize| {
        let [x, y, z, _] = camera.view_inv.rows[i];
        Vec3::new(x, y, z)
    };
    let (right, up, z) = (row(0), row(1), row(2));
    let unit = |v: Vec3| v.is_finite() && (v.dot(v) - 1.0).abs() <= 1e-3;
    let orthogonal = |a: Vec3, b: Vec3| a.dot(b).abs() <= 1e-3;
    (unit(right)
        && unit(up)
        && unit(z)
        && orthogonal(right, up)
        && orthogonal(right, z)
        && orthogonal(up, z))
    .then(|| Basis {
        right,
        up,
        forward: camera.forward(),
    })
}

fn rows(m: Mat4) -> abi::Float4x4 {
    m.rows
}

fn xyz(v: Vec3) -> [f32; 3] {
    [v.x, v.y, v.z]
}

/// Build one frame's `sl::Constants`.
///
/// `reset` says the previous frame has no connection to this one (camera
/// cut, game history reset, resize). Lens values that cannot be read and a
/// basis that is not orthonormal are sent as `INVALID_FLOAT`, never as a
/// plausible constant.
pub fn build(
    camera: &CameraFrame,
    jitter: &JitterSample,
    motion: MotionVectors,
    reset: bool,
) -> Result<abi::Constants, ConstantsError> {
    if camera.frame != jitter.frame {
        return Err(ConstantsError::FrameMismatch {
            camera: camera.frame,
            jitter: jitter.frame,
        });
    }
    if !camera.proj.is_finite() || !camera.view.is_finite() || !jitter.px.is_finite() {
        return Err(ConstantsError::NotFinite);
    }
    let mvec_scale = streamline_mvec_scale(motion).ok_or(ConstantsError::EmptyMotionVectors)?;
    let view_to_clip = camera.proj * DEPTH_FLIP;
    let clip_to_view = view_to_clip
        .inverse()
        .ok_or(ConstantsError::ProjectionNotInvertible)?;
    let clip_to_prev =
        clip_to_prev_clip(camera).ok_or(ConstantsError::ReprojectionNotInvertible)?;
    let prev_to_clip = clip_to_prev
        .inverse()
        .ok_or(ConstantsError::ReprojectionNotInvertible)?;
    let offset = streamline_jitter(jitter.px);
    let lens = lens(&camera.proj);
    let basis = basis(camera);
    let invalid3 = [abi::INVALID_FLOAT; 3];
    Ok(abi::Constants {
        base: abi::Base::new(abi::CONSTANTS, 2),
        camera_view_to_clip: rows(view_to_clip),
        clip_to_camera_view: rows(clip_to_view),
        clip_to_lens_clip: rows(Mat4::IDENTITY),
        clip_to_prev_clip: rows(clip_to_prev),
        prev_clip_to_clip: rows(prev_to_clip),
        jitter_offset: [offset.x, offset.y],
        mvec_scale: [mvec_scale.x, mvec_scale.y],
        camera_pinhole_offset: [0.0, 0.0],
        camera_pos: if camera.position.is_finite() {
            xyz(camera.position)
        } else {
            invalid3
        },
        camera_up: basis.map_or(invalid3, |b| xyz(b.up)),
        camera_right: basis.map_or(invalid3, |b| xyz(b.right)),
        camera_fwd: basis.map_or(invalid3, |b| xyz(b.forward)),
        camera_near: lens.map_or(abi::INVALID_FLOAT, |l| l.near),
        camera_far: lens.map_or(abi::INVALID_FLOAT, |l| l.far),
        camera_fov: lens.map_or(abi::INVALID_FLOAT, |l| l.fov_y),
        camera_aspect_ratio: lens.map_or(abi::INVALID_FLOAT, |l| l.aspect),
        motion_vectors_invalid_value: abi::INVALID_FLOAT,
        depth_inverted: abi::TRUE,
        camera_motion_included: abi::TRUE,
        motion_vectors_3d: abi::FALSE,
        reset: abi::boolean(reset),
        orthographic_projection: abi::FALSE,
        motion_vectors_dilated: abi::FALSE,
        motion_vectors_jittered: abi::FALSE,
        min_relative_linear_depth_object_separation: 40.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mhw_core::math::{Mat4, Vec2};

    /// Row-vector right-handed perspective looking down −z, standard depth.
    fn perspective(fov_y: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
        let f = 1.0 / (fov_y / 2.0).tan();
        Mat4::from_rows([
            [f / aspect, 0.0, 0.0, 0.0],
            [0.0, f, 0.0, 0.0],
            [0.0, 0.0, far / (near - far), -1.0],
            [0.0, 0.0, near * far / (near - far), 0.0],
        ])
    }

    fn translation(x: f32, y: f32, z: f32) -> Mat4 {
        Mat4::from_rows([
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [x, y, z, 1.0],
        ])
    }

    fn camera(view: Mat4, prev_view: Mat4, frame: u64) -> CameraFrame {
        let proj = perspective(1.0, 16.0 / 9.0, 0.5, 5000.0);
        CameraFrame::new(view, proj, prev_view, proj, frame).expect("valid camera")
    }

    fn jitter(px: Vec2, frame: u64) -> JitterSample {
        JitterSample {
            px,
            units: Vec2::default(),
            phase: 0,
            frame,
        }
    }

    const PIXELS: MotionVectors = MotionVectors {
        pixel_scale: Vec2 { x: 1.0, y: 1.0 },
        size: Size { w: 2560, h: 1440 },
    };

    fn mat(m: abi::Float4x4) -> Mat4 {
        Mat4::from_rows(m)
    }

    #[test]
    fn a_still_camera_reprojects_onto_itself() {
        let view = translation(10.0, -3.0, 250.0);
        let c = build(
            &camera(view, view, 7),
            &jitter(Vec2::default(), 7),
            PIXELS,
            false,
        )
        .unwrap();
        assert!(mat(c.clip_to_prev_clip).approx_eq(&Mat4::IDENTITY, 1e-4));
        assert!(mat(c.prev_clip_to_clip).approx_eq(&Mat4::IDENTITY, 1e-4));
    }

    #[test]
    fn reprojection_is_the_core_convention_and_its_inverse() {
        let cam = camera(
            translation(0.0, 0.0, -10.0),
            translation(0.5, 0.0, -10.0),
            3,
        );
        let c = build(&cam, &jitter(Vec2::default(), 3), PIXELS, false).unwrap();
        let expected = clip_to_prev_clip(&cam).unwrap();
        assert!(mat(c.clip_to_prev_clip).approx_eq(&expected, 1e-5));
        let round = mat(c.clip_to_prev_clip) * mat(c.prev_clip_to_clip);
        assert!(round.approx_eq(&Mat4::IDENTITY, 1e-4));
    }

    #[test]
    fn projection_is_unjittered_and_depth_flipped() {
        let cam = camera(Mat4::IDENTITY, Mat4::IDENTITY, 1);
        let c = build(&cam, &jitter(Vec2::new(0.3, -0.2), 1), PIXELS, false).unwrap();
        let expected = cam.proj * DEPTH_FLIP;
        assert!(mat(c.camera_view_to_clip).approx_eq(&expected, 1e-6));
        // No jitter in the matrix: row 2 x/y stay zero.
        assert_eq!(c.camera_view_to_clip[2][0], 0.0);
        assert_eq!(c.camera_view_to_clip[2][1], 0.0);
        let round = mat(c.camera_view_to_clip) * mat(c.clip_to_camera_view);
        assert!(round.approx_eq(&Mat4::IDENTITY, 1e-4));
        assert_eq!(c.depth_inverted, abi::TRUE);
    }

    #[test]
    fn jitter_is_the_streamline_convention_of_the_produced_shift() {
        let px = Vec2::new(0.25, 0.125);
        let cam = camera(Mat4::IDENTITY, Mat4::IDENTITY, 9);
        let c = build(&cam, &jitter(px, 9), PIXELS, false).unwrap();
        let want = streamline_jitter(px);
        assert_eq!(c.jitter_offset, [want.x, want.y]);
        // NVIDIA's +y down: the engine's +y up flips.
        assert_eq!(c.jitter_offset, [0.25, -0.125]);
    }

    #[test]
    fn mvec_scale_normalises_pixels_by_the_texture_size() {
        let scale = streamline_mvec_scale(PIXELS).unwrap();
        assert_eq!(scale, Vec2::new(1.0 / 2560.0, 1.0 / 1440.0));
        let raw = MotionVectors {
            pixel_scale: Vec2::new(-960.0, 540.0),
            size: Size::new(1920, 1080),
        };
        assert_eq!(streamline_mvec_scale(raw).unwrap(), Vec2::new(-0.5, 0.5));
        let empty = MotionVectors {
            pixel_scale: Vec2::new(1.0, 1.0),
            size: Size::new(0, 1080),
        };
        assert_eq!(streamline_mvec_scale(empty), None);
        let cam = camera(Mat4::IDENTITY, Mat4::IDENTITY, 1);
        assert_eq!(
            build(&cam, &jitter(Vec2::default(), 1), empty, false),
            Err(ConstantsError::EmptyMotionVectors)
        );
    }

    #[test]
    fn camera_scalars_come_from_the_matrices() {
        let cam = camera(translation(-100.0, -20.0, -30.0), Mat4::IDENTITY, 4);
        let c = build(&cam, &jitter(Vec2::default(), 4), PIXELS, true).unwrap();
        assert_eq!(c.camera_pos, [100.0, 20.0, 30.0]);
        assert_eq!(c.camera_right, [1.0, 0.0, 0.0]);
        assert_eq!(c.camera_up, [0.0, 1.0, 0.0]);
        assert_eq!(c.camera_fwd, [0.0, 0.0, -1.0]);
        assert!((c.camera_near - 0.5).abs() < 1e-3);
        assert!((c.camera_far - 5000.0).abs() / 5000.0 < 1e-3);
        assert!((c.camera_fov - 1.0).abs() < 1e-5);
        assert!((c.camera_aspect_ratio - 16.0 / 9.0).abs() < 1e-5);
        assert_eq!(c.reset, abi::TRUE);
        assert_eq!(c.camera_motion_included, abi::TRUE);
        assert_eq!(c.motion_vectors_jittered, abi::FALSE);
        assert_eq!(c.motion_vectors_dilated, abi::FALSE);
        assert_eq!(c.base.kind, abi::CONSTANTS);
        assert_eq!(c.base.version, 2);
    }

    #[test]
    fn a_skewed_view_leaves_the_basis_invalid() {
        let skew = Mat4::from_rows([
            [1.0, 0.0, 0.0, 0.0],
            [0.5, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ]);
        let cam = camera(skew, skew, 2);
        let c = build(&cam, &jitter(Vec2::default(), 2), PIXELS, false).unwrap();
        assert_eq!(c.camera_up, [abi::INVALID_FLOAT; 3]);
        assert_eq!(c.camera_fwd, [abi::INVALID_FLOAT; 3]);
        // The lens does not depend on the view.
        assert!(c.camera_near < abi::INVALID_FLOAT);
    }

    #[test]
    fn jitter_of_another_frame_is_refused() {
        let cam = camera(Mat4::IDENTITY, Mat4::IDENTITY, 5);
        assert_eq!(
            build(&cam, &jitter(Vec2::default(), 6), PIXELS, false),
            Err(ConstantsError::FrameMismatch {
                camera: 5,
                jitter: 6
            })
        );
    }
}
