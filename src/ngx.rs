//! NGX, called directly rather than through NVIDIA's static library.
//!
//! The driver's `_nvngx.dll` exports the whole `NVSDK_NGX_D3D12_*` surface, so
//! the MSVC-only `nvsdk_ngx_d3d12.lib` is never needed -- which is what keeps
//! this project on the `windows-gnu` toolchain. The module is already resident
//! in the process: MHW initialises NGX for its own DLSS 1 even when the option
//! is off.
//!
//! `NVSDK_NGX_Parameter` is a C++ object. Its vtable, from
//! `nvsdk_ngx_params.h`, is eight `Set` overloads (u64, f32, f64, u32, i32,
//! ID3D11Resource*, ID3D12Resource*, void*), then the same eight as `Get`,
//! then `Reset`. Those slot numbers are the whole ABI.

use std::ffi::{c_void, CString};
use std::sync::atomic::{AtomicUsize, Ordering};
use windows::core::PCSTR;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

/// `NVSDK_NGX_Result_Success`.
const SUCCESS: u32 = 0x1;
/// `NVSDK_NGX_Version_API` for SDK 310.x, which the game's `Init` is given in
/// place of its own 18.
const NGX_API_VERSION: usize = 21;
type GetCapabilityParametersFn = unsafe extern "C" fn(*mut *mut c_void) -> u32;
type DestroyParametersFn = unsafe extern "C" fn(*mut c_void) -> u32;

/// Vtable slots for the `Get` overloads. MSVC reverses overload order, so
/// these are counted from the end of the declared list rather than the start;
/// see the note in `dlss.rs`.
const GET_I32: usize = 11;
const GET_U32: usize = 12;
/// And the matching `Set` overloads, counted the same way.
const SET_I32: usize = 3;
const SET_U32: usize = 4;

/// Read an int-valued NGX parameter.
///
/// # Safety
/// `parameters` must be a live `NVSDK_NGX_Parameter`.
unsafe fn get_int(parameters: *mut c_void, name: &str, slot: usize) -> Option<i32> {
    if parameters.is_null() {
        return None;
    }
    let vtable = *(parameters as *const *const usize);
    let entry = *vtable.add(slot);
    let call: unsafe extern "C" fn(*mut c_void, PCSTR, *mut i32) -> u32 =
        std::mem::transmute(entry);

    let key = CString::new(name).ok()?;
    let mut value = 0i32;
    let result = call(parameters, PCSTR(key.as_ptr() as *const u8), &mut value);
    (result == SUCCESS).then_some(value)
}

/// Write an int-valued NGX parameter.
///
/// # Safety
/// `parameters` must be a live `NVSDK_NGX_Parameter`.
unsafe fn set_int(parameters: *mut c_void, name: &str, slot: usize, value: i32) -> bool {
    if parameters.is_null() {
        return false;
    }
    let vtable = *(parameters as *const *const usize);
    let entry = *vtable.add(slot);
    let call: unsafe extern "C" fn(*mut c_void, PCSTR, i32) = std::mem::transmute(entry);
    let Ok(key) = CString::new(name) else {
        return false;
    };
    call(parameters, PCSTR(key.as_ptr() as *const u8), value);
    true
}

/// The driver's NGX core, if it is loaded.
///
/// Deliberately does not go looking for it: resolving the driver store path
/// and loading the library from here, at start-up, takes the game down
/// before anything is logged. The game loads `_nvngx.dll` itself in the
/// course of initialising NGX, so by the time anything here asks, it is
/// there or it is not going to be.
unsafe fn core() -> Option<HMODULE> {
    let name: Vec<u16> = "_nvngx.dll\u{0}".encode_utf16().collect();
    GetModuleHandleW(windows::core::PCWSTR(name.as_ptr()))
        .ok()
        .filter(|module| !module.0.is_null())
}

unsafe fn resolve<T>(module: HMODULE, name: &[u8]) -> Option<T> {
    GetProcAddress(module, PCSTR(name.as_ptr())).map(|entry| std::mem::transmute_copy(&entry))
}

/// What the driver says about DLSS, read after initialisation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Availability {
    pub available: Option<i32>,
    pub init_result: Option<i32>,
    pub needs_updated_driver: Option<i32>,
    pub min_driver: (Option<i32>, Option<i32>),
}

