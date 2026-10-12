//! The Streamline 2.14.1 C ABI, declared by hand.
//!
//! Streamline is a C++ SDK without a Rust binding, and `sl.interposer.lib`
//! is an MSVC import library the GNU toolchain does not link, so every
//! entry point is resolved from `sl.interposer.dll` with `GetProcAddress`
//! and every structure is declared here `#[repr(C)]` from the public
//! headers of tag `v2.14.1` (`include/sl_*.h`).
//!
//! That is only safe because Streamline versions its structures: each one
//! begins with [`Base`] (`next`, a GUID, a version), so a structure that is
//! wrong is rejected by type or version instead of being misread. The GUIDs
//! and versions below are the header's, and the layout tests at the end pin
//! every offset Streamline reads.
//!
//! C++ references in the API (`const sl::Constants&`, `sl::FrameToken*&`)
//! are pointers at the ABI level, and `sl::Boolean` is a one-byte `char`
//! enum. Pure data: nothing here calls anything.

use std::ffi::c_void;

/// `sl::StructType`: a GUID.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StructType {
    /// First 32 bits.
    pub data1: u32,
    /// Next 16 bits.
    pub data2: u16,
    /// Next 16 bits.
    pub data3: u16,
    /// Last 64 bits, in byte order.
    pub data4: [u8; 8],
}

impl StructType {
    const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> StructType {
        StructType {
            data1,
            data2,
            data3,
            data4,
        }
    }
}

/// `sl::BaseStructure`: the header of every structure Streamline takes.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Base {
    /// Chained extension, or null.
    pub next: *mut c_void,
    /// Which structure this is.
    pub kind: StructType,
    /// Its version (`size_t`).
    pub version: usize,
}

impl Base {
    /// A header with no chained extension.
    pub const fn new(kind: StructType, version: usize) -> Base {
        Base {
            next: std::ptr::null_mut(),
            kind,
            version,
        }
    }
}

/// `sl::Result`.
pub type SlResult = u32;
/// `sl::Result::eOk`.
pub const OK: SlResult = 0;
/// `sl::Result::eErrorNotInitialized`: also what this crate reports for a
/// call made while Streamline is not loaded.
pub const ERROR_NOT_INITIALIZED: SlResult = 21;
/// `sl::Result::eErrorFeatureMissing`: also what this crate reports for a
/// feature function that is not bound.
pub const ERROR_FEATURE_MISSING: SlResult = 31;
/// `sl::Result::eErrorInvalidState`.
pub const ERROR_INVALID_STATE: SlResult = 38;
/// `sl::Result::eErrorExceptionHandler`: Streamline caught an exception
/// inside the call and unwound out of it.
pub const ERROR_EXCEPTION_HANDLER: SlResult = 24;

/// `sl::Result` by name, `"unknown"` past the end of the 2.14.1 list.
pub fn result_name(result: SlResult) -> &'static str {
    const NAMES: [&str; 40] = [
        "eOk",
        "eErrorIO",
        "eErrorDriverOutOfDate",
        "eErrorOSOutOfDate",
        "eErrorOSDisabledHWS",
        "eErrorDeviceNotCreated",
        "eErrorNoSupportedAdapterFound",
        "eErrorAdapterNotSupported",
        "eErrorNoPlugins",
        "eErrorVulkanAPI",
        "eErrorDXGIAPI",
        "eErrorD3DAPI",
        "eErrorNRDAPI",
        "eErrorNVAPI",
        "eErrorReflexAPI",
        "eErrorNGXFailed",
        "eErrorJSONParsing",
        "eErrorMissingProxy",
        "eErrorMissingResourceState",
        "eErrorInvalidIntegration",
        "eErrorMissingInputParameter",
        "eErrorNotInitialized",
        "eErrorComputeFailed",
        "eErrorInitNotCalled",
        "eErrorExceptionHandler",
        "eErrorInvalidParameter",
        "eErrorMissingConstants",
        "eErrorDuplicatedConstants",
        "eErrorMissingOrInvalidAPI",
        "eErrorCommonConstantsMissing",
        "eErrorUnsupportedInterface",
        "eErrorFeatureMissing",
        "eErrorFeatureNotSupported",
        "eErrorFeatureMissingHooks",
        "eErrorFeatureFailedToLoad",
        "eErrorFeatureWrongPriority",
        "eErrorFeatureMissingDependency",
        "eErrorFeatureManagerInvalidState",
        "eErrorInvalidState",
        "eWarnOutOfVRAM",
    ];
    NAMES.get(result as usize).copied().unwrap_or("unknown")
}

