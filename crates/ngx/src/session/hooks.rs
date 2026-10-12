//! The replacements the game calls instead of its NGX table entries.
//!
//! Each runs on the game's thread, avoids panicking operations (release
//! builds abort on panic) and holds no lock across a call into NGX.

use std::ffi::c_void;
use std::sync::atomic::Ordering;

use super::core::{self, InitStorage};
use super::{
    Entry, HostInfo, HostUse, InitRecord, CALLS, ORIGINALS, POLICY, READY, SPOOF, SPOOFED,
    STAND_IN_CALLS, STATE,
};
use crate::dlss::is_stand_in;
use crate::host::HostKind;
use crate::init::{self, availability_writes, GameInit, Identity, InitAction, Route, SNIPPET};
use crate::param::{names, Parameters};
use crate::status::Status;
use crate::version;
use crate::{Hex, TARGET_NGX as TARGET};

/// The address of the replacement for `entry`.
pub(super) fn replacement(entry: Entry) -> usize {
    let address: *const () = match entry {
        Entry::Init => game_init as *const (),
        Entry::Shutdown => game_shutdown as *const (),
        Entry::GetScratchBufferSize => game_scratch_buffer_size as *const (),
        Entry::GetParameters => game_get_parameters as *const (),
        Entry::CreateFeature => game_create_feature as *const (),
        Entry::EvaluateFeature => game_evaluate_feature as *const (),
        Entry::ReleaseFeature => game_release_feature as *const (),
    };
    address as usize
}

/// Count a call and return the original (0 when unknown).
fn enter(entry: Entry) -> usize {
    CALLS[entry as usize].fetch_add(1, Ordering::Relaxed);
    ORIGINALS[entry as usize].load(Ordering::Acquire)
}

/// Whether the host serving NGX must not be called.
fn host_refused() -> bool {
    STATE
        .lock()
        .host
        .as_ref()
        .is_some_and(|host| host.usable != HostUse::Usable)
}

/// A wide string at `address` in the game's memory, for the log.
fn wide_text(address: usize) -> Option<String> {
    const MAX_BYTES: usize = 520;
    let mut bytes = vec![0u8; mhw_win::mem::readable_len(address, MAX_BYTES) & !1];
    // SAFETY: the range was just found readable; the game's string is
    // static data that nothing frees during the call.
    if bytes.is_empty() || !unsafe { mhw_win::mem::read_bytes(address, &mut bytes) } {
        return None;
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|&unit| unit != 0)
        .collect();
    (units.len() * 2 < bytes.len()).then(|| String::from_utf16_lossy(&units))
}

/// Record the outcome of the game's `Init` and answer it.
fn answer(
    action: InitAction,
    route: Option<Route>,
    status: Status,
    refusal: Option<String>,
) -> u32 {
    let mut state = STATE.lock();
    state.init = Some(InitRecord {
        action,
        route,
        status,
    });
    if let Some(why) = refusal {
        log::error!(target: TARGET, "DLSS is not available: {why}");
        state.refusal = Some(why);
    }
    status.0
}

/// Stand in for the game's `NVSDK_NGX_D3D12_Init`.
///
/// # Safety
/// Only called by the game in place of the `Init` it resolved, whose
/// original [`super::set_original`] recorded; the arguments are its own.
unsafe extern "system" fn game_init(
    a0: u64,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
) -> u32 {
    let original = enter(Entry::Init);
    let game = GameInit {
        app_id: a0,
        data_path: a1,
        device: a2,
        version: a3,
        stack: [a4, a5],
    };
    log::info!(target: TARGET,
        app_id:% = Hex(a0 as usize),
        data_path:? = wide_text(a1),
        device:% = Hex(a2),
        version = a3,
        stack4:% = Hex(a4),
        stack5:% = Hex(a5);
        "the game initialises NGX");
    let host = {
        let mut state = STATE.lock();
        state.game = Some(game);
        state.host.clone()
    };
    if original == 0 {
        return Status::FAIL.0;
    }
    let policy = *POLICY.lock();
    let action = policy.map_or(InitAction::PassThrough, |decide| decide(&game));

    match action {
        InitAction::Refuse => answer(
            InitAction::Refuse,
            None,
            Status::FEATURE_NOT_SUPPORTED,
            Some("the mod refused the game's NGX initialisation".into()),
        ),
        InitAction::PassThrough => pass_through(original, game, host.as_ref()),
        InitAction::Redirect(options) => {
            let Some(host) = host else {
                return answer(
                    InitAction::Redirect(options),
                    None,
                    Status::FAIL,
                    Some("no host was recorded for the game's Init".into()),
                );
            };
            // SAFETY: forwarded: the game's own Init, its device.
            unsafe { redirect(original, game, &host, options) }
        }
    }
}

