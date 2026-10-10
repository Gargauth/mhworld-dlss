//! Streamline, called the way NGX is called: by hand, through GetProcAddress.
//!
//! Frame generation cannot be driven without owning `Present` -- NVIDIA say so
//! outright -- so `sl.dlss_g` does it, and this module is the seam. What it
//! asks of this library is small and is almost entirely work already done
//! for upscaling: tag the depth and motion vectors, hand over the camera
//! matrices and the jitter, keep the frame index straight.
//!
//! ## Why the structures are reproduced by hand, and why that is safe
//!
//! Streamline is a C++ SDK with no Rust binding, and `sl.interposer.lib` is
//! an MSVC import library, which the GNU toolchain this project is built
//! with does not link. Every entry point is therefore resolved from
//! `sl.interposer.dll` with `GetProcAddress`, exactly as `dlss.rs` does for
//! NGX, and every structure is declared here as `#[repr(C)]` from the public
//! headers of the SDK.
//!
//! That is only safe because Streamline versions its structures. Every one
//! begins with `sl::BaseStructure`:
//!
//! ```text
//!   BaseStructure* next;      8 bytes
//!   StructType structType;   16 bytes -- a GUID
//!   size_t structVersion;     8 bytes
//! ```
//!
//! so a structure we got wrong is rejected by type or by version rather than
//! misread. The GUIDs and versions below are copied from the 2.14.1 headers
//! and each one is named in the comment above it, so a future SDK that bumps a
//! version fails loudly at `slInit` instead of corrupting a frame.
//!
//! ## The one value the headers do not give
//!
//! `sl::RenderAPI` is referenced by `Preferences` but declared in no public
//! header in the `include/` directory. `eD3D12` is almost certainly 1, in the
//! conventional `eD3D11, eD3D12, eVulkan` order, and the cost of being wrong
//! is bounded: `slSetD3DDevice` would be handed an `ID3D12Device` while SL
//! believed it was on D3D11 and would fail with a code this module logs. So it
//! is a configuration value, `fg_render_api`, and the log says which number
//! went in.