/// `sl::kSDKVersion` of 2.14.1: major, minor, patch above the magic.
pub const SDK_VERSION: u64 = (2 << 48) | (14 << 32) | (1 << 16) | 0xfedc;

/// `sl::Feature`.
pub type Feature = u32;
/// `sl::kFeatureReflex`.
pub const FEATURE_REFLEX: Feature = 3;
/// `sl::kFeaturePCL`.
pub const FEATURE_PCL: Feature = 4;
/// `sl::kFeatureDLSS_G`.
pub const FEATURE_DLSS_G: Feature = 1000;

/// `sl::BufferType`.
pub type BufferType = u32;
/// `kBufferTypeDepth`.
pub const BUFFER_DEPTH: BufferType = 0;
/// `kBufferTypeMotionVectors`.
pub const BUFFER_MOTION_VECTORS: BufferType = 1;
/// `kBufferTypeHUDLessColor`.
pub const BUFFER_HUDLESS_COLOR: BufferType = 2;
/// `kBufferTypeUIAlpha`: one channel of interface opacity (not
/// `kBufferTypeUIColorAndAlpha`, 23, which carries colour as well).
pub const BUFFER_UI_ALPHA: BufferType = 69;

/// `sl::Boolean::eFalse`.
pub const FALSE: i8 = 0;
/// `sl::Boolean::eTrue`.
pub const TRUE: i8 = 1;
/// `sl::Boolean::eInvalid`.
pub const INVALID_BOOL: i8 = 2;
/// `sl::INVALID_FLOAT`: an optional float that was not supplied.
pub const INVALID_FLOAT: f32 = 3.402_823_5e38;

/// `sl::Boolean` from a `bool`.
pub const fn boolean(value: bool) -> i8 {
    if value {
        TRUE
    } else {
        FALSE
    }
}

/// `sl::LogLevel`.
pub mod log_level {
    /// `eOff`.
    pub const OFF: u32 = 0;
    /// `eDefault`.
    pub const DEFAULT: u32 = 1;
    /// `eVerbose`.
    pub const VERBOSE: u32 = 2;
}

/// `sl::LogType` as passed to the log callback.
pub mod log_type {
    /// `eInfo`.
    pub const INFO: u32 = 0;
    /// `eWarn`.
    pub const WARN: u32 = 1;
    /// `eError`.
    pub const ERROR: u32 = 2;
}

/// `sl::PreferenceFlags`.
pub mod preference {
    /// `eDisableCLStateTracking` (part of the SDK default).
    pub const DISABLE_CL_STATE_TRACKING: u64 = 1 << 0;
    /// `eUseManualHooking`.
    pub const USE_MANUAL_HOOKING: u64 = 1 << 2;
    /// `eAllowOTA` (part of the SDK default).
    pub const ALLOW_OTA: u64 = 1 << 3;
    /// `eUseDXGIFactoryProxy`.
    pub const USE_DXGI_FACTORY_PROXY: u64 = 1 << 5;
    /// `eLoadDownloadedPlugins` (part of the SDK default).
    pub const LOAD_DOWNLOADED_PLUGINS: u64 = 1 << 6;
    /// `eUseFrameBasedResourceTagging`: what `slSetTagForFrame` needs.
    pub const USE_FRAME_BASED_RESOURCE_TAGGING: u64 = 1 << 7;
}

/// `sl::EngineType`.
pub mod engine {
    /// `eCustom`.
    pub const CUSTOM: u32 = 0;
    /// `eUnreal`.
    pub const UNREAL: u32 = 1;
    /// `eUnity`.
    pub const UNITY: u32 = 2;
}

/// `sl::RenderAPI::eD3D12` (`sl_device_wrappers.h`: `eD3D11, eD3D12,
/// eVulkan`).
pub const RENDER_API_D3D12: u32 = 1;

