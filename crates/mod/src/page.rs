//! What the overlay's settings page shows and writes. Pure.
//!
//! The page is an editor for `mhw-dlss.ini` (public settings only): each
//! control writes one line through [`crate::ini_edit::set_key`] and the hot
//! reload applies it. Enumerations are sliders over their file names.

use mhw_core::config::{LevelFilter, Named};

/// Render scales a dragged slider settles on. 0.667, not two thirds: at
/// 3840×2160 it is what gives the engine 2560×1440 after its rounding.
pub const COMMON_SCALES: [f32; 6] = [0.5, 0.58, 0.667, 0.75, 0.85, 1.0];

/// How near a common scale a value must be to become it: a few pixels of
/// slider either side, everything between stays reachable.
const SNAP_WITHIN: f32 = 0.008;

/// The log levels the page offers, least verbose first.
pub const LEVELS: [(&str, LevelFilter); 5] = [
    ("Error", LevelFilter::Error),
    ("Warn", LevelFilter::Warn),
    ("Info", LevelFilter::Info),
    ("Debug", LevelFilter::Debug),
    ("Trace", LevelFilter::Trace),
];

/// `scale` in `0.5..=1.0`, moved onto a common scale when close to one.
pub fn snapped(scale: f32) -> f32 {
    let scale = if scale.is_finite() {
        scale.clamp(0.5, 1.0)
    } else {
        1.0
    };
    COMMON_SCALES
        .iter()
        .copied()
        .find(|common| (scale - common).abs() <= SNAP_WITHIN)
        .unwrap_or(scale)
}

/// The slider position of `value` among `T`'s names.
pub fn position<T: Named>(value: T) -> i32 {
    T::NAMES
        .iter()
        .position(|(_, v)| *v == value)
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(0)
}

/// The file name at slider `position` (clamped into range).
pub fn name_at<T: Named>(position: i32) -> &'static str {
    let last = T::NAMES.len().saturating_sub(1);
    let index = usize::try_from(position).unwrap_or(0).min(last);
    T::NAMES.get(index).map_or("", |(name, _)| name)
}

/// The slider position of a log level (`Off` shows as `Error`).
pub fn level_position(level: LevelFilter) -> i32 {
    LEVELS
        .iter()
        .position(|(_, l)| *l == level)
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(0)
}

/// The file name of the log level at slider `position`.
pub fn level_name_at(position: i32) -> &'static str {
    let index = usize::try_from(position).unwrap_or(0).min(LEVELS.len() - 1);
    LEVELS[index].0
}

/// A float as the file writes it.
pub fn decimal(value: f32, places: usize) -> String {
    format!("{value:.places$}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mhw_core::config::{Preset, UpscalingMode};

    #[test]
    fn scales_settle_on_common_values_and_stay_in_range() {
        assert_eq!(snapped(0.664), 0.667);
        assert_eq!(snapped(0.70), 0.70);
        assert_eq!(snapped(0.2), 0.5);
        assert_eq!(snapped(f32::NAN), 1.0);
        assert_eq!(snapped(0.995), 1.0);
    }

    #[test]
    fn enumerations_round_trip_through_slider_positions() {
        for (name, mode) in UpscalingMode::NAMES {
            assert_eq!(name_at::<UpscalingMode>(position(*mode)), *name);
        }
        assert_eq!(name_at::<Preset>(99), "M");
        assert_eq!(name_at::<Preset>(-3), "Default");
        for (name, level) in LEVELS {
            assert_eq!(level_name_at(level_position(level)), name);
        }
        assert_eq!(level_position(LevelFilter::Off), 0);
        assert_eq!(decimal(0.25, 2), "0.25");
    }
}
