//! The NGX core's exports this crate calls itself: binding them, reading
//! the capability block, and making the redirected `Init`.

use std::ffi::{c_void, CStr, CString};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use windows::core::{PCSTR, PCWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

use crate::host::HostKind;
use crate::init::{Availability, FeatureCommonInfo, Plan, Route, ENGINE_TYPE_CUSTOM};
use crate::param::{names, Parameters};
use crate::status::Status;
use crate::TARGET_NGX as TARGET;

/// `NVSDK_NGX_D3D12_AllocateParameters` / `GetCapabilityParameters`.
pub(crate) type AllocateFn = unsafe extern "system" fn(*mut *mut c_void) -> u32;
/// `NVSDK_NGX_D3D12_DestroyParameters`.
pub(crate) type DestroyFn = unsafe extern "system" fn(*mut c_void) -> u32;
/// `NVSDK_NGX_D3D12_CreateFeature(list, feature, parameters, out handle)`.
pub(crate) type CreateFn =
    unsafe extern "system" fn(*mut c_void, u32, *mut c_void, *mut *mut c_void) -> u32;
/// `NVSDK_NGX_D3D12_EvaluateFeature(list, handle, parameters, callback)`.
pub(crate) type EvaluateFn =
    unsafe extern "system" fn(*mut c_void, *const c_void, *mut c_void, *const c_void) -> u32;
/// `NVSDK_NGX_D3D12_ReleaseFeature(handle)`.
pub(crate) type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;

/// The game's legacy `Init`, forwarded as six words.
type LegacyInitFn = unsafe extern "system" fn(u64, usize, usize, usize, usize, usize) -> u32;
/// `NVSDK_NGX_D3D12_Init_Ext(id, path, device, version, info)`.
type InitExtFn =
    unsafe extern "system" fn(u64, *const u16, *mut c_void, u32, *const FeatureCommonInfo) -> u32;
/// `NVSDK_NGX_D3D12_Init_ProjectID(project, engine type, engine version,
/// path, device, version, info)`, the core export behind the static
/// library's `Init_with_ProjectID`.
type InitProjectIdFn = unsafe extern "system" fn(
    *const u8,
    u32,
    *const u8,
    *const u16,
    *mut c_void,
    u32,
    *const FeatureCommonInfo,
) -> u32;

/// The feature entry points, all from one module.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Api {
    pub allocate: AllocateFn,
    pub destroy: DestroyFn,
    pub create: CreateFn,
    pub evaluate: EvaluateFn,
    pub release: ReleaseFn,
    pub capability: AllocateFn,
    /// The module they were bound from.
    pub module: usize,
}

const BOUND: [&CStr; 6] = [
    c"NVSDK_NGX_D3D12_AllocateParameters",
    c"NVSDK_NGX_D3D12_DestroyParameters",
    c"NVSDK_NGX_D3D12_CreateFeature",
    c"NVSDK_NGX_D3D12_EvaluateFeature",
    c"NVSDK_NGX_D3D12_ReleaseFeature",
    c"NVSDK_NGX_D3D12_GetCapabilityParameters",
];

/// The address of export `name` of `module`.
pub(crate) fn export(module: usize, name: &CStr) -> Option<usize> {
    if module == 0 {
        return None;
    }
    // SAFETY: `module` is a loaded module's base (callers pass handles they
    // got from the loader); `name` is NUL-terminated.
    unsafe { GetProcAddress(HMODULE(module as *mut c_void), PCSTR(name.as_ptr().cast())) }
        .map(|entry| entry as usize)
}

/// The names in `required` that `module` does not export.
pub(crate) fn missing_exports(module: usize, required: &[&CStr]) -> Vec<String> {
    required
        .iter()
        .filter(|name| export(module, name).is_none())
        .map(|name| name.to_string_lossy().into_owned())
        .collect()
}

/// The loaded module called `name`, without loading it.
pub(crate) fn loaded(name: &str) -> Option<usize> {
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call; no reference
    // is taken on the module.
    unsafe { GetModuleHandleW(PCWSTR(wide.as_ptr())) }
        .ok()
        .map(|module| module.0 as usize)
        .filter(|&base| base != 0)
}