impl Availability {
    /// Why DLSS cannot be used, in words, or `None` when it can.
    pub(crate) fn refusal(&self) -> Option<String> {
        if self.available == Some(1) {
            return None;
        }
        let result = match self.init_result {
            Some(code) => format!(
                "{:#010x}{}",
                code as u32,
                status(code as u32).replace("  ", " ")
            ),
            None => "unset".to_string(),
        };
        // Whose answer it is: the driver's, or the module that serves NGX in
        // its place (`ngxhost`), whose capability block this then was.
        let who = match crate::ngxhost::answerer() {
            who if who == "the driver" => "the driver's NGX".to_string(),
            who => format!("{who}, which serves NGX in this process,"),
        };
        let mut text = format!(
            "{who} reports SuperSampling.Available = {} with FeatureInitResult {result}",
            match self.available {
                Some(value) => value.to_string(),
                None => "unset".to_string(),
            }
        );
        if self.needs_updated_driver == Some(1) {
            text.push_str(&format!(
                ", and says the driver has to be updated to at least {}.{}",
                self.min_driver.0.unwrap_or(0),
                self.min_driver.1.unwrap_or(0)
            ));
        }
        Some(text)
    }
}

/// Bind the feature entry points and ask the driver what it offers, now that
/// NGX is initialised. Returns the answer about DLSS, if there was one.
///
/// "The driver" is whichever module serves NGX (`ngxhost::serving`): the
/// driver's `_nvngx.dll`, found by name, unless another module answered the
/// game's resolve of the initialisation -- then everything is bound from that
/// one, the capability block read is its own, and readiness is judged from
/// what it says. The driver's module is never bound in its place.
///
/// Called from the game's initialisation itself, on the thread that made it:
/// the core does not take two threads initialising at once, and this is the
/// one moment that is certainly after the initialisation and before any
/// other use. A thread polling for it would have to give up at some point,
/// and a slow start would then bind nothing.
///
/// # Safety
/// NGX must have been initialised in this process.
unsafe fn after_init() -> Option<Availability> {
    let module = match crate::ngxhost::serving() {
        crate::ngxhost::Serving::Driver => {
            let Some(module) = core() else {
                crate::log!(
                    "[ngx] _nvngx.dll is not loaded after NGX was initialised; nothing to bind"
                );
                return None;
            };
            module
        }
        crate::ngxhost::Serving::Foreign(module) => module,
        crate::ngxhost::Serving::Nothing => {
            crate::log!(
                "[ngx] {} serves NGX and is not used; nothing is bound, from it or from the \
                 driver's module",
                crate::ngxhost::answerer()
            );
            return None;
        }
    };
    if crate::dlss::bind(module) {
        crate::log!(
            "[ngx] feature entry points bound from {}",
            crate::ngxhost::answerer()
        );
        // Reads one small file when a module other than the driver's serves
        // NGX, and nothing otherwise. Once, inside the game's own
        // initialisation of NGX, which loads libraries itself.
        crate::ngxhost::report_bound();
    } else {
        crate::log!("[ngx] could not bind the feature entry points");
        return None;
    }
    let Some(capabilities): Option<GetCapabilityParametersFn> =
        resolve(module, b"NVSDK_NGX_D3D12_GetCapabilityParameters\0")
    else {
        crate::log!("[ngx] NVSDK_NGX_D3D12_GetCapabilityParameters missing");
        return None;
    };

    let mut parameters: *mut c_void = std::ptr::null_mut();
    let result = capabilities(&mut parameters);
    crate::log!(
        "[ngx] GetCapabilityParameters -> {result:#010x}{}",
        status(result)
    );
    if result != SUCCESS || parameters.is_null() {
        return None;
    }

    // Upscaling first: these are the keys the decision below is made on.
    //
    // The frame generation keys after them are NOT ATTRIBUTABLE, and the
    // reason is one line of this file: `GetCapabilityParametersFn` takes a
    // single argument, the out pointer. There is no device and no context
    // selector. With two NGX initialisations in the process -- the game's
    // `Init` with application id 100182111 and Streamline's
    // `Init_with_ProjectID` -- nothing says which context the returned block
    // describes. They are kept because they are free and because a positive
    // reading is informative, but a negative one must not be quoted as a
    // cause.
    //
    // `DLSSG.MultiFrameCountMax` is the name the 310.7.0 guide gives and
    // `DLSSG.MultiFrameCount` the older one; both are read, and one of them
    // coming back unset is expected.
    for (name, slot) in [
        ("SuperSampling.Available", GET_I32),
        ("SuperSampling.NeedsUpdatedDriver", GET_I32),
        ("SuperSampling.MinDriverVersionMajor", GET_U32),
        ("SuperSampling.MinDriverVersionMinor", GET_U32),
        ("SuperSampling.FeatureInitResult", GET_I32),
        ("FrameGeneration.Available", GET_I32),
        ("FrameGeneration.NeedsUpdatedDriver", GET_I32),
        ("FrameGeneration.MinDriverVersionMajor", GET_U32),
        ("FrameGeneration.MinDriverVersionMinor", GET_U32),
        ("FrameGeneration.FeatureInitResult", GET_I32),
        ("DLSSG.MultiFrameCount", GET_U32),
        ("DLSSG.MultiFrameCountMax", GET_U32),
        ("Snippet.OptLevel", GET_I32),
        ("Snippet.IsDevBranch", GET_I32),
    ] {
        match get_int(parameters, name, slot) {
            // The init results are NGX status codes stored in a signed field,
            // which is how 0xBAD00004 once came back as -1160773628 and read
            // as noise.
            Some(value) if name.ends_with("FeatureInitResult") => crate::log!(
                "[ngx]   {name:<40} = {:#010x}{}",
                value as u32,
                status(value as u32)
            ),
            Some(value) => crate::log!("[ngx]   {name:<40} = {value}"),
            None => crate::log!("[ngx]   {name:<40} = <unset>"),
        }
    }
    let answer = Availability {
        available: get_int(parameters, "SuperSampling.Available", GET_I32),
        init_result: get_int(parameters, "SuperSampling.FeatureInitResult", GET_I32),
        needs_updated_driver: get_int(parameters, "SuperSampling.NeedsUpdatedDriver", GET_I32),
        min_driver: (
            get_int(parameters, "SuperSampling.MinDriverVersionMajor", GET_U32),
            get_int(parameters, "SuperSampling.MinDriverVersionMinor", GET_U32),
        ),
    };

    if let Some(destroy) =
        resolve::<DestroyParametersFn>(module, b"NVSDK_NGX_D3D12_DestroyParameters\0")
    {
        destroy(parameters);
    }
    Some(answer)
}

