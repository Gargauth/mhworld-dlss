//! Policy: what the settings in force mean for every subsystem. Pure.
//!
//! [`Policy::from_settings`] is the one place a setting turns into a
//! decision; the rest of the mod reads the [`Policy`] in force. A release
//! build reads `[Experimental]` as the constant `Experimental::DEFAULT`
//! (through `Settings::experimental`), so its policy is fixed apart from
//! the public settings.

use mhw_core::config::{
    AutoBool, InputPath, MipBias, ResetPolicy, Settings, TaaSharpen, WaitLatch,
};
use mhw_core::frame::Size;
use mhw_core::math::Vec2;
use mhw_engine::{DlssFlags, Params};
use mhw_fg::Options as FgOptions;
use mhw_ngx::{CreateFlags, FeatureDesc, Quality, RenderPreset};
use mhw_render::DecodeParams;

/// `MV.Scale` for the engine's dense velocity (`taa+0x1a8`, supersampling
/// path): the clip-space difference to the previous position times the
/// render size, so half of it with y flipped is the pixel offset DLSS
/// wants. Measured by odt1 (MHW-DLSS4 `docs/technical.md`, Conventions;
/// `MotionScaleX/Y = 0.5 / -0.5`); DLSS 1 was given 1, which put the
/// history in the wrong place whenever the camera moved. Dev builds can
/// change it while running (`slot::ss_mv_scale`).
pub const SS_MV_SCALE: Vec2 = Vec2::new(0.5, -0.5);

/// `MV.Scale` for the decoded `tGBuffer4` vectors: render pixels already.
pub const DECODED_MV_SCALE: Vec2 = Vec2::new(1.0, 1.0);

/// Mip biases closer to 0 than this are no bias (bank 0 kept).
const MIP_BIAS_EPSILON: f32 = 0.01;

/// Everything the settings decide, for every subsystem.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Policy {
    /// What the engine detours do.
    pub engine: Params,
    /// DLSS evaluates (and the temporal pass identifies its inputs).
    /// `false` while held or given up ([`Policy::fallen_back`]).
    pub dlss: bool,
    /// Where DLSS inputs come from.
    pub input_path: InputPath,
    /// DLSS model.
    pub preset: RenderPreset,
    /// RCAS after DLSS, 0 = off.
    pub sharpness: f32,
    /// `IsHDR` decision ([`dlss_is_hdr`]).
    pub dlss_hdr: AutoBool,
    /// What resets history ([`crate::reset`]).
    pub reset: ResetPolicy,
    /// HUD-less capture for frame generation.
    pub hudless: bool,
    /// When to latch the engine's wait helper.
    pub wait_latch: WaitLatch,
    /// Texture mip bias the engine is installed with (a start-up quantity).
    pub mip_bias: Option<f32>,
    /// `[FrameGeneration] Enabled` (start-up: Streamline is loaded or not).
    pub frame_generation: bool,
    /// Frame generation options (live).
    pub fg: FgOptions,
    /// The motion vector decode at the temporal pass (`InputPath = Pass`).
    pub decode: DecodeParams,
}

impl Policy {
    /// The policy `settings` describe.
    pub fn from_settings(settings: &Settings) -> Policy {
        let experimental = settings.experimental();
        let scale = settings.upscaling.scale();
        let flags = match experimental.input_path {
            InputPath::Pass => DlssFlags::UpscaleOnly,
            InputPath::Supersampling => DlssFlags::Supersampling,
        };
        Policy {
            dlss: true,
            engine: Params {
                hijack: true,
                flags,
                ratio: scale,
                jitter: experimental.jitter_scale,
                jitter_phases: experimental.jitter_phases,
                taa_sharpen_off: experimental.taa_sharpen == TaaSharpen::Off,
                hold_anti_aliasing: experimental.hold_anti_aliasing,
                undither: Params::RELEASE.undither,
            }
            .validated(),
            input_path: experimental.input_path,
            preset: settings.upscaling.preset.into(),
            sharpness: settings.upscaling.sharpness,
            dlss_hdr: experimental.dlss_hdr,
            reset: experimental.reset,
            hudless: experimental.hud_less,
            wait_latch: experimental.wait_latch,
            mip_bias: mip_bias(experimental.mip_bias, scale),
            frame_generation: settings.frame_generation.enabled,
            fg: FgOptions::from_settings(&settings.frame_generation),
            decode: DecodeParams::default(),
        }
    }

    /// The same policy while DLSS is held or given up (`fallback`): the
    /// game's own temporal AA at full resolution (ratio 1, nothing
    /// evaluated, the temporal pass left to the engine). The DLSS detours
    /// stay the mod's: the game's features are stand-ins, and its own DLSS 1
    /// path would evaluate them, fail, and tear its DLSS state down for the
    /// session (seen in game: `cfg+0xb4` and `device+0x42718` cleared, no
    /// upscale point after the hold). The jitter stays for the engine's TAA,
    /// as in 0.2.
    pub fn fallen_back(self) -> Policy {
        Policy {
            dlss: false,
            engine: Params {
                ratio: 1.0,
                ..self.engine
            },
            ..self
        }
    }