/// `sl::Preferences`, `{1CA10965-BF8E-432B-8DA1-6716D879FB14}` v1.
#[repr(C)]
#[derive(Debug)]
pub struct Preferences {
    /// Header.
    pub base: Base,
    /// `showConsole`.
    pub show_console: bool,
    /// `logLevel`, [`log_level`].
    pub log_level: u32,
    /// `pathsToPlugins`: absolute, NUL-terminated UTF-16 paths.
    pub paths_to_plugins: *const *const u16,
    /// `numPathsToPlugins`.
    pub num_paths_to_plugins: u32,
    /// `pathToLogsAndData`, or null for no log file.
    pub path_to_logs_and_data: *const u16,
    /// `allocateCallback`.
    pub allocate_callback: *mut c_void,
    /// `releaseCallback`.
    pub release_callback: *mut c_void,
    /// `logMessageCallback`: `void(LogType, const char*)`.
    pub log_message_callback: *mut c_void,
    /// `flags`, [`preference`].
    pub flags: u64,
    /// `featuresToLoad`.
    pub features_to_load: *const Feature,
    /// `numFeaturesToLoad`.
    pub num_features_to_load: u32,
    /// `applicationId`.
    pub application_id: u32,
    /// `engine`, [`engine`].
    pub engine: u32,
    /// `engineVersion`, NUL-terminated, or null.
    pub engine_version: *const u8,
    /// `projectId`, NUL-terminated GUID text, or null.
    pub project_id: *const u8,
    /// `renderAPI`.
    pub render_api: u32,
}

/// GUID of [`Preferences`].
pub const PREFERENCES: StructType = StructType::new(
    0x1ca1_0965,
    0xbf8e,
    0x432b,
    [0x8d, 0xa1, 0x67, 0x16, 0xd8, 0x79, 0xfb, 0x14],
);

/// `sl::float4x4`: four rows.
pub type Float4x4 = [[f32; 4]; 4];

/// `sl::Constants`, `{DCD35AD7-4E4A-4BAD-A90C-E0C49EB23AFE}` v2.
///
/// Matrices are row-major for row vectors and carry no jitter.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Constants {
    /// Header.
    pub base: Base,
    /// `cameraViewToClip`.
    pub camera_view_to_clip: Float4x4,
    /// `clipToCameraView`.
    pub clip_to_camera_view: Float4x4,
    /// `clipToLensClip` (optional).
    pub clip_to_lens_clip: Float4x4,
    /// `clipToPrevClip`.
    pub clip_to_prev_clip: Float4x4,
    /// `prevClipToClip`.
    pub prev_clip_to_clip: Float4x4,
    /// `jitterOffset`, render pixels.
    pub jitter_offset: [f32; 2],
    /// `mvecScale`.
    pub mvec_scale: [f32; 2],
    /// `cameraPinholeOffset` (optional).
    pub camera_pinhole_offset: [f32; 2],
    /// `cameraPos`.
    pub camera_pos: [f32; 3],
    /// `cameraUp`.
    pub camera_up: [f32; 3],
    /// `cameraRight`.
    pub camera_right: [f32; 3],
    /// `cameraFwd`.
    pub camera_fwd: [f32; 3],
    /// `cameraNear`.
    pub camera_near: f32,
    /// `cameraFar`.
    pub camera_far: f32,
    /// `cameraFOV`, radians.
    pub camera_fov: f32,
    /// `cameraAspectRatio`, width over height.
    pub camera_aspect_ratio: f32,
    /// `motionVectorsInvalidValue`.
    pub motion_vectors_invalid_value: f32,
    /// `depthInverted`.
    pub depth_inverted: i8,
    /// `cameraMotionIncluded`.
    pub camera_motion_included: i8,
    /// `motionVectors3D`.
    pub motion_vectors_3d: i8,
    /// `reset`.
    pub reset: i8,
    /// `orthographicProjection`.
    pub orthographic_projection: i8,
    /// `motionVectorsDilated`.
    pub motion_vectors_dilated: i8,
    /// `motionVectorsJittered`.
    pub motion_vectors_jittered: i8,
    /// `minRelativeLinearDepthObjectSeparation` (v2).
    pub min_relative_linear_depth_object_separation: f32,
}

