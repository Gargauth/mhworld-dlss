//! RCAS sharpening (FidelityFX FSR 1 "robust contrast adaptive sharpening")
//! of the DLSS resolve, before the engine's remaining passes and the HUD.
//!
//! DLSS has not sharpened since 2.5.1 and the engine's own sharpeners are
//! tuned off under the mod, so the resolve reaches the screen softer than the
//! engine's own anti-aliasing. A ReShade sharpener at `Present` would also
//! sharpen the HUD and, with frame generation, the generated frames. So the
//! sharpening is done at the upscale slot: DLSS writes an intermediate texture
//! owned by this crate, one compute pass reads it and writes the engine's
//! target, and everything later (post, HUD, frame generation inputs) sees the
//! sharpened picture.
//!
//! The filter is RCAS: a five-tap cross whose negative lobe is limited per
//! pixel so it cannot clip (no ringing). RCAS expects a perceptual encoding
//! with values up to one; the picture here is linear and high-range, so each
//! tap is brought into `[0, 1)` by a reversible tonemap (`c / (1 + max(c))`)
//! and square-rooted, and both are undone on the way out (run on the linear
//! values it did next to nothing below the highlights, where texture is).
//! FSR's optional noise attenuation is left out, as FSR leaves it by
//! default: it halves the strength exactly where a pixel differs from its
//! neighbours, which is fine detail. The ring excludes the centre (FSR 1.0;
//! the later variant that includes it sharpens least where the centre stands
//! out, which is the detail). A non-finite tap counts as black so it cannot
//! spread; a non-finite centre is passed through.
//!
//! This module is the pure part (constants, formats, the shader text and a
//! CPU reference of the per-pixel filter); the pass itself is Windows-only
//! and re-exported here: `intermediate`, `record`.

use mhw_core::frame::Size;

#[cfg(windows)]
pub use crate::sys::sharpen::{intermediate, record, Intermediate, SharpenError};

/// Threads per group side (`[numthreads(8, 8, 1)]`).
pub const GROUP: u32 = 8;

/// `FSR_RCAS_LIMIT`: the strongest lobe RCAS ever applies, `0.25 - 1/16`.
pub const LIMIT: f32 = 0.1875;

/// Formats the pass writes through a typed UAV of the same format:
/// `R11G11B10_FLOAT` (the engine's targets), `R16G16B16A16_FLOAT`,
/// `R10G10B10A2_UNORM`, `R8G8B8A8_UNORM`. All support typed UAV stores
/// on every D3D12 device.
pub const WRITABLE_FORMATS: [u32; 4] = [26, 10, 24, 28];

/// Whether the pass can write a target of `format`.
pub fn writable(format: u32) -> bool {
    WRITABLE_FORMATS.contains(&format)
}

/// The lobe scale for a `Sharpness` setting: clamped to `[0, 1]`, anything
/// that is not a number is 0 (off). 1 is FSR's maximum (0 stops).
pub fn amount(sharpness: f32) -> f32 {
    if sharpness.is_nan() {
        0.0
    } else {
        sharpness.clamp(0.0, 1.0)
    }
}

/// FSR's own parameterisation (`FsrRcasCon`): `stops` of attenuation from
/// the maximum, `exp2(-stops)`. `amount_from_stops(0) == 1`,
/// `amount_from_stops(1) == 0.5`.
pub fn amount_from_stops(stops: f32) -> f32 {
    amount((-stops.max(0.0)).exp2())
}

/// The pass's root constants (`cbuffer Params : register(b0)`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RcasConstants {
    /// Lobe scale, see [`amount`].
    pub amount: f32,
    /// Target width.
    pub width: u32,
    /// Target height.
    pub height: u32,
}

impl RcasConstants {
    /// Root constant count.
    pub const DWORDS: u32 = 4;

    /// Constants for `sharpness` at `size`.
    pub fn new(sharpness: f32, size: Size) -> RcasConstants {
        RcasConstants {
            amount: amount(sharpness),
            width: size.w,
            height: size.h,
        }
    }