/// An NGX status code by name.
///
/// Shared, because every status this library reports comes from the same
/// enumeration and a bare `0xbad00005` in a log costs more to look up than it
/// does to print properly.
pub(crate) fn status(result: u32) -> &'static str {
    match result {
        0x1 => "  success",
        0xBAD0_0001 => "  FeatureNotSupported",
        0xBAD0_0002 => "  PlatformError",
        0xBAD0_0003 => "  FeatureAlreadyExists",
        0xBAD0_0004 => "  FeatureNotFound",
        0xBAD0_0005 => "  InvalidParameter",
        0xBAD0_0006 => "  ScratchBufferTooSmall",
        0xBAD0_0007 => "  NotInitialized",
        0xBAD0_0008 => "  UnsupportedInputFormat",
        0xBAD0_0009 => "  RWFlagMissing",
        0xBAD0_000A => "  MissingInput",
        0xBAD0_000B => "  UnableToInitializeFeature",
        0xBAD0_000C => "  OutOfDate",
        0xBAD0_000D => "  OutOfGPUMemory",
        0xBAD0_000E => "  UnsupportedFormat",
        0xBAD0_000F => "  UnableToWriteToAppDataPath",
        0xBAD0_0010 => "  UnsupportedParameter",
        0xBAD0_0000 => "  generic failure",
        _ => "",
    }
}

// ---------------------------------------------------------------------------
// Learning the ABI from a working caller
// ---------------------------------------------------------------------------

/// The real `NVSDK_NGX_D3D12_Init`, as the game resolved it.
pub static GAME_INIT: AtomicUsize = AtomicUsize::new(0);
/// Set once the game's own initialisation has returned.
pub static GAME_INIT_DONE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Where NGX should look for a snippet: our folder, not the game's.
static DATA_PATH: parking_lot::Mutex<Option<Vec<u16>>> = parking_lot::Mutex::new(None);

/// Where NGX was told to look, as a path rather than as the wide string the
/// call wants. Frame generation needs it to say what is actually in there,
/// which is the first thing to check for any `FeatureNotFound`.
static DATA_FOLDER: parking_lot::Mutex<Option<std::path::PathBuf>> = parking_lot::Mutex::new(None);

pub fn data_path() -> Option<std::path::PathBuf> {
    DATA_FOLDER.lock().clone()
}

/// The DLSS library this mod needs, by the name NGX knows it by.
pub const SNIPPET: &str = "nvngx_dlss.dll";

