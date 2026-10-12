//! Parameter names, from `nvsdk_ngx_defs.h` (DLSS SDK 310.x) unless noted.

use std::ffi::CStr;

// Capability block (`GetCapabilityParameters`, the game's `GetParameters`).

/// `NVSDK_NGX_Parameter_SuperSampling_Available` (int).
pub const SS_AVAILABLE: &CStr = c"SuperSampling.Available";
/// `..._SuperSampling_NeedsUpdatedDriver` (int).
pub const SS_NEEDS_UPDATED_DRIVER: &CStr = c"SuperSampling.NeedsUpdatedDriver";
/// `..._SuperSampling_MinDriverVersionMajor` (unsigned int).
pub const SS_MIN_DRIVER_MAJOR: &CStr = c"SuperSampling.MinDriverVersionMajor";
/// `..._SuperSampling_MinDriverVersionMinor` (unsigned int).
pub const SS_MIN_DRIVER_MINOR: &CStr = c"SuperSampling.MinDriverVersionMinor";
/// `..._SuperSampling_FeatureInitResult` (int holding an `NVSDK_NGX_Result`).
pub const SS_FEATURE_INIT_RESULT: &CStr = c"SuperSampling.FeatureInitResult";
/// `..._ImageSignalProcessing_Available` (int).
pub const ISP_AVAILABLE: &CStr = c"ImageSignalProcessing.Available";
/// `..._ImageSignalProcessing_NeedsUpdatedDriver` (int).
pub const ISP_NEEDS_UPDATED_DRIVER: &CStr = c"ImageSignalProcessing.NeedsUpdatedDriver";
/// `..._ImageSignalProcessing_FeatureInitResult` (int).
pub const ISP_FEATURE_INIT_RESULT: &CStr = c"ImageSignalProcessing.FeatureInitResult";
/// `FrameGeneration.Available` (int); logged only, see the session notes.
pub const FG_AVAILABLE: &CStr = c"FrameGeneration.Available";
/// `FrameGeneration.FeatureInitResult` (int).
pub const FG_FEATURE_INIT_RESULT: &CStr = c"FrameGeneration.FeatureInitResult";
/// `DLSSG.MultiFrameCountMax` (unsigned int), the 310.x name.
pub const DLSSG_MULTI_FRAME_COUNT_MAX: &CStr = c"DLSSG.MultiFrameCountMax";
/// `Snippet.OptLevel` (int).
pub const SNIPPET_OPT_LEVEL: &CStr = c"Snippet.OptLevel";
/// `Snippet.IsDevBranch` (int).
pub const SNIPPET_IS_DEV_BRANCH: &CStr = c"Snippet.IsDevBranch";
/// The key OptiScaler sets on parameter objects of its own class (int).
/// Not an NVIDIA name.
pub const OPTISCALER_MARKER: &CStr = c"OptiScaler";

// Feature creation.

/// `CreationNodeMask` (unsigned int).
pub const CREATION_NODE_MASK: &CStr = c"CreationNodeMask";
/// `VisibilityNodeMask` (unsigned int).
pub const VISIBILITY_NODE_MASK: &CStr = c"VisibilityNodeMask";
/// `Width` (unsigned int): render width.
pub const WIDTH: &CStr = c"Width";
/// `Height` (unsigned int): render height.
pub const HEIGHT: &CStr = c"Height";
/// `OutWidth` (unsigned int): output width.
pub const OUT_WIDTH: &CStr = c"OutWidth";
/// `OutHeight` (unsigned int): output height.
pub const OUT_HEIGHT: &CStr = c"OutHeight";
/// `PerfQualityValue` (int, `NVSDK_NGX_PerfQuality_Value`).
pub const PERF_QUALITY_VALUE: &CStr = c"PerfQualityValue";
/// `RTXValue` (int); "some older DLSS dlls still expect this value to be set".
pub const RTX_VALUE: &CStr = c"RTXValue";
/// `DLSS.Feature.Create.Flags` (int, `NVSDK_NGX_DLSS_Feature_Flags`).
pub const CREATE_FLAGS: &CStr = c"DLSS.Feature.Create.Flags";
/// `DLSS.Enable.Output.Subrects` (int).
pub const ENABLE_OUTPUT_SUBRECTS: &CStr = c"DLSS.Enable.Output.Subrects";
/// `DLSS.Hint.Render.Preset.<level>` (unsigned int): one per quality level;
/// only the level in use matters, all are set.
pub const PRESET_HINTS: [&CStr; 6] = [
    c"DLSS.Hint.Render.Preset.DLAA",
    c"DLSS.Hint.Render.Preset.Quality",
    c"DLSS.Hint.Render.Preset.Balanced",
    c"DLSS.Hint.Render.Preset.Performance",
    c"DLSS.Hint.Render.Preset.UltraPerformance",
    c"DLSS.Hint.Render.Preset.UltraQuality",
];

