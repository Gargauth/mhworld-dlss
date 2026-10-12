//! The commented default file, written on first start.

use std::fmt::Write as _;

use super::value::Named;
use super::{Build, FrameGeneration, Preset, Settings, Upscaling, UpscalingMode};

/// The default file for `build`. Values come from [`Settings::default`], so
/// the file and the defaults cannot disagree.
pub(super) fn write(build: Build) -> String {
    let defaults = Settings::default();
    let mut out = String::with_capacity(2048);
    let (scale_low, scale_high) = Upscaling::RENDER_SCALE;
    let (fg_low, fg_high) = FrameGeneration::MULTIPLIER;
    let _ = write!(
        out,
        "\
; mhw-dlss settings.
;
; Read while the game runs: a change applies within a second, except keys
; marked (restart). Values are not case-sensitive; true/false may also be
; written on/off or 1/0. A key the mod does not know is ignored and named
; once in mhw-dlss.log.

[General]
; Whether the mod does anything at all. (restart)
Enabled = {enabled}
; How much goes into mhw-dlss.log: Error | Warn | Info | Debug | Trace
LogLevel = {log_level}

[Upscaling]
; {modes}
; DLAA renders at full resolution; Quality 0.667, Balanced 0.58 and
; Performance 0.5 of it, and DLSS reconstructs the rest.
Mode = {mode}
; The render scale with Mode = Custom: {scale_low:?} to {scale_high:?}.
RenderScale = {render_scale:?}
; The DLSS model: {presets}. Default lets the driver choose.
Preset = {preset}
; Sharpening after DLSS, before the interface is drawn: 0.0 (off) to 1.0.
Sharpness = {sharpness:?}
; The game draws its volumetric fog from a blurred half-resolution copy,
; which leaves stair-stepped edges where objects meet fog. false draws it at
; full resolution so DLSS smooths those edges; costs a little performance.
; (restart)
DownsampleVolume = {downsample_volume}

[FrameGeneration]
; DLSS frame generation. Needs NVIDIA's files: run fetch-nvidia.cmd. (restart)
Enabled = {fg_enabled}
; Frames shown per rendered frame: {fg_low} to {fg_high}.
Multiplier = {multiplier}
; Ask NVIDIA Reflex to keep clocks up as well, at the cost of power.
ReflexBoost = {reflex_boost}
",
        enabled = defaults.general.enabled,
        log_level = level_name(defaults.general.log_level),
        modes = UpscalingMode::choices(),
        mode = defaults.upscaling.mode.name(),
        render_scale = defaults.upscaling.render_scale,
        presets = Preset::choices(),
        preset = defaults.upscaling.preset.name(),
        sharpness = defaults.upscaling.sharpness,
        downsample_volume = defaults.upscaling.downsample_volume,
        fg_enabled = defaults.frame_generation.enabled,
        multiplier = defaults.frame_generation.multiplier,
        reflex_boost = defaults.frame_generation.reflex_boost,
    );
    #[cfg(feature = "dev")]
    if build.has_dev() {
        dev::write(&mut out, &defaults);
    }
    #[cfg(not(feature = "dev"))]
    let _ = build;
    out
}

/// `Info` rather than the facade's `INFO`.
fn level_name(level: super::LevelFilter) -> String {
    let upper = level.as_str();
    let mut name = upper.to_ascii_lowercase();
    name[..1].make_ascii_uppercase();
    name
}

#[cfg(feature = "dev")]
mod dev {
    use std::fmt::Write as _;

    use super::super::value::Named;
    use super::super::{
        Experimental, InputPath, JitterScale, ResetPolicy, Settings, TaaSharpen, WaitLatch,
    };
    use crate::config::MipBias;

    pub(super) fn write(out: &mut String, defaults: &Settings) {
        let dev = &defaults.dev;
        let exp = &defaults.experimental;
        let (phases_low, phases_high) = Experimental::JITTER_PHASES;
        let (bias_low, bias_high) = MipBias::RANGE;
        let targets: Vec<String> = dev
            .log_targets
            .iter()
            .map(|t| format!("{}={}", t.target, t.level.as_str().to_ascii_lowercase()))
            .collect();
        let _ = write!(
            out,
            "
; ---------------------------------------------------------------------------
; Dev build only. A release build ignores everything below.
; ---------------------------------------------------------------------------

[Dev]
; Dev server address; 0.0.0.0:47800 to reach it from the LAN. (restart)
Listen = {listen}
; Holds the auth token, created on first start. (restart)
TokenFile = {token_file}
; Per-target log levels, e.g. render=trace,fg=debug
LogTargets = {targets}
; Direct3D 12 debug layer. (restart)
DebugLayer = {debug_layer}
; DRED breadcrumbs. (restart)
Dred = {dred}
; Where captures are written.
CaptureDir = {capture_dir}
; Write the volumetric and fog shaders to CaptureDir\\shaders. (restart)
DumpShaders = {dump_shaders}

[Experimental]
; Defaults are the release behaviour.
; {input_paths}: temporal-pass tracking, or the game's SS path with depth alias.
InputPath = {input_path}
; {jitter_scales}: Display is RE-correct (2px/rs), Render the 0.2 behaviour.
JitterScale = {jitter_scale}
; Frames before the jitter pattern repeats: {phases_low} to {phases_high}.
JitterPhases = {jitter_phases}
; HUD-less capture with gamma/PQ conversion for frame generation.
HudLess = {hud_less}
; The game's TAA sharpening: {taa_sharpens}
TaaSharpen = {taa_sharpen}
; Keep the TAA word at 0x0001.
HoldAntiAliasing = {hold}
; {wait_latches}: Auto latches when frame generation owns the swapchain.
WaitLatch = {wait_latch}
; {resets}: Auto is the game flag plus camera cuts.
Reset = {reset}
; Auto | Off | a bias from {bias_low:?} to {bias_high:?}
MipBias = {mip_bias}
; Auto | true | false
DlssHdr = {dlss_hdr}
",
            listen = dev.listen,
            token_file = dev.token_file,
            targets = targets.join(","),
            debug_layer = dev.debug_layer,
            dred = dev.dred,
            capture_dir = dev.capture_dir,
            dump_shaders = dev.dump_shaders,
            input_paths = InputPath::choices(),
            input_path = exp.input_path.name(),
            jitter_scales = JitterScale::choices(),
            jitter_scale = exp.jitter_scale.name(),
            jitter_phases = exp.jitter_phases,
            hud_less = exp.hud_less,
            taa_sharpens = TaaSharpen::choices(),
            taa_sharpen = exp.taa_sharpen.name(),
            hold = exp.hold_anti_aliasing,
            wait_latches = WaitLatch::choices(),
            wait_latch = exp.wait_latch.name(),
            resets = ResetPolicy::choices(),
            reset = exp.reset.name(),
            mip_bias = exp.mip_bias,
            dlss_hdr = exp.dlss_hdr,
        );
    }
}