use parking_lot::Mutex;
use std::ffi::{c_void, CString};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use windows::core::PCSTR;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// `sl::StructType`, a GUID in the usual `{u32, u16, u16, u8[8]}` shape.
#[repr(C)]
#[derive(Clone, Copy)]
struct StructType {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// Everything Streamline takes begins with this.
#[repr(C)]
#[derive(Clone, Copy)]
struct Base {
    next: *mut c_void,
    kind: StructType,
    version: usize,
}

impl Base {
    const fn new(kind: StructType, version: usize) -> Base {
        Base {
            next: std::ptr::null_mut(),
            kind,
            version,
        }
    }
}

const fn guid(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> StructType {
    StructType {
        data1,
        data2,
        data3,
        data4,
    }
}

/// `sl::Result::eOk`.
const OK: u32 = 0;

/// Feature identifiers. `kFeatureDLSS_G` really is 1000.
const FEATURE_REFLEX: u32 = 3;
const FEATURE_PCL: u32 = 4;
const FEATURE_DLSS_G: u32 = 1000;

/// `sl::kBufferType*`. Only the ones that matter here.
const BUFFER_DEPTH: u32 = 0;
const BUFFER_MOTION_VECTORS: u32 = 1;
const BUFFER_HUDLESS_COLOUR: u32 = 2;
/// *"Optional - Mask for pixels to skip warping"*, and that comment in
/// `sl_core_types.h` is the whole of its documentation. See `fgmask.rs`.
const BUFFER_NO_WARP_MASK: u32 = 54;
/// A single channel of interface opacity. Not `kBufferTypeUIColorAndAlpha`,
/// which is 23 and carries colour as well.
const BUFFER_UI_ALPHA: u32 = 69;

/// `sl::ResourceType::eTex2d`.
const RESOURCE_TEX2D: i8 = 0;
/// `sl::ResourceLifecycle::eValidUntilPresent`. The guide is emphatic: tag
/// everything this way first and only fall back to the volatile lifecycles
/// when the content is genuinely not valid at present time, because the others
/// make Streamline copy and waste video memory.
const LIFECYCLE_VALID_UNTIL_PRESENT: u32 = 1;

/// `sl::Boolean`, which is a one-byte `char` enum.
const FALSE: i8 = 0;
const TRUE: i8 = 1;
const INVALID_BOOL: i8 = 2;
/// `sl::INVALID_FLOAT`, which is how an optional float says "not supplied".
const INVALID_FLOAT: f32 = 3.402_823_5e38;

/// `sl::PreferenceFlags`.
const FLAG_DISABLE_CL_STATE_TRACKING: u64 = 1 << 0;
const FLAG_USE_MANUAL_HOOKING: u64 = 1 << 2;
/// Part of Streamline's own default, and kept.
///
/// `Preferences::flags` defaults to
/// `eDisableCLStateTracking | eAllowOTA | eLoadDownloadedPlugins`. Nothing
/// measured in this game is a reason to deviate from the SDK's default in
/// either direction: NGX's log line `NGXSecureLoadFeature ... failed to load
/// ... from cache` is printed for features that then load from the local
/// file, and is not a failure.
///
/// What it means for a user: with `fg = 1`, Streamline and NGX may fetch
/// updated plugins and models over the air and load them in place of the
/// local ones. `fg_ota` in `Config` is the one place to turn that off.
const FLAG_ALLOW_OTA: u64 = 1 << 3;
/// The flag that exists for us by name: *"If specified SL will create DXGI
/// factory proxy rather than modifying the v-table for the base interface.
/// This can help with 3rd party overlays which are NOT integrated with the
/// host application but rather operate via injection."* That is this library
/// exactly, and it keeps Streamline's interposition an object rather than a
/// fight over the vtable slots `swap.rs` also patches.
const FLAG_USE_DXGI_FACTORY_PROXY: u64 = 1 << 5;
const FLAG_LOAD_DOWNLOADED_PLUGINS: u64 = 1 << 6;
/// What makes `slSetTagForFrame` mean anything.
///
/// This was simply missing, and it is a real defect rather than a refinement:
/// the guide says frame-based tagging is *enabled via* this flag, and every tag
/// this library sets goes through `slSetTagForFrame`. Without it the depth and
/// motion vectors were being handed over through a path that was not switched
/// on.
const FLAG_USE_FRAME_BASED_RESOURCE_TAGGING: u64 = 1 << 7;

/// `sl::kSDKVersion` for 2.14.1: major, minor and patch packed above a magic.
const SDK_VERSION: u64 = (2u64 << 48) | (14u64 << 32) | (1u64 << 16) | 0xfedc;

/// Monster Hunter World's NGX application id, as the game itself passes to
/// `NVSDK_NGX_D3D12_Init`.
const MHW_NGX_APPLICATION_ID: u32 = 100182111;

/// `sl::Preferences`, `{1CA10965-BF8E-432B-8DA1-6716D879FB14}` version 1.
#[repr(C)]
struct Preferences {
    base: Base,
    show_console: bool,
    log_level: u32,
    paths_to_plugins: *const *const u16,
    num_paths_to_plugins: u32,
    path_to_logs_and_data: *const u16,
    allocate_callback: *mut c_void,
    release_callback: *mut c_void,
    log_message_callback: *mut c_void,
    flags: u64,
    features_to_load: *const u32,
    num_features_to_load: u32,
    application_id: u32,
    engine: u32,
    engine_version: *const u8,
    project_id: *const u8,
    render_api: u32,
}

const PREFERENCES: StructType = guid(
    0x1ca1_0965,
    0xbf8e,
    0x432b,
    [0x8d, 0xa1, 0x67, 0x16, 0xd8, 0x79, 0xfb, 0x14],
);

/// `sl::Constants`, `{DCD35AD7-4E4A-4BAD-A90C-E0C49EB23AFE}` version 2.
///
/// Every matrix is row-major and **without jitter baked in**; the jitter goes
/// in `jitter_offset` on its own. That is the one thing this engine makes easy
/// rather than hard: `native.rs` writes the two floats the engine's own camera
/// setup reads, so the engine derives the jittered projection itself and keeps
/// `fNoJitterProj` clean. We can hand over an honestly unjittered matrix
/// instead of subtracting an offset back out of a jittered one.
#[repr(C)]
struct Constants {
    base: Base,
    camera_view_to_clip: [f32; 16],
    clip_to_camera_view: [f32; 16],
    clip_to_lens_clip: [f32; 16],
    clip_to_prev_clip: [f32; 16],
    prev_clip_to_clip: [f32; 16],
    jitter_offset: [f32; 2],
    mvec_scale: [f32; 2],
    camera_pinhole_offset: [f32; 2],
    camera_pos: [f32; 3],
    camera_up: [f32; 3],
    camera_right: [f32; 3],
    camera_fwd: [f32; 3],
    camera_near: f32,
    camera_far: f32,
    camera_fov: f32,
    camera_aspect_ratio: f32,
    motion_vectors_invalid_value: f32,
    depth_inverted: i8,
    camera_motion_included: i8,
    motion_vectors_3d: i8,
    reset: i8,
    orthographic_projection: i8,
    motion_vectors_dilated: i8,
    motion_vectors_jittered: i8,
    min_relative_linear_depth_object_separation: f32,
}

const CONSTANTS: StructType = guid(
    0xdcd3_5ad7,
    0x4e4a,
    0x4bad,
    [0xa9, 0x0c, 0xe0, 0xc4, 0x9e, 0xb2, 0x3a, 0xfe],
);

/// `sl::Resource`, `{3A9D70CF-2418-4B72-8391-13F8721C7261}` version 1.
#[repr(C)]
struct Resource {
    base: Base,
    kind: i8,
    native: *mut c_void,
    memory: *mut c_void,
    view: *mut c_void,
    /// `D3D12_RESOURCE_STATES`. The header is blunt about this: *"State is
    /// MANDATORY and needs to be correct when tagged resources are actually
    /// used."* `states.rs` is the only thing in this library that can answer
    /// it, and it refuses to guess -- which is why a resource whose state was
    /// never observed is not tagged at all.
    state: u32,
    width: u32,
    height: u32,
    native_format: u32,
    mip_levels: u32,
    array_layers: u32,
    gpu_virtual_address: u64,
    flags: u32,
    usage: u32,
    internal_flags: u16,
    reserved: u16,
}

const RESOURCE: StructType = guid(
    0x3a9d_70cf,
    0x2418,
    0x4b72,
    [0x83, 0x91, 0x13, 0xf8, 0x72, 0x1c, 0x72, 0x61],
);

/// `sl::Extent`. Note the order: top before left.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Extent {
    top: u32,
    left: u32,
    width: u32,
    height: u32,
}

/// `sl::ResourceTag`, `{4C6A5AAD-B445-496C-87FF-1AF3845BE653}` version 1.
#[repr(C)]
struct ResourceTag {
    base: Base,
    resource: *mut Resource,
    kind: u32,
    lifecycle: u32,
    extent: Extent,
}

const RESOURCE_TAG: StructType = guid(
    0x4c6a_5aad,
    0xb445,
    0x496c,
    [0x87, 0xff, 0x1a, 0xf3, 0x84, 0x5b, 0xe6, 0x53],
);

/// `sl::ViewportHandle`, `{171B6435-9B3C-4FC8-9994-FBE52569AAA4}` version 1.
#[repr(C)]
#[derive(Clone, Copy)]
struct ViewportHandle {
    base: Base,
    value: u32,
}

const VIEWPORT: StructType = guid(
    0x171b_6435,
    0x9b3c,
    0x4fc8,
    [0x99, 0x94, 0xfb, 0xe5, 0x25, 0x69, 0xaa, 0xa4],
);

/// `sl::DLSSGOptions`, `{FAC5F1CB-2DFD-4F36-A1E6-3A9E865256C5}` version 5.
#[repr(C)]
struct DlssgOptions {
    base: Base,
    mode: u32,
    num_frames_to_generate: u32,
    flags: u32,
    dynamic_res_width: u32,
    dynamic_res_height: u32,
    num_back_buffers: u32,
    mvec_depth_width: u32,
    mvec_depth_height: u32,
    colour_width: u32,
    colour_height: u32,
    colour_buffer_format: u32,
    mvec_buffer_format: u32,
    depth_buffer_format: u32,
    hudless_buffer_format: u32,
    ui_buffer_format: u32,
    on_error_callback: *mut c_void,
    reserved15: i8,
    queue_parallelism_mode: u32,
    enable_user_interface_recomposition: i8,
    dynamic_target_frame_rate: f32,
}

const DLSSG_OPTIONS: StructType = guid(
    0xfac5_f1cb,
    0x2dfd,
    0x4f36,
    [0xa1, 0xe6, 0x3a, 0x9e, 0x86, 0x52, 0x56, 0xc5],
);

/// `sl::DLSSGState`, `{CC8AC8E1-A179-44F5-97FA-E74112F9BC61}` version 4.
#[repr(C)]
struct DlssgState {
    base: Base,
    estimated_vram_usage_in_bytes: u64,
    status: u32,
    min_width_or_height: u32,
    num_frames_actually_presented: u32,
    num_frames_to_generate_max: u32,
    reserved4: i8,
    is_vsync_support_available: i8,
    inputs_processing_completion_fence: *mut c_void,
    last_present_inputs_processing_completion_fence_value: u64,
    is_dynamic_mfg_supported: i8,
}

const DLSSG_STATE: StructType = guid(
    0xcc8a_c8e1,
    0xa179,
    0x44f5,
    [0x97, 0xfa, 0xe7, 0x41, 0x12, 0xf9, 0xbc, 0x61],
);

/// `sl::ReflexOptions`, `{F03AF81A-6D0B-4902-A651-C4965E215434}` version 1.
#[repr(C)]
struct ReflexOptions {
    base: Base,
    mode: u32,
    frame_limit_us: u32,
    use_markers_to_optimize: bool,
    virtual_key: u16,
    id_thread: u32,
}

const REFLEX_OPTIONS: StructType = guid(
    0xf03a_f81a,
    0x6d0b,
    0x4902,
    [0xa6, 0x51, 0xc4, 0x96, 0x5e, 0x21, 0x54, 0x34],
);

/// `sl::DLSSGMode`.
pub const MODE_OFF: u32 = 0;
pub const MODE_ON: u32 = 1;

/// `sl::DLSSGFlags`. `eRetainResourcesWhenOff` is recommended by the guide:
/// without it, every toggle gives the memory back and takes it again.
const DLSSG_RETAIN_RESOURCES_WHEN_OFF: u32 = 1 << 3;

/// `sl::PCLMarker`. The same numbers the NVAPI latency markers use, which is
/// the one piece of the hand-rolled Reflex work that carries over unchanged.
pub const MARKER_SIMULATION_START: u32 = 0;
pub const MARKER_SIMULATION_END: u32 = 1;
pub const MARKER_RENDERSUBMIT_START: u32 = 2;
pub const MARKER_RENDERSUBMIT_END: u32 = 3;
pub const MARKER_PRESENT_START: u32 = 4;
pub const MARKER_PRESENT_END: u32 = 5;

/// `sl::ReflexMode`.
const REFLEX_OFF: u32 = 0;
const REFLEX_LOW_LATENCY: u32 = 1;
const REFLEX_LOW_LATENCY_WITH_BOOST: u32 = 2;

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

type InitFn = unsafe extern "C" fn(*const Preferences, u64) -> u32;
type ShutdownFn = unsafe extern "C" fn() -> u32;
type SetD3DDeviceFn = unsafe extern "C" fn(*mut c_void) -> u32;
type UpgradeInterfaceFn = unsafe extern "C" fn(*mut *mut c_void) -> u32;
type GetNativeInterfaceFn = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> u32;
type GetNewFrameTokenFn = unsafe extern "C" fn(*mut *mut c_void, *const u32) -> u32;
type SetConstantsFn =
    unsafe extern "C" fn(*const Constants, *const c_void, *const ViewportHandle) -> u32;
type SetTagForFrameFn = unsafe extern "C" fn(
    *const c_void,
    *const ViewportHandle,
    *const ResourceTag,
    u32,
    *mut c_void,
) -> u32;
type GetFeatureFunctionFn = unsafe extern "C" fn(u32, *const u8, *mut *mut c_void) -> u32;
type SetFeatureLoadedFn = unsafe extern "C" fn(u32, bool) -> u32;
type IsFeatureSupportedFn = unsafe extern "C" fn(u32, *const c_void) -> u32;

type DlssgSetOptionsFn = unsafe extern "C" fn(*const ViewportHandle, *const DlssgOptions) -> u32;
type DlssgGetStateFn =
    unsafe extern "C" fn(*const ViewportHandle, *mut DlssgState, *const DlssgOptions) -> u32;
type ReflexSetOptionsFn = unsafe extern "C" fn(*const ReflexOptions) -> u32;
type ReflexSleepFn = unsafe extern "C" fn(*const c_void) -> u32;
type PclSetMarkerFn = unsafe extern "C" fn(u32, *const c_void) -> u32;

#[derive(Clone, Copy)]
struct Core {
    set_d3d_device: SetD3DDeviceFn,
    upgrade_interface: UpgradeInterfaceFn,
    /// Optional: only `reshade::report_stack` uses it, and a report is not a
    /// reason to refuse an interposer that lacks it.
    get_native_interface: Option<GetNativeInterfaceFn>,
    get_new_frame_token: GetNewFrameTokenFn,
    set_constants: SetConstantsFn,
    set_tag_for_frame: SetTagForFrameFn,
    get_feature_function: GetFeatureFunctionFn,
    set_feature_loaded: SetFeatureLoadedFn,
    /// Resolved but not called. It needs an `sl::AdapterInfo` by reference and
    /// we have none to give it -- see the note at the end of
    /// `bind_feature_functions`. Kept resolved so an interposer missing the
    /// export still fails loudly at load, and so restoring the check is a
    /// matter of building the struct rather than finding the entry point.
    #[allow(dead_code)]
    is_feature_supported: IsFeatureSupportedFn,
    shutdown: ShutdownFn,
}

#[derive(Clone, Copy)]
struct Features {
    dlssg_set_options: DlssgSetOptionsFn,
    dlssg_get_state: DlssgGetStateFn,
    reflex_set_options: ReflexSetOptionsFn,
    reflex_sleep: ReflexSleepFn,
    pcl_set_marker: PclSetMarkerFn,
}

static CORE: Mutex<Option<Core>> = Mutex::new(None);
static FEATURES: Mutex<Option<Features>> = Mutex::new(None);
static INITIALISED: AtomicBool = AtomicBool::new(false);
static TRIED: AtomicBool = AtomicBool::new(false);
/// Set when `init` has finished, whatever it concluded. The difference from
/// `INITIALISED` is the whole point: a waiter has to be released by a failure
/// as well as by a success.
static SETTLED: AtomicBool = AtomicBool::new(false);
/// Set by `start`, synchronously, before the thread exists.
///
/// `TRIED` is set by the worker once it is running, which leaves a window:
/// a creation call arriving in it would find `TRIED` clear, conclude nothing
/// had been started, and carry on without Streamline. This flag is what
/// `wait_until_ready` keys on instead, and it is set before the thread is
/// spawned so the window does not exist.
static STARTED: AtomicBool = AtomicBool::new(false);
static DEVICE_SET: AtomicBool = AtomicBool::new(false);
/// The engine's native device, kept because `slSetD3DDevice` wants the native
/// one and the engine's own pointer has by then been replaced with the proxy.
static NATIVE_DEVICE: AtomicUsize = AtomicUsize::new(0);
/// The viewport everything is tagged against. Arbitrary and ours to choose;
/// one is enough because this game renders one.
static VIEWPORT_ID: AtomicU32 = AtomicU32::new(1);

/// Held across the two calls that must not cross: handing Streamline the
/// engine's device, and shutting Streamline down because the mod stands down.
static LIFECYCLE: Mutex<()> = Mutex::new(());

fn viewport() -> ViewportHandle {
    ViewportHandle {
        base: Base::new(VIEWPORT, 1),
        value: VIEWPORT_ID.load(Ordering::Relaxed),
    }
}

/// Whether Streamline is loaded and initialised.
pub fn ready() -> bool {
    INITIALISED.load(Ordering::Relaxed)
}

/// Whether frame generation was asked for when the game started: `fg` was 1
/// at load, so `start` began initialising Streamline. Says nothing about
/// whether that succeeded; `ready` does.
pub fn asked_for() -> bool {
    STARTED.load(Ordering::Relaxed)
}

/// Whether `slSetD3DDevice` has succeeded.
///
/// Worth its own accessor because **every** `slUpgradeInterface` call is
/// gated on it, not only the device's: upgrading any interface activates
/// Streamline's API hooks and drives `initializePlugins`, which refuses if the
/// device has not been set. The device happens to be created before the
/// factory in this game, so the order works out -- but relying on an
/// observation when a check costs nothing is how this failure happened once
/// already.
pub fn device_is_set() -> bool {
    DEVICE_SET.load(Ordering::Relaxed)
}

/// Whether the feature functions are reachable, which needs both the device
/// set and the plugins initialised by the upgrade that follows it.
pub fn device_ready() -> bool {
    DEVICE_SET.load(Ordering::Relaxed) && FEATURES.lock().is_some()
}

fn core() -> Option<Core> {
    *CORE.lock()
}

fn features() -> Option<Features> {
    *FEATURES.lock()
}

unsafe fn resolve<T>(module: HMODULE, name: &[u8]) -> Option<T> {
    GetProcAddress(module, PCSTR(name.as_ptr())).map(|entry| std::mem::transmute_copy(&entry))
}

/// Where the Streamline binaries live: a folder beside the game.
///
/// Derived here rather than taken from `ngx::data_path`, and that is not
/// tidiness. `slInit` runs from the `D3D12CreateDevice` forwarder, and whether
/// the data path has been published by then depends on which of two startup
/// paths installed the render hooks first -- a race that is usually won one way
/// and would have made this fail silently when it was not.
///
/// Not vendored. The release archive is 276 MB and the files are NVIDIA's to
/// distribute, so the user places them and their absence is a clean "frame
/// generation unavailable" rather than a failure.
fn folder() -> Option<std::path::PathBuf> {
    let base = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))?
        .join("mhw-dlss");
    // Preferred: the same folder our own NGX client is pointed at. Since
    // 2.14.0 `sl.common` uses the Streamline plugin directory as NGX's
    // `InApplicationDataPath`, so putting the plugins beside
    // `nvngx_dlss.dll` means both NGX clients in this process see one snippet
    // set and the order they initialise in stops mattering.
    if base.join("sl.interposer.dll").exists() {
        return Some(base);
    }
    // Accepted, and reported: a subfolder works for Streamline but gives the
    // two NGX clients different application data paths. If the game's own
    // initialisation then fails to find `nvngx_dlss.dll` the upscaling path
    // goes with it, so this is named rather than silently tolerated.
    let nested = base.join("sl");
    if nested.join("sl.interposer.dll").exists() {
        crate::log!(
            "[sl ] the plugins are in {} rather than beside nvngx_dlss.dll in {}. That \
             works, but it gives Streamline's NGX client a different application data \
             path from ours -- if the [ngx] lines later show the game's own Init \
             failing, moving them up one folder is the first thing to try",
            nested.display(),
            base.display()
        );
        return Some(nested);
    }
    Some(base)
}

