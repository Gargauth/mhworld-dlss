//! The engine's camera matrices: jitter and depth-flip matrices, the
//! unjittered clip-to-previous-clip transform, lens parameters and camera
//! cut detection.
//!
//! How the engine fills `CBViewProjection` (`docs/re/ReJitterCamera.md`
//! §2.3), row vectors:
//!
//! ```text
//! fProj      = P·J·R          fNoJitterProj     = P·R
//! fViewProj  = V·P·J·R        fNoJitterViewProj = V·P·R
//! fViewProjPF = Vprev·Pprev·J·R    (the *current* J: motion vectors carry no jitter)
//! ```
//!
//! `J` = [`jitter_matrix`], `R` = [`DEPTH_FLIP`].

use crate::frame::{CameraFrame, Size};

use super::{Mat4, Vec2};

/// `R`: the engine's depth flip, `z' = w − z` (`K = −1.0` at `0x142e4fe70`).
/// Its own inverse.
pub const DEPTH_FLIP: Mat4 = Mat4::from_rows([
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, -1.0, 0.0],
    [0.0, 0.0, 1.0, 1.0],
]);

/// `J`: identity with row 3 = `(units.x/Wd, units.y/Hd, 0, 1)`, as built in
/// the camera setup (`0x14228fa75..0x14228fadc`). Identity for an empty
/// display size.
pub fn jitter_matrix(units: Vec2, display: Size) -> Mat4 {
    let mut j = Mat4::IDENTITY;
    if !display.is_empty() {
        j.rows[3][0] = units.x / display.w as f32;
        j.rows[3][1] = units.y / display.h as f32;
    }
    j
}

/// The transform from this frame's clip space to the previous frame's,
/// **without jitter**, in the depth-flipped clip space the depth buffer is
/// in: `inv(V·P·R)·(Vprev·Pprev·R)`, for row vectors (`clip_prev ∝
/// clip_cur·M`).
///
/// Built from the camera's own `prev_view`/`prev_proj`, the engine's copies
/// that its motion vectors use (they stay put while the camera is frozen).
/// This is what `sl::Constants::clipToPrevClip` wants ("must not contain
/// jitter"); a consumer with column vectors transposes it. `None` if
/// `V·P·R` does not invert.
pub fn clip_to_prev_clip(camera: &CameraFrame) -> Option<Mat4> {
    let current = camera.view * camera.proj * DEPTH_FLIP;
    let previous = camera.prev_view * camera.prev_proj * DEPTH_FLIP;
    Some(current.inverse()? * previous)
}

/// What a perspective projection says about the lens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lens {
    /// Distance to the near plane, view-space units; finite.
    pub near: f32,
    /// Distance to the far plane, or infinity.
    pub far: f32,
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Width over height.
    pub aspect: f32,
    /// Depth 1 at the near plane and 0 at the far one.
    pub reversed: bool,
}