/// Where our snippets live: beside the game's executable, in `mhw-dlss`.
///
/// The game's own folder holds its DLSS 1.1 copy under the same name, which
/// is why ours are kept apart.
pub fn snippet_folder() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        // Apart from the game's DLSS 1.1 copy, so the two NGX clients cannot
        // pick up each other's. This library is installed in the same
        // folder, and that is what makes NGX find the snippet when this
        // library is the one initialising it: it looks in the folder of the
        // module that makes the call, and the application data path it is
        // handed -- this folder too -- does not decide it (`lib.rs`, `home`).
        .join("mhw-dlss")
}

pub fn set_data_path(path: &std::path::Path) {
    *DATA_FOLDER.lock() = Some(path.to_path_buf());
    let mut wide: Vec<u16> = path.to_string_lossy().encode_utf16().collect();
    wide.push(0);
    *DATA_PATH.lock() = Some(wide);
}

/// Publish the snippet folder, at load, and say what is in it.
///
/// The folder is made if it is not there: NGX is handed this path as its
/// application data path, where it writes its own log, and a path that does
/// not exist is one more thing to go wrong in a session that is already not
/// going to have DLSS. Whether the library is in it is looked at again when
/// the game initialises NGX, which is when it matters.
pub fn publish_snippet_folder() {
    let folder = snippet_folder();
    let made = !folder.is_dir() && std::fs::create_dir_all(&folder).is_ok();
    set_data_path(&folder);
    crate::log!(
        "[ngx] snippet folder {}{}: {SNIPPET} is {}",
        folder.display(),
        if made {
            " (it did not exist and has been created)"
        } else {
            ""
        },
        if snippet_present() {
            "there"
        } else {
            "NOT there"
        }
    );
}

/// Whether the DLSS library is in the snippet folder right now.
pub fn snippet_present() -> bool {
    data_path().is_some_and(|folder| folder.join(SNIPPET).is_file())
}

