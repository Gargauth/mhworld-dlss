//! Probe arithmetic, independent of the game: decoding readback images,
//! summary statistics, the jitter divisor, the motion-vector fit and the
//! depth comparison.

pub mod depth;
pub mod jitter;
pub mod mv;

use mhw_render::mvpack::{unpack, YMask};
use serde::Serialize;

/// `DXGI_FORMAT` values the probes decode.
pub mod dxgi {
    /// `R32G8X24_TYPELESS` (depth 32F + stencil).
    pub const R32G8X24_TYPELESS: u32 = 19;
    /// `D32_FLOAT_S8X24_UINT`.
    pub const D32_FLOAT_S8X24_UINT: u32 = 20;
    /// `R32G32_FLOAT`.
    pub const R32G32_FLOAT: u32 = 16;
    /// `R16G16_FLOAT`.
    pub const R16G16_FLOAT: u32 = 34;
    /// `R32_TYPELESS`.
    pub const R32_TYPELESS: u32 = 39;
    /// `D32_FLOAT`.
    pub const D32_FLOAT: u32 = 40;
    /// `R32_FLOAT`.
    pub const R32_FLOAT: u32 = 41;
    /// `R32_UINT` (`tGBuffer4`: packed motion vectors).
    pub const R32_UINT: u32 = 42;
    /// `R24G8_TYPELESS`.
    pub const R24G8_TYPELESS: u32 = 44;
    /// `D24_UNORM_S8_UINT`.
    pub const D24_UNORM_S8_UINT: u32 = 45;
    /// `R24_UNORM_X8_TYPELESS`.
    pub const R24_UNORM_X8_TYPELESS: u32 = 46;
    /// `R16_TYPELESS`.
    pub const R16_TYPELESS: u32 = 53;
    /// `R16_FLOAT`.
    pub const R16_FLOAT: u32 = 54;
    /// `D16_UNORM`.
    pub const D16_UNORM: u32 = 55;
    /// `R16_UNORM`.
    pub const R16_UNORM: u32 = 56;
}

/// Bytes per texel of the formats the probes and the capture writer know.
pub fn bytes_per_texel(format: u32) -> Option<usize> {
    Some(match format {
        2 => 16,
        10 | 11 | 16 | 19 | 20 => 8,
        24..=29 | 34..=46 | 87 | 91 => 4,
        49 | 53..=57 => 2,
        61 | 62 => 1,
        _ => return None,
    })
}

/// One readback image (a captured resource).
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    /// Request it belongs to.
    pub request: u64,
    /// Resource name (`colour`, `depth`, …).
    pub name: String,
    /// `DXGI_FORMAT`.
    pub format: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Bytes between rows.
    pub row_pitch: u32,
    /// Frame it was recorded in.
    pub frame: u64,
    /// The rows (the last may omit its padding).
    pub bytes: Vec<u8>,
}

fn f16(bits: u16) -> f32 {
    mhw_render::mvpack::f16_to_f32(bits)
}