/// Forward the game's call unchanged — except into a foreign host, whose
/// `Init` takes a pointer where the game passes the version.
fn pass_through(original: usize, game: GameInit, host: Option<&HostInfo>) -> u32 {
    if host.is_some_and(|host| host.kind == HostKind::Foreign) {
        return answer(
            InitAction::PassThrough,
            None,
            Status::FEATURE_NOT_SUPPORTED,
            Some(
                "the mod stands down, and the Init the game resolved from a foreign NGX host \
                 does not take the game's arguments"
                    .into(),
            ),
        );
    }
    // SAFETY: `original` is the game's resolved Init and these are the six
    // words the game passed.
    let status = Status(unsafe {
        std::mem::transmute::<
            usize,
            unsafe extern "system" fn(u64, usize, usize, usize, usize, usize) -> u32,
        >(original)(
            game.app_id,
            game.data_path,
            game.device,
            game.version,
            game.stack[0],
            game.stack[1],
        )
    });
    log::info!(target: TARGET, status:%; "passed the game's Init through");
    answer(InitAction::PassThrough, None, status, None)
}

/// Re-issue the game's `Init` with `options`.
///
/// # Safety
/// `original` is the game's resolved `Init`; `game.device` is its device.
unsafe fn redirect(
    original: usize,
    game: GameInit,
    host: &HostInfo,
    options: init::InitOptions,
) -> u32 {
    let action = InitAction::Redirect(options.clone());
    let fail = |status: Status, route: Option<Route>, why: String| {
        answer(action.clone(), route, status, Some(why))
    };
    if host.usable != HostUse::Usable {
        return fail(
            Status::FEATURE_NOT_SUPPORTED,
            None,
            format!(
                "{} serves NGX and cannot be used ({:?})",
                host.who(),
                host.usable
            ),
        );
    }
    let project = match &options.identity {
        Identity::ProjectId {
            project_id,
            engine_version,
        } => {
            if project_id.is_empty() || engine_version.is_empty() {
                return fail(
                    Status::INVALID_PARAMETER,
                    None,
                    "a project id initialisation needs both the project id and the engine \
                     version"
                        .into(),
                );
            }
            Some((project_id.as_str(), engine_version.as_str()))
        }
        Identity::GameAppId | Identity::AppId(_) => None,
    };
    let plan = init::plan(host.kind, &options.identity, options.tail, &game);
    let entry = match plan.route.export() {
        None => Some(original),
        Some(name) => core::export(host.module, name).or_else(|| {
            // The driver's Init_ProjectID may live in `_nvngx.dll` only.
            (host.kind == HostKind::Driver)
                .then(|| core::loaded("_nvngx.dll"))
                .flatten()
                .and_then(|module| core::export(module, name))
        }),
    };
    let Some(entry) = entry else {
        return fail(
            Status::FEATURE_NOT_SUPPORTED,
            Some(plan.route),
            format!("{} does not export {:?}", host.who(), plan.route.export()),
        );
    };
    let Some(storage) = InitStorage::new(&options.data_path, project) else {
        return fail(
            Status::INVALID_PARAMETER,
            Some(plan.route),
            "the project id or engine version holds a NUL".into(),
        );
    };
    let call = storage.call(plan, entry, game.device, options.api_version);
    {
        // Owned by the session from before the call until NGX shuts down.
        let mut state = STATE.lock();
        if !state.storage_pinned {
            state.storage = Some(storage);
        } else {
            return fail(
                Status::FEATURE_NOT_SUPPORTED,
                Some(plan.route),
                "an earlier NGX initialisation never returned".into(),
            );
        }
    }
    let snippet = options.data_path.join(SNIPPET);
    let snippet_version = version::read_file_version(&snippet)
        .ok()
        .flatten()
        .map(|v| v.file.to_string());
    log::info!(target: TARGET,
        route:? = plan.route,
        app_id:% = Hex(plan.app_id as usize),
        identity:? = options.identity,
        version = options.api_version,
        data_path:? = options.data_path,
        snippet:? = snippet_version;
        "initialising NGX in the game's place");

    // SAFETY: the entry point matches the route, the device is the game's,
    // the storage is owned by the session, and this is the game's own Init
    // moment (no concurrent initialisation).
    let Some(status) = (unsafe { call.run(options.timeout) }) else {
        STATE.lock().storage_pinned = true;
        if host.kind == HostKind::Foreign {
            super::give_up_on_foreign_host("its Init did not return in time");
        }
        return fail(
            Status::FEATURE_NOT_SUPPORTED,
            Some(plan.route),
            format!(
                "NGX's Init did not return within {:?}; the game carries on without NGX",
                options.timeout
            ),
        );
    };
    log::info!(target: TARGET, status:%; "NGX Init returned");
    if !status.is_success() {
        if host.kind == HostKind::Foreign {
            super::give_up_on_foreign_host(&format!("its Init returned {status}"));
        }
        return fail(
            status,
            Some(plan.route),
            format!(
                "{} did not initialise with API version {}: {status}",
                host.who(),
                options.api_version
            ),
        );
    }
    match after_init(host) {
        Ok(()) => {
            READY.store(true, Ordering::Relaxed);
            log::info!(target: TARGET, "NGX is ready: initialised, bound, SuperSampling available");
            answer(action, Some(plan.route), status, None)
        }
        Err(why) => {
            if host.kind == HostKind::Foreign {
                super::give_up_on_foreign_host(&why);
            }
            // NGX did initialise: the game is told so, the mod's DLSS is off.
            answer(action, Some(plan.route), status, Some(why))
        }
    }
}