/// Stand in for the game's own NGX initialisation.
///
/// It logs the game's arguments, and then does one of two things. On the
/// build this mod knows, with its DLSS library in place, it makes the call
/// with two arguments changed -- the API version and the application data
/// path -- binds NGX's entry points and asks the driver what it offers. In
/// every other case it makes the call exactly as the game made it.
///
/// The arguments are passed on as six machine words, without reference to a
/// declaration of this old SDK version's entry point. The game is a working
/// NGX client, so whatever it passes is correct by construction, and the
/// Microsoft x64 convention lets a caller pass more than the callee reads --
/// extra registers and stack slots are simply ignored, and the caller cleans
/// up -- so forwarding six is safe regardless of the real arity.
///
/// # Safety
/// Only to be called by the game in place of `NVSDK_NGX_D3D12_Init`, which
/// is what the `GetProcAddress` hook arranges: `GAME_INIT` must hold the
/// real entry point.
pub unsafe extern "system" fn observe_game_init(
    a0: u64,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
) -> u32 {
    let original = GAME_INIT.load(Ordering::Relaxed);
    crate::log!("[ngx] the game calls NVSDK_NGX_D3D12_Init with:");
    crate::log!("[ngx]   a0 = {a0:#018x}   ({a0})");
    crate::log!("[ngx]   a1 = {a1:#018x}{}", describe_pointer(a1));
    crate::log!("[ngx]   a2 = {a2:#018x}{}", describe_pointer(a2));
    crate::log!("[ngx]   a3 = {a3:#018x}{}", describe_pointer(a3));
    crate::log!("[ngx]   a4 = {a4:#018x}{}", describe_pointer(a4));
    crate::log!("[ngx]   a5 = {a5:#018x}{}", describe_pointer(a5));

    if original == 0 {
        crate::log!("[ngx] no original to forward to");
        return 0xBAD0_0000;
    }
    let untouched: unsafe extern "system" fn(u64, usize, usize, usize, usize, usize) -> u32 =
        std::mem::transmute(original);

    // The game is initialising NGX, so its code is running and whatever of
    // it does not match the known build now is not going to. This is the
    // last moment the question can be answered before the answer is needed:
    // for a build the mod does not know, the call goes through exactly as the
    // game made it.
    if !crate::gate::decide_now("the game initialised NGX") {
        // Except into a module that is not the driver's. The entry point the
        // game resolved there is not the one its arguments are for (`ngxhost`
        // has the detail: the game puts the API version where that export
        // takes a pointer), so "as the game made it" is a call this library
        // already knows to be a wild read. It is not made, here or on any
        // other path, and the game is told what a machine without DLSS tells
        // it.
        if crate::ngxhost::foreign() || crate::ngxhost::unusable() {
            let host = crate::ngxhost::answerer();
            crate::ngxhost::leave_uncalled(
                "this mod is standing down and initialises nothing, and the \
                 NVSDK_NGX_D3D12_Init the game resolved from it does not take the arguments \
                 the game passes",
            );
            crate::log!(
                "[ngx] the game's NVSDK_NGX_D3D12_Init was not passed on to {host}; it was \
                 answered with 0xbad00001 FeatureNotSupported"
            );
            return 0xBAD0_0001;
        }
        let result = untouched(a0, a1, a2, a3, a4, a5);
        crate::log!(
            "[ngx] passed the game's own NVSDK_NGX_D3D12_Init through unchanged, because the \
             mod is standing down; it returned {result:#010x}{}",
            status(result)
        );
        return result;
    }

    // And only when the library this mod needs is where it should be.
    // Without it the altered initialisation below buys nothing, and the game
    // is better served by the call it made.
    //
    // That is a statement about the driver's NGX, which looks for the DLSS
    // library in places this mod knows. A module that serves NGX in the
    // driver's place finds its libraries by its own settings (OptiScaler by
    // `NvngxFeaturePath`, and by the feature path list `ngxhost::init` hands
    // it), so under one the file's absence from this folder is said and not
    // judged: whether DLSS can be used is then that module's answer, read
    // after initialisation like the driver's.
    let hosted = crate::ngxhost::foreign() || crate::ngxhost::refused();
    if hosted && !snippet_present() {
        crate::log!(
            "[ngx] {} is not there. That is not held against this session: NGX is served by \
             {}, which looks for the DLSS library where its own settings say, and its answer \
             after initialisation is what decides",
            snippet_folder().join(SNIPPET).display(),
            crate::ngxhost::answerer()
        );
    }
    let path = DATA_PATH.lock().clone();
    let (Some(wide), true) = (&path, hosted || snippet_present()) else {
        crate::gate::declare_unusable(&format!(
            "{} is not there. Run fetch-nvidia.cmd in the mhw-dlss folder, which downloads \
             NVIDIA's DLSS library, and start the game again",
            snippet_folder().join(SNIPPET).display()
        ));
        let result = untouched(a0, a1, a2, a3, a4, a5);
        crate::log!(
            "[ngx] passed the game's own NVSDK_NGX_D3D12_Init through unchanged; it returned \
             {result:#010x}{}",
            status(result)
        );
        return result;
    };

    // Initialise NGX once, here, with our own folder substituted for the
    // game's. Calling Init a second time from another thread crashed the
    // process: the core does not tolerate concurrent initialisation, and the
    // game's call happens at exactly the right moment anyway.
    let redirected = wide.as_ptr() as usize;
    // The game's own application id, and the API version a 310.x snippet
    // needs. The game asks for 18, the 1.3-era value, and with that the
    // driver reports SuperSampling as unavailable.
    let app_id = a0;
    let version = NGX_API_VERSION;
    // What is passed, and as what. The second argument is NGX's application
    // data path; where NGX looks for a snippet is not decided by it.
    crate::log!(
        "[ngx] initialising instead with app_id={app_id} version={version} and application \
         data path {}",
        snippet_folder().display()
    );

    let target: unsafe extern "system" fn(u64, usize, usize, usize, usize, usize) -> u32 =
        std::mem::transmute(original);
    // The third argument is the game's device, and it is passed on as the
    // game holds it. With ReShade loaded that is ReShade's proxy, and it is
    // not unwrapped.
    //
    // NGX creates heaps and lists on the device it holds and uses them with
    // the list it is given, so what matters is that the two agree -- and
    // with frame generation on, Streamline has already initialised NGX with
    // the device *it* was given, which has to be ReShade's proxy or ReShade
    // drops out of the presentation path. So everything NGX is handed stays
    // on the proxy's side: this device, and the command list
    // (`dlss::list_for_ngx`).
    static DEVICE: crate::reshade::Kept = crate::reshade::Kept::new("NVSDK_NGX_D3D12_Init");
    DEVICE.note(a2 as *mut std::ffi::c_void);
    INIT_DEVICE.store(a2, Ordering::Relaxed);
    // To the entry point the game resolved, unless a module other than the
    // driver's answered that resolve: then through its `Init_Ext`, or not at
    // all. `ngxhost` has the reason.
    let result = match crate::ngxhost::init(app_id, redirected, a2, version) {
        Some(result) => result,
        // Streamline ended its own initialisation in an exception. A call
        // into the driver's NGX after that has been seen never to return,
        // with the game waiting on it for good. So it is made where it can
        // be given up on.
        None if crate::streamline::faulted() => {
            crate::log!(
                "[ngx] Streamline raised an exception earlier in this session, so this call \
                 is made on a thread of its own and given {} s",
                INIT_TIME_LIMIT.as_secs()
            );
            match within(INIT_TIME_LIMIT, move || {
                target(app_id, redirected, a2, version, a4, a5)
            }) {
                Some(result) => result,
                None => {
                    crate::gate::declare_unusable(&format!(
                        "NVSDK_NGX_D3D12_Init did not return within {} s after Streamline \
                         raised an exception while initialising. The game is told NGX is \
                         not available and carries on without it; starting the game again \
                         normally clears this",
                        INIT_TIME_LIMIT.as_secs()
                    ));
                    return 0xBAD0_0001;
                }
            }
        }
        None => target(app_id, redirected, a2, version, a4, a5),
    };
    crate::log!("[ngx] Init returned {result:#010x}{}", status(result));

    if result != SUCCESS {
        if crate::ngxhost::refused() {
            crate::gate::declare_unusable(&format!(
                "NGX is served by {}, which cannot be used (the [ngx] line above says why), \
                 and the driver's module is not used in its place",
                crate::ngxhost::answerer()
            ));
        } else {
            crate::gate::declare_unusable(&format!(
                "NGX did not initialise with API version {version}: {} returned \
                 {result:#010x}{}",
                crate::ngxhost::answerer(),
                status(result)
            ));
        }
        return result;
    }
    GAME_INIT_DONE.store(true, Ordering::Relaxed);
    // Bound now, on this thread, and the driver asked what it offers. DLSS
    // is used from here on only if the answer is yes.
    match after_init() {
        Some(answer) => match answer.refusal() {
            None if crate::ngxhost::foreign() => crate::gate::declare_ready(&format!(
                "NGX is initialised through {host}, its entry points are bound from that \
                 module and it reports SuperSampling.Available = 1. Which DLSS library it \
                 loads is its own choice; {} is {}",
                snippet_folder().join(SNIPPET).display(),
                if snippet_present() {
                    "in place"
                } else {
                    "not there"
                },
                host = crate::ngxhost::answerer()
            )),
            None => crate::gate::declare_ready(&format!(
                "NGX is initialised, its entry points are bound, the driver reports \
                 SuperSampling.Available = 1 and {} is in place",
                snippet_folder().join(SNIPPET).display()
            )),
            Some(why) => crate::gate::declare_unusable(&why),
        },
        None => crate::gate::declare_unusable(
            "NGX initialised, but its entry points could not be bound or it would not say \
             what it offers; the [ngx] lines above say which",
        ),
    }
    result
}