/// Begin initialising Streamline, on a thread, at load.
///
/// Not from the `D3D12CreateDevice` forwarder, natural as that place looks.
/// `slInit` loads six libraries and initialises a plugin manager, and doing
/// that inside a call the engine is waiting on delays everything after it:
/// the engine then creates its swapchain before the swapchain interception
/// is installed, the chain is DXGI's, nothing of this library is on the
/// presentation path, and the scene is never resolved -- a black screen with
/// the interface drawn over it.
///
/// The ordering required is this library first, Streamline second, and the
/// guide agrees: manual hooking says *"Before any hooked APIs are invoked,
/// call `slInit`"*, and `slInit` needs no device, because `slSetD3DDevice`
/// is the separate call for that. So this runs at load, on a thread of its
/// own, before the engine has asked for a device at all. Started any later
/// it loses the race with the engine's device and swapchain creation.
///
/// A thread, not `DllMain`: `LoadLibrary` under the loader lock deadlocks.
pub fn start() {
    if !crate::config::get().fg {
        return;
    }
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        init();
    });
}

/// Wait for `start` to finish, with a deadline.
///
/// This is what turns the remaining race into an ordering. The creation calls
/// cannot proceed without Streamline, and rather than hoping a worker thread
/// got there first they block on it -- once per process, on the engine's
/// thread, at a point where blocking is safe because the engine is inside a
/// call it expects to take time. The deadline is there so that a Streamline
/// that never settles costs a slow start rather than a hung game.
pub fn wait_until_ready(timeout: std::time::Duration) -> bool {
    if !STARTED.load(Ordering::SeqCst) {
        // Nothing was ever started, which is the `fg = 0` case.
        return false;
    }
    let deadline = std::time::Instant::now() + timeout;
    while !SETTLED.load(Ordering::SeqCst) {
        if std::time::Instant::now() > deadline {
            crate::log!(
                "[sl ] Streamline has not finished initialising after {:?}; carrying on \
                 without it",
                timeout
            );
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    INITIALISED.load(Ordering::Relaxed)
}

/// Wait for `start` to have finished, whichever way, with a deadline.
/// Returns at once when nothing was started.
///
/// For work that must not overlap `slInit`. A companion loaded with the
/// renderer -- ReShade -- rewrites the first bytes of functions in dxgi.dll
/// and d3d12.dll, and `slInit` is on another thread calling into those
/// libraries at the time: an exception inside `slInit` was seen at exactly
/// that moment. Returns whether initialisation had finished by the deadline.
pub fn wait_until_settled(timeout: std::time::Duration) -> bool {
    if !STARTED.load(Ordering::SeqCst) {
        return true;
    }
    let deadline = std::time::Instant::now() + timeout;
    while !SETTLED.load(Ordering::SeqCst) {
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    true
}

/// `sl::Result::eErrorExceptionHandler`: Streamline caught an exception
/// inside the call and unwound out of it.
const ERROR_EXCEPTION_HANDLER: u32 = 24;

/// Set when `slInit` ended in an exception.
static FAULTED: AtomicBool = AtomicBool::new(false);

/// Whether `slInit` ended in an exception in this session.
///
/// Streamline's handler unwinds its thread from wherever the exception
/// happened, and whatever that thread held at that point -- a lock of the
/// driver's, of NGX's -- may never be released. A call into NGX made after
/// that has been seen not to return. So the one call this library makes into
/// the driver for the game, NGX's initialisation, is not made unguarded in
/// such a session (`ngx::observe_game_init`).
pub fn faulted() -> bool {
    FAULTED.load(Ordering::SeqCst)
}

/// Load `sl.interposer.dll` and initialise Streamline.
///
/// By absolute path, because the guide requires it: *"Always pass in the full
/// path to a sl.*.dll file - relative paths are not allowed."*
fn init() -> bool {
    if TRIED.swap(true, Ordering::SeqCst) {
        return INITIALISED.load(Ordering::Relaxed);
    }
    let outcome = attempt();
    SETTLED.store(true, Ordering::SeqCst);
    outcome
}

/// Whether the mod has stood down, said once with how far Streamline had got.
///
/// A stand-down (a game build the mod does not know, a host that is not the
/// game) can be decided at any moment of start-up, and frame generation has
/// nothing to work with in one. Each step of `attempt` asks first, so
/// Streamline is left at whatever stage it had reached and goes no further;
/// none of the checks waits for anything, so the ordinary start is as early
/// as it was.
fn stood_down_before(step: &str, state: &str) -> bool {
    if !crate::gate::stood_down() {
        return false;
    }
    crate::log!(
        "[sl ] the mod is standing down, so {step} is not done: {state}. No frame generation in this session"
    );
    true
}

fn attempt() -> bool {
    let settings = crate::config::get();
    if stood_down_before("loading Streamline", "sl.interposer.dll is not loaded") {
        return false;
    }
    let Some(folder) = folder() else {
        crate::log!("[sl ] no data path yet, so Streamline cannot be located");
        return false;
    };
    // The folder Streamline is about to be handed is also the folder NGX
    // searches for snippets in Streamline's context, and one snippet must
    // not be in it. Measured outside the game, in two folders byte-identical
    // but for this one file:
    //
    //   plugin folder          slSetD3DDevice          dlss_gEntry  [FrameGeneration]
    //   with nvngx_dlisp.dll   eErrorExceptionHandler  0 lines      0
    //   without                eOk                     2 lines      12
    //
    // NGX initialises every snippet it selects, and it selects the game's
    // own 2019 DLISP 1.2.17 out of this folder. NGX's log shows that snippet
    // failing `NvAPI_D3D12_CreateCubinComputeShader` with nvapi status -1;
    // Streamline's log then shows `slSetD3DDevice` returning
    // `eErrorExceptionHandler` with a minidump written, and no line from
    // `sl.dlss_g`'s start-up at all. So frame generation never loads.
    //
    // The game's own capability check, which this mod depends on, does not
    // need this copy: it is served by the game's NGX context, from the
    // game's own nvngx_dlisp.dll in the game folder. Streamline's context
    // needs nvngx_dlss.dll and nvngx_dlssg.dll and nothing that faults. So
    // this folder must not hold a DLISP snippet, and that is checked.
    //
    // Checked rather than repaired: renaming a file is not this library's
    // business, and declining frame generation leaves the picture exactly
    // as `fg = 0` leaves it -- whereas proceeding puts an exception and then
    // a proxy swapchain into a running game.
    let hazard = folder.join("nvngx_dlisp.dll");
    if hazard.exists() {
        crate::log!(
            "[sl ] {} exists, so Streamline is not initialised and frame generation is \
             off. That snippet raises a debug break inside slSetD3DDevice, which aborts \
             sl.common's plugin startup before sl.dlss_g is ever reached. The game's own \
             DLSS option does not need this copy -- it is served from the game folder's \
             own nvngx_dlisp.dll -- so rename this one to nvngx_dlisp.dll.disabled",
            hazard.display()
        );
        return false;
    }

    let interposer = folder.join("sl.interposer.dll");
    if !interposer.exists() {
        crate::log!(
            "[sl ] {} is not there, so frame generation is unavailable. Run \
             fetch-nvidia.cmd in the mhw-dlss folder, which downloads NVIDIA's files into {}",
            interposer.display(),
            folder.display()
        );
        return false;
    }

    if stood_down_before("loading Streamline", "sl.interposer.dll is not loaded") {
        return false;
    }
    unsafe {
        let mut wide: Vec<u16> = interposer.to_string_lossy().encode_utf16().collect();
        wide.push(0);
        let module = match LoadLibraryW(windows::core::PCWSTR(wide.as_ptr())) {
            Ok(module) if !module.0.is_null() => module,
            _ => {
                crate::log!("[sl ] sl.interposer.dll would not load");
                return false;
            }
        };

        let core = (|| {
            Some(Core {
                set_d3d_device: resolve(module, b"slSetD3DDevice\0")?,
                upgrade_interface: resolve(module, b"slUpgradeInterface\0")?,
                get_native_interface: resolve(module, b"slGetNativeInterface\0"),
                get_new_frame_token: resolve(module, b"slGetNewFrameToken\0")?,
                set_constants: resolve(module, b"slSetConstants\0")?,
                set_tag_for_frame: resolve(module, b"slSetTagForFrame\0")?,
                get_feature_function: resolve(module, b"slGetFeatureFunction\0")?,
                set_feature_loaded: resolve(module, b"slSetFeatureLoaded\0")?,
                is_feature_supported: resolve(module, b"slIsFeatureSupported\0")?,
                shutdown: resolve(module, b"slShutdown\0")?,
            })
        })();
        let Some(core) = core else {
            crate::log!("[sl ] sl.interposer.dll is missing an entry point this needs");
            return false;
        };
        let Some(initialise): Option<InitFn> = resolve(module, b"slInit\0") else {
            crate::log!("[sl ] sl.interposer.dll has no slInit");
            return false;
        };

        let plugin_path: Vec<u16> = folder
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let paths = [plugin_path.as_ptr()];
        let logs: Vec<u16> = folder
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let wanted = [FEATURE_DLSS_G, FEATURE_REFLEX, FEATURE_PCL];

        // Manual hooking, because the alternative is Streamline replacing
        // dxgi.dll in the load order, and no library can be replaced in this
        // game's folder early enough (see the table in `proxy.rs`). The
        // factory proxy flag goes with it: it exists for injection, by name,
        // and it keeps Streamline's interposition an object rather than a
        // fight over the vtable slots `swap.rs` also patches.
        //
        // `fg_ota` is Streamline's own default; see `FLAG_ALLOW_OTA`.
        let mut flags = FLAG_DISABLE_CL_STATE_TRACKING
            | FLAG_USE_MANUAL_HOOKING
            | FLAG_USE_DXGI_FACTORY_PROXY
            | FLAG_USE_FRAME_BASED_RESOURCE_TAGGING;
        if settings.fg_ota {
            flags |= FLAG_ALLOW_OTA | FLAG_LOAD_DOWNLOADED_PLUGINS;
        }

        // Identity. `sl::Preferences` carries two alternatives and documents
        // them as alternatives in both directions: `applicationId` is
        // *"Optional - Id provided by NVIDIA, if not specified then engine
        // type and version are required"*, `engine` is *"Optional - Type of
        // the rendering engine used, if not specified then applicationId is
        // required"*, and `projectId` is *"Optional - GUID"* with an example.
        // The DLSS-G guide's integration checklist is the same sentence:
        // *"Provide either correct application ID or engine type (Unity, UE
        // etc.) when calling slInit"*.
        //
        // Which of the two Streamline takes is not a judgement call.
        // `sl.common`'s `slOnPluginStartup` decides it on one line:
        //
        //     bool hasProjectId = !engineVersion.empty() && !projectId.empty();
        //
        // With both strings non-empty it calls
        // `NVSDK_NGX_D3D12_Init_with_ProjectID` and the application id never
        // reaches NGX at all; otherwise it calls `NVSDK_NGX_D3D12_Init` with
        // the id. `getNGXFeatureRequirements`, in the same file, selects the
        // route with the opposite test -- `projectId.empty() &&
        // engineVersion.empty()` -- so **both** strings have to be set or the
        // requirements query and the init disagree about who we are. That is
        // why `fg_identity` sends the pair or neither, and never one.
        //
        // `application_id` is sent on both routes. On the project route NGX
        // does not see it, but Streamline still uses it for its own OTA
        // denylist check and logs it, and it is accurate: we are inside this
        // game. Note it must stay non-zero -- see the comment on the field.
        let engine_version = crate::config::packed_str(&settings.fg_engine_version);
        let project_id = crate::config::packed_str(&settings.fg_project_id);
        let by_project =
            settings.fg_identity == 1 && !engine_version.is_empty() && !project_id.is_empty();
        // Borrowed by `slInit` for the duration of the call only -- the plugin
        // manager copies both into `std::string`s before it returns.
        let engine_version_c = format!("{engine_version}\0");
        let project_id_c = format!("{project_id}\0");

        let preferences = Preferences {
            base: Base::new(PREFERENCES, 1),
            show_console: false,
            log_level: settings.fg_log_level,
            paths_to_plugins: paths.as_ptr(),
            num_paths_to_plugins: 1,
            path_to_logs_and_data: logs.as_ptr(),
            allocate_callback: std::ptr::null_mut(),
            release_callback: std::ptr::null_mut(),
            // The channel that should have existed from the start. See
            // `on_log_message`.
            log_message_callback: on_log_message as *mut c_void,
            flags,
            features_to_load: wanted.as_ptr(),
            num_features_to_load: wanted.len() as u32,
            // The game's own, observed from its `NVSDK_NGX_D3D12_Init` and
            // written to the log of every session: 0x5f8a85f. It cannot be
            // learnt at load time -- the game initialises NGX much later --
            // so it is written down, with `ngx_app_id` to override it.
            //
            // Zero is not an option on either route. With zero, `sl.common`
            // answers "Please provide correct application id when calling
            // slInit -- NGX based features will be disabled", which disables
            // exactly DLSS-G and leaves Reflex and PCL working, since those
            // need no NGX; Streamline's source shows the plugin manager
            // turning `applicationId == 0` into `kTemporaryAppId`, 100721531,
            // which production plugins refuse.
            //
            // This id does not decide the driver profile. That is matched on
            // the executable name, as NGX's own log says -- *"DRS PROFILE
            // FOUND : ProfileName 'Monster Hunter World', AppName
            // 'monsterhunterworld.exe'"* -- and this library runs inside that
            // executable.
            application_id: if settings.ngx_app_id != 0 {
                settings.ngx_app_id as u32
            } else {
                MHW_NGX_APPLICATION_ID
            },
            engine: if by_project { settings.fg_engine } else { 0 },
            engine_version: if by_project {
                engine_version_c.as_ptr()
            } else {
                std::ptr::null()
            },
            project_id: if by_project {
                project_id_c.as_ptr()
            } else {
                std::ptr::null()
            },
            render_api: settings.fg_render_api,
        };
        // Every field that decides who we are, before the call, so that a
        // failed run still says what was tried. The entry point named is the
        // one `sl.common` will reach given these fields.
        crate::log!(
            "[sl ] slInit identity: route={} applicationId={} engine={} engineVersion={} \
             projectId={} -> NGX via {}",
            if by_project {
                "engine/project"
            } else {
                "application-id"
            },
            preferences.application_id,
            match preferences.engine {
                0 => "0 eCustom",
                1 => "1 eUnreal",
                2 => "2 eUnity",
                _ => "?",
            },
            if by_project { engine_version } else { "<null>" },
            if by_project { project_id } else { "<null>" },
            if by_project {
                "NVSDK_NGX_D3D12_Init_with_ProjectID"
            } else {
                "NVSDK_NGX_D3D12_Init"
            }
        );
        if settings.fg_identity == 1 && !by_project {
            crate::log!(
                "[sl ] fg_identity = 1 but fg_engine_version or fg_project_id is empty, so \
                 the application-id route was taken instead. Both have to be set."
            );
        }
        if stood_down_before("slInit", "sl.interposer.dll is loaded and not initialised") {
            return false;
        }
        let result = if settings.diag_streamline_fault {
            crate::log!(
                "[sl ] diag_streamline_fault = 1: slInit is not called and is treated as \
                 having ended in an exception"
            );
            ERROR_EXCEPTION_HANDLER
        } else {
            initialise(&preferences, SDK_VERSION)
        };
        crate::log!(
            "[sl ] slInit(flags {flags:#x} with OTA {}, renderAPI {}, sdk {SDK_VERSION:#x}) -> {}",
            if flags & (FLAG_ALLOW_OTA | FLAG_LOAD_DOWNLOADED_PLUGINS) != 0 {
                "allowed"
            } else {
                "cleared"
            },
            settings.fg_render_api,
            name(result)
        );
        if result == ERROR_EXCEPTION_HANDLER {
            FAULTED.store(true, Ordering::SeqCst);
            crate::log!(
                "[sl ] Streamline raised an exception while initialising and is not used: no \
                 frame generation in this session. Its thread was unwound from wherever the \
                 exception happened, so the game's initialisation of NGX is given a time \
                 limit ([ngx] lines below)"
            );
        }
        if result != OK {
            return false;
        }
        *CORE.lock() = Some(core);
        INITIALISED.store(true, Ordering::SeqCst);
        crate::log!("[sl ] Streamline is loaded from {}", folder.display());
        // The stand-down may have been decided while `slInit` ran, in which
        // case its own call to `stand_down` found nothing initialised yet.
        // One of the two always sees the other: this flag is set before the
        // question is asked here, and the gate's state before it asks.
        if crate::gate::stood_down() {
            stand_down();
            return false;
        }

        // Report what the callback did, not that it was installed -- and use
        // the answer. By this point plugin loading has logged hundreds of
        // lines, so zero means the callback never fired and the file is the
        // only channel left.
        let delivered = LOG_LINES.load(Ordering::Relaxed);
        if delivered == 0 {
            crate::log!(
                "[sl ] the log callback delivered nothing during slInit, so sl.log is \
                 polled as a fallback"
            );
            fold_log(folder.clone(), "sl.log", true);
        } else {
            crate::log!(
                "[sl ] the log callback delivered {delivered} lines during slInit, so \
                 sl.log is not polled"
            );
        }

        // NGX writes its own file, which is not sl.log and is not forwarded
        // through Streamline. It is the file that held the answer: sl.log's
        // `ngxLog` stream carries only the NGX context `sl.common` created,
        // and the context the game creates -- a different one, with a
        // different search path -- reports only here. Folded whole, because
        // it is tens of kilobytes and has no severity tags to filter on.
        fold_log(folder, "nvngx.log", false);
        true
    }
}

/// How many lines Streamline has handed us through the callback.
///
/// Read once after `slInit` to decide whether the file fallback is needed: if
/// the callback is working there are hundreds by then, and if it is not there
/// are none.
static LOG_LINES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `sl::PFun_LogMessageCallback`, which is `void(LogType, const char*)`.
///
/// Everything Streamline says arrives here, in order, on the thread that said
/// it, with no file in between. The file is not a substitute: Streamline
/// truncates and reopens `sl.log`, so a polling reader such as `fold_log`
/// loses whatever is written and truncated between two polls, and that
/// includes the lines where features are decided.
///
/// `LogType` is `eInfo`, `eWarn`, `eError`. Streamline only calls this for
/// messages at or below the `logLevel` we asked for, which `fg_log_level`
/// sets. That decides which messages, not how many: one warning that
/// Streamline repeats every frame is a line every frame, so what is written
/// goes through `forward` and its bound.
///
/// # Safety
/// Called by Streamline from arbitrary threads. Nothing here may unwind into
/// C++ -- that is undefined behaviour, not a crash we would get to explain --
/// so the body is wrapped, and the only pointer read is null-checked.
unsafe extern "C" fn on_log_message(kind: u32, message: *const std::os::raw::c_char) {
    if message.is_null() {
        return;
    }
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let text = std::ffi::CStr::from_ptr(message).to_string_lossy();
        let level = match kind {
            0 => "info",
            1 => "warn",
            _ => "ERROR",
        };
        // Streamline's messages carry their own trailing newline, and NGX's
        // forwarded ones can carry several lines in one message. One line of
        // ours per line of theirs keeps the log greppable.
        for line in text.lines() {
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            LOG_LINES.fetch_add(1, Ordering::Relaxed);
            forward(level, line);
        }
    }));
}

