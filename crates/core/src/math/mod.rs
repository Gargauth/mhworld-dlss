//! Vectors, 4×4 matrices and the engine's camera/jitter conventions.
//!
//! Matrices are **row-major with row vectors**, as the engine stores and
//! multiplies them (`docs/re/ReJitterCamera.md`): a point transforms as
//! `v·M`, `a * b` is "first `a`, then `b`", and the 16 floats in memory are
//! `rows[0][0..4], rows[1][0..4], …`. Element `[8]` in the engine's flat
//! notation is `rows[2][0]`.

pub mod camera;
pub mod jitter;

use std::ops::{Add, Mul, Neg, Sub};

pub use self::camera::{
    camera_cut, camera_cut_metric, clip_to_prev_clip, jitter_matrix, lens, CutMetric,
    CutThresholds, Lens, CUT_THRESHOLDS, DEPTH_FLIP,
};
pub use self::jitter::{
    dlss_jitter, engine_jitter_px, engine_jitter_units, halton, streamline_jitter, JitterScale,
    JitterSequence,
};

/// A 2D vector.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec2 {
    /// Horizontal component.
    pub x: f32,
    /// Vertical component.
    pub y: f32,
}

/// A 3D vector.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec3 {
    /// X component.
    pub x: f32,
    /// Y component.
    pub y: f32,
    /// Z component.
    pub z: f32,
}

/// A 4D (homogeneous) vector.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec4 {
    /// X component.
    pub x: f32,
    /// Y component.
    pub y: f32,
    /// Z component.
    pub z: f32,
    /// Homogeneous component.
    pub w: f32,
}

impl Vec2 {
    /// `(0, 0)`.
    pub const ZERO: Vec2 = Vec2 { x: 0.0, y: 0.0 };

    /// `(x, y)`.
    pub const fn new(x: f32, y: f32) -> Vec2 {
        Vec2 { x, y }
    }

    /// Whether both components are finite.
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

impl Vec3 {
    /// `(0, 0, 0)`.
    pub const ZERO: Vec3 = Vec3 {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };

    /// `(x, y, z)`.
    pub const fn new(x: f32, y: f32, z: f32) -> Vec3 {
        Vec3 { x, y, z }
    }

    /// The dot product.
    pub fn dot(self, other: Vec3) -> f32 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    /// The Euclidean length.
    pub fn length(self) -> f32 {
        self.dot(self).sqrt()
    }

    /// Whether all components are finite.
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }

    /// The point as a homogeneous vector with `w = 1`.
    pub const fn point(self) -> Vec4 {
        Vec4::new(self.x, self.y, self.z, 1.0)
    }
}

impl Vec4 {
    /// `(x, y, z, w)`.
    pub const fn new(x: f32, y: f32, z: f32, w: f32) -> Vec4 {
        Vec4 { x, y, z, w }
    }

    /// `xyz / w`.
    pub fn project(self) -> Vec3 {
        Vec3::new(self.x / self.w, self.y / self.w, self.z / self.w)
    }
}

macro_rules! vector_ops {
    ($type:ident { $($field:ident),+ }) => {
        impl Add for $type {
            type Output = $type;
            fn add(self, other: $type) -> $type {
                $type { $($field: self.$field + other.$field),+ }
            }
        }
        impl Sub for $type {
            type Output = $type;
            fn sub(self, other: $type) -> $type {
                $type { $($field: self.$field - other.$field),+ }
            }
        }
        impl Mul<f32> for $type {
            type Output = $type;
            fn mul(self, scale: f32) -> $type {
                $type { $($field: self.$field * scale),+ }
            }
        }
        impl Neg for $type {
            type Output = $type;
            fn neg(self) -> $type {
                $type { $($field: -self.$field),+ }
            }
        }
    };
}

vector_ops!(Vec2 { x, y });
vector_ops!(Vec3 { x, y, z });
vector_ops!(Vec4 { x, y, z, w });

/// A 4×4 matrix, row-major, row vectors (`v·M`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mat4 {
    /// `rows[r][c]`: row `r`, column `c`.
    pub rows: [[f32; 4]; 4],
}

impl Default for Mat4 {
    fn default() -> Mat4 {
        Mat4::IDENTITY
    }
}