    /// Whether the temporal pass must identify (and decode) inputs.
    pub fn pass_inputs(&self) -> bool {
        self.dlss
    }

    /// `MV.Scale` of the vectors the active input path hands DLSS.
    pub fn mv_scale(&self, ss_scale: Vec2) -> Vec2 {
        match self.input_path {
            InputPath::Pass => DECODED_MV_SCALE,
            InputPath::Supersampling => ss_scale,
        }
    }
}

/// The mip bias for `setting` at render `scale`: `Auto` is `log2(scale)`
/// (the texture detail of the display resolution), none at full scale.
pub fn mip_bias(setting: MipBias, scale: f32) -> Option<f32> {
    let bias = match setting {
        MipBias::Off => return None,
        MipBias::Value(value) => value,
        MipBias::Auto if scale.is_finite() && scale > 0.0 => scale.min(1.0).log2(),
        MipBias::Auto => return None,
    };
    (bias.is_finite() && bias.abs() >= MIP_BIAS_EPSILON).then_some(bias)
}

/// Where the colour DLSS reads was taken in the engine's frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColourStage {
    /// Before tonemapping: the temporal pass's `tBaseMap` or the
    /// supersampling call's colour (DLAA at ratio 1).
    BeforeTonemap,
    /// At the upscale point (EvalISP), after tonemapping and grading
    /// (`ReDlss1Path.md` §5) and before the output encode.
    AfterTonemap,
}

/// `IsHDR` for DLSS.
///
/// `DlssHdr = Auto` decides by what the colour is: before tonemapping it is
/// linear scene light above 1.0 (HDR); after tonemapping it is
/// display-referred, unless the game outputs HDR, in which case the
/// tonemapper leaves values above 1.0 for the PQ encode (`[sRender+0x21a]`).
/// 0.2 set `IsHDR` always; the RE showed the upscale point is after
/// tonemapping, so in SDR that was wrong (`probe.hdr` measures it).
pub fn dlss_is_hdr(setting: AutoBool, stage: ColourStage, output_hdr: bool) -> bool {
    setting.resolve(match stage {
        ColourStage::BeforeTonemap => true,
        ColourStage::AfterTonemap => output_hdr,
    })
}

/// The DLSS feature for `render` → `display` under `policy`. Depth is
/// inverted (1 near, `math::DEPTH_FLIP`), vectors are jitter-free
/// (`00-SUMMARY` §1), exposure is DLSS's own.
pub fn feature_desc(policy: &Policy, render: Size, display: Size, is_hdr: bool) -> FeatureDesc {
    FeatureDesc {
        render,
        display,
        quality: Quality::for_sizes(render, display),
        preset: policy.preset,
        flags: CreateFlags {
            is_hdr,
            mv_low_res: CreateFlags::mv_low_res_for(render, display),
            mv_jittered: false,
            depth_inverted: true,
            auto_exposure: true,
        },
        output_subrects: false,
    }
}

