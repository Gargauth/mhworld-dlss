//! Reading the engine's camera, and the jitter sequence.
//!
//! The engine's own jitter slot is what moves the projection (`native.rs`);
//! writing an offset into `CBViewProjection` after the engine has filled it
//! cannot work, because the engine derives its other matrices and its motion
//! vectors before that point. What this module holds is what everything else
//! needs from the buffer: its layout, recognising it by shape, reading
//! matrices out of it, the matrix arithmetic frame generation uses, and the
//! Halton sequence the jitter draws from.
//!
//! The matrices are row-major with row-vector multiplication, so a clip-space
//! position is `v * M`. For the projection seen in this engine
//!
//! ```text
//! row0 = (P00,   0,   0,   0)
//! row1 = (  0, P11,   0,   0)
//! row2 = (  0,   0, P22,  -1)      <- w = -z, a reversed-depth projection
//! row3 = (  0,   0, P32,   0)
//! ```
//!
//! `clip.x` picks up `vz * P20` and `clip.w` is `-vz`, so setting
//! `P20 = -jx` adds exactly `jx * clip.w` to `clip.x`: a shift of `jx` in
//! normalised device coordinates, which is the definition of a jittered
//! projection. `P21` does the same for y.

/// Byte offsets within `CBViewProjection`, from the RDEF variable table.
pub const F_VIEW_PROJ: usize = 0x000;
pub const F_VIEW: usize = 0x040;
pub const F_PROJ: usize = 0x080;
pub const F_NO_JITTER_PROJ: usize = 0x320;

/// Bytes of the buffer that have to be readable for any of this to apply.
pub const NEEDED: usize = F_NO_JITTER_PROJ + 64;

pub type Matrix = [f32; 16];

pub fn read(bytes: &[u8], offset: usize) -> Option<Matrix> {
    let slice = bytes.get(offset..offset + 64)?;
    let mut matrix = [0f32; 16];
    for (index, word) in slice.as_chunks::<4>().0.iter().enumerate() {
        matrix[index] = f32::from_le_bytes([word[0], word[1], word[2], word[3]]);
    }
    Some(matrix)
}

/// Row-vector product: `C = A * B`.
pub fn multiply(a: &Matrix, b: &Matrix) -> Matrix {
    let mut out = [0f32; 16];
    for row in 0..4 {
        for column in 0..4 {
            out[row * 4 + column] = (0..4).map(|k| a[row * 4 + k] * b[k * 4 + column]).sum();
        }
    }
    out
}

/// Does this look like the engine's projection matrix?
///
/// Deliberately strict. The buffer is identified by shape rather than by which
/// root parameter it arrived on, because the root index differs between the
/// graphics and compute signatures, and writing jitter into the wrong buffer
/// would be both invisible and destructive.
/// The same shape test, but tolerant of an offset already written into the
/// two entries that carry one.
///
/// Needed once `fNoJitterProj` may itself be offset: the strict test demands
/// zeroes there and would then stop recognising the buffer at all.
pub fn looks_like_projection_offset(m: &Matrix) -> bool {
    let mut cleared = *m;
    cleared[8] = 0.0;
    cleared[9] = 0.0;
    looks_like_projection(&cleared)
}

pub fn looks_like_projection(m: &Matrix) -> bool {
    let zero = |v: f32| v.abs() < 1e-6;
    m[0] > 0.1
        && zero(m[1])
        && zero(m[2])
        && zero(m[3])
        && zero(m[4])
        && m[5] > 0.1
        && zero(m[6])
        && zero(m[7])
        && zero(m[8])
        && zero(m[9])
        && (m[11] + 1.0).abs() < 1e-6
        && zero(m[12])
        && zero(m[13])
        && zero(m[15])
}

/// Is this buffer `CBViewProjection`?
///
/// The shape is checked against `fNoJitterProj`, which nothing here ever
/// writes, so the test answers the same whether or not the frame is
/// jittered. Requiring `fProj == fNoJitterProj` as well would hold only for
/// an unjittered frame and would reject every buffer of a jittered one.
///
/// `fProj` must still agree with `fNoJitterProj` everywhere except the two
/// entries carrying the offset, so a buffer changed by anything else is not
/// mistaken for this one.
pub fn is_view_projection(bytes: &[u8]) -> bool {
    let (Some(proj), Some(no_jitter)) = (read(bytes, F_PROJ), read(bytes, F_NO_JITTER_PROJ)) else {
        return false;
    };
    looks_like_projection_offset(&no_jitter)
        && (0..16).all(|index| index == 8 || index == 9 || proj[index] == no_jitter[index])
}