/// Bind the feature entry points and read the capability block.
fn after_init(host: &HostInfo) -> Result<(), String> {
    let api = core::bind(host.kind, host.module)
        .map_err(|why| format!("NGX's entry points could not be bound: {why}"))?;
    let bound_from = mhw_win::module_path(api.module);
    log::info!(target: TARGET, module:? = bound_from; "feature entry points bound");
    {
        let mut state = STATE.lock();
        state.api = Some(api);
        state.bound_from = bound_from;
    }
    // SAFETY: NGX was just initialised through this host.
    let availability = unsafe { core::capability(&api) }
        .map_err(|status| format!("GetCapabilityParameters returned {status}"))?;
    STATE.lock().availability = Some(availability);
    match availability.refusal(&host.who()) {
        Some(why) => Err(why),
        None => Ok(()),
    }
}

/// Stand in for `NVSDK_NGX_D3D12_Shutdown`: this crate's features belong to
/// the context being shut down and are released first.
unsafe extern "system" fn game_shutdown() -> u32 {
    let original = enter(Entry::Shutdown);
    crate::feature::release_all_now();
    READY.store(false, Ordering::Relaxed);
    STATE.lock().api = None;
    log::info!(target: TARGET, "the game shuts NGX down");
    if original == 0 || host_refused() {
        return Status::SUCCESS.0;
    }
    // SAFETY: the game's resolved `Shutdown(void)`.
    let status =
        unsafe { std::mem::transmute::<usize, unsafe extern "system" fn() -> u32>(original)() };
    let mut state = STATE.lock();
    if !state.storage_pinned {
        state.storage = None;
    }
    status
}

/// Stand in for `NVSDK_NGX_D3D12_GetScratchBufferSize`: forwarded.
unsafe extern "system" fn game_scratch_buffer_size(
    feature: u32,
    parameters: *const c_void,
    out: *mut usize,
) -> u32 {
    let original = enter(Entry::GetScratchBufferSize);
    if original == 0 || host_refused() {
        return Status::FEATURE_NOT_SUPPORTED.0;
    }
    // SAFETY: the game's resolved entry, its own arguments.
    unsafe {
        std::mem::transmute::<usize, unsafe extern "system" fn(u32, *const c_void, *mut usize) -> u32>(
            original,
        )(feature, parameters, out)
    }
}