impl Mat4 {
    /// The identity.
    pub const IDENTITY: Mat4 = Mat4 {
        rows: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ],
    };

    /// From four rows.
    pub const fn from_rows(rows: [[f32; 4]; 4]) -> Mat4 {
        Mat4 { rows }
    }

    /// From 16 floats in memory order (row-major).
    pub const fn from_array(m: [f32; 16]) -> Mat4 {
        Mat4 {
            rows: [
                [m[0], m[1], m[2], m[3]],
                [m[4], m[5], m[6], m[7]],
                [m[8], m[9], m[10], m[11]],
                [m[12], m[13], m[14], m[15]],
            ],
        }
    }

    /// The 16 floats in memory order.
    pub fn to_array(&self) -> [f32; 16] {
        let mut out = [0.0; 16];
        for (index, value) in out.iter_mut().enumerate() {
            *value = self.rows[index / 4][index % 4];
        }
        out
    }

    /// Read 64 little-endian bytes as stored by the engine; `None` if
    /// `bytes` is shorter.
    pub fn from_le_bytes(bytes: &[u8]) -> Option<Mat4> {
        let bytes = bytes.get(..64)?;
        let mut m = [0.0f32; 16];
        for (value, word) in m.iter_mut().zip(bytes.as_chunks::<4>().0) {
            *value = f32::from_le_bytes(*word);
        }
        Some(Mat4::from_array(m))
    }

    /// Element in the engine's flat notation (`[8]` = row 2, column 0);
    /// `index` is taken modulo 16.
    pub fn flat(&self, index: usize) -> f32 {
        self.rows[(index / 4) % 4][index % 4]
    }

    /// Rows become columns (for consumers with column vectors).
    pub fn transpose(&self) -> Mat4 {
        let mut out = [[0.0f32; 4]; 4];
        for (r, row) in out.iter_mut().enumerate() {
            for (c, value) in row.iter_mut().enumerate() {
                *value = self.rows[c][r];
            }
        }
        Mat4 { rows: out }
    }

    /// The row vector `v·M`.
    pub fn transform(&self, v: Vec4) -> Vec4 {
        let column = |c: usize| {
            v.x * self.rows[0][c]
                + v.y * self.rows[1][c]
                + v.z * self.rows[2][c]
                + v.w * self.rows[3][c]
        };
        Vec4::new(column(0), column(1), column(2), column(3))
    }

    /// Whether every element is finite.
    pub fn is_finite(&self) -> bool {
        self.rows.iter().flatten().all(|value| value.is_finite())
    }

    /// Whether every element is within `epsilon` of `other`'s.
    pub fn approx_eq(&self, other: &Mat4, epsilon: f32) -> bool {
        self.rows
            .iter()
            .flatten()
            .zip(other.rows.iter().flatten())
            .all(|(a, b)| (a - b).abs() <= epsilon)
    }

    /// The general inverse (Gauss–Jordan with partial pivoting in `f64`), or
    /// `None` for a singular or non-finite matrix.
    ///
    /// General on purpose: nothing about the engine's matrices (affine view,
    /// perspective projection) is assumed, so an unexpected matrix cannot
    /// get a wrong inverse.
    pub fn inverse(&self) -> Option<Mat4> {
        if !self.is_finite() {
            return None;
        }
        let mut a = [[0.0f64; 8]; 4];
        for ((r, row), source) in a.iter_mut().enumerate().zip(self.rows) {
            for (value, s) in row.iter_mut().zip(source) {
                *value = f64::from(s);
            }
            row[4 + r] = 1.0;
        }
        let scale = self
            .rows
            .iter()
            .flatten()
            .fold(0.0f64, |max, v| max.max(f64::from(*v).abs()));
        if scale == 0.0 {
            return None;
        }
        for column in 0..4 {
            let pivot =
                (column..4).max_by(|&i, &j| a[i][column].abs().total_cmp(&a[j][column].abs()))?;
            if a[pivot][column].abs() <= scale * 1e-12 {
                return None;
            }
            a.swap(column, pivot);
            let divisor = a[column][column];
            for value in a[column].iter_mut() {
                *value /= divisor;
            }
            let pivot_row = a[column];
            for (r, row) in a.iter_mut().enumerate() {
                if r != column {
                    let factor = row[column];
                    for (value, p) in row.iter_mut().zip(pivot_row) {
                        *value -= factor * p;
                    }
                }
            }
        }
        let mut out = [[0.0f32; 4]; 4];
        for (r, row) in out.iter_mut().enumerate() {
            for (c, value) in row.iter_mut().enumerate() {
                *value = a[r][4 + c] as f32;
            }
        }
        let inverse = Mat4 { rows: out };
        inverse.is_finite().then_some(inverse)
    }
}