impl Image {
    fn texel(&self, x: u32, y: u32) -> Option<&[u8]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let bpp = bytes_per_texel(self.format)?;
        let at = (y as usize)
            .checked_mul(self.row_pitch as usize)?
            .checked_add((x as usize).checked_mul(bpp)?)?;
        self.bytes.get(at..at.checked_add(bpp)?)
    }

    /// The first channel as a number, for depth-like formats.
    pub fn scalar(&self, x: u32, y: u32) -> Option<f32> {
        use dxgi::*;
        let t = self.texel(x, y)?;
        let word = |i: usize| -> Option<[u8; 4]> { t.get(i..i + 4)?.try_into().ok() };
        Some(match self.format {
            R32_FLOAT | D32_FLOAT | R32_TYPELESS | R32G8X24_TYPELESS | D32_FLOAT_S8X24_UINT => {
                f32::from_le_bytes(word(0)?)
            }
            D24_UNORM_S8_UINT | R24G8_TYPELESS | R24_UNORM_X8_TYPELESS => {
                (u32::from_le_bytes(word(0)?) & 0x00ff_ffff) as f32 / 16_777_215.0
            }
            R16_FLOAT => f16(u16::from_le_bytes([t[0], t[1]])),
            R16_UNORM | D16_UNORM | R16_TYPELESS => {
                f32::from(u16::from_le_bytes([t[0], t[1]])) / 65535.0
            }
            _ => return None,
        })
    }

    /// Whether [`Image::scalar`] decodes this format.
    pub fn has_scalar(&self) -> bool {
        self.width > 0 && self.height > 0 && self.scalar(0, 0).is_some()
    }

    /// Two channels as a vector, for motion-vector formats. `R32_UINT` is
    /// `tGBuffer4`: x in the high half, y in the low half, both f16 NDC.
    pub fn vec2(&self, x: u32, y: u32) -> Option<[f32; 2]> {
        use dxgi::*;
        let t = self.texel(x, y)?;
        Some(match self.format {
            R16G16_FLOAT => [
                f16(u16::from_le_bytes([t[0], t[1]])),
                f16(u16::from_le_bytes([t[2], t[3]])),
            ],
            R32G32_FLOAT => [
                f32::from_le_bytes(t.get(0..4)?.try_into().ok()?),
                f32::from_le_bytes(t.get(4..8)?.try_into().ok()?),
            ],
            R32_UINT => {
                let v = unpack(
                    u32::from_le_bytes(t.get(0..4)?.try_into().ok()?),
                    YMask::Full,
                );
                [v.x, v.y]
            }
            _ => return None,
        })
    }

    /// Whether [`Image::vec2`] decodes this format.
    pub fn has_vec2(&self) -> bool {
        self.width > 0 && self.height > 0 && self.vec2(0, 0).is_some()
    }

    /// The format's name (`R32_FLOAT`).
    pub fn format_name(&self) -> String {
        mhw_render::names::format(self.format).into_owned()
    }
}

/// Positions of a grid sampled so that at most `max` points are visited:
/// every `step`-th texel in both directions.
pub fn sample_grid(width: u32, height: u32, max: usize) -> impl Iterator<Item = (u32, u32)> {
    let total = width as usize * height as usize;
    let step = ((total as f64 / max.max(1) as f64).sqrt().ceil() as u32).max(1);
    (0..height)
        .step_by(step as usize)
        .flat_map(move |y| (0..width).step_by(step as usize).map(move |x| (x, y)))
}

/// The texel of a `to` grid at the same relative position as `(x, y)` on a
/// `from` grid (nearest, by texel centres).
pub fn map_texel(x: u32, y: u32, from: (u32, u32), to: (u32, u32)) -> (u32, u32) {
    let map = |v: u32, f: u32, t: u32| -> u32 {
        if f == 0 || t == 0 {
            return 0;
        }
        let c = (f64::from(v) + 0.5) * f64::from(t) / f64::from(f);
        (c.floor() as u32).min(t - 1)
    };
    (map(x, from.0, to.0), map(y, from.1, to.1))
}

/// Summary statistics of a set of numbers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Summary {
    /// Finite values.
    pub count: usize,
    /// NaN or infinite values (excluded from the rest).
    pub nonfinite: usize,
    /// Smallest.
    pub min: f64,
    /// Largest.
    pub max: f64,
    /// Mean.
    pub mean: f64,
    /// Median.
    pub p50: f64,
    /// 99th percentile.
    pub p99: f64,
}

/// Summarise `values` (nearest-rank percentiles); `None` if no value is
/// finite.
pub fn summarize(values: impl IntoIterator<Item = f64>) -> Option<Summary> {
    let mut nonfinite = 0;
    let mut finite: Vec<f64> = values
        .into_iter()
        .filter(|v| {
            let ok = v.is_finite();
            nonfinite += usize::from(!ok);
            ok
        })
        .collect();
    if finite.is_empty() {
        return None;
    }
    finite.sort_by(f64::total_cmp);
    let rank =
        |p: f64| finite[((p * finite.len() as f64).ceil() as usize).clamp(1, finite.len()) - 1];
    Some(Summary {
        count: finite.len(),
        nonfinite,
        min: finite[0],
        max: finite[finite.len() - 1],
        mean: finite.iter().sum::<f64>() / finite.len() as f64,
        p50: rank(0.5),
        p99: rank(0.99),
    })
}

/// The median of `values` (ignoring non-finite ones).
pub fn median(values: impl IntoIterator<Item = f64>) -> Option<f64> {
    summarize(values).map(|s| s.p50)
}