/// Offset of `fViewI`, the inverse view matrix, within `CBViewProjection`.
pub const F_INVERSE_VIEW: usize = 0x0c0;

/// A general four-by-four inverse, by cofactor expansion.
///
/// The structure of these matrices is not relied on: whatever the engine
/// derived, the inverse of the offset view-projection is what has to replace
/// it, and a general routine cannot be wrong about a matrix it was not
/// expecting.
pub fn invert(m: &Matrix) -> Option<Matrix> {
    let mut out = [0f32; 16];
    out[0] = m[5] * m[10] * m[15] - m[5] * m[11] * m[14] - m[9] * m[6] * m[15]
        + m[9] * m[7] * m[14]
        + m[13] * m[6] * m[11]
        - m[13] * m[7] * m[10];
    out[4] = -m[4] * m[10] * m[15] + m[4] * m[11] * m[14] + m[8] * m[6] * m[15]
        - m[8] * m[7] * m[14]
        - m[12] * m[6] * m[11]
        + m[12] * m[7] * m[10];
    out[8] = m[4] * m[9] * m[15] - m[4] * m[11] * m[13] - m[8] * m[5] * m[15]
        + m[8] * m[7] * m[13]
        + m[12] * m[5] * m[11]
        - m[12] * m[7] * m[9];
    out[12] = -m[4] * m[9] * m[14] + m[4] * m[10] * m[13] + m[8] * m[5] * m[14]
        - m[8] * m[6] * m[13]
        - m[12] * m[5] * m[10]
        + m[12] * m[6] * m[9];
    out[1] = -m[1] * m[10] * m[15] + m[1] * m[11] * m[14] + m[9] * m[2] * m[15]
        - m[9] * m[3] * m[14]
        - m[13] * m[2] * m[11]
        + m[13] * m[3] * m[10];
    out[5] = m[0] * m[10] * m[15] - m[0] * m[11] * m[14] - m[8] * m[2] * m[15]
        + m[8] * m[3] * m[14]
        + m[12] * m[2] * m[11]
        - m[12] * m[3] * m[10];
    out[9] = -m[0] * m[9] * m[15] + m[0] * m[11] * m[13] + m[8] * m[1] * m[15]
        - m[8] * m[3] * m[13]
        - m[12] * m[1] * m[11]
        + m[12] * m[3] * m[9];
    out[13] = m[0] * m[9] * m[14] - m[0] * m[10] * m[13] - m[8] * m[1] * m[14]
        + m[8] * m[2] * m[13]
        + m[12] * m[1] * m[10]
        - m[12] * m[2] * m[9];
    out[2] = m[1] * m[6] * m[15] - m[1] * m[7] * m[14] - m[5] * m[2] * m[15]
        + m[5] * m[3] * m[14]
        + m[13] * m[2] * m[7]
        - m[13] * m[3] * m[6];
    out[6] = -m[0] * m[6] * m[15] + m[0] * m[7] * m[14] + m[4] * m[2] * m[15]
        - m[4] * m[3] * m[14]
        - m[12] * m[2] * m[7]
        + m[12] * m[3] * m[6];
    out[10] = m[0] * m[5] * m[15] - m[0] * m[7] * m[13] - m[4] * m[1] * m[15]
        + m[4] * m[3] * m[13]
        + m[12] * m[1] * m[7]
        - m[12] * m[3] * m[5];
    out[14] = -m[0] * m[5] * m[14] + m[0] * m[6] * m[13] + m[4] * m[1] * m[14]
        - m[4] * m[2] * m[13]
        - m[12] * m[1] * m[6]
        + m[12] * m[2] * m[5];
    out[3] = -m[1] * m[6] * m[11] + m[1] * m[7] * m[10] + m[5] * m[2] * m[11]
        - m[5] * m[3] * m[10]
        - m[9] * m[2] * m[7]
        + m[9] * m[3] * m[6];
    out[7] = m[0] * m[6] * m[11] - m[0] * m[7] * m[10] - m[4] * m[2] * m[11]
        + m[4] * m[3] * m[10]
        + m[8] * m[2] * m[7]
        - m[8] * m[3] * m[6];
    out[11] = -m[0] * m[5] * m[11] + m[0] * m[7] * m[9] + m[4] * m[1] * m[11]
        - m[4] * m[3] * m[9]
        - m[8] * m[1] * m[7]
        + m[8] * m[3] * m[5];
    out[15] = m[0] * m[5] * m[10] - m[0] * m[6] * m[9] - m[4] * m[1] * m[10]
        + m[4] * m[2] * m[9]
        + m[8] * m[1] * m[6]
        - m[8] * m[2] * m[5];

    let determinant = m[0] * out[0] + m[1] * out[4] + m[2] * out[8] + m[3] * out[12];
    if determinant.abs() < 1e-20 || !determinant.is_finite() {
        return None;
    }
    let scale = 1.0 / determinant;
    for value in out.iter_mut() {
        *value *= scale;
    }
    Some(out)
}