/// Bind every feature entry point from `module`, or none.
fn bind_from(module: usize) -> Result<Api, Vec<String>> {
    let missing = missing_exports(module, &BOUND);
    if !missing.is_empty() {
        return Err(missing);
    }
    let entry = |index: usize| BOUND.get(index).and_then(|name| export(module, name));
    let (Some(a), Some(d), Some(c), Some(e), Some(r), Some(cap)) =
        (entry(0), entry(1), entry(2), entry(3), entry(4), entry(5))
    else {
        return Err(vec!["(export vanished between lookups)".into()]);
    };
    // SAFETY: each address is the named export, whose C signature (public
    // SDK headers, unchanged since the 1.x cores) is the type it becomes.
    unsafe {
        Ok(Api {
            allocate: std::mem::transmute::<usize, AllocateFn>(a),
            destroy: std::mem::transmute::<usize, DestroyFn>(d),
            create: std::mem::transmute::<usize, CreateFn>(c),
            evaluate: std::mem::transmute::<usize, EvaluateFn>(e),
            release: std::mem::transmute::<usize, ReleaseFn>(r),
            capability: std::mem::transmute::<usize, AllocateFn>(cap),
            module,
        })
    }
}

/// Bind the feature entry points for a session served by `host` from
/// `module`.
///
/// For the driver, `_nvngx.dll` is preferred when it is loaded and exports
/// everything: that is where 0.2 bound from, proven on every driver it met,
/// while whether the driver-store `nvngx.dll` the game loads exports the
/// whole modern surface itself is unknown (RE experiment E4). A foreign host
/// is bound from its own module only.
pub(crate) fn bind(host: HostKind, module: usize) -> Result<Api, String> {
    if host == HostKind::Driver {
        if let Some(core) = loaded("_nvngx.dll") {
            match bind_from(core) {
                Ok(api) => return Ok(api),
                Err(missing) => log::warn!(target: TARGET,
                    "_nvngx.dll is loaded but lacks {}; binding from the module that served Init",
                    missing.join(", ")),
            }
        }
    }
    bind_from(module).map_err(|missing| format!("it does not export {}", missing.join(", ")))
}

/// Read the capability block: what NGX says about DLSS-SR.
///
/// The frame generation keys are logged, not used: `GetCapabilityParameters`
/// takes no context, so with two NGX initialisations in the process (the
/// game's and Streamline's) nothing says which one the block describes. A
/// positive reading is informative; a negative one must not be quoted as a
/// cause.
///
/// # Safety
/// NGX has been initialised through the module `api` was bound from.
pub(crate) unsafe fn capability(api: &Api) -> Result<Availability, Status> {
    let mut raw = std::ptr::null_mut();
    // SAFETY: `capability` allocates a parameter object into `raw`.
    let status = Status(unsafe { (api.capability)(&mut raw) });
    // SAFETY: a non-null result is a live parameter object until destroyed.
    let parameters = match unsafe { Parameters::from_raw(raw) } {
        Some(parameters) if status.is_success() => parameters,
        _ => return Err(status),
    };
    // SAFETY: `parameters` is live until `destroy` below.
    let answer = unsafe {
        let answer = Availability {
            available: parameters.get_i32(names::SS_AVAILABLE),
            init_result: parameters
                .get_i32(names::SS_FEATURE_INIT_RESULT)
                .map(Status::from_i32),
            needs_updated_driver: parameters.get_i32(names::SS_NEEDS_UPDATED_DRIVER),
            min_driver: (
                parameters.get_u32(names::SS_MIN_DRIVER_MAJOR),
                parameters.get_u32(names::SS_MIN_DRIVER_MINOR),
            ),
        };
        let fg_available = parameters.get_i32(names::FG_AVAILABLE);
        let fg_init_result = parameters
            .get_i32(names::FG_FEATURE_INIT_RESULT)
            .map(Status::from_i32);
        let dlssg_frames_max = parameters.get_u32(names::DLSSG_MULTI_FRAME_COUNT_MAX);
        let snippet_opt_level = parameters.get_i32(names::SNIPPET_OPT_LEVEL);
        let snippet_dev_branch = parameters.get_i32(names::SNIPPET_IS_DEV_BRANCH);
        log::info!(target: TARGET,
            available:? = answer.available,
            init_result:? = answer.init_result,
            needs_driver:? = answer.needs_updated_driver,
            min_driver:? = answer.min_driver,
            fg_available:?, fg_init_result:?, dlssg_frames_max:?, snippet_opt_level:?,
            snippet_dev_branch:?;
            "capability block");
        (api.destroy)(parameters.as_raw());
        answer
    };
    Ok(answer)
}

/// Everything a redirected `Init` hands NGX by pointer: the data path, the
/// feature path list and, for a project id, its strings.
///
/// NGX may keep these pointers for as long as its context lives, so the
/// session owns the storage until NGX has been shut down (and for good when
/// an `Init` was abandoned on its timeout and may still be running). The
/// boxes keep their addresses when the storage moves.
#[derive(Debug)]
pub(crate) struct InitStorage {
    data_path: Box<[u16]>,
    /// The feature path list `info` points to; owned, never read here.
    _list: Box<[usize; 1]>,
    info: Box<FeatureCommonInfo>,
    project: Option<(CString, CString)>,
}