/// Stand in for `NVSDK_NGX_D3D12_GetParameters`: the availability answer.
///
/// The game checks `SuperSampling.Available` and
/// `ImageSignalProcessing.Available`; current drivers carry no ISP, so
/// without the answer the game hides its DLSS option and the whole
/// render-small path this mod takes over (see [`availability_writes`]).
unsafe extern "system" fn game_get_parameters(out: *mut *mut c_void) -> u32 {
    let original = enter(Entry::GetParameters);
    if original == 0 || host_refused() {
        return Status::FEATURE_NOT_SUPPORTED.0;
    }
    // SAFETY: the game's resolved entry, its own out pointer.
    let status = Status(unsafe {
        std::mem::transmute::<usize, unsafe extern "system" fn(*mut *mut c_void) -> u32>(original)(
            out,
        )
    });
    if !status.is_success() || out.is_null() {
        return status.0;
    }
    // SAFETY: on success NGX stored a live parameter object (or null) in *out.
    let Some(parameters) = (unsafe { Parameters::from_raw(*out) }) else {
        return status.0;
    };
    // SAFETY: live until the game destroys it, after this returns.
    let (ss, isp) = unsafe {
        (
            parameters.get_i32(names::SS_AVAILABLE),
            parameters.get_i32(names::ISP_AVAILABLE),
        )
    };
    mhw_core::log_once!(Info, TARGET, "game-parameters",
        ss_available:? = ss, isp_available:? = isp;
        "the game asks NGX about DLSS");
    if !SPOOF.load(Ordering::Relaxed) {
        return status.0;
    }
    if !READY.load(Ordering::Relaxed) {
        mhw_core::log_once!(
            Warn,
            TARGET,
            "spoof-not-ready",
            "the game is not told DLSS is available: NGX is not ready ({:?})",
            super::refusal()
        );
        return status.0;
    }
    if ss != Some(1) {
        mhw_core::log_once!(
            Warn,
            TARGET,
            "spoof-ss-unavailable",
            "the game is not told DLSS is available: the block NGX returned says \
             SuperSampling.Available = {ss:?}"
        );
        return status.0;
    }
    // SAFETY: live parameter object, values typed per `availability_writes`.
    unsafe { parameters.apply(&availability_writes()) };
    SPOOFED.fetch_add(1, Ordering::Relaxed);
    mhw_core::log_once!(
        Info,
        TARGET,
        "spoofed",
        "the game is told SuperSampling and ImageSignalProcessing are available"
    );
    status.0
}

/// Stand in for `NVSDK_NGX_D3D12_CreateFeature`: forwarded, unless the host
/// must not be called.
unsafe extern "system" fn game_create_feature(
    list: *mut c_void,
    feature: u32,
    parameters: *mut c_void,
    handle: *mut *mut c_void,
) -> u32 {
    let original = enter(Entry::CreateFeature);
    if original == 0 || host_refused() {
        return Status::FEATURE_NOT_SUPPORTED.0;
    }
    // SAFETY: the game's resolved entry, its own arguments.
    let status = Status(unsafe {
        std::mem::transmute::<
            usize,
            unsafe extern "system" fn(*mut c_void, u32, *mut c_void, *mut *mut c_void) -> u32,
        >(original)(list, feature, parameters, handle)
    });
    mhw_core::log_once!(Info, TARGET, ("game-create", feature),
        feature, status:%;
        "the game created an NGX feature itself");
    status.0
}

/// Stand in for `NVSDK_NGX_D3D12_EvaluateFeature`: a stand-in handle never
/// reaches NGX.
unsafe extern "system" fn game_evaluate_feature(
    list: *mut c_void,
    handle: *const c_void,
    parameters: *mut c_void,
    callback: *const c_void,
) -> u32 {
    let original = enter(Entry::EvaluateFeature);
    if is_stand_in(handle as usize) {
        STAND_IN_CALLS.fetch_add(1, Ordering::Relaxed);
        mhw_core::log_once!(Warn, TARGET, "stand-in-evaluate",
            handle:% = Hex(handle as usize);
            "the game evaluated a stand-in feature itself; not passed to NGX");
        return Status::FEATURE_NOT_FOUND.0;
    }
    if original == 0 || host_refused() {
        return Status::FEATURE_NOT_SUPPORTED.0;
    }
    // SAFETY: the game's resolved entry, its own arguments.
    unsafe {
        std::mem::transmute::<
            usize,
            unsafe extern "system" fn(
                *mut c_void,
                *const c_void,
                *mut c_void,
                *const c_void,
            ) -> u32,
        >(original)(list, handle, parameters, callback)
    }
}

/// Stand in for `NVSDK_NGX_D3D12_ReleaseFeature`: a stand-in handle never
/// reaches NGX (the game releases both whenever it rebuilds its targets).
unsafe extern "system" fn game_release_feature(handle: *mut c_void) -> u32 {
    let original = enter(Entry::ReleaseFeature);
    if is_stand_in(handle as usize) {
        STAND_IN_CALLS.fetch_add(1, Ordering::Relaxed);
        mhw_core::log_every!(16, Debug, TARGET,
            handle:% = Hex(handle as usize);
            "the game released a stand-in feature");
        return Status::SUCCESS.0;
    }
    if original == 0 || host_refused() {
        return Status::SUCCESS.0;
    }
    // SAFETY: the game's resolved entry, its own handle.
    unsafe {
        std::mem::transmute::<usize, unsafe extern "system" fn(*mut c_void) -> u32>(original)(
            handle,
        )
    }
}