/// The device this library handed the driver's NGX at initialisation.
static INIT_DEVICE: AtomicUsize = AtomicUsize::new(0);

/// That device, or zero before the game has initialised NGX.
pub fn init_device() -> usize {
    INIT_DEVICE.load(Ordering::Relaxed)
}

/// A short note on what a value might be: a wide string, or just a small
/// integer. Enough to tell an application data path from a version number.
unsafe fn describe_pointer(value: usize) -> String {
    if value == 0 {
        return "   null".into();
    }
    if value < 0x10000 {
        return format!("   small integer {value}");
    }
    // Reading an arbitrary address can fault, so only obviously plausible
    // user-space pointers are inspected, and only as UTF-16.
    if !(0x1000..=0x7FFF_FFFF_FFFF).contains(&value) {
        return String::new();
    }
    let mut text = String::new();
    for index in 0..64 {
        let unit = std::ptr::read_unaligned((value as *const u16).add(index));
        if unit == 0 {
            break;
        }
        match char::from_u32(unit as u32) {
            Some(c) if c.is_ascii_graphic() || c == ' ' || c == '\u{5c}' => text.push(c),
            _ => return String::new(),
        }
    }
    if text.len() >= 3 {
        format!("   wide string {text:?}")
    } else {
        String::new()
    }
}

// ---------------------------------------------------------------------------
// Telling the game its DLSS is available
// ---------------------------------------------------------------------------

/// The real `NVSDK_NGX_D3D12_GetParameters`, as the game resolved it.
pub static GAME_GET_PARAMETERS: AtomicUsize = AtomicUsize::new(0);
static SPOOFED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

type GetParametersFn = unsafe extern "C" fn(*mut *mut c_void) -> u32;