/// GUID of [`Constants`].
pub const CONSTANTS: StructType = StructType::new(
    0xdcd3_5ad7,
    0x4e4a,
    0x4bad,
    [0xa9, 0x0c, 0xe0, 0xc4, 0x9e, 0xb2, 0x3a, 0xfe],
);

/// `sl::ResourceType::eTex2d`.
pub const RESOURCE_TEX2D: i8 = 0;

/// `sl::Resource`, `{3A9D70CF-2418-4B72-8391-13F8721C7261}` v1.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Resource {
    /// Header.
    pub base: Base,
    /// `type`.
    pub kind: i8,
    /// `native`: the `ID3D12Resource`.
    pub native: *mut c_void,
    /// `memory` (Vulkan).
    pub memory: *mut c_void,
    /// `view` (Vulkan).
    pub view: *mut c_void,
    /// `state`: `D3D12_RESOURCE_STATES`, mandatory and must be right.
    pub state: u32,
    /// `width`.
    pub width: u32,
    /// `height`.
    pub height: u32,
    /// `nativeFormat`: `DXGI_FORMAT`.
    pub native_format: u32,
    /// `mipLevels`.
    pub mip_levels: u32,
    /// `arrayLayers`.
    pub array_layers: u32,
    /// `gpuVirtualAddress`.
    pub gpu_virtual_address: u64,
    /// `flags` (Vulkan).
    pub flags: u32,
    /// `usage` (Vulkan).
    pub usage: u32,
    /// `internalFlags`.
    pub internal_flags: u16,
    /// `reserved`.
    pub reserved: u16,
}

/// GUID of [`Resource`].
pub const RESOURCE: StructType = StructType::new(
    0x3a9d_70cf,
    0x2418,
    0x4b72,
    [0x83, 0x91, 0x13, 0xf8, 0x72, 0x1c, 0x72, 0x61],
);

/// `sl::Extent`: top before left.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Extent {
    /// `top`.
    pub top: u32,
    /// `left`.
    pub left: u32,
    /// `width`.
    pub width: u32,
    /// `height`.
    pub height: u32,
}

/// `sl::ResourceLifecycle::eValidUntilPresent`: the resource does not
/// change from tagging until the frame is presented, so Streamline does
/// not copy it.
pub const LIFECYCLE_VALID_UNTIL_PRESENT: u32 = 1;

/// `sl::ResourceTag`, `{4C6A5AAD-B445-496C-87FF-1AF3845BE653}` v1.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ResourceTag {
    /// Header.
    pub base: Base,
    /// `resource`; null removes the tag.
    pub resource: *mut Resource,
    /// `type`.
    pub kind: BufferType,
    /// `lifecycle`.
    pub lifecycle: u32,
    /// `extent`; zero for the whole resource.
    pub extent: Extent,
}

/// GUID of [`ResourceTag`].
pub const RESOURCE_TAG: StructType = StructType::new(
    0x4c6a_5aad,
    0xb445,
    0x496c,
    [0x87, 0xff, 0x1a, 0xf3, 0x84, 0x5b, 0xe6, 0x53],
);

/// `sl::ViewportHandle`, `{171B6435-9B3C-4FC8-9994-FBE52569AAA4}` v1.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ViewportHandle {
    /// Header.
    pub base: Base,
    /// `value`.
    pub value: u32,
}

/// GUID of [`ViewportHandle`].
pub const VIEWPORT_HANDLE: StructType = StructType::new(
    0x171b_6435,
    0x9b3c,
    0x4fc8,
    [0x99, 0x94, 0xfb, 0xe5, 0x25, 0x69, 0xaa, 0xa4],
);

impl ViewportHandle {
    /// The handle for viewport `value`.
    pub const fn new(value: u32) -> ViewportHandle {
        ViewportHandle {
            base: Base::new(VIEWPORT_HANDLE, 1),
            value,
        }
    }
}

/// `sl::DLSSGMode`.
pub mod dlssg_mode {
    /// `eOff`.
    pub const OFF: u32 = 0;
    /// `eOn`: fixed multiplier.
    pub const ON: u32 = 1;
}

