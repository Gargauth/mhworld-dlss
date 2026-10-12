//! `probe.mv`: the game's dense velocity (`taa+0x1a8`, the supersampling
//! path's motion texture) against the temporal pass's `tGBuffer4`, decoded
//! to render pixels the way DLSS is given them (`pixels = ndc·(−w/2,
//! +h/2)`, `mhw_render::mvpack`). Per axis the least-squares scale `s`
//! with `game ≈ s·decoded` says the dense velocity's unit and sign.

use mhw_core::frame::Size;
use mhw_render::mvpack::ndc_to_pixels;
use serde::Serialize;

use super::{map_texel, sample_grid, Image};

/// Most texel pairs compared per frame.
pub const MAX_SAMPLES: usize = 1 << 16;

/// One axis' fit `game ≈ scale·reference`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct AxisFit {
    /// Least-squares scale.
    pub scale: f64,
    /// `±1` (sign of `scale`).
    pub sign: i8,
    /// Root-mean-square of `game − scale·reference`.
    pub residual: f64,
    /// `residual` relative to the RMS of `game` (0 = exact).
    pub relative: f64,
}

/// Fit `game ≈ s·reference` over `pairs`; `None` when the reference does
/// not move (no information about `s`).
pub fn fit_axis(pairs: &[(f64, f64)]) -> Option<AxisFit> {
    let (mut gt, mut tt, mut gg) = (0.0, 0.0, 0.0);
    for &(g, t) in pairs {
        gt += g * t;
        tt += t * t;
        gg += g * g;
    }
    if pairs.is_empty() || tt <= f64::EPSILON {
        return None;
    }
    let scale = gt / tt;
    let n = pairs.len() as f64;
    // Σ(g − s·t)² = Σg² − 2sΣgt + s²Σtt = Σg² − s·Σgt.
    let residual = ((gg - scale * gt).max(0.0) / n).sqrt();
    let game_rms = (gg / n).sqrt();
    Some(AxisFit {
        scale,
        sign: if scale < 0.0 { -1 } else { 1 },
        residual,
        relative: if game_rms > 0.0 {
            residual / game_rms
        } else {
            0.0
        },
    })
}

/// The comparison of one frame.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MvComparison {
    /// Texel pairs compared.
    pub samples: usize,
    /// `[x, y]` fits of game velocity per decoded render pixel.
    pub fit: [Option<AxisFit>; 2],
    /// `[x, y]` scales against the raw NDC values of `tGBuffer4`.
    pub scale_ndc: [Option<f64>; 2],
}

/// Compare `game` (dense velocity, two channels) with `packed`
/// (`tGBuffer4`), sampling the packed grid and the game texel at the same
/// relative position.
pub fn compare(game: &Image, packed: &Image, render: Size) -> Result<MvComparison, String> {
    if !game.has_vec2() {
        return Err(format!(
            "game motion format {} is not decoded",
            game.format_name()
        ));
    }
    if !packed.has_vec2() {
        return Err(format!(
            "tGBuffer4 format {} is not decoded",
            packed.format_name()
        ));
    }
    let per_px = ndc_to_pixels(render);
    let (mut xs, mut ys, mut xs_ndc, mut ys_ndc) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (x, y) in sample_grid(packed.width, packed.height, MAX_SAMPLES) {
        let (gx, gy) = map_texel(
            x,
            y,
            (packed.width, packed.height),
            (game.width, game.height),
        );
        let (Some(t), Some(g)) = (packed.vec2(x, y), game.vec2(gx, gy)) else {
            continue;
        };
        if !(t.iter().chain(&g)).all(|v| v.is_finite()) {
            continue;
        }
        let (g0, g1) = (f64::from(g[0]), f64::from(g[1]));
        xs.push((g0, f64::from(t[0] * per_px.x)));
        ys.push((g1, f64::from(t[1] * per_px.y)));
        xs_ndc.push((g0, f64::from(t[0])));
        ys_ndc.push((g1, f64::from(t[1])));
    }
    Ok(MvComparison {
        samples: xs.len(),
        fit: [fit_axis(&xs), fit_axis(&ys)],
        scale_ndc: [
            fit_axis(&xs_ndc).map(|f| f.scale),
            fit_axis(&ys_ndc).map(|f| f.scale),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::super::dxgi;
    use super::super::tests::image;
    use super::*;

    fn half(v: f32) -> u16 {
        // Exact for the small dyadic values used here.
        let bits = v.to_bits();
        let sign = ((bits >> 16) & 0x8000) as u16;
        if v == 0.0 {
            return sign;
        }
        let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
        sign | ((exp as u16) << 10) | ((bits >> 13) & 0x3ff) as u16
    }

    #[test]
    fn an_exact_relation_fits_with_zero_residual() {
        let f = fit_axis(&[(2.0, 1.0), (-4.0, -2.0), (0.0, 0.0)]).unwrap();
        assert_eq!(
            (f.scale, f.sign, f.residual, f.relative),
            (2.0, 1, 0.0, 0.0)
        );
        let f = fit_axis(&[(-1.0, 2.0), (1.0, -2.0)]).unwrap();
        assert_eq!((f.scale, f.sign), (-0.5, -1));
        assert_eq!(fit_axis(&[(1.0, 0.0)]), None);
        assert_eq!(fit_axis(&[]), None);
    }

    #[test]
    fn noise_shows_in_the_residual() {
        let f = fit_axis(&[(1.0, 1.0), (3.0, 2.0), (2.0, 3.0)]).unwrap();
        assert!(f.residual > 0.1 && f.relative > 0.0 && f.relative < 1.0);
    }

    #[test]
    fn the_game_texture_is_compared_in_decoded_pixels() {
        // tGBuffer4 at 8×4 holds NDC constant over 2×2 blocks; the game's
        // velocity at half size holds −pixels (sign flipped on both axes).
        let render = Size::new(8, 4);
        let ndc = |x: u32, y: u32| ((x / 2) as f32 / 16.0, -((y / 2) as f32) / 16.0);
        let packed = image(dxgi::R32_UINT, 8, 4, |x, y| {
            let (nx, ny) = ndc(x, y);
            ((u32::from(half(nx)) << 16) | u32::from(half(ny)))
                .to_le_bytes()
                .to_vec()
        });
        let px = ndc_to_pixels(render);
        let game = image(dxgi::R32G32_FLOAT, 4, 2, |x, y| {
            let (nx, ny) = ndc(x * 2, y * 2);
            [(-nx * px.x).to_le_bytes(), (-ny * px.y).to_le_bytes()].concat()
        });
        let c = compare(&game, &packed, render).unwrap();
        assert_eq!(c.samples, 32);
        let [x, y] = c.fit;
        let (x, y) = (x.unwrap(), y.unwrap());
        assert!((x.scale + 1.0).abs() < 1e-6 && x.sign == -1 && x.residual < 1e-6);
        assert!((y.scale + 1.0).abs() < 1e-6 && y.sign == -1);
        assert!(
            (c.scale_ndc[0].unwrap() - 4.0).abs() < 1e-5,
            "{:?}",
            c.scale_ndc
        );
        let colour = image(26, 1, 1, |_, _| vec![0; 4]);
        assert!(compare(&colour, &packed, render).is_err());
    }
}