/// Report the half of the game's DLSS the driver no longer has as available,
/// when this mod can supply the whole.
///
/// The game wants two features: supersampling, and Image Signal Processing,
/// the sharpening half of DLSS 1, which current drivers do not carry. Without
/// the second the game hides its DLSS option and with it the whole path that
/// renders the scene small and brings it up to the window. That path is the
/// one thing in the engine this mod actually wants: it already knows to leave
/// the UI at full size.
///
/// So the keys the game's check reads are overwritten on the way back -- but
/// only when DLSS is ready (`gate::dlss_ready`: this build, NGX initialised
/// by this mod, entry points bound, the driver's own answer yes) and the
/// block being returned itself says supersampling is available. Done
/// unconditionally, a machine on which this mod's DLSS cannot run would be
/// told to render small for a resolve that never comes.
///
/// # Safety
/// Only to be called by the game in place of
/// `NVSDK_NGX_D3D12_GetParameters`; `out` is the game's own out pointer.
pub unsafe extern "C" fn observe_game_get_parameters(out: *mut *mut c_void) -> u32 {
    let entry = GAME_GET_PARAMETERS.load(Ordering::Relaxed);
    // A module that answers for NGX and cannot be used is not called here
    // either, and its parameter object is not written through.
    if entry == 0 || crate::ngxhost::unusable() {
        return 0xBAD0_0001;
    }
    let original: GetParametersFn = std::mem::transmute(entry);
    let result = original(out);
    if result != SUCCESS || out.is_null() || (*out).is_null() {
        return result;
    }
    // Only for the build whose DLSS path has been taken over. What is said
    // below makes the game build its DLSS object and enqueue a pass that
    // only this mod's stand-ins can serve; said to a game whose path was not
    // taken, it is an access violation in the engine.
    if !crate::gate::ngx_is_ours() {
        return result;
    }
    // Safe because the feature creation is ours, which the check above has
    // just established. Reporting DLSS available makes the game build its
    // DLSS object, and the object alone is enough to unlock the supersampling
    // enqueue -- while the features behind it would still fail to create, so
    // the target that enqueue binds would never be allocated.
    let parameters = *out;
    let available = get_int(parameters, "SuperSampling.Available", GET_I32);
    let first = !SPOOFED.swap(true, std::sync::atomic::Ordering::Relaxed);
    if first {
        crate::log!(
            "[ngx] the game asked {} about DLSS and was told: available={} \
             needsDriver={} initResult={}",
            crate::ngxhost::answerer(),
            shown(available),
            shown(get_int(
                parameters,
                "SuperSampling.NeedsUpdatedDriver",
                GET_I32
            )),
            shown_status(get_int(
                parameters,
                "SuperSampling.FeatureInitResult",
                GET_I32
            ))
        );
        crate::log!(
            "[ngx]   and about image signal processing: available={} initResult={}",
            shown(get_int(
                parameters,
                "ImageSignalProcessing.Available",
                GET_I32
            )),
            shown_status(get_int(
                parameters,
                "ImageSignalProcessing.FeatureInitResult",
                GET_I32
            ))
        );
    }
    if !crate::gate::dlss_ready() {
        if first {
            crate::log!(
                "[ngx] leaving that answer as {} gave it: DLSS is not in use in this \
                 session, for the reason on the [dls] or [bld] line above",
                crate::ngxhost::answerer()
            );
        }
        return result;
    }
    if available != Some(1) {
        crate::gate::declare_unusable(&format!(
            "{} told the game SuperSampling.Available = {}, so the game is not told \
             otherwise",
            crate::ngxhost::answerer(),
            shown(available)
        ));
        return result;
    }
    set_int(parameters, "SuperSampling.Available", SET_I32, 1);
    // The check that actually fails. The game wants two features, and the
    // second is Image Signal Processing -- the sharpening half of DLSS 1,
    // which current drivers do not carry. Supersampling reports available
    // and the option disappears anyway, one line further down, so both keys
    // have to be answered.
    set_int(parameters, "ImageSignalProcessing.Available", SET_I32, 1);
    set_int(
        parameters,
        "ImageSignalProcessing.NeedsUpdatedDriver",
        SET_I32,
        0,
    );
    set_int(
        parameters,
        "ImageSignalProcessing.FeatureInitResult",
        SET_I32,
        SUCCESS as i32,
    );
    set_int(parameters, "SuperSampling.NeedsUpdatedDriver", SET_I32, 0);
    set_int(
        parameters,
        "SuperSampling.FeatureInitResult",
        SET_I32,
        SUCCESS as i32,
    );
    set_int(
        parameters,
        "SuperSampling.MinDriverVersionMajor",
        SET_U32,
        0,
    );
    set_int(
        parameters,
        "SuperSampling.MinDriverVersionMinor",
        SET_U32,
        0,
    );
    if first {
        crate::log!(
            "[ngx] supersampling is available by the answer {} gave; the availability keys \
             for it and for image signal processing, which this mod stands in for, were then \
             written on the object returned to the game",
            crate::ngxhost::answerer()
        );
    }
    result
}

