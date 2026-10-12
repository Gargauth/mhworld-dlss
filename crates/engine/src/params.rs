//! What the mod decides and the engine carries out.
//!
//! The engine reads no settings: `mod` turns them into [`Params`] and hands
//! them over with [`set_params`](crate::set_params), any time, from any
//! thread. The detours read the current value on every call.

use mhw_core::math::JitterScale;

use crate::ratio;

/// The feature set the engine is told to create (`GetOptimalSettings`
/// flags, `ReDlss1Path.md` §2.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DlssFlags {
    /// `2`: the upscale (ISP) only. No supersampling pass is recorded, the
    /// dense velocity target is never allocated, and with the game's own
    /// NVIDIA DLSS option on the engine forces TAA+FXAA (the anti-aliasing
    /// hold undoes that). Inputs come from the temporal pass
    /// (`InputPath = Pass`).
    #[default]
    UpscaleOnly,
    /// `3`: supersampling and upscale. The SS pass runs with the engine's
    /// own depth copy and dense velocity; it crashes in the barrier planner
    /// unless the depth alias is on
    /// ([`set_depth_alias`](crate::set_depth_alias)).
    /// (`InputPath = Supersampling`.)
    Supersampling,
}

impl DlssFlags {
    /// The flags word the engine stores at `cfg+0xb4`.
    pub const fn bits(self) -> u32 {
        match self {
            DlssFlags::UpscaleOnly => 2,
            DlssFlags::Supersampling => 3,
        }
    }

    /// Whether the supersampling pass runs.
    pub const fn supersampling(self) -> bool {
        matches!(self, DlssFlags::Supersampling)
    }
}

/// Everything policy decides for the engine detours.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    /// Take over the game's DLSS 1 path. `false` passes every DLSS detour
    /// through to the game, stops the jitter writes and gives the Image
    /// Quality option back to the player. Only before the game created its
    /// features: once `CreateFeatures` stored the stand-ins, its DLSS 1 path
    /// would evaluate them and tear its DLSS state down.
    pub hijack: bool,
    /// Flags answered to `GetOptimalSettings`.
    pub flags: DlssFlags,
    /// Render ratio, `0.5..=1.0` (clamped); applied through the engine's
    /// setter whenever installed, also without `hijack`.
    pub ratio: f32,
    /// How the jitter is converted to engine units; `Off` writes none.
    pub jitter: JitterScale,
    /// Jitter sequence length (at least 1).
    pub jitter_phases: u32,
    /// Clear `mSharpen` (`taa+0x187`) on every temporal pass
    /// (`[Experimental] TaaSharpen = Off`).
    pub taa_sharpen_off: bool,
    /// Hold the anti-aliasing word `rs+0xe9e4` at `0x0001` (TAA only)
    /// (`HoldAntiAliasing`).
    pub hold_anti_aliasing: bool,
    /// Turn off the screen-space dithers DLSS reads as detail: SSLR dither
    /// and g-buffer jitter, contact-shadow noise (also facial).
    pub undither: bool,
}

impl Params {
    /// The engine's values until `mod` hands over its policy: the DLSS
    /// path taken over, upscale only (safe without the depth alias; the
    /// mod's default `InputPath = Supersampling` asks for flags 3), full
    /// window until told otherwise, display-correct jitter over 32 phases,
    /// TAA sharpen off, anti-aliasing held, dithers off.
    pub const RELEASE: Params = Params {
        hijack: true,
        flags: DlssFlags::UpscaleOnly,
        ratio: 1.0,
        jitter: JitterScale::Display,
        jitter_phases: 32,
        taa_sharpen_off: true,
        hold_anti_aliasing: true,
        undither: true,
    };

    /// The same with out-of-range values brought into range: ratio into
    /// `0.5..=1.0` (NaN → 1.0), phases at least 1.
    pub fn validated(self) -> Params {
        Params {
            ratio: if self.ratio.is_nan() {
                ratio::LARGEST
            } else {
                self.ratio.clamp(ratio::SMALLEST, ratio::LARGEST)
            },
            jitter_phases: self.jitter_phases.max(1),
            ..self
        }
    }

    /// Whether the camera is to be jittered.
    pub fn jittering(&self) -> bool {
        self.hijack && self.jitter != JitterScale::Off
    }
}

impl Default for Params {
    fn default() -> Params {
        Params::RELEASE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_are_the_engines_words() {
        assert_eq!(DlssFlags::UpscaleOnly.bits(), 2);
        assert_eq!(DlssFlags::Supersampling.bits(), 3);
        assert!(DlssFlags::Supersampling.supersampling());
        assert!(!DlssFlags::UpscaleOnly.supersampling());
    }

    #[test]
    fn validation_brings_values_into_range() {
        let wild = Params {
            ratio: 0.2,
            jitter_phases: 0,
            ..Params::RELEASE
        };
        assert_eq!(wild.validated().ratio, 0.5);
        assert_eq!(wild.validated().jitter_phases, 1);
        let nan = Params {
            ratio: f32::NAN,
            ..Params::RELEASE
        };
        assert_eq!(nan.validated().ratio, 1.0);
        assert_eq!(Params::RELEASE.validated(), Params::RELEASE);
    }

    #[test]
    fn jitter_needs_the_hijack_and_a_mode() {
        assert!(Params::RELEASE.jittering());
        let off = Params {
            jitter: JitterScale::Off,
            ..Params::RELEASE
        };
        assert!(!off.jittering());
        let pass = Params {
            hijack: false,
            ..Params::RELEASE
        };
        assert!(!pass.jittering());
    }
}
