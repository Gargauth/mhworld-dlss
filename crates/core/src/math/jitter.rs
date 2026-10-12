//! The jitter sequence and the jitter conventions.
//!
//! Facts (`docs/re/ReJitterCamera.md` §2.2–§2.4):
//!
//! * The engine reads `u = sCamera+0x38/+0x3c` in the camera setup and
//!   builds `J` with row 3 = `(u.x/Wd, u.y/Hd, 0, 1)`, dividing by the
//!   **display** (viewport) size, not the render size.
//! * In clip space that is an NDC shift `(u.x/Wd, u.y/Hd)`; through the
//!   viewport onto the render target, a shift of the image by
//!   `(u.x·Wr/(2·Wd), u.y·Hr/(2·Hd))` render pixels with **+y up**. One
//!   unit is half a display pixel, i.e. `rs/2` render pixels.
//! * DLSS and Streamline want the shift of the image in render pixels with
//!   **+y down** (the motion-vector axes): `(px.x, −px.y)`.
//!
//! Throughout, `px` is the image shift in render pixels on engine axes
//! (+x right, +y up).

use crate::frame::{JitterSample, Size};

use super::Vec2;

/// The radical inverse of `index` in `base`: the Halton sequence,
/// in `[0, 1)`. `base < 2` yields 0.
pub fn halton(mut index: u32, base: u32) -> f32 {
    if base < 2 {
        return 0.0;
    }
    let mut result = 0.0f64;
    let mut fraction = 1.0f64;
    let inverse = 1.0 / f64::from(base);
    while index > 0 {
        fraction *= inverse;
        result += fraction * f64::from(index % base);
        index /= base;
    }
    result as f32
}

/// How sub-pixel offsets are turned into engine units.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum JitterScale {
    /// Divide by the display size, as the engine does: the image moves by
    /// exactly the requested render-pixel offset (RE-correct).
    #[default]
    Display,
    /// The 0.2 behaviour, `units = 2·px`: the image moves by only `px·rs`,
    /// so the samples cover `rs` of a render pixel.
    Render,
    /// No jitter.
    Off,
}

/// What to write to `sCamera+0x38/+0x3c` to move the image by `px` render
/// pixels (engine axes).
///
/// `Display`: `units = 2·px·display/render` per axis. `Render`:
/// `units = 2·px`. `Off` or an empty size: zero.
pub fn engine_jitter_units(px: Vec2, render: Size, display: Size, mode: JitterScale) -> Vec2 {
    match mode {
        JitterScale::Off => Vec2::ZERO,
        JitterScale::Render => px * 2.0,
        JitterScale::Display => {
            if render.is_empty() || display.is_empty() {
                return Vec2::ZERO;
            }
            Vec2::new(
                2.0 * px.x * display.w as f32 / render.w as f32,
                2.0 * px.y * display.h as f32 / render.h as f32,
            )
        }
    }
}

/// The image shift in render pixels (engine axes) that writing `units`
/// actually produces: `units·render/(2·display)`. The inverse of
/// [`engine_jitter_units`] in `Display` mode, and the true offset in every
/// mode. Zero for an empty size.
pub fn engine_jitter_px(units: Vec2, render: Size, display: Size) -> Vec2 {
    if display.is_empty() {
        return Vec2::ZERO;
    }
    Vec2::new(
        units.x * render.w as f32 / (2.0 * display.w as f32),
        units.y * render.h as f32 / (2.0 * display.h as f32),
    )
}

/// The jitter offset DLSS-SR is given: render pixels, +y down.
pub fn dlss_jitter(px: Vec2) -> Vec2 {
    Vec2::new(px.x, -px.y)
}

/// The jitter offset Streamline (`sl::Constants::jitterOffset`) is given:
/// the same as DLSS-SR, render pixels, +y down.
pub fn streamline_jitter(px: Vec2) -> Vec2 {
    Vec2::new(px.x, -px.y)
}