// SAFETY: the raw pointers inside `info`/`list` point into the storage's own
// boxes, which move with it; nothing is shared with another owner.
unsafe impl Send for InitStorage {}

impl InitStorage {
    /// Storage for `data_path` (also the one feature path) and an optional
    /// `(project id, engine version)`; `None` when a string holds a NUL.
    pub(crate) fn new(data_path: &Path, project: Option<(&str, &str)>) -> Option<InitStorage> {
        let data_path: Box<[u16]> = data_path
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let project = match project {
            Some((id, engine)) => Some((CString::new(id).ok()?, CString::new(engine).ok()?)),
            None => None,
        };
        let list = Box::new([data_path.as_ptr() as usize]);
        let info = Box::new(FeatureCommonInfo::with_paths(
            list.as_ptr() as *const *const u16,
            1,
        ));
        Some(InitStorage {
            data_path,
            _list: list,
            info,
            project,
        })
    }

    /// The call for `plan` through `entry`, pointing into this storage.
    pub(crate) fn call(&self, plan: Plan, entry: usize, device: usize, version: u32) -> InitCall {
        InitCall {
            plan,
            entry,
            data_path: self.data_path.as_ptr() as usize,
            device,
            version,
            info: &*self.info as *const FeatureCommonInfo as usize,
            project: self
                .project
                .as_ref()
                .map(|(id, engine)| (id.as_ptr() as usize, engine.as_ptr() as usize)),
        }
    }
}

/// A redirected `Init`, as addresses into an [`InitStorage`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct InitCall {
    plan: Plan,
    /// The game's `Init` for [`Route::Legacy`], else the route's export.
    entry: usize,
    data_path: usize,
    device: usize,
    version: u32,
    info: usize,
    /// `(project id, engine version)` C strings for [`Route::ProjectId`].
    project: Option<(usize, usize)>,
}

impl InitCall {
    /// Make the call.
    ///
    /// # Safety
    /// `entry` is the entry point the route names, `device` a live
    /// `ID3D12Device`, the storage the call points into outlives every use
    /// NGX makes of it, and no other thread is initialising NGX (the core
    /// does not take concurrent initialisation; the game's own call is the
    /// moment).
    unsafe fn call(self) -> Status {
        let path = self.data_path as *const u16;
        let device = self.device as *mut c_void;
        let info = self.info as *const FeatureCommonInfo;
        // SAFETY: per the contract, `entry` has the route's signature and
        // every pointer is live.
        Status(unsafe {
            match self.plan.route {
                Route::Legacy { tail } => std::mem::transmute::<usize, LegacyInitFn>(self.entry)(
                    self.plan.app_id,
                    self.data_path,
                    self.device,
                    self.version as usize,
                    tail[0],
                    tail[1],
                ),
                Route::InitExt => std::mem::transmute::<usize, InitExtFn>(self.entry)(
                    self.plan.app_id,
                    path,
                    device,
                    self.version,
                    info,
                ),
                Route::ProjectId => {
                    let Some((project, engine)) = self.project else {
                        return Status::INVALID_PARAMETER;
                    };
                    std::mem::transmute::<usize, InitProjectIdFn>(self.entry)(
                        project as *const u8,
                        ENGINE_TYPE_CUSTOM,
                        engine as *const u8,
                        path,
                        device,
                        self.version,
                        info,
                    )
                }
            }
        })
    }

    /// Make the call here, or on a thread of its own given `limit`
    /// (`None` when it has not returned by then; the thread is left: a
    /// call that does not return cannot be taken back, only not waited for).
    ///
    /// # Safety
    /// As [`InitCall::call`]; after `None` the storage must be kept for the
    /// rest of the process, since the call may still be using it.
    pub(crate) unsafe fn run(self, limit: Option<Duration>) -> Option<Status> {
        let Some(limit) = limit else {
            // SAFETY: forwarded.
            return Some(unsafe { self.call() });
        };
        let (sender, receiver) = mpsc::channel();
        let spawned = mhw_win::spawn("ngx-init", move || {
            // SAFETY: forwarded; the game's thread waits on this one, so
            // initialisation is still not concurrent.
            let _ = sender.send(unsafe { self.call() });
        });
        match spawned {
            Ok(_) => receiver.recv_timeout(limit).ok(),
            // SAFETY: forwarded.
            Err(_) => Some(unsafe { self.call() }),
        }
    }
}