/// What to write to the engine's wait latch (`Device+0x10DA`), if
/// anything: set it once when `wanted`; write it back only when the policy
/// is `Never` and this mod set it (the engine sets it on its own after a 1 s
/// timeout, and that is left alone).
pub fn latch_write(wanted: bool, policy: WaitLatch, we_latched: bool) -> Option<bool> {
    match (wanted, we_latched) {
        (true, false) => Some(true),
        (false, true) if policy == WaitLatch::Never => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mhw_core::config::{JitterScale, Preset, UpscalingMode};

    fn settings() -> Settings {
        Settings::default()
    }

    #[test]
    fn the_default_policy_is_the_release_behaviour() {
        let policy = Policy::from_settings(&settings());
        assert_eq!(
            policy.engine,
            Params {
                ratio: 0.667,
                flags: DlssFlags::Supersampling,
                ..Params::RELEASE
            }
        );
        assert_eq!(policy.input_path, InputPath::Supersampling);
        assert_eq!(policy.preset, RenderPreset::Default);
        assert_eq!(policy.sharpness, 0.0);
        assert_eq!(policy.reset, ResetPolicy::Auto);
        assert!(!policy.hudless);
        assert_eq!(policy.wait_latch, WaitLatch::Auto);
        assert!(!policy.frame_generation);
        let bias = policy.mip_bias.expect("Auto at 0.667 biases");
        assert!((bias - 0.667f32.log2()).abs() < 1e-6);
        assert_eq!(policy.mv_scale(SS_MV_SCALE), SS_MV_SCALE);
    }

    #[test]
    fn the_mode_decides_the_ratio_and_custom_reads_the_render_scale() {
        let mut s = settings();
        for (mode, ratio) in [
            (UpscalingMode::Dlaa, 1.0),
            (UpscalingMode::Quality, 0.667),
            (UpscalingMode::Balanced, 0.58),
            (UpscalingMode::Performance, 0.5),
        ] {
            s.upscaling.mode = mode;
            assert_eq!(Policy::from_settings(&s).engine.ratio, ratio);
        }
        s.upscaling.mode = UpscalingMode::Custom;
        s.upscaling.render_scale = 0.75;
        assert_eq!(Policy::from_settings(&s).engine.ratio, 0.75);
        s.upscaling.mode = UpscalingMode::Dlaa;
        assert_eq!(Policy::from_settings(&s).mip_bias, None);
    }

    #[test]
    fn public_settings_reach_their_subsystems() {
        let mut s = settings();
        s.upscaling.preset = Preset::K;
        s.upscaling.sharpness = 0.4;
        s.frame_generation.enabled = true;
        s.frame_generation.multiplier = 3;
        s.frame_generation.reflex_boost = true;
        let policy = Policy::from_settings(&s);
        assert_eq!(policy.preset, RenderPreset::K);
        assert_eq!(policy.sharpness, 0.4);
        assert!(policy.frame_generation);
        assert!(policy.fg.enabled);
        assert_eq!(policy.fg.multiplier, 3);
        assert!(policy.fg.reflex_boost);
    }

    #[cfg(feature = "dev")]
    #[test]
    fn experimental_settings_change_the_engine_and_the_paths() {
        let mut s = settings();
        s.experimental.input_path = InputPath::Supersampling;
        s.experimental.jitter_scale = JitterScale::Render;
        s.experimental.jitter_phases = 0;
        s.experimental.taa_sharpen = TaaSharpen::Game;
        s.experimental.hold_anti_aliasing = false;
        s.experimental.mip_bias = MipBias::Off;
        let policy = Policy::from_settings(&s);
        assert_eq!(policy.engine.flags, DlssFlags::Supersampling);
        assert_eq!(policy.engine.jitter, JitterScale::Render);
        assert_eq!(policy.engine.jitter_phases, 1, "validated");
        assert!(!policy.engine.taa_sharpen_off);
        assert!(!policy.engine.hold_anti_aliasing);
        assert_eq!(policy.mip_bias, None);
        assert_eq!(policy.mv_scale(Vec2::new(2.0, -3.0)), Vec2::new(2.0, -3.0));
    }

    #[test]
    fn release_reads_no_experimental_section() {
        // Whatever the build, the policy of default settings uses the
        // release constants.
        let policy = Policy::from_settings(&settings());
        assert_eq!(policy.engine.jitter, JitterScale::Display);
        assert_eq!(policy.engine.jitter_phases, 32);
    }

    #[test]
    fn falling_back_gives_the_game_its_own_full_resolution_path() {
        let policy = Policy::from_settings(&settings()).fallen_back();
        assert!(!policy.dlss);
        assert_eq!(policy.engine.ratio, 1.0);
        assert!(!policy.pass_inputs());
        // The game's DLSS entry points stay away from its own DLSS 1 path.
        assert!(policy.engine.hijack);
    }

    #[test]
    fn mip_bias_auto_follows_the_scale_and_fixed_values_stand() {
        assert_eq!(mip_bias(MipBias::Auto, 1.0), None);
        assert_eq!(mip_bias(MipBias::Auto, 0.5), Some(-1.0));
        assert_eq!(mip_bias(MipBias::Auto, f32::NAN), None);
        assert_eq!(mip_bias(MipBias::Off, 0.5), None);
        assert_eq!(mip_bias(MipBias::Value(-0.5), 1.0), Some(-0.5));
        assert_eq!(mip_bias(MipBias::Value(0.0), 0.5), None);
    }

    #[test]
    fn hdr_auto_follows_the_colour_stage_and_the_output() {
        use ColourStage::*;
        assert!(dlss_is_hdr(AutoBool::Auto, BeforeTonemap, false));
        assert!(!dlss_is_hdr(AutoBool::Auto, AfterTonemap, false));
        assert!(dlss_is_hdr(AutoBool::Auto, AfterTonemap, true));
        assert!(!dlss_is_hdr(AutoBool::Forced(false), BeforeTonemap, true));
        assert!(dlss_is_hdr(AutoBool::Forced(true), AfterTonemap, false));
    }

    #[test]
    fn the_feature_describes_what_dlss_is_given() {
        let policy = Policy::from_settings(&settings());
        let up = feature_desc(&policy, Size::new(2560, 1440), Size::new(3840, 2160), false);
        assert_eq!(up.quality, Quality::MaxQuality);
        assert!(up.flags.mv_low_res && up.flags.depth_inverted && up.flags.auto_exposure);
        assert!(!up.flags.mv_jittered && !up.flags.is_hdr);
        let dlaa = feature_desc(&policy, Size::new(1920, 1080), Size::new(1920, 1080), true);
        assert_eq!(dlaa.quality, Quality::Dlaa);
        assert!(!dlaa.flags.mv_low_res && dlaa.flags.is_hdr);
    }

    #[test]
    fn the_latch_is_set_once_and_undone_only_for_never() {
        assert_eq!(latch_write(true, WaitLatch::Auto, false), Some(true));
        assert_eq!(latch_write(true, WaitLatch::Always, true), None);
        assert_eq!(latch_write(false, WaitLatch::Auto, true), None);
        assert_eq!(latch_write(false, WaitLatch::Never, true), Some(false));
        assert_eq!(latch_write(false, WaitLatch::Never, false), None);
    }
}