/// A repeating Halton(2, 3) jitter pattern of `phases` frames, centred on
/// zero, one render pixel wide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JitterSequence {
    phases: u32,
}

impl JitterSequence {
    /// A sequence of `phases` frames (at least 1).
    pub const fn new(phases: u32) -> JitterSequence {
        JitterSequence {
            phases: if phases == 0 { 1 } else { phases },
        }
    }

    /// Frames before the pattern repeats.
    pub const fn phases(&self) -> u32 {
        self.phases
    }

    /// The phase of `frame`: `frame % phases`.
    pub fn phase(&self, frame: u64) -> u32 {
        (frame % u64::from(self.phases)) as u32
    }

    /// The offset of `phase` in render pixels, each axis in `[−0.5, 0.5)`.
    /// Halton indices start at 1: index 0 would be the corner `(−0.5, −0.5)`.
    pub fn offset(&self, phase: u32) -> Vec2 {
        let index = phase % self.phases + 1;
        Vec2::new(halton(index, 2) - 0.5, halton(index, 3) - 0.5)
    }

    /// The complete jitter of `frame`: the requested offset converted for
    /// the engine by `mode`, and `px` set to the shift that write actually
    /// produces (which differs from the request in `Render` mode).
    pub fn sample(
        &self,
        frame: u64,
        render: Size,
        display: Size,
        mode: JitterScale,
    ) -> JitterSample {
        let phase = self.phase(frame);
        let units = engine_jitter_units(self.offset(phase), render, display, mode);
        JitterSample {
            px: engine_jitter_px(units, render, display),
            units,
            phase,
            frame,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Vec2, b: Vec2) -> bool {
        (a.x - b.x).abs() < 1e-5 && (a.y - b.y).abs() < 1e-5
    }

    /// The engine's own table at `scene+0xe6a0`: Halton(2,3) for indices
    /// 20..39 minus 0.5 (ReJitterCamera §2.5), printed to 5 digits.
    const ENGINE_TABLE: [(f32, f32); 20] = [
        (-0.34375, 0.24074),
        (0.15625, -0.31481),
        (-0.09375, 0.01852),
        (0.40625, 0.35185),
        (-0.40625, -0.2037),
        (0.09375, 0.12963),
        (-0.15625, 0.46296),
        (0.34375, -0.48765),
        (-0.28125, -0.15432),
        (0.21875, 0.17901),
        (-0.03125, -0.37654),
        (0.46875, -0.04321),
        (-0.48438, 0.29012),
        (0.01562, -0.26543),
        (-0.23438, 0.0679),
        (0.26562, 0.40123),
        (-0.35938, -0.45062),
        (0.14062, -0.11728),
        (-0.10938, 0.21605),
        (0.39062, -0.33951),
    ];

    #[test]
    fn halton_reproduces_the_engines_table() {
        for (k, (x, y)) in ENGINE_TABLE.iter().enumerate() {
            let index = 20 + k as u32;
            assert!((halton(index, 2) - 0.5 - x).abs() < 1e-5, "x at {index}");
            assert!((halton(index, 3) - 0.5 - y).abs() < 1e-5, "y at {index}");
        }
        assert_eq!(halton(0, 2), 0.0);
        assert_eq!(halton(1, 2), 0.5);
        assert_eq!(halton(5, 1), 0.0);
    }

    #[test]
    fn the_sequence_repeats_and_stays_in_one_pixel() {
        let sequence = JitterSequence::new(8);
        assert_eq!(sequence.phase(13), 5);
        assert_eq!(sequence.offset(0), Vec2::new(0.0, -0.5 + 1.0 / 3.0));
        assert_eq!(sequence.offset(3), sequence.offset(11));
        for phase in 0..64 {
            let o = JitterSequence::new(64).offset(phase);
            assert!((-0.5..0.5).contains(&o.x) && (-0.5..0.5).contains(&o.y));
        }
        assert_eq!(JitterSequence::new(0).phases(), 1);
        assert_eq!(
            JitterSequence::new(0).offset(7),
            Vec2::new(0.0, -0.5 + 1.0 / 3.0)
        );
    }

    const DISPLAY: Size = Size::new(3840, 2160);
    const PERFORMANCE: Size = Size::new(1920, 1080);

    #[test]
    fn display_mode_divides_by_the_display_size() {
        // ReJitterCamera §3.1: write x·2·Wd/Wr.
        let units = engine_jitter_units(
            Vec2::new(0.5, 0.25),
            PERFORMANCE,
            DISPLAY,
            JitterScale::Display,
        );
        assert!(close(units, Vec2::new(2.0, 1.0)));
        // At DLAA it is the old 2·px.
        let units = engine_jitter_units(
            Vec2::new(0.5, -0.25),
            DISPLAY,
            DISPLAY,
            JitterScale::Display,
        );
        assert!(close(units, Vec2::new(1.0, -0.5)));
        // And it round-trips exactly.
        let px = Vec2::new(-0.34375, 0.24074);
        for render in [PERFORMANCE, Size::new(2561, 1440), Size::new(2227, 1252)] {
            let units = engine_jitter_units(px, render, DISPLAY, JitterScale::Display);
            assert!(close(engine_jitter_px(units, render, DISPLAY), px));
        }
    }

    #[test]
    fn render_mode_covers_only_rs_of_a_pixel() {
        // ReJitterCamera §2.2: u = 2x moves the image x·rs render pixels.
        let px = Vec2::new(0.5, -0.5);
        let units = engine_jitter_units(px, PERFORMANCE, DISPLAY, JitterScale::Render);
        assert!(close(units, Vec2::new(1.0, -1.0)));
        assert!(close(
            engine_jitter_px(units, PERFORMANCE, DISPLAY),
            Vec2::new(0.25, -0.25)
        ));
    }

    #[test]
    fn one_vanilla_unit_is_half_a_display_pixel() {
        // The vanilla table's ±0.5 units = ±0.25 display px.
        let shift = engine_jitter_px(Vec2::new(0.5, 0.5), DISPLAY, DISPLAY);
        assert!(close(shift, Vec2::new(0.25, 0.25)));
    }

    #[test]
    fn off_and_empty_sizes_write_nothing() {
        let px = Vec2::new(0.3, 0.3);
        assert_eq!(
            engine_jitter_units(px, PERFORMANCE, DISPLAY, JitterScale::Off),
            Vec2::ZERO
        );
        assert_eq!(
            engine_jitter_units(px, Size::new(0, 0), DISPLAY, JitterScale::Display),
            Vec2::ZERO
        );
        assert_eq!(
            engine_jitter_px(px, PERFORMANCE, Size::new(0, 1)),
            Vec2::ZERO
        );
    }

    #[test]
    fn dlss_and_streamline_flip_y_and_agree() {
        // gpu.rs (1, −1) is right; fg.rs (x, y) was wrong (ReJitterCamera Q3).
        let px = Vec2::new(0.25, 0.125);
        assert_eq!(dlss_jitter(px), Vec2::new(0.25, -0.125));
        assert_eq!(streamline_jitter(px), dlss_jitter(px));
    }

    #[test]
    fn a_sample_reports_the_true_shift() {
        let sequence = JitterSequence::new(32);
        let display = sequence.sample(33, PERFORMANCE, DISPLAY, JitterScale::Display);
        assert_eq!(display.phase, 1);
        assert_eq!(display.frame, 33);
        assert!(close(display.px, sequence.offset(1)));
        let render = sequence.sample(33, PERFORMANCE, DISPLAY, JitterScale::Render);
        assert!(close(render.px, sequence.offset(1) * 0.5));
        let off = sequence.sample(33, PERFORMANCE, DISPLAY, JitterScale::Off);
        assert_eq!((off.px, off.units), (Vec2::ZERO, Vec2::ZERO));
    }
}
