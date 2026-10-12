//! `probe.depth`: the supersampling path's R32F depth copy against the
//! depth the temporal pass's DSV picked (`mhw_render::depth`): sizes,
//! formats, value ranges and the per-texel difference.

use serde::Serialize;

use super::{map_texel, sample_grid, summarize, Image, Summary};

/// Most texels compared per frame.
pub const MAX_SAMPLES: usize = 1 << 16;

/// One side of the comparison.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DepthSide {
    /// `[w, h]`.
    pub size: [u32; 2],
    /// Format name.
    pub format: String,
    /// Value statistics, when the format decodes.
    pub values: Option<Summary>,
}

/// One frame's comparison.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DepthComparison {
    /// The SS path's depth copy.
    pub copy: DepthSide,
    /// The picked DSV depth.
    pub dsv: DepthSide,
    /// `|copy − dsv|` statistics over texels at the same relative position.
    pub diff: Option<Summary>,
    /// Texels where `copy ≈ 1 − dsv` better than `copy ≈ dsv` (a flipped
    /// depth convention), as a fraction of the compared ones.
    pub flipped_fraction: Option<f64>,
}

fn side(image: &Image) -> DepthSide {
    let values = image.has_scalar().then(|| {
        summarize(
            sample_grid(image.width, image.height, MAX_SAMPLES)
                .filter_map(|(x, y)| image.scalar(x, y))
                .map(f64::from),
        )
    });
    DepthSide {
        size: [image.width, image.height],
        format: image.format_name(),
        values: values.flatten(),
    }
}

/// Compare the two depth images.
pub fn compare(copy: &Image, dsv: &Image) -> DepthComparison {
    let mut diff = None;
    let mut flipped_fraction = None;
    if copy.has_scalar() && dsv.has_scalar() {
        let mut diffs = Vec::new();
        let mut flipped = 0usize;
        for (x, y) in sample_grid(copy.width, copy.height, MAX_SAMPLES) {
            let (dx, dy) = map_texel(x, y, (copy.width, copy.height), (dsv.width, dsv.height));
            let (Some(a), Some(b)) = (copy.scalar(x, y), dsv.scalar(dx, dy)) else {
                continue;
            };
            let (a, b) = (f64::from(a), f64::from(b));
            let d = (a - b).abs();
            if (a - (1.0 - b)).abs() < d {
                flipped += 1;
            }
            diffs.push(d);
        }
        if !diffs.is_empty() {
            flipped_fraction = Some(flipped as f64 / diffs.len() as f64);
        }
        diff = summarize(diffs);
    }
    DepthComparison {
        copy: side(copy),
        dsv: side(dsv),
        diff,
        flipped_fraction,
    }
}

#[cfg(test)]
mod tests {
    use super::super::dxgi;
    use super::super::tests::image;
    use super::*;

    fn depth(format: u32, w: u32, h: u32, f: impl Fn(u32, u32) -> f32) -> Image {
        image(format, w, h, |x, y| f(x, y).to_le_bytes().to_vec())
    }

    #[test]
    fn identical_depth_has_no_difference() {
        let copy = depth(dxgi::R32_FLOAT, 8, 8, |x, y| 0.1 * (x + y) as f32 / 14.0);
        let dsv = depth(dxgi::D32_FLOAT, 8, 8, |x, y| 0.1 * (x + y) as f32 / 14.0);
        let c = compare(&copy, &dsv);
        let diff = c.diff.unwrap();
        assert_eq!((diff.count, diff.max), (64, 0.0));
        assert_eq!(c.flipped_fraction, Some(0.0));
        assert_eq!(
            (c.copy.format.as_str(), c.dsv.format.as_str()),
            ("R32_FLOAT", "D32_FLOAT")
        );
        assert_eq!(c.dsv.values.unwrap().max, f64::from(0.1f32 * 14.0 / 14.0));
    }

    #[test]
    fn a_flipped_convention_and_a_size_mismatch_show() {
        let copy = depth(dxgi::R32_FLOAT, 8, 8, |x, _| 0.05 * x as f32);
        let dsv = depth(dxgi::R32_FLOAT, 4, 4, |x, _| 1.0 - 0.1 * x as f32);
        let c = compare(&copy, &dsv);
        assert_eq!(c.flipped_fraction, Some(1.0));
        assert_eq!((c.copy.size, c.dsv.size), ([8, 8], [4, 4]));
        assert!(c.diff.unwrap().mean > 0.5);
    }

    #[test]
    fn undecodable_formats_still_report_size_and_format() {
        let copy = depth(dxgi::R32_FLOAT, 2, 2, |_, _| 0.5);
        let colour = image(26, 2, 2, |_, _| vec![0; 4]);
        let c = compare(&copy, &colour);
        assert_eq!((c.diff, c.flipped_fraction), (None, None));
        assert_eq!(c.dsv.values, None);
        assert_eq!(c.dsv.format, "R11G11B10_FLOAT");
    }
}