/// `sl::DLSSGFlags::eRetainResourcesWhenOff`: `eOff` suspends generation
/// without freeing its memory, so toggling does not stutter.
pub const DLSSG_RETAIN_RESOURCES_WHEN_OFF: u32 = 1 << 3;

/// `sl::DLSSGOptions`, `{FAC5F1CB-2DFD-4F36-A1E6-3A9E865256C5}` v5.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DlssgOptions {
    /// Header.
    pub base: Base,
    /// `mode`, [`dlssg_mode`].
    pub mode: u32,
    /// `numFramesToGenerate`: multiplier − 1.
    pub num_frames_to_generate: u32,
    /// `flags`.
    pub flags: u32,
    /// `dynamicResWidth`.
    pub dynamic_res_width: u32,
    /// `dynamicResHeight`.
    pub dynamic_res_height: u32,
    /// `numBackBuffers`.
    pub num_back_buffers: u32,
    /// `mvecDepthWidth`.
    pub mvec_depth_width: u32,
    /// `mvecDepthHeight`.
    pub mvec_depth_height: u32,
    /// `colorWidth`.
    pub color_width: u32,
    /// `colorHeight`.
    pub color_height: u32,
    /// `colorBufferFormat`.
    pub color_buffer_format: u32,
    /// `mvecBufferFormat`.
    pub mvec_buffer_format: u32,
    /// `depthBufferFormat`.
    pub depth_buffer_format: u32,
    /// `hudLessBufferFormat`.
    pub hud_less_buffer_format: u32,
    /// `uiBufferFormat`.
    pub ui_buffer_format: u32,
    /// `onErrorCallback`.
    pub on_error_callback: *mut c_void,
    /// `bReserved15` (v2).
    pub reserved15: i8,
    /// `queueParallelismMode` (v3).
    pub queue_parallelism_mode: u32,
    /// `enableUserInterfaceRecomposition` (v4).
    pub enable_user_interface_recomposition: i8,
    /// `dynamicTargetFrameRate` (v5).
    pub dynamic_target_frame_rate: f32,
}

/// GUID of [`DlssgOptions`].
pub const DLSSG_OPTIONS: StructType = StructType::new(
    0xfac5_f1cb,
    0x2dfd,
    0x4f36,
    [0xa1, 0xe6, 0x3a, 0x9e, 0x86, 0x52, 0x56, 0xc5],
);

/// `sl::DLSSGState`, `{CC8AC8E1-A179-44F5-97FA-E74112F9BC61}` v4.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DlssgState {
    /// Header.
    pub base: Base,
    /// `estimatedVRAMUsageInBytes`.
    pub estimated_vram_usage_in_bytes: u64,
    /// `status`, [`dlssg_status`].
    pub status: u32,
    /// `minWidthOrHeight`.
    pub min_width_or_height: u32,
    /// `numFramesActuallyPresented` since the last call.
    pub num_frames_actually_presented: u32,
    /// `numFramesToGenerateMax` (v2).
    pub num_frames_to_generate_max: u32,
    /// `bReserved4`.
    pub reserved4: i8,
    /// `bIsVsyncSupportAvailable`.
    pub is_vsync_support_available: i8,
    /// `inputsProcessingCompletionFence` (v3).
    pub inputs_processing_completion_fence: *mut c_void,
    /// `lastPresentInputsProcessingCompletionFenceValue` (v3).
    pub last_present_inputs_processing_completion_fence_value: u64,
    /// `bIsDynamicMFGSupported` (v4).
    pub is_dynamic_mfg_supported: i8,
}

/// GUID of [`DlssgState`].
pub const DLSSG_STATE: StructType = StructType::new(
    0xcc8a_c8e1,
    0xa179,
    0x44f5,
    [0x97, 0xfa, 0xe7, 0x41, 0x12, 0xf9, 0xbc, 0x61],
);

