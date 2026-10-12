//! DLSS super resolution (`NVSDK_NGX_Feature_SuperSampling`, id 1): what a
//! feature is created with, what an evaluation passes, and how both map to
//! parameter writes. Pure; [`crate::feature`] makes the calls.

use std::fmt;

use mhw_core::config::Preset;
use mhw_core::frame::Size;
use mhw_core::math::Vec2;

use crate::param::{names, Value, Write};

/// `NVSDK_NGX_Feature_SuperSampling`.
pub const FEATURE_SUPER_SAMPLING: u32 = 1;
/// `NVSDK_NGX_Feature_ImageSignalProcessing`, the game's second DLSS 1
/// feature (sharpening/upscale), which current drivers no longer carry.
pub const FEATURE_IMAGE_SIGNAL_PROCESSING: u32 = 9;

/// The handle the engine's `CreateFeatures` stand-in gives the game for
/// SuperSampling (stored at `ctx+0x38`).
///
/// The game never evaluates through it (both evaluations are the mod's) but
/// passes it back to `ReleaseFeature` whenever it rebuilds its targets, and
/// handing NGX a handle it did not create is an access violation inside the
/// core; the session keeps these away from NGX. Recognisable constants, not
/// pointer-like values, so the check cannot match by accident.
pub const STAND_IN_SUPER_SAMPLING: usize = 0x0000_D155_0000_0001;
/// The stand-in handle for ImageSignalProcessing (`ctx+0x40`).
pub const STAND_IN_IMAGE_SIGNAL: usize = 0x0000_D155_0000_0009;

/// Whether `handle` is one of the stand-ins.
pub const fn is_stand_in(handle: usize) -> bool {
    handle == STAND_IN_SUPER_SAMPLING || handle == STAND_IN_IMAGE_SIGNAL
}

/// `NVSDK_NGX_PerfQuality_Value`: the quality level a feature is created for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Quality {
    /// `MaxPerf` (Performance, 0.5).
    MaxPerf = 0,
    /// `Balanced` (0.58).
    Balanced = 1,
    /// `MaxQuality` (Quality, 0.667).
    MaxQuality = 2,
    /// `UltraPerformance` (0.333).
    UltraPerformance = 3,
    /// `UltraQuality` (0.77); DLSS-SR does not support it and is never chosen
    /// by [`Quality::for_sizes`].
    UltraQuality = 4,
    /// `DLAA` (1.0).
    Dlaa = 5,
}

impl Quality {
    /// The levels [`Quality::for_sizes`] chooses from, with their nominal
    /// render scale.
    const BY_SCALE: [(f32, Quality); 5] = [
        (1.0, Quality::Dlaa),
        (0.667, Quality::MaxQuality),
        (0.58, Quality::Balanced),
        (0.5, Quality::MaxPerf),
        (0.333, Quality::UltraPerformance),
    ];

    /// The value `PerfQualityValue` takes.
    pub const fn value(self) -> i32 {
        self as i32
    }

    /// The level whose nominal scale is nearest to `render.w / display.w`.
    ///
    /// The engine decides the render size; DLSS is told the level that
    /// matches what it is actually given rather than one asserted from
    /// settings. An empty display size reads as DLAA.
    pub fn for_sizes(render: Size, display: Size) -> Quality {
        if display.w == 0 {
            return Quality::Dlaa;
        }
        let ratio = render.w as f32 / display.w as f32;
        let mut best = Quality::Dlaa;
        let mut best_distance = f32::INFINITY;
        for (scale, quality) in Quality::BY_SCALE {
            let distance = (scale - ratio).abs();
            if distance < best_distance {
                best = quality;
                best_distance = distance;
            }
        }
        best
    }
}