/// Write one of Streamline's lines into our log, within a bound.
///
/// Everything Streamline says during start-up is written: that is a few
/// hundred lines at the default level and a few thousand at the verbose one,
/// and it is where features are decided. After that allowance a kind of line
/// is held to a rate, with the number left out said when writing resumes, so
/// a message repeated per frame costs a handful of lines a minute instead of
/// one per frame for the whole session. Complaints have their own allowance,
/// so that information cannot crowd them out.
fn forward(level: &str, line: &str) {
    static INFORMATION: crate::logger::Throttle = crate::logger::Throttle::new(4000, 20, 10);
    static COMPLAINTS: crate::logger::Throttle = crate::logger::Throttle::new(1000, 30, 10);
    if level == "info" || level == "ngx " {
        crate::log_limited!(INFORMATION, "[sl*] {level}: {line}");
    } else {
        crate::log_limited!(COMPLAINTS, "[sl*] {level}: {line}");
    }
}

/// Whether initialisation has finished, either way.
pub fn settled() -> bool {
    SETTLED.load(Ordering::SeqCst)
}

/// Replace a freshly created D3D or DXGI interface with Streamline's proxy.
///
/// `slUpgradeInterface` takes a pointer to the interface pointer and replaces
/// it **in place**, and must be called immediately after creation -- which is
/// the shape of every interception point this library owns, so substitution is
/// one call on a pointer we already hold.
///
/// # Safety
/// `slot` must point at a live interface pointer the caller owns.
pub unsafe fn upgrade(what: &str, slot: *mut *mut c_void) -> bool {
    if !INITIALISED.load(Ordering::Relaxed) || slot.is_null() || (*slot).is_null() {
        return false;
    }
    let Some(core) = core() else {
        return false;
    };
    let before = *slot;
    let result = (core.upgrade_interface)(slot);
    crate::log!(
        "[sl ] slUpgradeInterface({what}) -> {}{}",
        name(result),
        if result == OK && *slot != before {
            format!("  ({:#x} became {:#x})", before as usize, *slot as usize)
        } else if result == OK {
            "  (same pointer; SL patched the vtable in place)".to_string()
        } else {
            String::new()
        }
    );
    result == OK
}