/// Halton low-discrepancy sequence, the standard choice for jitter phases.
pub fn halton(mut index: u32, base: u32) -> f32 {
    let mut result = 0.0f32;
    let mut fraction = 1.0f32;
    while index > 0 {
        fraction /= base as f32;
        result += fraction * (index % base) as f32;
        index /= base;
    }
    result
}

/// Sub-pixel offset for a frame, in pixels, centred on zero.
pub fn jitter_pixels(frame: u64, phases: u32) -> (f32, f32) {
    let index = (frame % phases as u64) as u32 + 1;
    (halton(index, 2) - 0.5, halton(index, 3) - 0.5)
}

// ---------------------------------------------------------------------------
// The camera as scalars and vectors, for consumers that want those
// ---------------------------------------------------------------------------

/// The engine's own camera fields in `CBViewProjection`, by the names and
/// offsets its shader reflection gives them. Read from a logged dump of the
/// buffer in this game:
///
/// ```text
///   fCameraPos      +0x180   three floats, world space
///   fCameraDir      +0x190   three floats, world space, unit length
///   fCameraNearClip +0x1ac
///   fCameraFarClip  +0x1b0
/// ```
///
/// In that dump `fCameraPos` equals the fourth row of `fViewI` and
/// `fCameraDir` equals the negated third row, to every printed digit.
pub const F_CAMERA_POS: usize = 0x180;
pub const F_CAMERA_DIR: usize = 0x190;
pub const F_CAMERA_NEAR_CLIP: usize = 0x1ac;
pub const F_CAMERA_FAR_CLIP: usize = 0x1b0;

pub fn read_float(bytes: &[u8], offset: usize) -> Option<f32> {
    let word = bytes.get(offset..offset + 4)?;
    Some(f32::from_le_bytes([word[0], word[1], word[2], word[3]]))
}

pub fn read_vector(bytes: &[u8], offset: usize) -> Option<[f32; 3]> {
    Some([
        read_float(bytes, offset)?,
        read_float(bytes, offset + 4)?,
        read_float(bytes, offset + 8)?,
    ])
}

/// What a perspective projection says about the lens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lens {
    /// Distance to the near plane, in view-space units. Always finite.
    pub near: f32,
    /// Distance to the far plane, or infinity when the projection has none.
    pub far: f32,
    /// **Vertical** field of view, in radians.
    pub fov_vertical: f32,
    /// Width over height.
    pub aspect: f32,
    /// Whether depth is 1 at the near plane and 0 at the far one.
    pub reversed: bool,
}

/// Read the lens back out of a perspective projection.
///
/// Row-major, row vectors, so a view-space point `(x, y, z, 1)` becomes
///
/// ```text
///   clip.x = x * P00        clip.z = z * P22 + P32
///   clip.y = y * P11        clip.w = z * P23          P23 is -1 or +1
/// ```
///
/// `P23 = -1` is a camera looking down **negative** z, which is what this
/// engine has; the distance in front of the camera is then `-z`. Depth is
/// `clip.z / clip.w`. For a point at distance `d`, `z = d * P23`, so
///
/// ```text
///   depth = P23 * P22 + P32 / d
///   depth = 0   at   d0 = -P32 / (P23 * P22)
///   depth = 1   at   d1 =  P32 / (1 - P23 * P22)
/// ```
///
/// The nearer of the two is the near plane whichever way the depth runs, and
/// if depth is 1 there the projection is reversed. `P22 == 0` has no plane at
/// depth 0, which is an infinite far plane and is reported as infinity rather
/// than replaced by a number.
///
/// Nothing here assumes the engine's convention beyond the shape
/// `looks_like_projection_offset` checks, and anything that does not come out
/// as a positive near plane in front of a further far plane is `None`.
pub fn lens(projection: &Matrix) -> Option<Lens> {
    let p00 = projection[0] as f64;
    let p11 = projection[5] as f64;
    let p22 = projection[10] as f64;
    let p23 = projection[11] as f64;
    let p32 = projection[14] as f64;
    if !(p00.is_finite() && p11.is_finite() && p22.is_finite() && p32.is_finite()) {
        return None;
    }
    if p00 <= 0.0 || p11 <= 0.0 || (p23.abs() - 1.0).abs() > 1e-6 || projection[15] != 0.0 {
        return None;
    }
    let s = p23 * p22;
    // Where depth reads 1, and where it reads 0.
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
    let usable = |d: f64| d > 0.0;
    if !usable(one) || !usable(zero) {
        return None;
    }
    let (near, far, reversed) = if one < zero {
        (one, zero, true)
    } else {
        (zero, one, false)
    };
    if !near.is_finite() || near <= 0.0 || far <= near {
        return None;
    }
    Some(Lens {
        near: near as f32,
        far: far as f32,
        fov_vertical: (2.0 * (1.0 / p11).atan()) as f32,
        aspect: (p11 / p00) as f32,
        reversed,
    })
}