/// `NVSDK_NGX_DLSS_Hint_Render_Preset`: the DLSS model.
///
/// Only the presets the 310.x header gives a meaning are representable:
/// A–D were removed ("use preset J or K"), E/F are deprecated and G–I, N, O
/// "revert to default behavior".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RenderPreset {
    /// 0: the library's choice (K for DLAA/Quality/Balanced, M for
    /// Performance, L for Ultra Performance in 310.x).
    Default = 0,
    /// 10.
    J = 10,
    /// 11: transformer model.
    K = 11,
    /// 12.
    L = 12,
    /// 13.
    M = 13,
}

impl RenderPreset {
    /// The value the `DLSS.Hint.Render.Preset.*` keys take.
    pub const fn value(self) -> u32 {
        self as u32
    }
}

impl From<Preset> for RenderPreset {
    fn from(preset: Preset) -> RenderPreset {
        match preset {
            Preset::Default => RenderPreset::Default,
            Preset::J => RenderPreset::J,
            Preset::K => RenderPreset::K,
            Preset::L => RenderPreset::L,
            Preset::M => RenderPreset::M,
        }
    }
}

/// `NVSDK_NGX_DLSS_Feature_Flags` this crate sets, each explicit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CreateFlags {
    /// Bit 0 `IsHDR`: colour is linear HDR rather than display-referred.
    pub is_hdr: bool,
    /// Bit 1 `MVLowRes`: motion vectors are at render, not output,
    /// resolution (see [`CreateFlags::mv_low_res_for`]).
    pub mv_low_res: bool,
    /// Bit 2 `MVJittered`: motion vectors include the jitter. The engine's
    /// do not (`docs/re/00-SUMMARY.md`).
    pub mv_jittered: bool,
    /// Bit 3 `DepthInverted`: 1 is near.
    pub depth_inverted: bool,
    /// Bit 6 `AutoExposure`: DLSS computes exposure itself (no exposure
    /// texture needed).
    pub auto_exposure: bool,
}

impl CreateFlags {
    /// The `DLSS.Feature.Create.Flags` value.
    pub const fn bits(self) -> i32 {
        (self.is_hdr as i32)
            | (self.mv_low_res as i32) << 1
            | (self.mv_jittered as i32) << 2
            | (self.depth_inverted as i32) << 3
            | (self.auto_exposure as i32) << 6
    }

    /// Whether render-resolution motion vectors need `MVLowRes`: exactly when
    /// upscaling. At DLAA the sizes are equal and the bit is cleared so the
    /// flags (and the feature) do not change with it.
    pub const fn mv_low_res_for(render: Size, display: Size) -> bool {
        render.w < display.w || render.h < display.h
    }
}

/// Everything a DLSS-SR feature is created with. A feature is recreated
/// whenever the wanted description differs from the one it was created with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FeatureDesc {
    /// Render (input) size: `Width`/`Height`.
    pub render: Size,
    /// Output size: `OutWidth`/`OutHeight`.
    pub display: Size,
    /// `PerfQualityValue`.
    pub quality: Quality,
    /// `DLSS.Hint.Render.Preset.*` (not written for `Default`).
    pub preset: RenderPreset,
    /// `DLSS.Feature.Create.Flags`.
    pub flags: CreateFlags,
    /// `DLSS.Enable.Output.Subrects`: honour the output subrect base at
    /// evaluation.
    pub output_subrects: bool,
}

/// Why a [`FeatureDesc`] or [`EvalParams`] cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamError {
    /// A size is zero.
    EmptySize,
    /// The render size exceeds the output size (DLSS-SR only upscales).
    RenderLargerThanOutput,
    /// The render subrect is empty or larger than the feature's render size.
    SubrectOutOfRange,
    /// Jitter or motion-vector scale is NaN or infinite.
    NotFinite,
    /// A required input resource is null.
    MissingInput(&'static str),
}

impl fmt::Display for ParamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParamError::EmptySize => f.write_str("a size is zero"),
            ParamError::RenderLargerThanOutput => {
                f.write_str("the render size exceeds the output size")
            }
            ParamError::SubrectOutOfRange => {
                f.write_str("the render subrect is empty or exceeds the render size")
            }
            ParamError::NotFinite => f.write_str("jitter or motion-vector scale is not finite"),
            ParamError::MissingInput(which) => write!(f, "input {which} is null"),
        }
    }
}