/// The object a Streamline proxy wraps, for reporting what a stack is made of.
///
/// `slGetNativeInterface` adds a reference to what it returns; it is released
/// here at once, because the proxy holds it for as long as the proxy lives and
/// the caller only wants to know what it is.
///
/// # Safety
/// `proxy` must be a live COM object.
pub unsafe fn native_interface(proxy: *mut c_void) -> Option<*mut c_void> {
    if !INITIALISED.load(Ordering::Relaxed) || proxy.is_null() {
        return None;
    }
    let get = core()?.get_native_interface?;
    let mut base: *mut c_void = std::ptr::null_mut();
    if get(proxy, &mut base) != OK || base.is_null() {
        return None;
    }
    type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;
    let release: ReleaseFn = std::mem::transmute(*(*(base as *const *const usize)).add(2));
    release(base);
    Some(base)
}

/// Remember the engine's native device for `slSetD3DDevice`.
///
/// Numbered, because the game asks for more than one. The last one wins, on
/// the reasoning that an engine which probes first keeps the device it created
/// last -- but that is a guess, and a guess is why this says which it took.
/// `gpu::install` latches onto the *first*, so when the numbers disagree our
/// hooks and Streamline are looking at different objects and the log has to
/// say so rather than leave it to be inferred.
pub fn note_native_device(which: u32, device: *mut c_void) {
    let previous = NATIVE_DEVICE.swap(device as usize, Ordering::Relaxed);
    if previous != 0 && previous != device as usize {
        crate::log!(
            "[sl ] device #{which} is {:#x}, replacing {previous:#x} -- the game has \
             created more than one and this is the one Streamline will be given",
            device as usize
        );
    }
}

/// Hand Streamline the device.
///
/// **This must happen before `slUpgradeInterface`.** `sl.h` only says the
/// feature functions need the device, but upgrading an interface is itself
/// what activates Streamline's API hooks and drives `initializePlugins`, and
/// without a device that fails, in Streamline's own words:
///
/// ```text
///   sl.cpp:654  Upgrading ID3D12Device to use SL proxy ...
///   pluginManager.cpp:1362[initializePlugins] D3D or VK API hook is activated
///     without device being created, did you forget to call `slSetD3DDevice`
///     ... or trying to use another SL API before setting the device?
/// ```
///
/// Plugin initialisation fails there and nothing downstream of it exists.
///
/// This call starts Streamline's feature initialisation, and through
/// `sl.dlss_g` a second NGX client in a process where this library already
/// observes the game's `NVSDK_NGX_D3D12_Init`. NGX does not take two threads
/// initialising at once; this runs on the engine's own thread inside its
/// swapchain creation, before the game initialises NGX, so the two are
/// sequential.
///
/// Both NGX clients must see one snippet set. Since 2.14.0 `sl.common` uses
/// the Streamline plugin directory as NGX's `InApplicationDataPath`, and the
/// plugins are in the folder that holds `nvngx_dlss.dll`. `folder` prefers
/// that layout and says so when it cannot have it.
///
/// # Safety
/// `device` must be a live `ID3D12Device`, as the engine holds it.
pub unsafe fn set_device(device: *mut c_void) -> bool {
    if DEVICE_SET.load(Ordering::Relaxed) {
        return true;
    }
    // Against `stand_down`, which shuts Streamline down only while this has
    // not happened.
    let _lifecycle = LIFECYCLE.lock();
    if !INITIALISED.load(Ordering::Relaxed) || device.is_null() {
        return false;
    }
    let Some(core) = core() else {
        return false;
    };
    let result = (core.set_d3d_device)(device);
    crate::log!(
        "[sl ] slSetD3DDevice({:#x}) -> {}",
        device as usize,
        name(result)
    );
    if result != OK {
        return false;
    }
    GIVEN_DEVICE.store(device as usize, Ordering::Relaxed);
    DEVICE_SET.store(true, Ordering::SeqCst);
    true
}

/// The device `slSetD3DDevice` accepted, as it was given.
static GIVEN_DEVICE: AtomicUsize = AtomicUsize::new(0);

/// That device, or zero. With ReShade loaded it is ReShade's proxy device, on
/// purpose, and Streamline initialises its own NGX context with it.
pub fn given_device() -> usize {
    GIVEN_DEVICE.load(Ordering::Relaxed)
}

/// Ask for DLSS-G to be loaded, which has to happen before the swapchain is
/// created.
///
/// `slInit` names the features to load, but the guide's own swapchain-
/// recreation sample calls `slSetFeatureLoaded` either side of a toggle, and
/// OptiScaler calls it on the way up. It costs one call and it removes a
/// difference between us and a working integration.
pub fn load_feature() -> bool {
    let Some(core) = core() else {
        return false;
    };
    unsafe {
        let result = (core.set_feature_loaded)(FEATURE_DLSS_G, true);
        crate::log!("[sl ] slSetFeatureLoaded(DLSS-G, true) -> {}", name(result));
        result == OK
    }
}

/// Resolve the feature functions, once the device is set.
///
/// Separate from `set_device` because the plugins are initialised by the
/// *upgrade* that follows it, and asking for a feature function before that
/// has happened is the error above in a different costume.
pub fn bind_feature_functions() -> bool {
    if FEATURES.lock().is_some() {
        return true;
    }
    if !DEVICE_SET.load(Ordering::Relaxed) {
        return false;
    }
    let Some(core) = core() else {
        return false;
    };
    unsafe {
        bind_features(&core);
    }
    FEATURES.lock().is_some()
}

unsafe fn bind_features(core: &Core) {
    let mut found: Option<Features> = None;
    let get = |feature: u32, name: &str| -> Option<*mut c_void> {
        let key = CString::new(name).ok()?;
        let mut address: *mut c_void = std::ptr::null_mut();
        let result = (core.get_feature_function)(feature, key.as_ptr() as *const u8, &mut address);
        if result != OK || address.is_null() {
            crate::log!("[sl ] {name} did not resolve ({})", result_name(result));
            return None;
        }
        Some(address)
    };
    let all = (|| {
        Some(Features {
            dlssg_set_options: std::mem::transmute(get(FEATURE_DLSS_G, "slDLSSGSetOptions")?),
            dlssg_get_state: std::mem::transmute(get(FEATURE_DLSS_G, "slDLSSGGetState")?),
            reflex_set_options: std::mem::transmute(get(FEATURE_REFLEX, "slReflexSetOptions")?),
            reflex_sleep: std::mem::transmute(get(FEATURE_REFLEX, "slReflexSleep")?),
            pcl_set_marker: std::mem::transmute(get(FEATURE_PCL, "slPCLSetMarker")?),
        })
    })();
    match all {
        Some(features) => {
            found = Some(features);
            crate::log!("[sl ] the feature functions are bound");
        }
        None => crate::log!("[sl ] a feature function is missing; frame generation is off"),
    }
    *FEATURES.lock() = found;

    // `slIsFeatureSupported` is deliberately *not* called here. Its
    // declaration is
    //
    //     sl::Result slIsFeatureSupported(sl::Feature feature,
    //                                     const sl::AdapterInfo& adapter);
    //
    // and the second parameter is a **reference**, not an optional pointer:
    // a null there is dereferenced, and Streamline's exception handler
    // returns `eErrorExceptionHandler`. This module declares no
    // `sl::AdapterInfo`, and nothing acts on the answer; the answer that is
    // used is `slSetFeatureLoaded`'s. Calling it would mean declaring the
    // structure and filling it from `ID3D12Device::GetAdapterLuid`.
}

// ---------------------------------------------------------------------------
// Per frame
// ---------------------------------------------------------------------------

/// This frame's token, which every other call is keyed on.
///
/// Streamline owns the object; we only ever hold the pointer and hand it back.
/// `FrameToken` is an abstract class with a virtual `operator uint32_t`, so
/// reading the index out of it would mean calling through its vtable -- which
/// is never necessary, because we choose the index.
static FRAME_TOKEN: AtomicUsize = AtomicUsize::new(0);
static FRAME_INDEX: AtomicU32 = AtomicU32::new(0);

/// Start a frame. Returns false if Streamline is not in a state to take one.
pub fn begin_frame() -> bool {
    if !DEVICE_SET.load(Ordering::Relaxed) {
        return false;
    }
    let Some(core) = core() else {
        return false;
    };
    // Claimed, not consumed, until the call succeeds. A failed call must not
    // burn an index: a hole in the sequence is what the plugin complains
    // about when it asks for an index nothing was ever set under.
    let index = FRAME_INDEX.load(Ordering::Relaxed) + 1;
    let mut token: *mut c_void = std::ptr::null_mut();
    unsafe {
        let result = (core.get_new_frame_token)(&mut token, &index);
        if result != OK || token.is_null() {
            if !SAID_TOKEN.swap(true, Ordering::Relaxed) {
                crate::log!("[sl ] slGetNewFrameToken({index}) -> {}", name(result));
            }
            return false;
        }
    }
    FRAME_INDEX.store(index, Ordering::Relaxed);
    FRAME_TOKEN.store(token as usize, Ordering::Relaxed);
    // Cleared here and set by `set_constants`, so `token_used` answers whether
    // anything was ever filed under the token we are about to replace.
    CONSTANTS_INDEX.store(0, Ordering::Relaxed);
    TAGS_INDEX.store(0, Ordering::Relaxed);
    true
}

/// The index of the token now current, and the indices the constants and the
/// tags were last filed under.
///
/// These three have to be equal for a rendered frame, and the plugin's own
/// complaints name a fourth number to compare them against:
///
/// ```text
///   commonInterface.h:622[get] Unable to find 'common' constants for
///                              frame 3545 - id 1 - using last set for frame ...
///   commonInterface.h:256[getTaggedResource] Failed to find global tag
///                              'kBufferTypeDepth'
/// ```
///
/// `id 1` is our viewport, which matches, so the disagreement is in the frame
/// index -- and *which* of these three drifts from the plugin's number says
/// which call is in the wrong place. Three things that must agree, moved in
/// three separate rounds, is how this kind of mismatch happens; printing them
/// together is how it stops being a hypothesis.
pub fn frame_indices() -> (u32, u32, u32) {
    (
        FRAME_INDEX.load(Ordering::Relaxed),
        CONSTANTS_INDEX.load(Ordering::Relaxed),
        TAGS_INDEX.load(Ordering::Relaxed),
    )
}

