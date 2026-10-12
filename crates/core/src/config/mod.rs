//! Settings: `mhw-dlss.ini` in the mod folder.
//!
//! INI with CamelCase sections and keys (matched case-insensitively),
//! enum/bool values case-insensitive (`true/false/on/off/1/0`), `;` and `#`
//! comments (whole-line, or inline after whitespace), optional double
//! quotes around a value, UTF-8 with or without BOM (UTF-16 with BOM is
//! accepted too, as Notepad's "Unicode").
//!
//! [`parse`] never fails: unknown sections and keys, invalid or
//! out-of-range values and dev keys in a release build produce
//! [`Diagnostic`]s, to be logged once each. The file is polled by `mod`;
//! [`Settings::diff`] says which [`Group`]s changed and whether each applies
//! live or needs a restart.
//!
//! `[Dev]` and `[Experimental]` exist only in dev builds (feature `dev`).
//! A release build has no code that reads them: it reports every key in
//! them as "ignored, dev build only", and [`Settings::experimental`] is the
//! constant [`Experimental::DEFAULT`], which is the release behaviour.

mod default_file;
pub mod diagnostic;
mod diff;
mod parse;
pub mod value;

#[cfg(feature = "dev")]
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

pub use ::log::LevelFilter;

pub use self::diagnostic::{Diagnostic, DiagnosticKind, FILE_NAME};
pub use self::diff::{Changes, Group, Reload};
pub use self::parse::{parse, parse_bytes};
use self::value::named_enum;
pub use self::value::{AutoBool, MipBias, Named};
pub use crate::math::JitterScale;

/// Which settings a build accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Build {
    /// Public sections only.
    Release,
    /// Public sections plus `[Dev]` and `[Experimental]`.
    #[cfg(feature = "dev")]
    Dev,
}

impl Build {
    /// The build this crate was compiled as.
    #[cfg(feature = "dev")]
    pub const CURRENT: Build = Build::Dev;
    /// The build this crate was compiled as.
    #[cfg(not(feature = "dev"))]
    pub const CURRENT: Build = Build::Release;

    /// Whether dev-only sections are accepted.
    pub const fn has_dev(self) -> bool {
        !matches!(self, Build::Release)
    }
}

/// `[General]`.
#[derive(Clone, Debug, PartialEq)]
pub struct General {
    /// `Enabled`: the mod does anything at all (restart).
    pub enabled: bool,
    /// `LogLevel`: `Error | Warn | Info | Debug | Trace` (live).
    pub log_level: LevelFilter,
}

named_enum! {
    /// `[Upscaling] Mode`.
    pub enum UpscalingMode {
        /// Full resolution, scale 1.0.
        Dlaa = "DLAA",
        /// Scale 0.667.
        Quality = "Quality",
        /// Scale 0.58.
        Balanced = "Balanced",
        /// Scale 0.5.
        Performance = "Performance",
        /// `RenderScale`.
        Custom = "Custom",
    }
    default Quality
}

impl UpscalingMode {
    /// The render scale of a fixed mode; `None` for `Custom`.
    pub const fn scale(self) -> Option<f32> {
        match self {
            UpscalingMode::Dlaa => Some(1.0),
            UpscalingMode::Quality => Some(0.667),
            UpscalingMode::Balanced => Some(0.58),
            UpscalingMode::Performance => Some(0.5),
            UpscalingMode::Custom => None,
        }
    }
}

named_enum! {
    /// `[Upscaling] Preset`: the DLSS model; `Default` lets the driver choose.
    pub enum Preset {
        /// The driver's choice.
        Default = "Default",
        /// Preset J.
        J = "J",
        /// Preset K.
        K = "K",
        /// Preset L.
        L = "L",
        /// Preset M.
        M = "M",
    }
    default Default
}