impl std::error::Error for ParamError {}

impl FeatureDesc {
    /// Check the sizes.
    pub fn validate(&self) -> Result<(), ParamError> {
        if self.render.is_empty() || self.display.is_empty() {
            return Err(ParamError::EmptySize);
        }
        if self.render.w > self.display.w || self.render.h > self.display.h {
            return Err(ParamError::RenderLargerThanOutput);
        }
        Ok(())
    }

    /// The parameter writes before `CreateFeature`, in the order the SDK's
    /// `NGX_D3D12_CREATE_DLSS_EXT` makes them, plus `RTXValue = 0` (older
    /// libraries still read it; the game sets it too) and the preset hints.
    pub fn create_writes(&self) -> Vec<Write> {
        let mut writes = vec![
            Write::new(names::CREATION_NODE_MASK, Value::U32(1)),
            Write::new(names::VISIBILITY_NODE_MASK, Value::U32(1)),
            Write::new(names::WIDTH, Value::U32(self.render.w)),
            Write::new(names::HEIGHT, Value::U32(self.render.h)),
            Write::new(names::OUT_WIDTH, Value::U32(self.display.w)),
            Write::new(names::OUT_HEIGHT, Value::U32(self.display.h)),
            Write::new(names::PERF_QUALITY_VALUE, Value::I32(self.quality.value())),
            Write::new(names::RTX_VALUE, Value::I32(0)),
            Write::new(names::CREATE_FLAGS, Value::I32(self.flags.bits())),
            Write::new(
                names::ENABLE_OUTPUT_SUBRECTS,
                Value::I32(i32::from(self.output_subrects)),
            ),
        ];
        // Each quality level has its own hint and only the one in use
        // matters; setting all saves mapping the level back to a name.
        if self.preset != RenderPreset::Default {
            writes.extend(
                names::PRESET_HINTS
                    .iter()
                    .map(|name| Write::new(name, Value::U32(self.preset.value()))),
            );
        }
        writes
    }
}

/// A texel offset into a resource.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Offset {
    /// Column.
    pub x: u32,
    /// Row.
    pub y: u32,
}

/// Which part of each resource an evaluation reads and writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Subrects {
    /// `DLSS.Input.Color.Subrect.Base`.
    pub colour: Offset,
    /// `DLSS.Input.Depth.Subrect.Base`.
    pub depth: Offset,
    /// `DLSS.Input.MV.Subrect.Base`.
    pub motion: Offset,
    /// `DLSS.Output.Subrect.Base` (only honoured with
    /// [`FeatureDesc::output_subrects`]).
    pub output: Offset,
    /// `DLSS.Render.Subrect.Dimensions`: the rendered area, at most the
    /// feature's render size.
    pub render: Size,
}

impl Subrects {
    /// Every resource read from its origin, `render` texels.
    pub const fn full(render: Size) -> Subrects {
        Subrects {
            colour: Offset { x: 0, y: 0 },
            depth: Offset { x: 0, y: 0 },
            motion: Offset { x: 0, y: 0 },
            output: Offset { x: 0, y: 0 },
            render,
        }
    }
}

/// Per-evaluation values besides the resources.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EvalParams {
    /// `Jitter.Offset`: the sub-pixel offset of this frame's projection in
    /// render pixels, NVIDIA convention (+y down; `mhw_core::math::dlss_jitter`).
    pub jitter: Vec2,
    /// `MV.Scale`: multiplies each motion vector into render pixels
    /// (current → previous), e.g. `(1, 1)` for pixel vectors or
    /// `(render.w, render.h)` for UV vectors.
    pub mv_scale: Vec2,
    /// Where each resource is read.
    pub subrects: Subrects,
    /// `Reset`: discard history this frame.
    pub reset: bool,
}