impl Mul for Mat4 {
    type Output = Mat4;

    /// `self · rhs`: with row vectors, first `self`, then `rhs`.
    fn mul(self, rhs: Mat4) -> Mat4 {
        let mut out = [[0.0f32; 4]; 4];
        for (r, row) in out.iter_mut().enumerate() {
            for (c, value) in row.iter_mut().enumerate() {
                *value = (0..4).map(|k| self.rows[r][k] * rhs.rows[k][c]).sum();
            }
        }
        Mat4 { rows: out }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Mat4 {
        Mat4::from_array([
            2.0, 0.5, 0.0, 1.0, //
            -1.0, 3.0, 0.25, 0.0, //
            0.0, 1.0, 4.0, -2.0, //
            7.0, -3.0, 2.0, 1.0,
        ])
    }

    #[test]
    fn memory_order_is_row_major() {
        let m = Mat4::from_array(std::array::from_fn(|i| i as f32));
        assert_eq!(m.rows[2][0], 8.0);
        assert_eq!(m.flat(8), 8.0);
        assert_eq!(m.to_array()[13], 13.0);
        let bytes: Vec<u8> = m.to_array().iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(Mat4::from_le_bytes(&bytes), Some(m));
        assert_eq!(Mat4::from_le_bytes(&bytes[..63]), None);
    }

    #[test]
    fn multiplication_composes_left_to_right_for_row_vectors() {
        // Engine: out[0] = A[0]·B[0] + A[1]·B[4] + … (0x14028e28a).
        let a = sample();
        let b = sample().transpose();
        let ab = a * b;
        let expected: f32 = (0..4).map(|k| a.flat(k) * b.flat(k * 4)).sum();
        assert_eq!(ab.flat(0), expected);
        let v = Vec4::new(1.0, -2.0, 0.5, 1.0);
        let stepwise = b.transform(a.transform(v));
        let combined = ab.transform(v);
        assert!((stepwise - combined).x.abs() < 1e-4 && (stepwise - combined).w.abs() < 1e-4);
    }

    #[test]
    fn the_inverse_undoes_the_matrix() {
        let m = sample();
        let inverse = m.inverse().unwrap();
        assert!((m * inverse).approx_eq(&Mat4::IDENTITY, 1e-5));
        assert!((inverse * m).approx_eq(&Mat4::IDENTITY, 1e-5));
        assert_eq!(Mat4::IDENTITY.inverse(), Some(Mat4::IDENTITY));
    }

    #[test]
    fn singular_and_non_finite_matrices_have_no_inverse() {
        let mut singular = sample();
        singular.rows[3] = singular.rows[1];
        assert_eq!(singular.inverse(), None);
        assert_eq!(Mat4::from_array([0.0; 16]).inverse(), None);
        let mut nan = sample();
        nan.rows[1][2] = f32::NAN;
        assert_eq!(nan.inverse(), None);
    }

    #[test]
    fn a_projection_with_a_huge_far_plane_still_inverts() {
        // The engine's reversed projection (near 16, far ~3.9e6): tiny P22.
        let (n, f) = (16.0f64, 3_947_580.3f64);
        let p = Mat4::from_rows([
            [4.936_999, 0.0, 0.0, 0.0],
            [0.0, 8.776_888, 0.0, 0.0],
            [0.0, 0.0, (n / (f - n)) as f32, -1.0],
            [0.0, 0.0, (n * f / (f - n)) as f32, 0.0],
        ]);
        let inverse = p.inverse().unwrap();
        let point = Vec4::new(3.0, -2.0, -500.0, 1.0);
        let back = inverse.transform(p.transform(point));
        assert!((back.project() - Vec3::new(3.0, -2.0, -500.0)).length() < 1e-2);
    }
}