/// The GPU colour reduction for one frame (`probe.hdr`), mirrored from
/// the render crate's capture service.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct ColourStats {
    /// Per-channel minimum.
    pub min: [f32; 3],
    /// Per-channel maximum.
    pub max: [f32; 3],
    /// Per-channel mean.
    pub mean: [f32; 3],
    /// Luma median.
    pub p50: f32,
    /// Luma 99th percentile.
    pub p99: f32,
    /// Luma 99.9th percentile.
    pub p999: f32,
    /// Fraction of pixels whose largest channel exceeds 1.0.
    pub above_one: f32,
    /// Pixels with a NaN or infinite channel.
    pub nan_or_inf: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn image(
        format: u32,
        width: u32,
        height: u32,
        texel: impl Fn(u32, u32) -> Vec<u8>,
    ) -> Image {
        let bpp = bytes_per_texel(format).unwrap();
        let pitch = (width as usize * bpp).div_ceil(256) * 256;
        let mut bytes = vec![0u8; pitch * height as usize];
        for y in 0..height {
            for x in 0..width {
                let at = y as usize * pitch + x as usize * bpp;
                bytes[at..at + bpp].copy_from_slice(&texel(x, y));
            }
        }
        Image {
            request: 1,
            name: "t".into(),
            format,
            width,
            height,
            row_pitch: pitch as u32,
            frame: 0,
            bytes,
        }
    }

    #[test]
    fn texels_decode_by_format_and_respect_the_pitch() {
        let img = image(dxgi::R32_FLOAT, 3, 2, |x, y| {
            ((x + 10 * y) as f32).to_le_bytes().to_vec()
        });
        assert_eq!(img.scalar(2, 1), Some(12.0));
        assert_eq!(img.scalar(3, 0), None);
        assert_eq!(img.vec2(0, 0), None);
        let d24 = image(dxgi::D24_UNORM_S8_UINT, 1, 1, |_, _| {
            0xab_ff_ff_ffu32.to_le_bytes().to_vec()
        });
        assert_eq!(d24.scalar(0, 0), Some(1.0));
        // tGBuffer4: x = high half, y = low half (1.0 = 0x3c00, -0.5 = 0xb800).
        let packed = image(dxgi::R32_UINT, 1, 1, |_, _| {
            0x3c00_b800u32.to_le_bytes().to_vec()
        });
        assert_eq!(packed.vec2(0, 0), Some([1.0, -0.5]));
        let half = image(dxgi::R16G16_FLOAT, 1, 1, |_, _| {
            vec![0x00, 0x3c, 0x00, 0xc0]
        });
        assert_eq!(half.vec2(0, 0), Some([1.0, -2.0]));
        // A short buffer (last row without padding) still decodes its texels.
        let mut short = img.clone();
        short.bytes.truncate(short.row_pitch as usize + 12);
        assert_eq!(short.scalar(2, 1), Some(12.0));
        short.bytes.truncate(short.row_pitch as usize + 11);
        assert_eq!(short.scalar(2, 1), None);
    }

    #[test]
    fn grids_are_sampled_within_the_budget() {
        let n = sample_grid(1920, 1080, 10_000).count();
        assert!(n <= 10_000 && n > 5_000, "{n}");
        assert_eq!(sample_grid(4, 4, 100).count(), 16);
        assert_eq!(sample_grid(0, 4, 100).count(), 0);
    }

    #[test]
    fn texels_map_between_grids_by_centre() {
        assert_eq!(map_texel(0, 0, (4, 4), (2, 2)), (0, 0));
        assert_eq!(map_texel(3, 3, (4, 4), (2, 2)), (1, 1));
        assert_eq!(map_texel(1, 2, (2, 4), (4, 8)), (3, 5));
        assert_eq!(map_texel(5, 5, (8, 8), (0, 0)), (0, 0));
    }

    #[test]
    fn summaries_use_nearest_rank_and_skip_non_finite_values() {
        let s = summarize((1..=100).map(f64::from).chain([f64::NAN, f64::INFINITY])).unwrap();
        assert_eq!((s.count, s.nonfinite), (100, 2));
        assert_eq!(
            (s.min, s.max, s.mean, s.p50, s.p99),
            (1.0, 100.0, 50.5, 50.0, 99.0)
        );
        assert_eq!(summarize([f64::NAN]), None);
        assert_eq!(median([3.0, 1.0, 2.0]), Some(2.0));
    }
}
