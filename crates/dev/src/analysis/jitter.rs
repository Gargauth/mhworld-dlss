//! `probe.jitter`: which size the engine divides the jitter by.
//!
//! The camera setup builds `J` with row 3 = `(u_x/D_x, u_y/D_y)` from the
//! units written to `sCamera+0x38/+0x3c`, and the jittered projection the
//! temporal pass binds has `fProj[8] = −u_x/D_x`, `fProj[9] = −u_y/D_y`
//! (`ReJitterCamera.md` §2.4). So `D = −u / fProj[8|9]` per axis, and the
//! RE says `D` is the **display** size.

use mhw_core::frame::Size;
use serde::Serialize;

use super::median;

/// One frame's measurement.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct JitterRow {
    /// Engine frame of the jitter.
    pub frame: u64,
    /// Index in the jitter sequence.
    pub phase: u32,
    /// Units written to `sCamera+0x38/+0x3c`.
    pub written: [f32; 2],
    /// The shift that write produces, render pixels (engine axes).
    pub px: [f32; 2],
    /// `fProj[8]` of the camera buffer the temporal pass bound.
    pub proj8: Option<f32>,
    /// `fProj[9]`.
    pub proj9: Option<f32>,
    /// `−written / proj[8|9]` per axis, when both are measurable.
    pub divisor: [Option<f64>; 2],
}

/// Smallest |units| or |proj| entry the divisor is derived from: a zero
/// jitter (phase centre, `JitterScale = Off`) says nothing.
const TINY: f64 = 1e-6;

/// `−units / proj`, if both are measurable.
pub fn divisor(units: f32, proj: Option<f32>) -> Option<f64> {
    let (u, p) = (f64::from(units), f64::from(proj?));
    (u.is_finite() && p.is_finite() && u.abs() > TINY && p.abs() > TINY * 1e-3).then(|| -u / p)
}

impl JitterRow {
    /// A row from what was written and what the pass bound.
    pub fn new(
        frame: u64,
        phase: u32,
        written: [f32; 2],
        px: [f32; 2],
        proj8: Option<f32>,
        proj9: Option<f32>,
    ) -> JitterRow {
        JitterRow {
            frame,
            phase,
            written,
            px,
            proj8,
            proj9,
            divisor: [divisor(written[0], proj8), divisor(written[1], proj9)],
        }
    }
}

/// What the divisor turned out to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Both axes divide by the display size (the RE's claim).
    Display,
    /// Both axes divide by the render size (the 0.2 assumption).
    Render,
    /// Render and display size are equal: the probe cannot tell.
    SameSize,
    /// Measured, but matches neither size (or the axes disagree).
    Inconsistent,
    /// Nothing measurable (no jitter, or no camera buffer).
    NoJitter,
}

/// Relative tolerance of a size match (rounding of tiny projection terms).
pub const TOLERANCE: f64 = 0.01;

fn matches(measured: f64, size: u32) -> bool {
    size > 0 && ((measured - f64::from(size)) / f64::from(size)).abs() <= TOLERANCE
}

/// The verdict over `rows` and the per-axis median divisor.
pub fn verdict(rows: &[JitterRow], render: Size, display: Size) -> (Verdict, [Option<f64>; 2]) {
    let axis = |i: usize| median(rows.iter().filter_map(|r| r.divisor[i]));
    let measured = [axis(0), axis(1)];
    let [Some(dx), Some(dy)] = measured else {
        return (Verdict::NoJitter, measured);
    };
    let on_display = matches(dx, display.w) && matches(dy, display.h);
    let on_render = matches(dx, render.w) && matches(dy, render.h);
    let verdict = match (on_display, on_render) {
        (true, true) => Verdict::SameSize,
        (true, false) => Verdict::Display,
        (false, true) => Verdict::Render,
        (false, false) => Verdict::Inconsistent,
    };
    (verdict, measured)
}

#[cfg(test)]
mod tests {
    use mhw_core::math::{engine_jitter_px, jitter_matrix, JitterScale, Vec2};
    use mhw_core::math::{engine_jitter_units, Mat4};

    use super::*;

    const RENDER: Size = Size::new(1280, 720);
    const DISPLAY: Size = Size::new(1920, 1080);

    /// What the engine binds: `P·J` with `J` built from the display size
    /// (or from `by` to model a wrong divisor).
    fn row(frame: u64, px: Vec2, mode: JitterScale, by: Size) -> JitterRow {
        let units = engine_jitter_units(px, RENDER, DISPLAY, mode);
        let proj = Mat4::from_rows([
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.8, 0.0, 0.0],
            [0.0, 0.0, 0.0, -1.0],
            [0.0, 0.0, 0.1, 0.0],
        ]);
        let bound = proj * jitter_matrix(units, by);
        let shift = engine_jitter_px(units, RENDER, DISPLAY);
        JitterRow::new(
            frame,
            frame as u32,
            [units.x, units.y],
            [shift.x, shift.y],
            Some(bound.flat(8)),
            Some(bound.flat(9)),
        )
    }

    #[test]
    fn the_engine_convention_gives_the_display_verdict_in_every_mode() {
        for mode in [JitterScale::Display, JitterScale::Render] {
            let rows: Vec<JitterRow> = (1..9)
                .map(|i| {
                    row(
                        i,
                        Vec2::new(0.3 - 0.07 * i as f32, 0.1 * i as f32 - 0.4),
                        mode,
                        DISPLAY,
                    )
                })
                .collect();
            let (v, d) = verdict(&rows, RENDER, DISPLAY);
            assert_eq!(v, Verdict::Display, "{mode:?} {d:?}");
            assert!((d[0].unwrap() - 1920.0).abs() < 1.0 && (d[1].unwrap() - 1080.0).abs() < 1.0);
        }
    }

    #[test]
    fn a_render_divisor_and_a_mixed_one_are_told_apart() {
        let rows: Vec<JitterRow> = (1..5)
            .map(|i| row(i, Vec2::new(0.25, -0.25), JitterScale::Display, RENDER))
            .collect();
        assert_eq!(verdict(&rows, RENDER, DISPLAY).0, Verdict::Render);
        let odd = Size::new(1920, 720);
        let rows: Vec<JitterRow> = (1..5)
            .map(|i| row(i, Vec2::new(0.25, -0.25), JitterScale::Display, odd))
            .collect();
        assert_eq!(verdict(&rows, RENDER, DISPLAY).0, Verdict::Inconsistent);
        assert_eq!(verdict(&rows, DISPLAY, DISPLAY).0, Verdict::Inconsistent);
        let same: Vec<JitterRow> = (1..5)
            .map(|i| row(i, Vec2::new(0.25, -0.25), JitterScale::Display, DISPLAY))
            .collect();
        assert_eq!(verdict(&same, DISPLAY, DISPLAY).0, Verdict::SameSize);
    }

    #[test]
    fn zero_jitter_or_a_missing_buffer_measures_nothing() {
        let off: Vec<JitterRow> = (1..5)
            .map(|i| row(i, Vec2::new(0.25, 0.25), JitterScale::Off, DISPLAY))
            .collect();
        assert!(off.iter().all(|r| r.divisor == [None, None]));
        assert_eq!(verdict(&off, RENDER, DISPLAY).0, Verdict::NoJitter);
        let blind = JitterRow::new(1, 0, [0.5, 0.5], [0.1, 0.1], None, None);
        assert_eq!(
            verdict(&[blind], RENDER, DISPLAY),
            (Verdict::NoJitter, [None, None])
        );
        assert_eq!(divisor(0.5, Some(f32::NAN)), None);
    }
}