impl DlssgState {
    /// An empty state to be filled in.
    pub const fn new() -> DlssgState {
        DlssgState {
            base: Base::new(DLSSG_STATE, 4),
            estimated_vram_usage_in_bytes: 0,
            status: 0,
            min_width_or_height: 0,
            num_frames_actually_presented: 0,
            num_frames_to_generate_max: 0,
            reserved4: 0,
            is_vsync_support_available: 0,
            inputs_processing_completion_fence: std::ptr::null_mut(),
            last_present_inputs_processing_completion_fence_value: 0,
            is_dynamic_mfg_supported: 0,
        }
    }
}

impl Default for DlssgState {
    fn default() -> DlssgState {
        DlssgState::new()
    }
}

/// `sl::DLSSGStatus` bits.
pub mod dlssg_status {
    /// `eFailResolutionTooLow`.
    pub const RESOLUTION_TOO_LOW: u32 = 1 << 0;
    /// `eFailReflexNotDetectedAtRuntime`.
    pub const REFLEX_NOT_DETECTED: u32 = 1 << 1;
    /// `eFailHDRFormatNotSupported`.
    pub const HDR_FORMAT_NOT_SUPPORTED: u32 = 1 << 2;
    /// `eFailCommonConstantsInvalid`.
    pub const COMMON_CONSTANTS_INVALID: u32 = 1 << 3;
    /// `eFailGetCurrentBackBufferIndexNotCalled`.
    pub const GET_CURRENT_BACK_BUFFER_INDEX_NOT_CALLED: u32 = 1 << 4;
}

/// The reasons in a `DLSSGStatus` word, by name; `["ok"]` for zero and
/// the raw bits for anything unnamed.
pub fn dlssg_status_names(status: u32) -> Vec<String> {
    if status == 0 {
        return vec!["ok".into()];
    }
    const NAMES: [(u32, &str); 5] = [
        (dlssg_status::RESOLUTION_TOO_LOW, "resolution too low"),
        (dlssg_status::REFLEX_NOT_DETECTED, "Reflex not active"),
        (
            dlssg_status::HDR_FORMAT_NOT_SUPPORTED,
            "HDR format not supported",
        ),
        (
            dlssg_status::COMMON_CONSTANTS_INVALID,
            "common constants invalid",
        ),
        (
            dlssg_status::GET_CURRENT_BACK_BUFFER_INDEX_NOT_CALLED,
            "GetCurrentBackBufferIndex never called",
        ),
    ];
    let mut names: Vec<String> = NAMES
        .iter()
        .filter(|(bit, _)| status & bit != 0)
        .map(|(_, name)| (*name).to_owned())
        .collect();
    let named = NAMES.iter().fold(0, |all, (bit, _)| all | bit);
    if status & !named != 0 {
        names.push(format!("{:#x}", status & !named));
    }
    names
}

/// `sl::ReflexMode` (a C enum, `int`).
pub mod reflex_mode {
    /// `eOff`.
    pub const OFF: u32 = 0;
    /// `eLowLatency`.
    pub const LOW_LATENCY: u32 = 1;
    /// `eLowLatencyWithBoost`.
    pub const LOW_LATENCY_WITH_BOOST: u32 = 2;
}

/// `sl::ReflexOptions`, `{F03AF81A-6D0B-4902-A651-C4965E215434}` v1.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ReflexOptions {
    /// Header.
    pub base: Base,
    /// `mode`, [`reflex_mode`].
    pub mode: u32,
    /// `frameLimitUs`: 0 off, else the minimum frame interval.
    pub frame_limit_us: u32,
    /// `useMarkersToOptimize`.
    pub use_markers_to_optimize: bool,
    /// `virtualKey`.
    pub virtual_key: u16,
    /// `idThread`.
    pub id_thread: u32,
}

/// GUID of [`ReflexOptions`].
pub const REFLEX_OPTIONS: StructType = StructType::new(
    0xf03a_f81a,
    0x6d0b,
    0x4902,
    [0xa6, 0x51, 0xc4, 0x96, 0x5e, 0x21, 0x54, 0x34],
);

/// `sl::PCLMarker`: the frame markers this crate sets.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Marker {
    /// `eSimulationStart`.
    SimulationStart = 0,
    /// `eSimulationEnd`.
    SimulationEnd = 1,
    /// `eRenderSubmitStart`.
    RenderSubmitStart = 2,
    /// `eRenderSubmitEnd`.
    RenderSubmitEnd = 3,
    /// `ePresentStart`.
    PresentStart = 4,
    /// `ePresentEnd`.
    PresentEnd = 5,
}