/// Whether anything was filed under the current token.
///
/// A token nothing was set under is worse than no new token at all: the plugin
/// asks under it, finds neither constants nor tags, and disables interpolation
/// for that frame -- which is the toggling, ten times a session.
pub fn token_used() -> bool {
    CONSTANTS_INDEX.load(Ordering::Relaxed) != 0
}

static CONSTANTS_INDEX: AtomicU32 = AtomicU32::new(0);
static TAGS_INDEX: AtomicU32 = AtomicU32::new(0);

static SAID_TOKEN: AtomicBool = AtomicBool::new(false);

fn token() -> *const c_void {
    FRAME_TOKEN.load(Ordering::Relaxed) as *const c_void
}

/// Whether a token for the current frame is open.
pub fn has_frame() -> bool {
    FRAME_TOKEN.load(Ordering::Relaxed) != 0
}

/// The frame index Streamline was given, so the markers can agree with it.
pub fn frame_index() -> u32 {
    FRAME_INDEX.load(Ordering::Relaxed)
}

/// What one frame's constants are built from.
pub struct Camera {
    /// Near, far, vertical field of view and aspect, read out of the
    /// projection below. `None` leaves those four constants invalid.
    pub lens: Option<crate::camera::Lens>,
    /// Position and basis in world space, read out of the engine's inverse
    /// view matrix. `None` leaves those four constants invalid.
    pub pose: Option<crate::camera::Pose>,
    /// Row-major, no jitter.
    pub view_to_clip: [f32; 16],
    pub clip_to_view: [f32; 16],
    pub clip_to_prev_clip: [f32; 16],
    pub prev_clip_to_clip: [f32; 16],
    /// In pixels, as the engine was told.
    pub jitter: (f32, f32),
    /// Scale that takes the motion vectors into the -1..1 Streamline wants.
    pub mvec_scale: (f32, f32),
    /// Whether the tagged vectors still carry the jitter. See the note at the
    /// flag itself.
    pub jittered: bool,
    /// Whether they have been dilated towards the nearest depth.
    pub dilated: bool,
    pub reset: bool,
}

/// Hand over this frame's common constants.
pub fn set_constants(camera: &Camera) -> bool {
    let Some(core) = core() else {
        return false;
    };
    let frame = token();
    if frame.is_null() {
        return false;
    }
    let constants = Constants {
        base: Base::new(CONSTANTS, 2),
        camera_view_to_clip: camera.view_to_clip,
        clip_to_camera_view: camera.clip_to_view,
        // Optional, and we have no lens distortion to describe.
        clip_to_lens_clip: IDENTITY,
        clip_to_prev_clip: camera.clip_to_prev_clip,
        prev_clip_to_clip: camera.prev_clip_to_clip,
        jitter_offset: [camera.jitter.0, camera.jitter.1],
        mvec_scale: [camera.mvec_scale.0, camera.mvec_scale.1],
        camera_pinhole_offset: [0.0, 0.0],
        // **Not optional, and they were left invalid from the first working
        // run until Streamline's own warning was finally read:**
        //
        //     commonEntry.cpp:599[validateCommonConstants] Value
        //     consts.cameraPos should not be left as invalid
        //
        // and the same for up, right, forward, near, far, field of view and
        // aspect. The header's rule is *"all parameters must be provided
        // unless they are marked as optional"*, none of these eight is so
        // marked, and the snippet has a `DLSSG.Camera*` parameter for each.
        // The comment that stood here said the snippet checks the basis for
        // orthonormality and complains; neither binary contains a string to
        // that effect, and the basis below is the engine's own inverse view
        // matrix, checked for orthonormality before it gets here.
        //
        // Conventions, each read rather than assumed:
        //   * world space, the engine's units, for the position and basis;
        //   * the field of view is the **vertical** one, in radians. The
        //     header says only *"in radians"* and the NGX guide says *"in
        //     degrees or radians"*; vertical is what OptiScaler passes in
        //     the same field, and it is the one the projection's P11 defines
        //     without reference to the aspect;
        //   * the aspect is width over height, as the header says;
        //   * near is the nearer plane whichever way depth runs, and an
        //     infinite far plane is sent as infinity, which is what
        //     OptiScaler sends for it.
        camera_pos: camera.pose.map_or([INVALID_FLOAT; 3], |pose| pose.position),
        camera_up: camera.pose.map_or([INVALID_FLOAT; 3], |pose| pose.up),
        camera_right: camera.pose.map_or([INVALID_FLOAT; 3], |pose| pose.right),
        camera_fwd: camera.pose.map_or([INVALID_FLOAT; 3], |pose| pose.forward),
        camera_near: camera.lens.map_or(INVALID_FLOAT, |lens| lens.near),
        camera_far: camera.lens.map_or(INVALID_FLOAT, |lens| lens.far),
        camera_fov: camera.lens.map_or(INVALID_FLOAT, |lens| lens.fov_vertical),
        camera_aspect_ratio: camera.lens.map_or(INVALID_FLOAT, |lens| lens.aspect),
        motion_vectors_invalid_value: INVALID_FLOAT,
        // The engine's depth is reversed, which is the same fact `dlss_flags`
        // bit 3 encodes for upscaling.
        depth_inverted: TRUE,
        // The engine's vectors carry the camera's motion: they are derived
        // from the current and previous view-projections, which is why
        // upscaling needs no separate camera term.
        camera_motion_included: TRUE,
        motion_vectors_3d: FALSE,
        reset: if camera.reset { TRUE } else { FALSE },
        orthographic_projection: FALSE,
        // Both of these describe `mv::decode`'s output rather than the
        // engine's raw buffer, and both are switchable because "the decode
        // removes the jitter" is a claim about our own shader that has been
        // verified for upscaling and never for interpolation.
        //
        // The reasoning for the defaults: `mv_dejitter = 1.0` adds `J_curr`
        // back, which exists because the engine derives its vectors from the
        // matrices this library jitters while keeping the previous frame's
        // unjittered -- so a still point reads `-J_curr` instead of zero, and
        // the decode corrects it. That makes `jittered` false. And the decode
        // writes one vector per pixel with no neighbourhood search, which
        // makes `dilated` false.
        motion_vectors_dilated: if camera.dilated { TRUE } else { FALSE },
        motion_vectors_jittered: if camera.jittered { TRUE } else { FALSE },
        min_relative_linear_depth_object_separation: 40.0,
    };
    let handle = viewport();
    unsafe {
        let result = (core.set_constants)(&constants, frame, &handle);
        if result == OK {
            CONSTANTS_INDEX.store(FRAME_INDEX.load(Ordering::Relaxed), Ordering::Relaxed);
            *LAST_CAMERA.lock() = Some((camera.lens, camera.pose));
        }
        if result != OK {
            if !SAID_CONSTANTS.swap(true, Ordering::Relaxed) {
                crate::log!("[sl ] slSetConstants -> {}", name(result));
            }
            return false;
        }
        // Once, on success, with the four values that decide whether the
        // interpolation can be right rather than merely accepted. A wrong
        // motion-vector scale or an upright depth is taken silently and shows
        // up as warping, which is the hardest kind of fault to attribute --
        // so the numbers go in the log before anyone has to judge a picture.
        if !SAID_CONSTANTS_OK.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[sl ] slSetConstants took: mvec scale ({}, {}), jitter ({:.4}, {:.4}), \
                 depth inverted, camera motion included, vectors {} and {}",
                camera.mvec_scale.0,
                camera.mvec_scale.1,
                camera.jitter.0,
                camera.jitter.1,
                if camera.dilated {
                    "dilated"
                } else {
                    "not dilated"
                },
                if camera.jittered {
                    "jittered"
                } else {
                    "not jittered"
                }
            );
            crate::log!(
                "[sl ] slSetConstants took: {}",
                describe_camera(camera.lens, camera.pose)
            );
        }
        true
    }
}

static SAID_CONSTANTS_OK: AtomicBool = AtomicBool::new(false);

/// The lens and pose `slSetConstants` last accepted, so the periodic state
/// line can print the camera of the frame being judged and not only the
/// camera of the first frame of the session, which is a title screen's.
static LAST_CAMERA: Mutex<Option<(Option<crate::camera::Lens>, Option<crate::camera::Pose>)>> =
    Mutex::new(None);

/// The eight camera constants as they were sent, in the header's words.
fn describe_camera(lens: Option<crate::camera::Lens>, pose: Option<crate::camera::Pose>) -> String {
    let lens = match lens {
        Some(lens) => format!(
            "cameraNear {}, cameraFar {}, cameraFOV {:.5} rad vertical ({:.2} deg), \
             cameraAspectRatio {:.5}, read from a {} projection",
            lens.near,
            lens.far,
            lens.fov_vertical,
            lens.fov_vertical.to_degrees(),
            lens.aspect,
            if lens.reversed {
                "reversed-depth"
            } else {
                "conventional-depth"
            }
        ),
        None => String::from("cameraNear, cameraFar, cameraFOV, cameraAspectRatio LEFT INVALID"),
    };
    let pose = match pose {
        Some(pose) => {
            let v = |a: [f32; 3]| format!("({:.4}, {:.4}, {:.4})", a[0], a[1], a[2]);
            format!(
                "cameraPos ({:.3}, {:.3}, {:.3}), cameraRight {}, cameraUp {}, cameraFwd {}",
                pose.position[0],
                pose.position[1],
                pose.position[2],
                v(pose.right),
                v(pose.up),
                v(pose.forward)
            )
        }
        None => String::from("cameraPos, cameraRight, cameraUp, cameraFwd LEFT INVALID"),
    };
    format!("{lens}; {pose}")
}

static SAID_CONSTANTS: AtomicBool = AtomicBool::new(false);

const IDENTITY: [f32; 16] = [
    1.0, 0.0, 0.0, 0.0, //
    0.0, 1.0, 0.0, 0.0, //
    0.0, 0.0, 1.0, 0.0, //
    0.0, 0.0, 0.0, 1.0,
];

/// One resource to tag, with the state it is actually in.
pub struct Tagged {
    pub resource: *mut c_void,
    pub kind: Kind,
    pub state: u32,
    pub size: (u32, u32),
    pub format: u32,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Depth,
    MotionVectors,
    HudlessColour,
    NoWarpMask,
    UiAlpha,
}

impl Kind {
    fn id(self) -> u32 {
        match self {
            Kind::Depth => BUFFER_DEPTH,
            Kind::MotionVectors => BUFFER_MOTION_VECTORS,
            Kind::HudlessColour => BUFFER_HUDLESS_COLOUR,
            Kind::NoWarpMask => BUFFER_NO_WARP_MASK,
            Kind::UiAlpha => BUFFER_UI_ALPHA,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Depth => "depth",
            Kind::MotionVectors => "mvec",
            Kind::HudlessColour => "hudless",
            Kind::NoWarpMask => "no-warp mask",
            Kind::UiAlpha => "UI alpha",
        }
    }
}