/// Where the camera is and which way it faces, in world space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    pub position: [f32; 3],
    pub right: [f32; 3],
    pub up: [f32; 3],
    pub forward: [f32; 3],
}

/// Read the pose out of an **inverse view** matrix (view space to world).
///
/// With row vectors, the first three rows of a rigid view-to-world matrix are
/// the images of view-space x, y and z in world space, and the fourth is the
/// image of the origin: the camera's position. View-space x is right and y is
/// up. View-space z points *behind* a camera that looks down negative z, so
/// forward is the negated third row then, and the third row itself otherwise
/// -- which the projection's `P23` says, and is passed in rather than assumed.
///
/// Returns `None` unless the three axes are unit length and mutually
/// perpendicular to within a thousandth. A basis that is not orthonormal is
/// not handed over: it would mean this is not the matrix it is taken to be.
pub fn pose(view_inverse: &Matrix, looks_down_negative_z: bool) -> Option<Pose> {
    let row = |index: usize| {
        [
            view_inverse[index * 4],
            view_inverse[index * 4 + 1],
            view_inverse[index * 4 + 2],
        ]
    };
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let (right, up, z) = (row(0), row(1), row(2));
    let position = row(3);
    // An affine matrix, or it is not a view-to-world transform at all.
    let last_column = [
        view_inverse[3],
        view_inverse[7],
        view_inverse[11],
        view_inverse[15] - 1.0,
    ];
    if last_column.iter().any(|value| value.abs() > 1e-4) {
        return None;
    }
    for axis in [right, up, z] {
        if !axis.iter().all(|value| value.is_finite()) || (dot(axis, axis) - 1.0).abs() > 1e-3 {
            return None;
        }
    }
    if dot(right, up).abs() > 1e-3 || dot(right, z).abs() > 1e-3 || dot(up, z).abs() > 1e-3 {
        return None;
    }
    if !position.iter().all(|value| value.is_finite()) {
        return None;
    }
    let forward = if looks_down_negative_z {
        [-z[0], -z[1], -z[2]]
    } else {
        z
    };
    Some(Pose {
        position,
        right,
        up,
        forward,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `fViewI`, `fCameraPos`, `fCameraDir`, `fCameraNearClip`,
    /// `fCameraFarClip` and the two diagonal entries of `fNoJitterProj`, as
    /// the engine uploaded them in one frame of this game: values copied
    /// from the mod's log, not invented.
    const VIEW_INVERSE: Matrix = [
        0.994944,
        -0.000000,
        0.100432,
        0.0, //
        0.026558,
        0.964404,
        -0.263097,
        0.0, //
        -0.096857,
        0.264434,
        0.959528,
        0.0, //
        -532.331_54,
        1_547.948_7,
        4_021.068,
        1.0,
    ];
    const CAMERA_POS: [f32; 3] = [-532.331_54, 1_547.948_7, 4_021.068];
    const CAMERA_DIR: [f32; 3] = [0.096857, -0.264434, -0.959528];
    const NEAR_CLIP: f32 = 16.0;
    const FAR_CLIP: f32 = 3_947_580.3;
    const P00: f32 = 4.936999;
    const P11: f32 = 8.776888;
    /// The log printed `P32` as 16.000065 and `P22` as 0.000004, which is one
    /// significant digit, so `P22` is rebuilt from the engine's own planes --
    /// and the test below checks that doing so reproduces the `P32` that was
    /// printed, which is what ties the formula to the measurement.
    const P32_PRINTED: f32 = 16.000065;

    fn engine_projection() -> Matrix {
        let (n, f) = (NEAR_CLIP as f64, FAR_CLIP as f64);
        let mut m = [0f32; 16];
        m[0] = P00;
        m[5] = P11;
        m[10] = (n / (f - n)) as f32;
        m[11] = -1.0;
        m[14] = (n * f / (f - n)) as f32;
        m
    }

    #[test]
    fn the_reversed_projection_formula_reproduces_the_logged_entry() {
        let m = engine_projection();
        assert!((m[14] - P32_PRINTED).abs() < 1e-5, "P32 = {}", m[14]);
        assert!((m[10] - 0.000004).abs() < 1e-6, "P22 = {}", m[10]);
        assert!(looks_like_projection(&m));
    }

    #[test]
    fn the_lens_comes_back_out_of_the_engines_projection() {
        let lens = lens(&engine_projection()).expect("a lens");
        assert!(lens.reversed);
        assert!((lens.near - NEAR_CLIP).abs() < 1e-3, "near {}", lens.near);
        assert!((lens.far / FAR_CLIP - 1.0).abs() < 1e-3, "far {}", lens.far);
        // 16:9, which is the window.
        assert!(
            (lens.aspect - 16.0 / 9.0).abs() < 1e-4,
            "aspect {}",
            lens.aspect
        );
        // 2 * atan(1 / 8.776888): 13 degrees, the camera of that frame.
        assert!(
            (lens.fov_vertical - 0.226_89).abs() < 1e-4,
            "fov {}",
            lens.fov_vertical
        );
    }

    #[test]
    fn jitter_in_the_third_row_does_not_move_the_planes() {
        let mut m = engine_projection();
        m[8] = -0.000195;
        m[9] = -0.000051;
        assert_eq!(lens(&m), lens(&engine_projection()));
    }

    #[test]
    fn a_conventional_projection_reads_the_other_way_round() {
        // Left-handed, depth 0 at near and 1 at far, as D3D documents it.
        let (n, f) = (0.5f32, 1000.0f32);
        let mut m = [0f32; 16];
        m[0] = 1.0;
        m[5] = 1.777_777_8;
        m[10] = f / (f - n);
        m[11] = 1.0;
        m[14] = -n * f / (f - n);
        let lens = lens(&m).expect("a lens");
        assert!(!lens.reversed);
        assert!((lens.near - n).abs() < 1e-4, "near {}", lens.near);
        assert!((lens.far - f).abs() < 0.5, "far {}", lens.far);
    }

    #[test]
    fn no_far_plane_is_reported_as_infinity_and_not_as_a_number() {
        let mut m = engine_projection();
        m[10] = 0.0;
        m[14] = 16.0;
        let lens = lens(&m).expect("a lens");
        assert!(lens.reversed);
        assert_eq!(lens.near, 16.0);
        assert!(lens.far.is_infinite());
    }

    #[test]
    fn something_that_is_not_a_perspective_projection_is_refused() {
        let identity: Matrix = [
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ];
        assert!(lens(&identity).is_none());
    }

    #[test]
    fn the_pose_agrees_with_the_engines_own_fields() {
        let pose = pose(&VIEW_INVERSE, true).expect("a pose");
        assert_eq!(pose.position, CAMERA_POS);
        for axis in 0..3 {
            assert!((pose.forward[axis] - CAMERA_DIR[axis]).abs() < 1e-6);
        }
        // Right-handed: right x up is the third row, which is *behind*.
        let cross = [
            pose.right[1] * pose.up[2] - pose.right[2] * pose.up[1],
            pose.right[2] * pose.up[0] - pose.right[0] * pose.up[2],
            pose.right[0] * pose.up[1] - pose.right[1] * pose.up[0],
        ];
        for axis in 0..3 {
            assert!((cross[axis] + pose.forward[axis]).abs() < 1e-5);
        }
    }

    #[test]
    fn a_basis_that_is_not_orthonormal_is_refused() {
        let mut skewed = VIEW_INVERSE;
        skewed[0] = 2.0;
        assert!(pose(&skewed, true).is_none());
        // A projection is not a view-to-world matrix either.
        assert!(pose(&engine_projection(), true).is_none());
    }
}