/// A parameter as the log shows it: the number, or that it was not set.
fn shown(value: Option<i32>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unset".to_string())
}

/// An NGX status stored in a signed parameter, as hexadecimal and by name.
fn shown_status(value: Option<i32>) -> String {
    match value {
        Some(code) => format!(
            "{:#010x}{}",
            code as u32,
            status(code as u32).replace("  ", " ")
        ),
        None => "unset".to_string(),
    }
}

/// How long NGX's initialisation is given in a session in which Streamline
/// raised an exception. It takes well under a second when it works.
const INIT_TIME_LIMIT: std::time::Duration = std::time::Duration::from_secs(15);

/// Run `call` on a thread of its own and wait for it at most `limit`.
///
/// `None` when it has not returned by then. The thread is left where it is:
/// a call that does not return cannot be taken back, only not waited for.
fn within<T: Send + 'static>(
    limit: std::time::Duration,
    call: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(call());
    });
    receiver.recv_timeout(limit).ok()
}

/// The real `NVSDK_NGX_D3D12_ReleaseFeature`, as the game resolved it.
pub static GAME_RELEASE_FEATURE: AtomicUsize = AtomicUsize::new(0);

type ReleaseFeatureFn = unsafe extern "C" fn(usize) -> u32;

/// Keep the stand-in feature handles away from the driver.
///
/// The game releases its DLSS features whenever it rebuilds its render
/// targets, which happens on the first resolution change -- and the handles it
/// has are ours, not the driver's. Passing one on is an access violation
/// inside `_nvngx.dll`.
///
/// # Safety
/// Only to be called by the game in place of
/// `NVSDK_NGX_D3D12_ReleaseFeature`.
pub unsafe extern "C" fn observe_game_release_feature(handle: usize) -> u32 {
    if crate::hijack::is_ours(handle) {
        // Twice for every rebuild of the engine's targets, and something
        // that makes the engine rebuild repeatedly makes this repeat with it.
        static SAID: crate::logger::Throttle = crate::logger::Throttle::new(12, 4, 60);
        crate::log_limited!(
            SAID,
            "[ngx] the game released one of our stand-in features ({handle:#x})"
        );
        return SUCCESS;
    }
    let entry = GAME_RELEASE_FEATURE.load(Ordering::Relaxed);
    if entry == 0 {
        return SUCCESS;
    }
    let original: ReleaseFeatureFn = std::mem::transmute(entry);
    original(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_that_does_not_return_is_given_up_on() {
        let quick = std::time::Duration::from_millis(50);
        assert_eq!(within(std::time::Duration::from_secs(5), || 7u32), Some(7));
        assert_eq!(
            within(quick, || {
                std::thread::sleep(std::time::Duration::from_secs(2));
                7u32
            }),
            None
        );
    }

    #[test]
    fn dlss_is_refused_unless_the_driver_says_available() {
        let yes = Availability {
            available: Some(1),
            init_result: Some(1),
            needs_updated_driver: Some(0),
            min_driver: (Some(470), Some(0)),
        };
        assert_eq!(yes.refusal(), None);

        // As measured with no usable snippet on NGX's search path.
        let not_found = Availability {
            available: Some(0),
            init_result: Some(0xBAD0_0004u32 as i32),
            ..yes
        };
        let why = not_found.refusal().unwrap();
        assert!(why.contains("SuperSampling.Available = 0"), "{why}");
        assert!(why.contains("0xbad00004 FeatureNotFound"), "{why}");

        let old_driver = Availability {
            available: Some(0),
            init_result: Some(0xBAD0_000Cu32 as i32),
            needs_updated_driver: Some(1),
            min_driver: (Some(565), Some(90)),
        };
        let why = old_driver.refusal().unwrap();
        assert!(why.contains("OutOfDate"), "{why}");
        assert!(why.contains("at least 565.90"), "{why}");

        // No answer at all is not a yes.
        let silent = Availability {
            available: None,
            init_result: None,
            needs_updated_driver: None,
            min_driver: (None, None),
        };
        assert!(silent.refusal().unwrap().contains("Available = unset"));
    }
}