/// Tag this frame's resources.
///
/// An item whose `resource` is null **removes** that tag for the frame: the
/// header says of the tag pointer *"set to null to remove the specified tag"*,
/// and the `ResourceTag` is sent with a null resource pointer. That is how an
/// experimental input is switched off again rather than merely no longer
/// refreshed.
///
/// `list` is a command list Streamline may record into -- it is allowed to be
/// null only when every tag is `eValidUntilPresent`, which these are, but one
/// is passed where the caller has one because the guide prefers it.
///
/// # Safety
/// Every resource must be live and every state must be the state it is
/// actually in; the header calls that mandatory and it is the one thing here
/// that cannot be checked.
pub unsafe fn tag(items: &[Tagged], list: *mut c_void) -> bool {
    let Some(core) = core() else {
        return false;
    };
    let frame = token();
    if frame.is_null() || items.is_empty() {
        return false;
    }
    // The resources have to outlive the call, and `ResourceTag` holds pointers
    // into them, so both arrays live here until it returns.
    let mut resources: Vec<Resource> = Vec::with_capacity(items.len());
    for item in items {
        resources.push(Resource {
            base: Base::new(RESOURCE, 1),
            kind: RESOURCE_TEX2D,
            native: item.resource,
            memory: std::ptr::null_mut(),
            view: std::ptr::null_mut(),
            state: item.state,
            width: item.size.0,
            height: item.size.1,
            native_format: item.format,
            mip_levels: 1,
            array_layers: 1,
            gpu_virtual_address: 0,
            flags: 0,
            usage: 0,
            internal_flags: 0,
            reserved: 0,
        });
    }
    let mut tags: Vec<ResourceTag> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        tags.push(ResourceTag {
            base: Base::new(RESOURCE_TAG, 1),
            resource: if item.resource.is_null() {
                std::ptr::null_mut()
            } else {
                &mut resources[index] as *mut Resource
            },
            kind: item.kind.id(),
            lifecycle: LIFECYCLE_VALID_UNTIL_PRESENT,
            extent: Extent {
                top: 0,
                left: 0,
                width: item.size.0,
                height: item.size.1,
            },
        });
    }
    let handle = viewport();
    let result = (core.set_tag_for_frame)(frame, &handle, tags.as_ptr(), tags.len() as u32, list);
    if result != OK {
        if !SAID_TAGS.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[sl ] slSetTagForFrame({} tags) -> {}",
                tags.len(),
                name(result)
            );
        }
        return false;
    }
    TAGS_INDEX.store(FRAME_INDEX.load(Ordering::Relaxed), Ordering::Relaxed);
    // Once, on success, and naming what went over. Failure was the only thing
    // this path reported, so a run in which nothing was tagged at all read
    // exactly like a run in which everything was -- and "I have no evidence
    // either way that depth and mvec are reaching it" is the cost of that.
    // Depth and motion vectors are the two mandatory tags; if either is
    // missing from this line, that is the defect, visible without a guess.
    let mandatory = items.iter().any(|item| item.kind == Kind::Depth);
    if mandatory && !SAID_TAGGED.swap(true, Ordering::Relaxed) {
        let mut names = String::new();
        for item in items {
            if !names.is_empty() {
                names.push_str(", ");
            }
            names.push_str(&format!(
                "{} {}x{} state {:#x} format {}",
                item.kind.label(),
                item.size.0,
                item.size.1,
                item.state,
                item.format
            ));
        }
        crate::log!("[sl ] slSetTagForFrame took {} tags: {names}", items.len());
    }
    true
}

static SAID_TAGGED: AtomicBool = AtomicBool::new(false);

static SAID_TAGS: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// The feature itself
// ---------------------------------------------------------------------------

static OPTIONS_SET: AtomicU32 = AtomicU32::new(u32::MAX);

/// Ask for a mode. Takes effect at the next `Present`.
pub fn set_mode(
    mode: u32,
    generated: u32,
    display: (u32, u32),
    render: (u32, u32),
    recompose: bool,
) -> bool {
    let Some(features) = features() else {
        return false;
    };
    let wanted = mode | (generated << 8) | ((recompose as u32) << 16);
    let changed = OPTIONS_SET.swap(wanted, Ordering::Relaxed) != wanted;
    let options = DlssgOptions {
        base: Base::new(DLSSG_OPTIONS, 5),
        mode,
        num_frames_to_generate: generated.clamp(1, 3),
        // Retaining the resources is what the guide recommends: the
        // alternative gives the memory back on every toggle and takes it again.
        flags: DLSSG_RETAIN_RESOURCES_WHEN_OFF,
        // Dynamic resolution is deliberately off and the fields left zero.
        // This engine's render size is fixed for a session by design -- it is
        // settled at startup through the upscale ratio -- and the guide
        // requires the colour buffer to stay a fixed size, which it does.
        dynamic_res_width: 0,
        dynamic_res_height: 0,
        num_back_buffers: 0,
        mvec_depth_width: render.0,
        mvec_depth_height: render.1,
        colour_width: display.0,
        colour_height: display.1,
        colour_buffer_format: 0,
        mvec_buffer_format: 0,
        depth_buffer_format: 0,
        hudless_buffer_format: 0,
        ui_buffer_format: 0,
        on_error_callback: std::ptr::null_mut(),
        reserved15: INVALID_BOOL,
        queue_parallelism_mode: 0,
        // False by default. Recomposition means the plugin puts the
        // interface back itself, and the guide's section 6.6 asks for
        // *"both Hudless and a UI buffer"* for it.
        //
        // `kBufferTypeUIColorAndAlpha` cannot be supplied in this engine.
        // MHW draws its interface directly into the scene's own target,
        // which is `R11G11B10_FLOAT` -- three channels, no alpha -- and
        // composites it destructively, so neither the alpha nor the colour
        // of a separate UI layer is an image that exists. Producing one
        // would mean redirecting all 11 to 31 interface draws to a target
        // with alpha, which changes what the engine draws rather than only
        // reading what it drew.
        //
        // `kBufferTypeUIAlpha` (69) is a single channel of opacity with no
        // colour, and coverage can be approximated without redirecting a
        // draw. `fg_uir` exists to ask the plugin what it does with that;
        // the `UIR:` field of NVIDIA's overlay is the measurement.
        enable_user_interface_recomposition: if recompose { TRUE } else { FALSE },
        dynamic_target_frame_rate: 0.0,
    };
    let handle = viewport();
    unsafe {
        let result = (features.dlssg_set_options)(&handle, &options);
        if changed || result != OK {
            crate::log!(
                "[sl ] slDLSSGSetOptions(mode {mode}{}, {} generated, {}x{} from {}x{}, \
                 interface recomposition {}) -> {}",
                if mode == MODE_OFF {
                    " = eOff, nothing is generated"
                } else {
                    ""
                },
                options.num_frames_to_generate,
                display.0,
                display.1,
                render.0,
                render.1,
                if recompose {
                    "asked for"
                } else {
                    "not asked for"
                },
                name(result)
            );
        }
        result == OK
    }
}

/// What the plugin says about itself.
/// The last `slDLSSGGetState` sample that succeeded: status in the high half,
/// frames presented in the low, and bit 63 to say there has been one.
static LAST_STATE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Status and frames presented per rendered frame at the last sample, if
/// frame generation has been sampled in this session.
pub fn last_state() -> Option<(u32, u32)> {
    let packed = LAST_STATE.load(Ordering::Relaxed);
    (packed >> 63 == 1).then_some((((packed >> 32) & 0x7fff_ffff) as u32, packed as u32))
}

pub fn report_state() {
    let Some(features) = features() else {
        return;
    };
    let handle = viewport();
    let mut state = DlssgState {
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
    };
    unsafe {
        // No options pointer: passing one together with the VRAM-estimate flag
        // makes the plugin compute an estimate, which the guide calls too
        // expensive to do per frame.
        let result = (features.dlssg_get_state)(&handle, &mut state, std::ptr::null());
        if result == OK {
            LAST_STATE.store(
                1 << 63
                    | ((state.status as u64 & 0x7fff_ffff) << 32)
                    | state.num_frames_actually_presented as u64,
                Ordering::Relaxed,
            );
        }
        let (fresh, missing, repeats) = crate::fg::hudless_counts();
        let (current, constants, tagged_at) = frame_indices();
        // Whether the game's window was the foreground window when this
        // sample was taken, asked of the system at the sample and not assumed.
        //
        // It belongs beside `presented` because DLSS-G stops interpolating
        // when the window loses focus and says so only in its own log --
        // `dlfgPresent.cpp:2544[shouldInterpolate] DLSS-G disabled: window not
        // focused` -- so a sample taken while the user is in another window
        // reads `presented 1` with `status 0x0 (ok)`, which is otherwise the
        // signature of interpolation having switched itself off.
        let focus = crate::swap::chain().map(|chain| {
            use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
            GetForegroundWindow().0 as isize == chain.window
        });
        crate::log!(
            "[sl ] slDLSSGGetState -> {}; status {:#x} ({}), presented {}, max generated {}, \
             vsync support {}{}{}{}",
            name(result),
            state.status,
            status_name(state.status),
            state.num_frames_actually_presented,
            state.num_frames_to_generate_max,
            state.is_vsync_support_available,
            match focus {
                Some(true) => "; the game window is the foreground window",
                Some(false) => {
                    "; the game window is NOT the foreground window at this sample -- DLSS-G \
                     does not interpolate without focus, so presented 1 here is not a fault"
                }
                None => "; no swapchain window is known, so focus was not checked",
            },
            // The three indices that must agree, and the plugin's own
            // complaint names a fourth to compare them against. See
            // `frame_indices`.
            format!(
                "; frame {current}, constants filed under {constants}, tags under \
                 {tagged_at}"
            ),
            // Read together with `presented`. If generation falls off while
            // the missing count climbs, the HUD-less tag is costing frames
            // rather than only quality, and `fg_hudless = 0` is the answer.
            if fresh + missing + repeats > 0 {
                format!(
                    "; HUD-less tagged on {fresh} of {} rendered frames ({missing} without a \
                     capture), and {repeats} Presents carried no rendered frame",
                    fresh + missing
                )
            } else {
                String::new()
            }
        );
        // The camera of *this* frame, beside the state it produced. The line
        // at the first accepted constants is a loading screen's camera.
        if let Some((lens, pose)) = *LAST_CAMERA.lock() {
            crate::log!(
                "[sl ] constants last accepted: {}",
                describe_camera(lens, pose)
            );
        }
    }
}