    /// As the shader reads them: `float Amount; uint Width; uint Height;
    /// uint Pad`.
    pub fn to_dwords(&self) -> [u32; 4] {
        [self.amount.to_bits(), self.width, self.height, 0]
    }

    /// Thread groups covering the target.
    pub fn groups(&self) -> (u32, u32) {
        (self.width.div_ceil(GROUP), self.height.div_ceil(GROUP))
    }
}

/// The compute shader (`cs_5_0`, entry `main`): `t0` the resolve, `u0` the
/// target, `b0` [`RcasConstants`]. Mirrors [`reference`].
pub fn hlsl() -> &'static str {
    SHADER
}

const SHADER: &str = r#"
Texture2D<float4>   Resolved : register(t0);
RWTexture2D<float4> Target   : register(u0);

cbuffer Params : register(b0)
{
    float Amount;
    uint  Width;
    uint  Height;
    uint  Pad;
};

float peak(float3 c) { return max(c.r, max(c.g, c.b)); }

bool finite3(float3 c) { return isfinite(c.r) && isfinite(c.g) && isfinite(c.b); }

// Into [0, 1) by a reversible tonemap, then into a perceptual encoding.
float3 Tap(int2 at, int2 last)
{
    float3 c = Resolved[clamp(at, int2(0, 0), last)].rgb;
    c = finite3(c) ? max(c, 0.0) : float3(0.0, 0.0, 0.0);
    return sqrt(c / (1.0 + peak(c)));
}

[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    if (id.x >= Width || id.y >= Height) { return; }
    int2 at = int2(id.xy);
    int2 last = int2(Width - 1, Height - 1);
    float4 centre = Resolved[at];
    if (!finite3(centre.rgb)) { Target[at] = centre; return; }

    //    b
    //  d e f
    //    h
    float3 b = Tap(at + int2(0, -1), last);
    float3 d = Tap(at + int2(-1, 0), last);
    float3 e = Tap(at, last);
    float3 f = Tap(at + int2(1, 0), last);
    float3 h = Tap(at + int2(0, 1), last);

    float3 smallest = min(min(b, d), min(f, h));
    float3 largest = max(max(b, d), max(f, h));
    float3 hit_min = smallest / max(4.0 * largest, 1e-5);
    float3 hit_max = (1.0 - largest) / min(4.0 * smallest - 4.0, -1e-5);
    float3 lobes = max(-hit_min, hit_max);
    float lobe = max(-0.1875, min(peak(lobes), 0.0)) * Amount;

    float3 sharp = (lobe * (b + d + f + h) + e) / (4.0 * lobe + 1.0);
    sharp = clamp(sharp, 0.0, 0.9999);
    sharp *= sharp;
    Target[at] = float4(sharp / (1.0 - peak(sharp)), centre.a);
}
"#;

fn peak(c: [f32; 3]) -> f32 {
    c[0].max(c[1]).max(c[2])
}

fn tap(c: [f32; 3]) -> [f32; 3] {
    let c = if c.iter().all(|v| v.is_finite()) {
        c.map(|v| v.max(0.0))
    } else {
        [0.0; 3]
    };
    let scale = 1.0 + peak(c);
    c.map(|v| (v / scale).sqrt())
}