/// Read the lens out of a perspective projection (row vectors):
///
/// ```text
/// row0 = (P00, 0, 0, 0)   row2 = (j8, j9, P22, P23)   P23 = ±1
/// row1 = (0, P11, 0, 0)   row3 = (0, 0, P32, 0)
/// ```
///
/// For a point at distance `d` in front of the camera (`z = d·P23`),
/// `depth = P23·P22 + P32/d`: depth 0 at `−P32/(P23·P22)`, depth 1 at
/// `P32/(1 − P23·P22)`. The nearer is the near plane whichever way depth
/// runs; `P22·P23 = 0` has no plane at depth 0, an infinite far plane.
/// Jitter in `j8/j9` does not change the lens. `None` for anything that
/// does not give a positive near plane in front of a farther far plane.
pub fn lens(projection: &Mat4) -> Option<Lens> {
    let m = projection.rows;
    let p00 = f64::from(m[0][0]);
    let p11 = f64::from(m[1][1]);
    let p22 = f64::from(m[2][2]);
    let p23 = f64::from(m[2][3]);
    let p32 = f64::from(m[3][2]);
    let off_diagonal = [
        m[0][1], m[0][2], m[0][3], m[1][0], m[1][2], m[1][3], m[3][0], m[3][1], m[3][3],
    ];
    if !projection.is_finite()
        || p00 <= 0.0
        || p11 <= 0.0
        || (p23.abs() - 1.0).abs() > 1e-6
        || off_diagonal.iter().any(|v| v.abs() > 1e-6)
    {
        return None;
    }
    let s = p23 * p22;
    let one = if (1.0 - s).abs() > 1e-12 {
        p32 / (1.0 - s)
    } else {
        f64::INFINITY
    };
    let zero = if s.abs() > 1e-12 {
        -p32 / s
    } else {
        f64::INFINITY
    };
    if one <= 0.0 || zero <= 0.0 {
        return None;
    }
    let (near, far, reversed) = if one < zero {
        (one, zero, true)
    } else {
        (zero, one, false)
    };
    if !near.is_finite() || far <= near {
        return None;
    }
    Some(Lens {
        near: near as f32,
        far: far as f32,
        fov_y: (2.0 * (1.0 / p11).atan()) as f32,
        aspect: (p11 / p00) as f32,
        reversed,
    })
}

/// Limits beyond which a frame-to-frame camera change is a cut.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CutThresholds {
    /// Camera position jump, world units.
    pub position_jump: f32,
    /// Change of view direction, radians.
    pub turn: f32,
    /// Relative change of the projection scale (`P00` or `P11`).
    pub projection_change: f32,
}

/// The thresholds [`camera_cut`] uses. World units are centimetres-ish
/// (camera positions in the thousands); 500 is a jump no gameplay camera
/// makes in one frame, 45° a turn no mouse makes at playable frame rates,
/// 5 % a zoom step larger than any smooth zoom per frame. [INFERENCE: to be
/// confirmed with `probe.reset`.]
pub const CUT_THRESHOLDS: CutThresholds = CutThresholds {
    position_jump: 500.0,
    turn: std::f32::consts::FRAC_PI_4,
    projection_change: 0.05,
};

/// How much the camera changed between two frames.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CutMetric {
    /// Distance between the positions, world units.
    pub position_jump: f32,
    /// Angle between the view directions, radians.
    pub turn: f32,
    /// Largest relative change of `P00`/`P11`.
    pub projection_change: f32,
}

impl CutMetric {
    /// Whether any component exceeds `limits` or is not a number.
    pub fn exceeds(&self, limits: &CutThresholds) -> bool {
        let over = |value: f32, limit: f32| value.is_nan() || value > limit;
        over(self.position_jump, limits.position_jump)
            || over(self.turn, limits.turn)
            || over(self.projection_change, limits.projection_change)
    }
}

/// Measure the change from `prev` to `cur`.
pub fn camera_cut_metric(prev: &CameraFrame, cur: &CameraFrame) -> CutMetric {
    let a = prev.forward();
    let b = cur.forward();
    let cos = a.dot(b) / (a.length() * b.length());
    // NaN would vanish in `max`; a projection that is not a number is a cut.
    let relative = |old: f32, new: f32| {
        let change = ((new - old) / old).abs();
        if change.is_nan() {
            f32::INFINITY
        } else {
            change
        }
    };
    CutMetric {
        position_jump: (cur.position - prev.position).length(),
        turn: cos.clamp(-1.0, 1.0).acos(),
        projection_change: relative(prev.proj.rows[0][0], cur.proj.rows[0][0])
            .max(relative(prev.proj.rows[1][1], cur.proj.rows[1][1])),
    }
}

/// Whether the camera cut between `prev` and `cur`: position jump, view
/// direction change or projection change above [`CUT_THRESHOLDS`], or
/// matrices that do not give numbers.
pub fn camera_cut(prev: &CameraFrame, cur: &CameraFrame) -> bool {
    camera_cut_metric(prev, cur).exceeds(&CUT_THRESHOLDS)
}

#[cfg(test)]
mod tests;