/// The resources of one evaluation as raw `ID3D12Resource*` addresses
/// (no references held). 0 = null.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RawInputs {
    /// `Color`: render-resolution colour.
    pub colour: usize,
    /// `MotionVectors`.
    pub motion: usize,
    /// `Depth`.
    pub depth: usize,
    /// `Output`: output-resolution UAV target.
    pub output: usize,
    /// `ExposureTexture` (1×1), or 0 for none.
    pub exposure: usize,
}

/// How many writes [`eval_writes`] makes.
pub const EVAL_WRITES: usize = 21;

impl EvalParams {
    /// Check against the feature the evaluation runs on and the inputs.
    pub fn validate(&self, desc: &FeatureDesc, inputs: &RawInputs) -> Result<(), ParamError> {
        for (address, which) in [
            (inputs.colour, "Color"),
            (inputs.motion, "MotionVectors"),
            (inputs.depth, "Depth"),
            (inputs.output, "Output"),
        ] {
            if address == 0 {
                return Err(ParamError::MissingInput(which));
            }
        }
        let render = self.subrects.render;
        if render.is_empty() || render.w > desc.render.w || render.h > desc.render.h {
            return Err(ParamError::SubrectOutOfRange);
        }
        if !(self.jitter.x.is_finite()
            && self.jitter.y.is_finite()
            && self.mv_scale.x.is_finite()
            && self.mv_scale.y.is_finite())
        {
            return Err(ParamError::NotFinite);
        }
        Ok(())
    }
}

/// The parameter writes before `EvaluateFeature`, following the SDK's
/// `NGX_D3D12_EVALUATE_DLSS_EXT` for the inputs this crate uses.
///
/// Every key is written every frame, so nothing from an earlier evaluation
/// lingers in the reused parameter object: a missing exposure texture is
/// written as null, `Sharpness` is always 0 (deprecated in DLSS 3+;
/// sharpening is a separate RCAS pass).
pub fn eval_writes(inputs: &RawInputs, params: &EvalParams) -> [Write; EVAL_WRITES] {
    let s = &params.subrects;
    [
        Write::new(names::COLOR, Value::D3d12Resource(inputs.colour)),
        Write::new(names::OUTPUT, Value::D3d12Resource(inputs.output)),
        Write::new(names::DEPTH, Value::D3d12Resource(inputs.depth)),
        Write::new(names::MOTION_VECTORS, Value::D3d12Resource(inputs.motion)),
        Write::new(names::JITTER_OFFSET_X, Value::F32(params.jitter.x)),
        Write::new(names::JITTER_OFFSET_Y, Value::F32(params.jitter.y)),
        Write::new(names::SHARPNESS, Value::F32(0.0)),
        Write::new(names::RESET, Value::I32(i32::from(params.reset))),
        Write::new(names::MV_SCALE_X, Value::F32(params.mv_scale.x)),
        Write::new(names::MV_SCALE_Y, Value::F32(params.mv_scale.y)),
        Write::new(
            names::EXPOSURE_TEXTURE,
            Value::D3d12Resource(inputs.exposure),
        ),
        Write::new(names::COLOR_SUBRECT_X, Value::U32(s.colour.x)),
        Write::new(names::COLOR_SUBRECT_Y, Value::U32(s.colour.y)),
        Write::new(names::DEPTH_SUBRECT_X, Value::U32(s.depth.x)),
        Write::new(names::DEPTH_SUBRECT_Y, Value::U32(s.depth.y)),
        Write::new(names::MV_SUBRECT_X, Value::U32(s.motion.x)),
        Write::new(names::MV_SUBRECT_Y, Value::U32(s.motion.y)),
        Write::new(names::OUTPUT_SUBRECT_X, Value::U32(s.output.x)),
        Write::new(names::OUTPUT_SUBRECT_Y, Value::U32(s.output.y)),
        Write::new(names::RENDER_SUBRECT_WIDTH, Value::U32(s.render.w)),
        Write::new(names::RENDER_SUBRECT_HEIGHT, Value::U32(s.render.h)),
    ]
}

#[cfg(test)]
mod tests;