/// `[Upscaling]`, all live except `DownsampleVolume`.
#[derive(Clone, Debug, PartialEq)]
pub struct Upscaling {
    /// `Mode`.
    pub mode: UpscalingMode,
    /// `RenderScale`, [`Upscaling::RENDER_SCALE`]; used only with `Custom`.
    pub render_scale: f32,
    /// `Preset` (changing it recreates the DLSS feature).
    pub preset: Preset,
    /// `Sharpness`, [`Upscaling::SHARPNESS`]; RCAS after DLSS, 0 = off.
    pub sharpness: f32,
    /// `DownsampleVolume` (restart): the game composites its volumetric fog
    /// from a blurred half-resolution copy (`true`, the game's own way);
    /// `false` composites it at full resolution with a per-pixel upsample,
    /// so fog edges move with the jitter and anti-alias.
    pub downsample_volume: bool,
}

impl Upscaling {
    /// Range of `RenderScale`.
    pub const RENDER_SCALE: (f32, f32) = (0.5, 1.0);
    /// Range of `Sharpness`.
    pub const SHARPNESS: (f32, f32) = (0.0, 1.0);

    /// The render scale in effect: the mode's, or `RenderScale` for
    /// `Custom`.
    pub fn scale(&self) -> f32 {
        self.mode.scale().unwrap_or(self.render_scale)
    }
}

/// `[FrameGeneration]`.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameGeneration {
    /// `Enabled` (restart).
    pub enabled: bool,
    /// `Multiplier`: frames shown per rendered frame, 2–4 (live).
    pub multiplier: u32,
    /// `ReflexBoost` (live).
    pub reflex_boost: bool,
}

impl FrameGeneration {
    /// Range of `Multiplier`.
    pub const MULTIPLIER: (u32, u32) = (2, 4);
}

/// `[Dev]` (dev builds only).
#[cfg(feature = "dev")]
#[derive(Clone, Debug, PartialEq)]
pub struct Dev {
    /// `Listen`: the dev server address (restart).
    pub listen: SocketAddr,
    /// `TokenFile`, relative to the mod folder (restart).
    pub token_file: String,
    /// `LogTargets`: per-target level overrides (live).
    pub log_targets: Vec<crate::log::TargetLevel>,
    /// `DebugLayer`: D3D12 debug layer (restart).
    pub debug_layer: bool,
    /// `Dred`: DRED breadcrumbs (restart).
    pub dred: bool,
    /// `CaptureDir`, relative to the mod folder (live).
    pub capture_dir: String,
    /// `DumpShaders`: write the volumetric and fog shaders to
    /// `CaptureDir/shaders` as the game creates them (restart).
    pub dump_shaders: bool,
}

#[cfg(feature = "dev")]
impl Default for Dev {
    fn default() -> Dev {
        Dev {
            listen: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 47800)),
            token_file: "dev.token".to_owned(),
            log_targets: Vec::new(),
            debug_layer: false,
            dred: false,
            capture_dir: "captures".to_owned(),
            dump_shaders: false,
        }
    }
}

named_enum! {
    /// Where DLSS inputs come from.
    pub enum InputPath {
        /// Temporal-pass tracking.
        Pass = "Pass",
        /// The game's supersampling path, with the depth alias.
        Supersampling = "Supersampling",
    }
    default Supersampling
}

named_enum! {
    /// The game's TAA sharpening.
    pub enum TaaSharpen {
        /// Not sharpened (release behaviour).
        Off = "Off",
        /// The game's own sharpening.
        Game = "Game",
    }
    default Off
}

named_enum! {
    /// When to latch the engine's wait helper (`Device+0x10DA`).
    pub enum WaitLatch {
        /// When frame generation owns the swapchain.
        Auto = "Auto",
        /// Never latched.
        Never = "Never",
        /// Always latched.
        Always = "Always",
    }
    default Auto
}

named_enum! {
    /// What resets the DLSS history.
    pub enum ResetPolicy {
        /// The game's flag and camera cuts.
        Auto = "Auto",
        /// The game's flag only.
        GameOnly = "GameOnly",
        /// History is never reset.
        Never = "Never",
    }
    default Auto
}

impl Named for JitterScale {
    const NAMES: &'static [(&'static str, JitterScale)] = &[
        ("Display", JitterScale::Display),
        ("Render", JitterScale::Render),
        ("Off", JitterScale::Off),
    ];
}