impl Marker {
    /// Every marker, in value order.
    pub const ALL: [Marker; 6] = [
        Marker::SimulationStart,
        Marker::SimulationEnd,
        Marker::RenderSubmitStart,
        Marker::RenderSubmitEnd,
        Marker::PresentStart,
        Marker::PresentEnd,
    ];

    /// The header's name.
    pub const fn name(self) -> &'static str {
        match self {
            Marker::SimulationStart => "eSimulationStart",
            Marker::SimulationEnd => "eSimulationEnd",
            Marker::RenderSubmitStart => "eRenderSubmitStart",
            Marker::RenderSubmitEnd => "eRenderSubmitEnd",
            Marker::PresentStart => "ePresentStart",
            Marker::PresentEnd => "ePresentEnd",
        }
    }
}

/// `slInit(const Preferences&, uint64_t sdkVersion)`.
pub type InitFn = unsafe extern "C" fn(*const Preferences, u64) -> SlResult;
/// `slShutdown()`.
pub type ShutdownFn = unsafe extern "C" fn() -> SlResult;
/// `slSetD3DDevice(void*)`.
pub type SetD3dDeviceFn = unsafe extern "C" fn(*mut c_void) -> SlResult;
/// `slUpgradeInterface(void**)`: replaces the pointer in place.
pub type UpgradeInterfaceFn = unsafe extern "C" fn(*mut *mut c_void) -> SlResult;
/// `slGetNativeInterface(void* proxy, void** base)`: adds a reference.
pub type GetNativeInterfaceFn = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> SlResult;
/// `slGetNewFrameToken(FrameToken*&, const uint32_t* frameIndex)`.
pub type GetNewFrameTokenFn = unsafe extern "C" fn(*mut *mut c_void, *const u32) -> SlResult;
/// `slSetConstants(const Constants&, const FrameToken&, const ViewportHandle&)`.
pub type SetConstantsFn =
    unsafe extern "C" fn(*const Constants, *const c_void, *const ViewportHandle) -> SlResult;
/// `slSetTagForFrame(const FrameToken&, const ViewportHandle&, const
/// ResourceTag*, uint32_t, CommandBuffer*)`.
pub type SetTagForFrameFn = unsafe extern "C" fn(
    *const c_void,
    *const ViewportHandle,
    *const ResourceTag,
    u32,
    *mut c_void,
) -> SlResult;
/// `slGetFeatureFunction(Feature, const char*, void*&)`.
pub type GetFeatureFunctionFn =
    unsafe extern "C" fn(Feature, *const u8, *mut *mut c_void) -> SlResult;
/// `slSetFeatureLoaded(Feature, bool)`.
pub type SetFeatureLoadedFn = unsafe extern "C" fn(Feature, bool) -> SlResult;
/// `slDLSSGSetOptions(const ViewportHandle&, const DLSSGOptions&)`.
pub type DlssgSetOptionsFn =
    unsafe extern "C" fn(*const ViewportHandle, *const DlssgOptions) -> SlResult;
/// `slDLSSGGetState(const ViewportHandle&, DLSSGState&, const DLSSGOptions*)`.
pub type DlssgGetStateFn =
    unsafe extern "C" fn(*const ViewportHandle, *mut DlssgState, *const DlssgOptions) -> SlResult;
/// `slReflexSetOptions(const ReflexOptions&)`.
pub type ReflexSetOptionsFn = unsafe extern "C" fn(*const ReflexOptions) -> SlResult;
/// `slReflexSleep(const FrameToken&)`.
pub type ReflexSleepFn = unsafe extern "C" fn(*const c_void) -> SlResult;
/// `slPCLSetMarker(PCLMarker, const FrameToken&)`.
pub type PclSetMarkerFn = unsafe extern "C" fn(u32, *const c_void) -> SlResult;
/// `PFun_LogMessageCallback`: `void(LogType, const char*)`.
pub type LogMessageFn = unsafe extern "C" fn(u32, *const std::ffi::c_char);

#[cfg(test)]
mod tests;