// Evaluation.

/// `Color` (`ID3D12Resource*`).
pub const COLOR: &CStr = c"Color";
/// `Output` (`ID3D12Resource*`).
pub const OUTPUT: &CStr = c"Output";
/// `Depth` (`ID3D12Resource*`).
pub const DEPTH: &CStr = c"Depth";
/// `MotionVectors` (`ID3D12Resource*`).
pub const MOTION_VECTORS: &CStr = c"MotionVectors";
/// `ExposureTexture` (`ID3D12Resource*`): 1×1 exposure scale.
pub const EXPOSURE_TEXTURE: &CStr = c"ExposureTexture";
/// `Jitter.Offset.X` (float).
pub const JITTER_OFFSET_X: &CStr = c"Jitter.Offset.X";
/// `Jitter.Offset.Y` (float).
pub const JITTER_OFFSET_Y: &CStr = c"Jitter.Offset.Y";
/// `Sharpness` (float); deprecated in DLSS 3+, always 0 here.
pub const SHARPNESS: &CStr = c"Sharpness";
/// `Reset` (int).
pub const RESET: &CStr = c"Reset";
/// `MV.Scale.X` (float).
pub const MV_SCALE_X: &CStr = c"MV.Scale.X";
/// `MV.Scale.Y` (float).
pub const MV_SCALE_Y: &CStr = c"MV.Scale.Y";
/// `DLSS.Input.Color.Subrect.Base.X` (unsigned int).
pub const COLOR_SUBRECT_X: &CStr = c"DLSS.Input.Color.Subrect.Base.X";
/// `DLSS.Input.Color.Subrect.Base.Y` (unsigned int).
pub const COLOR_SUBRECT_Y: &CStr = c"DLSS.Input.Color.Subrect.Base.Y";
/// `DLSS.Input.Depth.Subrect.Base.X` (unsigned int).
pub const DEPTH_SUBRECT_X: &CStr = c"DLSS.Input.Depth.Subrect.Base.X";
/// `DLSS.Input.Depth.Subrect.Base.Y` (unsigned int).
pub const DEPTH_SUBRECT_Y: &CStr = c"DLSS.Input.Depth.Subrect.Base.Y";
/// `DLSS.Input.MV.Subrect.Base.X` (unsigned int; the macro is
/// `..._MV_SubrectBase_X`, the string has the dot).
pub const MV_SUBRECT_X: &CStr = c"DLSS.Input.MV.Subrect.Base.X";
/// `DLSS.Input.MV.Subrect.Base.Y` (unsigned int).
pub const MV_SUBRECT_Y: &CStr = c"DLSS.Input.MV.Subrect.Base.Y";
/// `DLSS.Output.Subrect.Base.X` (unsigned int).
pub const OUTPUT_SUBRECT_X: &CStr = c"DLSS.Output.Subrect.Base.X";
/// `DLSS.Output.Subrect.Base.Y` (unsigned int).
pub const OUTPUT_SUBRECT_Y: &CStr = c"DLSS.Output.Subrect.Base.Y";
/// `DLSS.Render.Subrect.Dimensions.Width` (unsigned int).
pub const RENDER_SUBRECT_WIDTH: &CStr = c"DLSS.Render.Subrect.Dimensions.Width";
/// `DLSS.Render.Subrect.Dimensions.Height` (unsigned int).
pub const RENDER_SUBRECT_HEIGHT: &CStr = c"DLSS.Render.Subrect.Dimensions.Height";

// Slot check (`Parameters::verify_slots`); not NVIDIA names.

/// Integer written and read back by the slot check.
pub const PROBE_INT: &CStr = c"mhw.probe.int";
/// Float written and read back by the slot check.
pub const PROBE_FLOAT: &CStr = c"mhw.probe.float";
