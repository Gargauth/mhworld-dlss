//! The engine's render ratio arithmetic (`docs/re/RePostProcess.md` §2.4).
//!
//! The ratio setter `0x14229be60` clamps to `[0.5, 1]` **before** it cuts the
//! render width down to a multiple of sixteen:
//!
//! ```text
//! x = clamp(x, 0.5, 1.0); cfg+0x200 = ((int)(w·x) & ~15) / w
//! ```
//!
//! so at 0.5 and a width whose half is not a multiple of sixteen (3440,
//! 1680, 1366, 1360) it stores less than 0.5, while the per-frame product
//! and the target set clamp to 0.5: the render rectangle is then larger than
//! its textures. [`setter_input`] picks the value to hand the setter so that
//! what it stores is never under 0.5, and [`stored_ratio`] is what it will
//! store; the optimal-settings answer uses the same number so the engine
//! does not rebuild once for the difference.

/// The smallest ratio the engine supports (its clamp constant `0x142e4fe54`).
pub const SMALLEST: f32 = 0.5;
/// The largest (`0x142e4fe58`).
pub const LARGEST: f32 = 1.0;

/// The engine setter, emulated in its own order and precision: `None` for a
/// zero width.
pub fn stored_ratio(input: f32, width: u32) -> Option<f32> {
    if width == 0 {
        return None;
    }
    let clamped = if input.is_nan() {
        SMALLEST
    } else {
        input.clamp(SMALLEST, LARGEST)
    };
    let w = width as f32;
    let render = ((w * clamped) as i64 & !0xf) as f32;
    Some(render / w)
}

/// The render width the engine builds for a stored ratio (`trunc(w·r)`,
/// `0x142391f30`).
pub fn render_width(ratio: f32, width: u32) -> u32 {
    (width as f32 * ratio) as u32
}

/// The value to hand the setter for `wanted` at a display `width` wide.
///
/// The render width is `wanted`'s cut to a multiple of sixteen, but never
/// less than half the display rounded up to one; the input is the middle of
/// that width's pixel, so the setter's truncation lands on it exactly.
/// `wanted ≥ 1` (and any width of zero) is passed through: 1.0 is not cut
/// at all (the caller writes it directly; see [`full_window`]).
pub fn setter_input(wanted: f32, width: u32) -> f32 {
    let wanted = if wanted.is_nan() {
        SMALLEST
    } else {
        wanted.clamp(SMALLEST, LARGEST)
    };
    if width == 0 || full_window(wanted) {
        return wanted;
    }
    let w = width as f32;
    let cut = (w * wanted) as u32 & !0xf;
    let floor = width.div_ceil(2).next_multiple_of(16);
    let render = cut.max(floor).min(width & !0xf);
    ((render as f32 + 0.5) / w).min(LARGEST)
}

/// The ratio that results from asking for `wanted` at `width`: what the
/// optimal-settings answer must say so that it matches the setter later.
pub fn snapped(wanted: f32, width: u32) -> f32 {
    if full_window(wanted) || width == 0 {
        return wanted.clamp(SMALLEST, LARGEST);
    }
    stored_ratio(setter_input(wanted, width), width).unwrap_or(wanted)
}

/// Whether `ratio` means the whole window. The setter would make 1.0 a
/// shade less at widths that are not a multiple of sixteen, and anything
/// under 1.0 brings the engine's upscale stage back into the frame; 1.0 is
/// written to `cfg+0x200` directly instead.
pub fn full_window(ratio: f32) -> bool {
    ratio >= LARGEST
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_emulated_setter_cuts_after_clamping() {
        // 3440·0.5 = 1720 → 1712: under 0.5, the case that crashes.
        let stored = stored_ratio(0.5, 3440).unwrap();
        assert!(stored < 0.5);
        assert_eq!(render_width(stored, 3440), 1712);
        assert_eq!(stored_ratio(0.2, 3840), Some(0.5));
        assert_eq!(stored_ratio(1.0, 3840), Some(1.0));
        assert_eq!(stored_ratio(0.5, 0), None);
    }

    #[test]
    fn half_is_never_stored_as_less_than_half() {
        for width in 640..=7680 {
            let stored = snapped(0.5, width);
            assert!(stored >= 0.5, "{width}: {stored}");
            // The setter stores a multiple of sixteen over the width; the
            // engine's own `trunc(w·r)` may come out one under it in f32.
            let render = (width as f32 * stored).round() as u32;
            assert_eq!(render % 16, 0, "{width}: {render}");
            assert!(render * 2 >= width, "{width}: {render}");
            assert!(render_width(stored, width) + 1 >= render, "{width}");
        }
    }

    #[test]
    fn the_setter_lands_on_the_intended_width() {
        for width in [1360, 1366, 1680, 1920, 2560, 3440, 3840, 5120] {
            for wanted in [0.5, 0.58, 0.667, 0.75, 0.9, 0.99] {
                let input = setter_input(wanted, width);
                let stored = stored_ratio(input, width).unwrap();
                let render = (width as f32 * stored).round() as u32;
                assert_eq!(render % 16, 0, "{width} {wanted}");
                // The optimal-settings answer is the same number, so the
                // engine does not rebuild once for a difference.
                assert_eq!(snapped(wanted, width), stored, "{width} {wanted}");
                // At most 16 px under what was asked for; above only when
                // lifted to half.
                let asked = (width as f32 * wanted) as u32;
                assert!(
                    render + 16 > asked || render * 2 >= width,
                    "{width} {wanted}"
                );
            }
        }
    }

    #[test]
    fn ordinary_ratios_snap_like_the_engine() {
        // 3840·0.667 = 2561.28 → 2560.
        assert_eq!(render_width(snapped(0.667, 3840), 3840), 2560);
        assert_eq!(snapped(0.5, 3840), 0.5);
        assert_eq!(snapped(1.0, 1366), 1.0);
        assert_eq!(snapped(1.5, 1366), 1.0);
        assert_eq!(snapped(0.1, 1920), 0.5);
        assert!(full_window(1.0));
        assert!(!full_window(0.999));
    }
}