/// CPU reference of the shader for one pixel: `centre` and its four
/// neighbours `[up, left, right, down]`, linear RGB. Used by the tests to
/// pin the filter's properties.
pub fn reference(centre: [f32; 3], ring: [[f32; 3]; 4], amount: f32) -> [f32; 3] {
    if !centre.iter().all(|v| v.is_finite()) {
        return centre;
    }
    let [b, d, f, h] = ring.map(tap);
    let e = tap(centre);
    let mut out = [0.0; 3];
    let mut lobes = [0.0f32; 3];
    for i in 0..3 {
        let smallest = b[i].min(d[i]).min(f[i]).min(h[i]);
        let largest = b[i].max(d[i]).max(f[i]).max(h[i]);
        let hit_min = smallest / (4.0 * largest).max(1e-5);
        let hit_max = (1.0 - largest) / (4.0 * smallest - 4.0).min(-1e-5);
        lobes[i] = (-hit_min).max(hit_max);
    }
    let lobe = peak(lobes).clamp(-LIMIT, 0.0) * amount;
    for i in 0..3 {
        let sharp = (lobe * (b[i] + d[i] + f[i] + h[i]) + e[i]) / (4.0 * lobe + 1.0);
        let sharp = sharp.clamp(0.0, 0.9999);
        out[i] = sharp * sharp;
    }
    let scale = 1.0 - peak(out);
    out.map(|v| v / scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter()
            .zip(b)
            .all(|(x, y)| (x - y).abs() <= 1e-4 * (1.0 + y.abs()))
    }

    #[test]
    fn sharpness_maps_to_a_bounded_lobe_scale() {
        assert_eq!(amount(0.5), 0.5);
        assert_eq!(amount(-1.0), 0.0);
        assert_eq!(amount(7.0), 1.0);
        assert_eq!(amount(f32::NAN), 0.0);
        assert_eq!(amount_from_stops(0.0), 1.0);
        assert_eq!(amount_from_stops(1.0), 0.5);
        assert_eq!(amount_from_stops(-3.0), 1.0, "no stops above FSR's maximum");
    }

    #[test]
    fn constants_are_laid_out_as_the_shader_reads_them() {
        let c = RcasConstants::new(0.25, Size::new(1921, 1080));
        assert_eq!(c.to_dwords(), [0.25f32.to_bits(), 1921, 1080, 0]);
        assert_eq!(c.groups(), (241, 135));
        assert_eq!(RcasConstants::DWORDS as usize, c.to_dwords().len());
    }

    #[test]
    fn only_typed_uav_formats_are_writable() {
        for format in [26, 10, 24, 28] {
            assert!(writable(format));
        }
        // Typeless, depth, BGRA, single channel, unknown.
        for format in [0, 27, 40, 87, 61] {
            assert!(!writable(format));
        }
    }

    #[test]
    fn flat_regions_and_zero_amount_are_left_alone() {
        let flat = [0.3, 2.0, 0.01];
        assert!(close(reference(flat, [flat; 4], 1.0), flat));
        let ring = [[0.0; 3], [1.0; 3], [0.2; 3], [5.0; 3]];
        assert!(close(reference([0.7; 3], ring, 0.0), [0.7; 3]));
    }

    #[test]
    fn sharpening_increases_local_contrast_without_ringing() {
        // A bright centre on a darker ring gets brighter, but never beyond
        // what the lobe limit allows: in the perceptual domain the result
        // stays within [0, 1) and is finite.
        let ring = [[0.2; 3]; 4];
        let out = reference([0.5; 3], ring, 1.0);
        assert!(out[0] > 0.5, "{out:?}");
        assert!(out.iter().all(|v| v.is_finite() && *v >= 0.0));
        // A dark centre on a bright ring gets darker but not negative.
        let out = reference([0.1; 3], [[3.0; 3]; 4], 1.0);
        assert!(out[0] < 0.1 && out[0] >= 0.0, "{out:?}");
        // Stronger amount, stronger effect.
        let weak = reference([0.5; 3], ring, 0.25);
        assert!(weak[0] > 0.5 && weak[0] < reference([0.5; 3], ring, 1.0)[0]);
    }

    #[test]
    fn non_finite_input_does_not_spread() {
        let nan = [f32::NAN, 0.0, 0.0];
        let centre = reference(nan, [[0.5; 3]; 4], 1.0);
        assert!(centre[0].is_nan(), "a non-finite centre passes through");
        let out = reference([0.5; 3], [nan, [0.5; 3], [0.5; 3], [f32::INFINITY; 3]], 1.0);
        assert!(out.iter().all(|v| v.is_finite()), "{out:?}");
    }
}
