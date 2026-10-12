//! Loading and initialising Streamline, and every call into it.
//!
//! `slInit` runs on a thread of its own, started at load
//! ([`start`]): it loads six libraries and a plugin manager, and doing that
//! inside a call the engine is waiting on delayed the engine's swapchain
//! creation past the moment the interception had to be in place (black
//! screen with the interface over it). The guide agrees: with manual
//! hooking, `slInit` comes before any hooked API is invoked and needs no
//! device. Not from `DllMain`: `LoadLibrary` under the loader lock
//! deadlocks.
//!
//! The device (`slSetD3DDevice`) and the feature functions come later, at
//! the engine's swapchain creation (`swap`), and in that order:
//! upgrading any interface activates Streamline's hooks and initialises the
//! plugins, which refuses without a device.

use std::ffi::{c_char, c_void, CStr};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use windows::core::{PCSTR, PCWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

use crate::abi::{self, SlResult};
use crate::setup::{self, Identity, RepeatFilter, SlLogLevel};
use crate::status::InitState;

const TARGET: &str = "sl";

/// How Streamline is started. Policy, decided by the mod.
#[derive(Clone, Debug)]
pub struct StartConfig {
    /// The mod folder (absolute): the plugins are in it or in its `sl`
    /// subfolder, beside `nvngx_dlss.dll`.
    pub mod_folder: std::path::PathBuf,
    /// Who we are to NGX.
    pub identity: Identity,
    /// Let Streamline and NGX fetch and load updated plugins and models
    /// over the air (Streamline's own default).
    pub allow_ota: bool,
    /// Require a valid Authenticode signature by NVIDIA Corporation on
    /// `sl.interposer.dll` before loading it (the guide asks production
    /// integrations to check; a self-built Streamline needs `false`).
    pub verify_signature: bool,
    /// Streamline's log level (its lines arrive under target `sl`).
    pub log_level: SlLogLevel,
}

#[derive(Clone, Copy)]
struct Core {
    set_d3d_device: abi::SetD3dDeviceFn,
    upgrade_interface: abi::UpgradeInterfaceFn,
    get_new_frame_token: abi::GetNewFrameTokenFn,
    set_constants: abi::SetConstantsFn,
    set_tag_for_frame: abi::SetTagForFrameFn,
    get_feature_function: abi::GetFeatureFunctionFn,
    set_feature_loaded: abi::SetFeatureLoadedFn,
    shutdown: abi::ShutdownFn,
}

#[derive(Clone, Copy)]
struct Features {
    dlssg_set_options: abi::DlssgSetOptionsFn,
    dlssg_get_state: abi::DlssgGetStateFn,
    reflex_set_options: abi::ReflexSetOptionsFn,
    reflex_sleep: abi::ReflexSleepFn,
    pcl_set_marker: abi::PclSetMarkerFn,
}

static CORE: Mutex<Option<Core>> = Mutex::new(None);
static FEATURES: Mutex<Option<Features>> = Mutex::new(None);
static STATE: Mutex<InitState> = Mutex::new(InitState::NotStarted);
static SETTLED: Condvar = Condvar::new();
static DEVICE_SET: AtomicBool = AtomicBool::new(false);
static FAULTED: AtomicBool = AtomicBool::new(false);
static STOOD_DOWN: AtomicBool = AtomicBool::new(false);
/// Held across handing over the device and shutting down, which must not
/// cross.
static LIFECYCLE: Mutex<()> = Mutex::new(());
static LOG_LINES: AtomicU64 = AtomicU64::new(0);
static LOG_FILTER: Mutex<Option<RepeatFilter>> = Mutex::new(None);

/// The one viewport everything is filed under.
pub(crate) const VIEWPORT: abi::ViewportHandle = abi::ViewportHandle::new(1);

/// Begin initialising Streamline on a thread named `mhw-sl-init`. Call at
/// load, from the boot thread, only when frame generation is enabled.
/// `false` if it was started before.
pub fn start(config: StartConfig) -> bool {
    {
        let mut state = STATE.lock();
        if *state != InitState::NotStarted {
            return false;
        }
        *state = InitState::Running;
    }
    let spawned = mhw_win::spawn("sl-init", move || settle(initialise(&config)));
    match spawned {
        Ok(_) => true,
        Err(error) => {
            settle(Err(format!("the slInit thread did not start: {error}")));
            false
        }
    }
}

fn settle(outcome: Result<(), String>) {
    let state = match outcome {
        Ok(()) => InitState::Ready,
        Err(reason) => {
            log::warn!(target: TARGET, "no frame generation this session: {reason}");
            InitState::Failed(reason)
        }
    };
    *STATE.lock() = state;
    SETTLED.notify_all();
}

/// Wait until initialisation has finished, either way, at most `timeout`.
/// `true` once it has (or when it was never started).
pub fn wait_settled(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut state = STATE.lock();
    while *state == InitState::Running {
        if SETTLED.wait_until(&mut state, deadline).timed_out() {
            return *state != InitState::Running;
        }
    }
    true
}

/// How far initialisation got.
pub fn init_state() -> InitState {
    STATE.lock().clone()
}

/// Whether `slInit` succeeded and Streamline was not shut down.
pub fn ready() -> bool {
    *STATE.lock() == InitState::Ready
}

/// Whether `slInit` ended in an exception. Streamline unwinds its thread
/// from wherever that happened, possibly holding a driver or NGX lock; a
/// later NGX call has been seen not to return. The mod should give the
/// game's own NGX initialisation a time limit in such a session.
pub fn faulted() -> bool {
    FAULTED.load(Ordering::SeqCst)
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// # Safety
/// `T` must be the function-pointer type of the export `name`.
unsafe fn resolve<T: Copy>(module: HMODULE, name: &CStr) -> Option<T> {
    const { assert!(std::mem::size_of::<T>() == std::mem::size_of::<usize>()) };
    // SAFETY: `module` is a loaded library and `name` NUL-terminated.
    let entry = unsafe { GetProcAddress(module, PCSTR(name.as_ptr().cast())) }?;
    // SAFETY: the caller names the export's type; both are pointer-sized.
    Some(unsafe { std::mem::transmute_copy(&entry) })
}

fn initialise(config: &StartConfig) -> Result<(), String> {
    let plugins =
        setup::locate_plugins(&config.mod_folder, Path::exists).map_err(|r| r.to_string())?;
    if !plugins.beside_ngx {
        log::warn!(
            target: TARGET,
            "the plugins are in {} rather than beside nvngx_dlss.dll; Streamline's NGX client \
             then has another application data path than the game's. If the game's NGX \
             initialisation fails, move them up one folder",
            plugins.folder.display()
        );
    }
    let interposer = plugins.interposer();
    if config.verify_signature {
        let signer = crate::trust::verify(&interposer)?;
        log::info!(target: TARGET, "{} is validly signed by {signer}", interposer.display());
    } else {
        log::warn!(target: TARGET, "{} is loaded without a signature check", interposer.display());
    }
    if STOOD_DOWN.load(Ordering::SeqCst) {
        return Err("the mod stood down before Streamline was loaded".into());
    }
    let path = wide(&interposer);
    // SAFETY: an absolute, NUL-terminated path (Streamline requires absolute
    // paths); not under the loader lock, this is a thread of our own.
    let module = unsafe { LoadLibraryW(PCWSTR(path.as_ptr())) }
        .map_err(|e| format!("{} would not load: {e}", interposer.display()))?;
    // SAFETY: each name is resolved to the type of its declaration in
    // sl_core_api.h (2.14.1).
    let (core, init) = unsafe {
        let core = PartialCore {
            set_d3d_device: resolve(module, c"slSetD3DDevice"),
            upgrade_interface: resolve(module, c"slUpgradeInterface"),
            get_new_frame_token: resolve(module, c"slGetNewFrameToken"),
            set_constants: resolve(module, c"slSetConstants"),
            set_tag_for_frame: resolve(module, c"slSetTagForFrame"),
            get_feature_function: resolve(module, c"slGetFeatureFunction"),
            set_feature_loaded: resolve(module, c"slSetFeatureLoaded"),
            shutdown: resolve(module, c"slShutdown"),
        };
        (core, resolve::<abi::InitFn>(module, c"slInit"))
    };
    let (Some(core), Some(init)) = (core.complete(), init) else {
        return Err("sl.interposer.dll lacks an entry point this needs".into());
    };

    let plugin_path = wide(&plugins.folder);
    let paths = [plugin_path.as_ptr()];
    let features = [abi::FEATURE_DLSS_G, abi::FEATURE_REFLEX, abi::FEATURE_PCL];
    let identity = config.identity.fields();
    let flags = setup::preference_flags(config.allow_ota);
    let on_log: abi::LogMessageFn = on_log;
    let preferences = abi::Preferences {
        base: abi::Base::new(abi::PREFERENCES, 1),
        show_console: false,
        log_level: config.log_level.to_abi(),
        paths_to_plugins: paths.as_ptr(),
        num_paths_to_plugins: 1,
        path_to_logs_and_data: plugin_path.as_ptr(),
        allocate_callback: std::ptr::null_mut(),
        release_callback: std::ptr::null_mut(),
        log_message_callback: on_log as *mut c_void,
        flags,
        features_to_load: features.as_ptr(),
        num_features_to_load: features.len() as u32,
        application_id: identity.application_id,
        engine: identity.engine,
        engine_version: identity
            .engine_version
            .as_ref()
            .map_or(std::ptr::null(), |s| s.as_ptr()),
        project_id: identity
            .project_id
            .as_ref()
            .map_or(std::ptr::null(), |s| s.as_ptr()),
        render_api: abi::RENDER_API_D3D12,
    };
    if identity.fell_back {
        log::warn!(
            target: TARGET,
            "the project identity needs both engine version and project id; the \
             application-id route is used instead"
        );
    }
    log::info!(
        target: TARGET,
        "slInit identity: NGX via {}, applicationId {}, engine {}, OTA {}, flags {flags:#x}",
        identity.route,
        identity.application_id,
        identity.engine,
        if config.allow_ota { "allowed" } else { "off" }
    );
    if STOOD_DOWN.load(Ordering::SeqCst) {
        return Err("the mod stood down before slInit".into());
    }
    // SAFETY: every pointer in `preferences` outlives the call (Streamline
    // copies the strings before returning).
    let result = unsafe { init(&preferences, abi::SDK_VERSION) };
    log::info!(
        target: TARGET,
        "slInit -> {} ({} log lines delivered)",
        abi::result_name(result),
        LOG_LINES.load(Ordering::Relaxed)
    );
    if result == abi::ERROR_EXCEPTION_HANDLER {
        FAULTED.store(true, Ordering::SeqCst);
    }
    if result != abi::OK {
        return Err(format!("slInit returned {}", abi::result_name(result)));
    }
    *CORE.lock() = Some(core);
    // The stand-down may have come while slInit ran; one of the two always
    // sees the other (the flag is set before `stand_down` reads the core).
    if STOOD_DOWN.load(Ordering::SeqCst) {
        shut_down_now("the mod stood down while slInit ran");
        return Err("the mod stood down while slInit ran".into());
    }
    log::info!(target: TARGET, "Streamline loaded from {}", plugins.folder.display());
    Ok(())
}

/// Every resolved entry point, before they are known to be all there.
struct PartialCore {
    set_d3d_device: Option<abi::SetD3dDeviceFn>,
    upgrade_interface: Option<abi::UpgradeInterfaceFn>,
    get_new_frame_token: Option<abi::GetNewFrameTokenFn>,
    set_constants: Option<abi::SetConstantsFn>,
    set_tag_for_frame: Option<abi::SetTagForFrameFn>,
    get_feature_function: Option<abi::GetFeatureFunctionFn>,
    set_feature_loaded: Option<abi::SetFeatureLoadedFn>,
    shutdown: Option<abi::ShutdownFn>,
}

impl PartialCore {
    fn complete(self) -> Option<Core> {
        Some(Core {
            set_d3d_device: self.set_d3d_device?,
            upgrade_interface: self.upgrade_interface?,
            get_new_frame_token: self.get_new_frame_token?,
            set_constants: self.set_constants?,
            set_tag_for_frame: self.set_tag_for_frame?,
            get_feature_function: self.get_feature_function?,
            set_feature_loaded: self.set_feature_loaded?,
            shutdown: self.shutdown?,
        })
    }
}

/// `PFun_LogMessageCallback`. Called from any Streamline thread; nothing in
/// it can unwind (an `extern "C"` panic aborts).
unsafe extern "C" fn on_log(kind: u32, message: *const c_char) {
    if message.is_null() {
        return;
    }
    // SAFETY: Streamline passes a NUL-terminated string valid for the call.
    let text = unsafe { CStr::from_ptr(message) }.to_string_lossy();
    for line in text.lines().map(str::trim_end).filter(|l| !l.is_empty()) {
        LOG_LINES.fetch_add(1, Ordering::Relaxed);
        let admitted = LOG_FILTER
            .lock()
            .get_or_insert_with(RepeatFilter::new)
            .admit(line);
        let Some(count) = admitted else {
            continue;
        };
        let line = line.trim_start();
        match kind {
            abi::log_type::INFO => log::debug!(target: TARGET, count = count; "{line}"),
            abi::log_type::WARN => log::warn!(target: TARGET, count = count; "{line}"),
            _ => log::error!(target: TARGET, count = count; "{line}"),
        }
    }
}

fn core() -> Option<Core> {
    *CORE.lock()
}

fn features() -> Option<Features> {
    *FEATURES.lock()
}

/// Hand Streamline the engine's device, once. Starts Streamline's feature
/// initialisation (and through `sl.dlss_g` a second NGX client; the game
/// initialises its own later, on the same thread, so the two are
/// sequential).
///
/// # Safety
/// `device` must be a live `ID3D12Device` that stays alive for the process.
pub(crate) unsafe fn set_device(device: *mut c_void) -> bool {
    let _lifecycle = LIFECYCLE.lock();
    if DEVICE_SET.load(Ordering::SeqCst) {
        return true;
    }
    let Some(core) = core() else {
        return false;
    };
    // SAFETY: the caller guarantees a live device.
    let result = unsafe { (core.set_d3d_device)(device) };
    log::info!(target: TARGET, "slSetD3DDevice({device:p}) -> {}", abi::result_name(result));
    if result == abi::ERROR_EXCEPTION_HANDLER {
        FAULTED.store(true, Ordering::SeqCst);
    }
    let ok = result == abi::OK;
    DEVICE_SET.store(ok, Ordering::SeqCst);
    ok
}

/// Whether `slSetD3DDevice` succeeded.
pub(crate) fn device_set() -> bool {
    DEVICE_SET.load(Ordering::SeqCst)
}

/// `slSetFeatureLoaded(DLSS_G, true)`, before the swapchain exists.
pub(crate) fn load_dlssg() -> bool {
    let Some(core) = core() else {
        return false;
    };
    // SAFETY: a resolved entry point with plain arguments.
    let result = unsafe { (core.set_feature_loaded)(abi::FEATURE_DLSS_G, true) };
    log::info!(target: TARGET, "slSetFeatureLoaded(DLSS-G) -> {}", abi::result_name(result));
    result == abi::OK
}

/// Resolve the DLSS-G, Reflex and PCL functions (needs the device).
pub(crate) fn bind_features() -> bool {
    if features().is_some() {
        return true;
    }
    let Some(core) = core() else {
        return false;
    };
    if !device_set() {
        return false;
    }
    let get = |feature: abi::Feature, name: &CStr| -> Option<usize> {
        let mut address: *mut c_void = std::ptr::null_mut();
        // SAFETY: `name` is NUL-terminated and `address` a valid out slot.
        let result =
            unsafe { (core.get_feature_function)(feature, name.as_ptr().cast(), &mut address) };
        if result != abi::OK || address.is_null() {
            log::warn!(
                target: TARGET,
                "{} did not resolve: {}",
                name.to_string_lossy(),
                abi::result_name(result)
            );
            return None;
        }
        Some(address as usize)
    };
    let bound = (|| {
        let dlssg_set_options = get(abi::FEATURE_DLSS_G, c"slDLSSGSetOptions")?;
        let dlssg_get_state = get(abi::FEATURE_DLSS_G, c"slDLSSGGetState")?;
        let reflex_set_options = get(abi::FEATURE_REFLEX, c"slReflexSetOptions")?;
        let reflex_sleep = get(abi::FEATURE_REFLEX, c"slReflexSleep")?;
        let pcl_set_marker = get(abi::FEATURE_PCL, c"slPCLSetMarker")?;
        // SAFETY: each address is the named feature function, whose type is
        // its declaration in sl_dlss_g.h / sl_reflex.h / sl_pcl.h (2.14.1).
        unsafe {
            Some(Features {
                dlssg_set_options: std::mem::transmute::<usize, abi::DlssgSetOptionsFn>(
                    dlssg_set_options,
                ),
                dlssg_get_state: std::mem::transmute::<usize, abi::DlssgGetStateFn>(
                    dlssg_get_state,
                ),
                reflex_set_options: std::mem::transmute::<usize, abi::ReflexSetOptionsFn>(
                    reflex_set_options,
                ),
                reflex_sleep: std::mem::transmute::<usize, abi::ReflexSleepFn>(reflex_sleep),
                pcl_set_marker: std::mem::transmute::<usize, abi::PclSetMarkerFn>(pcl_set_marker),
            })
        }
    })();
    let ok = bound.is_some();
    *FEATURES.lock() = bound;
    log::info!(
        target: TARGET,
        "feature functions {}",
        if ok { "bound" } else { "missing: no frame generation" }
    );
    ok
}

/// Whether the feature functions are bound.
pub(crate) fn features_bound() -> bool {
    features().is_some()
}

/// `slUpgradeInterface`: replaces `*slot` with Streamline's proxy in place.
///
/// # Safety
/// `slot` must hold a live interface pointer the caller owns a reference to.
pub(crate) unsafe fn upgrade(slot: *mut *mut c_void) -> SlResult {
    match core() {
        // SAFETY: as the caller guarantees.
        Some(core) => unsafe { (core.upgrade_interface)(slot) },
        None => abi::ERROR_NOT_INITIALIZED,
    }
}

/// `slGetNewFrameToken` with our own index.
pub(crate) fn new_token(index: u32) -> Result<usize, SlResult> {
    let Some(core) = core() else {
        return Err(abi::ERROR_NOT_INITIALIZED);
    };
    let mut token: *mut c_void = std::ptr::null_mut();
    // SAFETY: valid out slot and index for the duration of the call.
    let result = unsafe { (core.get_new_frame_token)(&mut token, &index) };
    match (result, token.is_null()) {
        (abi::OK, false) => Ok(token as usize),
        (abi::OK, true) => Err(abi::ERROR_INVALID_STATE),
        (error, _) => Err(error),
    }
}

/// `slSetConstants` for `token`.
pub(crate) fn set_constants(constants: &abi::Constants, token: usize) -> SlResult {
    match core() {
        // SAFETY: `token` came from `new_token`; the structures live
        // through the call.
        Some(core) => unsafe { (core.set_constants)(constants, token as *const c_void, &VIEWPORT) },
        None => abi::ERROR_NOT_INITIALIZED,
    }
}

/// `slSetTagForFrame` for `token`.
///
/// # Safety
/// Every `ResourceTag::resource` must point at a live `abi::Resource`
/// describing a live resource in the stated state; `list` is null or an
/// open command list.
pub(crate) unsafe fn set_tags(
    token: usize,
    tags: &[abi::ResourceTag],
    list: *mut c_void,
) -> SlResult {
    match core() {
        // SAFETY: as the caller guarantees; the slice lives through the call.
        Some(core) => unsafe {
            (core.set_tag_for_frame)(
                token as *const c_void,
                &VIEWPORT,
                tags.as_ptr(),
                tags.len() as u32,
                list,
            )
        },
        None => abi::ERROR_NOT_INITIALIZED,
    }
}

/// `slDLSSGSetOptions`. Present thread only.
pub(crate) fn dlssg_set_options(options: &abi::DlssgOptions) -> SlResult {
    match features() {
        // SAFETY: a bound feature function; the structures live through it.
        Some(f) => unsafe { (f.dlssg_set_options)(&VIEWPORT, options) },
        None => abi::ERROR_FEATURE_MISSING,
    }
}

/// `slDLSSGGetState` without options (no VRAM estimate). Present thread
/// only ("must be synchronized with the present thread").
pub(crate) fn dlssg_get_state() -> Result<abi::DlssgState, SlResult> {
    let Some(f) = features() else {
        return Err(abi::ERROR_FEATURE_MISSING);
    };
    let mut state = abi::DlssgState::new();
    // SAFETY: a bound feature function writing into `state`.
    let result = unsafe { (f.dlssg_get_state)(&VIEWPORT, &mut state, std::ptr::null()) };
    if result == abi::OK {
        Ok(state)
    } else {
        Err(result)
    }
}

/// `slReflexSetOptions`.
pub(crate) fn reflex_set_options(options: &abi::ReflexOptions) -> SlResult {
    match features() {
        // SAFETY: a bound feature function; the structure lives through it.
        Some(f) => unsafe { (f.reflex_set_options)(options) },
        None => abi::ERROR_FEATURE_MISSING,
    }
}

/// `slReflexSleep` for `token`: blocks until the driver says the frame
/// should start.
pub(crate) fn reflex_sleep(token: usize) -> SlResult {
    match features() {
        // SAFETY: `token` came from `new_token`.
        Some(f) => unsafe { (f.reflex_sleep)(token as *const c_void) },
        None => abi::ERROR_FEATURE_MISSING,
    }
}

/// `slPCLSetMarker` for `token`.
pub(crate) fn marker(marker: abi::Marker, token: usize) -> SlResult {
    match features() {
        // SAFETY: `token` came from `new_token`.
        Some(f) => unsafe { (f.pcl_set_marker)(marker as u32, token as *const c_void) },
        None => abi::ERROR_FEATURE_MISSING,
    }
}

fn shut_down_now(why: &str) {
    *FEATURES.lock() = None;
    if let Some(core) = CORE.lock().take() {
        // SAFETY: the documented inverse of slInit; nothing of the engine's
        // holds a proxy (no device was given).
        let result = unsafe { (core.shutdown)() };
        log::info!(target: TARGET, "{why}: slShutdown -> {}", abi::result_name(result));
    }
    *STATE.lock() = InitState::ShutDown;
}

/// The mod stands down (unknown game build, not the game): shut Streamline
/// down if it has not been given the engine's device. Once it has, the
/// engine may hold its proxies and it is left as it is. The interposer stays
/// loaded (inert without `slInit`).
pub fn stand_down() {
    STOOD_DOWN.store(true, Ordering::SeqCst);
    let Some(_lifecycle) = LIFECYCLE.try_lock() else {
        log::info!(target: TARGET, "standing down while the device is being handed over: left initialised");
        return;
    };
    if DEVICE_SET.load(Ordering::SeqCst) {
        log::info!(target: TARGET, "standing down after the device was given: left initialised");
        return;
    }
    if CORE.lock().is_some() {
        shut_down_now("the mod stands down");
    }
}

/// `slShutdown` at process exit, if Streamline is up.
pub fn shutdown() {
    if CORE.lock().is_some() {
        shut_down_now("process exit");
    }
}