/// Every reason the plugin gives for refusing, by name.
fn status_name(status: u32) -> String {
    if status == 0 {
        return "ok".into();
    }
    const NAMES: &[(u32, &str)] = &[
        (1 << 0, "resolution too low"),
        (1 << 1, "Reflex not active"),
        (1 << 2, "HDR format not supported"),
        (1 << 3, "common constants invalid"),
        (1 << 4, "GetCurrentBackBufferIndex was never called"),
    ];
    let listed: Vec<&str> = NAMES
        .iter()
        .filter(|(bit, _)| status & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    if listed.is_empty() {
        format!("{status:#x}")
    } else {
        listed.join(", ")
    }
}

// ---------------------------------------------------------------------------
// Reflex, which is mandatory
// ---------------------------------------------------------------------------

static REFLEX_MODE: AtomicU32 = AtomicU32::new(u32::MAX);

/// Set the Reflex mode. Required at least once even when it is off.
pub fn reflex_options(low_latency: bool, boost: bool) -> bool {
    let Some(features) = features() else {
        return false;
    };
    let mode = match (low_latency, boost) {
        (true, true) => REFLEX_LOW_LATENCY_WITH_BOOST,
        (true, false) => REFLEX_LOW_LATENCY,
        _ => REFLEX_OFF,
    };
    if REFLEX_MODE.swap(mode, Ordering::Relaxed) == mode {
        return true;
    }
    let options = ReflexOptions {
        base: Base::new(REFLEX_OPTIONS, 1),
        mode,
        frame_limit_us: 0,
        use_markers_to_optimize: false,
        virtual_key: 0,
        id_thread: 0,
    };
    unsafe {
        let result = (features.reflex_set_options)(&options);
        crate::log!("[sl ] slReflexSetOptions(mode {mode}) -> {}", name(result));
        result == OK
    }
}

/// Let the driver decide when this frame starts.
///
/// *"slReflexSleep must always be called even when Reflex Low Latency mode is
/// Off."* With the mode off it returns immediately, so this is not a switch.
pub fn reflex_sleep() {
    let Some(features) = features() else {
        return;
    };
    let frame = token();
    if frame.is_null() {
        return;
    }
    unsafe {
        let result = (features.reflex_sleep)(frame);
        if result != OK && !SAID_SLEEP.swap(true, Ordering::Relaxed) {
            crate::log!("[sl ] slReflexSleep -> {}", name(result));
        }
    }
}

static SAID_SLEEP: AtomicBool = AtomicBool::new(false);

/// One latency marker.
pub fn marker(which: u32) {
    let Some(features) = features() else {
        return;
    };
    let frame = token();
    if frame.is_null() {
        return;
    }
    unsafe {
        let result = (features.pcl_set_marker)(which, frame);
        if result != OK && !SAID_MARKER.swap(true, Ordering::Relaxed) {
            crate::log!("[sl ] slPCLSetMarker({which}) -> {}", name(result));
        }
    }
}

static SAID_MARKER: AtomicBool = AtomicBool::new(false);

/// Unload the feature, which is the only honest way to say "off".
///
/// Turning the mode off is not the same thing: the guide is explicit that the
/// proxy swapchain makes the host render off-screen whenever the plugin is
/// loaded, `DLSSGMode::eOff` included. That is why `fg = 0` loads nothing at
/// all rather than loading and disabling.
pub fn unload() {
    let Some(core) = core() else {
        return;
    };
    unsafe {
        let result = (core.set_feature_loaded)(FEATURE_DLSS_G, false);
        crate::log!(
            "[sl ] slSetFeatureLoaded(DLSS-G, false) -> {}",
            name(result)
        );
    }
}

/// The mod has stood down: shut Streamline down again if it is initialised
/// and has not been given the engine's device.
///
/// Called by `gate` when it stands down, and by `attempt` when it finds the
/// stand-down was decided while `slInit` was running. Before the device is
/// set nothing of the engine's has been upgraded and no API hook of
/// Streamline's is active, so `slShutdown` is the documented inverse of
/// `slInit` and nothing else refers to it. Once the device has been set the
/// engine may hold Streamline's proxies, and it is left as it is.
///
/// The interposer library itself stays loaded: it is inert without `slInit`,
/// and unloading a library whose threads cannot be accounted for is not.
pub fn stand_down() {
    // Not waited for. The only other holder is `set_device`, and a device
    // being handed over at this moment is a device Streamline has.
    let Some(_lifecycle) = LIFECYCLE.try_lock() else {
        crate::log!(
            "[sl ] the mod is standing down while Streamline is being given the engine's device, so Streamline is left initialised"
        );
        return;
    };
    if DEVICE_SET.load(Ordering::SeqCst) {
        crate::log!(
            "[sl ] the mod is standing down after Streamline was given the engine's device, so              Streamline is left initialised"
        );
        return;
    }
    if !INITIALISED.swap(false, Ordering::SeqCst) {
        return;
    }
    // Taken, so that nothing reaches an entry point of a Streamline that
    // has been shut down.
    let Some(core) = CORE.lock().take() else {
        return;
    };
    let result = unsafe { (core.shutdown)() };
    crate::log!(
        "[sl ] the mod is standing down and Streamline had not been given a device: slShutdown -> {}. sl.interposer.dll stays loaded and uninitialised; no frame generation in this session",
        name(result)
    );
}

/// Shut Streamline down. Only at process exit, and only if it was started.
pub fn shutdown() {
    if !INITIALISED.swap(false, Ordering::Relaxed) {
        return;
    }
    let Some(core) = core() else {
        return;
    };
    unsafe {
        let result = (core.shutdown)();
        crate::log!("[sl ] slShutdown -> {}", name(result));
    }
}

// ---------------------------------------------------------------------------
// Result codes
// ---------------------------------------------------------------------------

fn result_name(result: u32) -> &'static str {
    name(result)
}

/// `sl::Result` by name. Worth having in full: most of these name the mistake
/// outright, and a bare number in a log costs more to look up than it does to
/// print.
pub fn name(result: u32) -> &'static str {
    const NAMES: &[&str] = &[
        "ok",
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

// ---------------------------------------------------------------------------
// Streamline's own log, folded into ours
// ---------------------------------------------------------------------------

/// Tail `sl.log` and copy its complaints into our log.
///
/// Streamline reports asynchronously and in its own file, and a call that
/// returned success can be followed there by the line that says why nothing
/// works -- `D3D or VK API hook is activated without device being created`,
/// for one. The only way for the two logs to be read together is to bring
/// one into the other. This is the fallback for when the message callback
/// delivers nothing.
///
/// Only `[error]`, `[warn]` and `[fatal]` lines, because the file is a
/// thousand lines of plugin bookkeeping and the point is to make the
/// complaints impossible to miss rather than to duplicate the whole thing.
/// The file itself stays where it is and is still worth opening when something
/// is subtle.
fn fold_log(folder: std::path::PathBuf, name: &'static str, filtered: bool) {
    std::thread::spawn(move || {
        let path = folder.join(name);
        // Streamline truncates and reopens the file at init, so start from
        // nothing rather than from the previous session's length.
        let mut read: u64 = 0;
        let mut announced = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let length = std::fs::metadata(&path).map(|data| data.len()).unwrap_or(0);
            if length == 0 {
                if std::time::Instant::now() > deadline && !announced {
                    crate::log!(
                        "[sl ] {} never appeared; its complaints will not reach \
                                 our log",
                        path.display()
                    );
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
                continue;
            }
            if !announced {
                announced = true;
                crate::log!(
                    "[sl ] folding {} from {}",
                    if filtered {
                        "the errors and warnings"
                    } else {
                        "every line"
                    },
                    path.display()
                );
            }
            // Truncated, which means a new session: start again.
            if length < read {
                read = 0;
            }
            if length > read {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    // Re-read in full and skip what has been seen. The file is
                    // small and this runs four times a second, which is
                    // cheaper than keeping a file handle open on something
                    // Streamline wants to truncate.
                    let fresh = text.get(read as usize..).unwrap_or("");
                    for line in fresh.lines() {
                        // Complaints always. And, when the configuration has
                        // asked for verbose logging, the lines that carry NGX's
                        // own text: `sl.common` pipes NGX messages through
                        // `commonEntry.cpp[ngxLog]`, which is the channel by
                        // which NGX's messages for Streamline's context reach
                        // this process.
                        let complaint = line.contains("[error]")
                            || line.contains("[warn]")
                            || line.contains("[fatal]");
                        let ngx = crate::config::get().log_verbose
                            && (line.contains("ngxLog")
                                || line.contains("dlss_g")
                                || line.contains("DLSSG")
                                || line.contains("FrameGeneration")
                                || line.contains("DRS"));
                        if filtered && !complaint && !ngx {
                            continue;
                        }
                        // The useful half is after the file and function, and
                        // the timestamps are Streamline's rather than ours.
                        let trimmed = line
                            .rsplit_once(']')
                            .map(|(_, tail)| tail.trim())
                            .filter(|tail| !tail.is_empty())
                            .unwrap_or(line);
                        let level = if line.contains("[error]") || line.contains("[fatal]") {
                            "ERROR"
                        } else if line.contains("[warn]") {
                            "warn"
                        } else {
                            "ngx "
                        };
                        forward(level, trimmed);
                    }
                    read = text.len() as u64;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one thing about a hand-reproduced C++ ABI that can be checked
    /// without the library present.
    ///
    /// `sl::BaseStructure` is 32 bytes -- an 8-byte pointer, a 16-byte GUID and
    /// an 8-byte size_t -- and every structure begins with it. A change in
    /// Rust's layout rules, or a typo in a field, moves everything after it and
    /// Streamline would read a different structure than the one we wrote.
    #[test]
    fn the_structure_header_is_where_streamline_expects_it() {
        assert_eq!(name(ERROR_EXCEPTION_HANDLER), "eErrorExceptionHandler");
        assert_eq!(std::mem::size_of::<StructType>(), 16);
        assert_eq!(std::mem::size_of::<Base>(), 32);
        assert_eq!(std::mem::offset_of!(Base, next), 0);
        assert_eq!(std::mem::offset_of!(Base, kind), 8);
        assert_eq!(std::mem::offset_of!(Base, version), 24);

        // And the first member of each structure lands immediately after it.
        assert_eq!(std::mem::offset_of!(Preferences, show_console), 32);
        assert_eq!(std::mem::offset_of!(Constants, camera_view_to_clip), 32);
        assert_eq!(std::mem::offset_of!(Resource, kind), 32);
        assert_eq!(std::mem::offset_of!(ResourceTag, resource), 32);
        assert_eq!(std::mem::offset_of!(DlssgOptions, mode), 32);
        assert_eq!(
            std::mem::offset_of!(DlssgState, estimated_vram_usage_in_bytes),
            32
        );
        assert_eq!(std::mem::offset_of!(ReflexOptions, mode), 32);

        // The five 4x4 matrices are contiguous, which is what makes the rest
        // of Constants line up at all.
        assert_eq!(std::mem::offset_of!(Constants, jitter_offset), 32 + 5 * 64);
        // Extent is top, left, width, height -- in that order, which is not
        // the order anyone would guess.
        assert_eq!(std::mem::offset_of!(Extent, top), 0);
        assert_eq!(std::mem::offset_of!(Extent, left), 4);
    }
}