/// `[Experimental]`: every key live. The defaults are the release
/// behaviour; release builds use [`Experimental::DEFAULT`] as a constant.
#[derive(Clone, Debug, PartialEq)]
pub struct Experimental {
    /// `InputPath`.
    pub input_path: InputPath,
    /// `JitterScale`.
    pub jitter_scale: JitterScale,
    /// `JitterPhases`, [`Experimental::JITTER_PHASES`].
    pub jitter_phases: u32,
    /// `HudLess`: HUD-less capture with gamma/PQ conversion for frame
    /// generation.
    pub hud_less: bool,
    /// `TaaSharpen`.
    pub taa_sharpen: TaaSharpen,
    /// `HoldAntiAliasing`: keep the TAA word at 0x0001.
    pub hold_anti_aliasing: bool,
    /// `WaitLatch`.
    pub wait_latch: WaitLatch,
    /// `Reset`.
    pub reset: ResetPolicy,
    /// `MipBias`.
    pub mip_bias: MipBias,
    /// `DlssHdr`: `Auto` decides from the swapchain format.
    pub dlss_hdr: AutoBool,
}

impl Experimental {
    /// Range of `JitterPhases`.
    pub const JITTER_PHASES: (u32, u32) = (1, 256);

    /// The release behaviour.
    pub const DEFAULT: Experimental = Experimental {
        input_path: InputPath::DEFAULT,
        jitter_scale: JitterScale::Display,
        jitter_phases: 32,
        hud_less: false,
        taa_sharpen: TaaSharpen::DEFAULT,
        hold_anti_aliasing: true,
        wait_latch: WaitLatch::DEFAULT,
        reset: ResetPolicy::DEFAULT,
        mip_bias: MipBias::DEFAULT,
        dlss_hdr: AutoBool::Auto,
    };
}

impl Default for Experimental {
    fn default() -> Experimental {
        Experimental::DEFAULT
    }
}

/// Every setting.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// `[General]`.
    pub general: General,
    /// `[Upscaling]`.
    pub upscaling: Upscaling,
    /// `[FrameGeneration]`.
    pub frame_generation: FrameGeneration,
    /// `[Dev]`.
    #[cfg(feature = "dev")]
    pub dev: Dev,
    /// `[Experimental]`; read it through [`Settings::experimental`] so that
    /// release code compiles too.
    #[cfg(feature = "dev")]
    pub experimental: Experimental,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            general: General {
                enabled: true,
                log_level: LevelFilter::Info,
            },
            upscaling: Upscaling {
                mode: UpscalingMode::DEFAULT,
                render_scale: 0.667,
                preset: Preset::DEFAULT,
                sharpness: 0.0,
                downsample_volume: true,
            },
            frame_generation: FrameGeneration {
                enabled: false,
                multiplier: 2,
                reflex_boost: false,
            },
            #[cfg(feature = "dev")]
            dev: Dev::default(),
            #[cfg(feature = "dev")]
            experimental: Experimental::DEFAULT,
        }
    }
}

impl Settings {
    /// The experimental settings in force: the file's in a dev build, the
    /// release behaviour otherwise.
    pub fn experimental(&self) -> &Experimental {
        #[cfg(feature = "dev")]
        {
            &self.experimental
        }
        #[cfg(not(feature = "dev"))]
        {
            &Experimental::DEFAULT
        }
    }

    /// Set one key as the file would (same validation), e.g. for the dev
    /// server's `settings.set`.
    ///
    /// `Ok(None)`: applied. `Ok(Some(note))`: applied with a note (clamped,
    /// no effect). `Err(diagnostic)`: not applied.
    pub fn set(
        &mut self,
        section: &str,
        key: &str,
        value: &str,
        build: Build,
    ) -> Result<Option<Diagnostic>, Diagnostic> {
        parse::set(self, section, key, value, build)
    }

    /// Which groups differ between `old` and `new`.
    pub fn diff(old: &Settings, new: &Settings) -> Changes {
        diff::diff(old, new)
    }

    /// The commented default file for `build` (release: public sections
    /// only). Parsing it gives [`Settings::default`] without diagnostics.
    pub fn write_default(build: Build) -> String {
        default_file::write(build)
    }
}

#[cfg(test)]
mod tests;
