//! The executable's imports the mod owns: `KERNEL32!GetProcAddress`,
//! `LoadLibraryExW`/`LoadLibraryExA`, and the delay-import slot of
//! `d3d12.dll` ordinal #101.
//!
//! * `GetProcAddress`: the game resolves its NGX entry points (its static
//!   NGX library, inside its `Init` wrapper) and `CreateDXGIFactory2` by
//!   name; the delay-load helper resolves `D3D12CreateDevice` by **ordinal
//!   #101** through the same IAT entry (`ReLoadInitDx11.md` §2.2). Each
//!   answer goes to its owner (`ngx::session`, `render`), which may hand
//!   back a replacement. A renderer lookup also means the executable is
//!   unpacked and running: the gate is asked right then, so the detours are
//!   in before the device exists.
//! * `LoadLibraryEx*`: the game loads `d3d12.dll` (`LOAD_LIBRARY_SEARCH_SYSTEM32`)
//!   on the line before it creates its first factory. Libraries the
//!   launcher lists as `renderer` (ReShade) are loaded here, on the game's
//!   thread, so their hooks precede the factory and the device every time;
//!   then the launcher's `after` entries are released
//!   (`mhw_dlss_launcher::handoff`).
//! * The delay slot is also patched directly, so device creation is owned
//!   even if the helper never runs through `GetProcAddress`.
//!
//! The KERNEL32 slots are written from `DllMain` ([`install_imports`]):
//! plain table writes, legal under the loader lock, done before the game's
//! own code can run (0.2 did the same). Until the boot thread has wired
//! the mod, a renderer lookup or the `d3d12.dll` load waits for it
//! ([`boot::wait_active`], bounded); with the mod disabled the hooks pass
//! everything through and still load the `renderer` libraries.

use std::ffi::{c_void, CStr};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use mhw_dlss_launcher::handoff::{parse_renderer_list, AT_RENDERER_VARIABLE, RENDERER_READY_EVENT};
use mhw_render::D3D12_CREATE_DEVICE_ORDINAL;
use mhw_win::{patch_iat, IatError, ImportRef, Module};
use parking_lot::Mutex;
use windows::core::{PCSTR, PCWSTR};
use windows::Win32::Foundation::{FARPROC, HANDLE, HMODULE};
use windows::Win32::System::LibraryLoader::LoadLibraryW;

use super::{boot, gate, runtime};

type GetProcAddressFn = unsafe extern "system" fn(HMODULE, PCSTR) -> FARPROC;
type LoadLibraryExWFn = unsafe extern "system" fn(PCWSTR, HANDLE, u32) -> HMODULE;
type LoadLibraryExAFn = unsafe extern "system" fn(PCSTR, HANDLE, u32) -> HMODULE;

static GET_PROC_ADDRESS: AtomicUsize = AtomicUsize::new(0);
static LOAD_LIBRARY_EX_W: AtomicUsize = AtomicUsize::new(0);
static LOAD_LIBRARY_EX_A: AtomicUsize = AtomicUsize::new(0);
static CREATE_DEVICE: AtomicUsize = AtomicUsize::new(0);

/// More `renderer` libraries than this are not loaded.
const MAX_AT_RENDERER: usize = 4;
/// How long the `renderer` loads wait for Streamline's initialisation
/// (they rewrite functions in dxgi/d3d12 that `slInit` calls).
const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

const IDLE: u8 = 0;
const LOADING: u8 = 1;
const DONE: u8 = 2;
static AT_RENDERER: AtomicU8 = AtomicU8::new(IDLE);

/// Per import, the slots patched or the error; or why the executable was
/// unreadable.
type ImportsOutcome = Result<Vec<(&'static str, Result<usize, IatError>)>, String>;

/// What [`install_imports`] did, for the log once there is one.
static INSTALLED: Mutex<Option<ImportsOutcome>> = Mutex::new(None);

/// Patch the executable's KERNEL32 imports. Called from `DllMain`: only
/// table reads and pointer writes (no library is loaded, nothing logs);
/// each original is stored before its slot changes. [`report_imports`]
/// logs the outcome later.
pub(crate) fn install_imports() {
    let outcome = match Module::main() {
        Ok(exe) => {
            let targets: [(&'static str, *const c_void, &AtomicUsize); 3] = [
                (
                    "GetProcAddress",
                    get_proc_address as *const c_void,
                    &GET_PROC_ADDRESS,
                ),
                (
                    "LoadLibraryExW",
                    load_library_ex_w as *const c_void,
                    &LOAD_LIBRARY_EX_W,
                ),
                (
                    "LoadLibraryExA",
                    load_library_ex_a as *const c_void,
                    &LOAD_LIBRARY_EX_A,
                ),
            ];
            Ok(targets
                .into_iter()
                .map(|(name, hook, original)| {
                    // SAFETY: each hook has exactly the import's signature.
                    let patched = unsafe {
                        patch_iat(&exe, "KERNEL32.dll", ImportRef::Name(name), hook, original)
                    };
                    (name, patched.map(|slots| slots.len()))
                })
                .collect())
        }
        Err(error) => Err(error.to_string()),
    };
    *INSTALLED.lock() = Some(outcome);
}

/// Log what [`install_imports`] did (from the boot thread).
pub(crate) fn report_imports() {
    match INSTALLED.lock().as_ref() {
        Some(Ok(imports)) => {
            for (name, outcome) in imports {
                match outcome {
                    Ok(slots) => {
                        log::info!(target: "boot", "hooked KERNEL32!{name} ({slots} slots)")
                    }
                    Err(error) => {
                        log::error!(target: "boot", "KERNEL32!{name} not hooked: {error}")
                    }
                }
            }
        }
        Some(Err(error)) => {
            log::error!(target: "boot", "the game module is unreadable ({error}); no import hooks")
        }
        None => log::error!(target: "boot", "the import hooks were not installed at load"),
    }
}

/// Point the `d3d12.dll` #101 delay slot at render's `D3D12CreateDevice`.
/// The import tables may still be packed at boot; the gate calls this again
/// once the code is unpacked. Idempotent.
pub(crate) fn patch_create_device(exe: &Module) {
    let Some(replacement) = mhw_render::intercept_ordinal("d3d12.dll", D3D12_CREATE_DEVICE_ORDINAL)
    else {
        return;
    };
    // SAFETY: render's replacement has D3D12CreateDevice's signature.
    let patched = unsafe {
        patch_iat(
            exe,
            "d3d12.dll",
            ImportRef::Ordinal(D3D12_CREATE_DEVICE_ORDINAL),
            replacement as *const c_void,
            &CREATE_DEVICE,
        )
    };
    match patched {
        Ok(slots) if !slots.is_empty() => {
            mhw_render::set_original("D3D12CreateDevice", CREATE_DEVICE.load(Ordering::Acquire));
            log::info!(target: "boot", "d3d12.dll #101 delay slot points at the mod's D3D12CreateDevice");
        }
        Ok(_) => {}
        Err(error) => mhw_core::log_changed!(
            Debug,
            "boot",
            "delay slot",
            error.to_string(),
            "d3d12.dll #101 delay slot not patched ({error}); GetProcAddress handles it"
        ),
    }
}

/// Once a foreign module (OptiScaler) serves NGX and frame generation runs:
/// say so if the game's `nvngx_dlisp.dll` is beside it
/// (`mhw_fg::setup::dlisp_beside`), which ends frame generation at
/// `slSetD3DDevice`. Checked once.
fn warn_of_dlisp_beside_host() {
    static CHECKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !runtime::fg_started() {
        return;
    }
    let Some(host) = mhw_ngx::session::host() else {
        return;
    };
    if CHECKED.swap(true, Ordering::AcqRel) || host.kind != mhw_ngx::host::HostKind::Foreign {
        return;
    }
    if let Some(snippet) =
        mhw_fg::setup::dlisp_beside(std::path::Path::new(&host.path), |path| path.exists())
    {
        log::warn!(
            target: "fg",
            "{} serves NGX and has {} beside it: NGX may initialise that snippet for \
             Streamline, and the game's copy raises a breakpoint on current GPUs. If \
             slSetD3DDevice then fails with eErrorExceptionHandler, that is why and frame \
             generation is off for the session; keeping {} in the mhw-dlss folder instead of \
             the game folder avoids it",
            host.who(),
            snippet.display(),
            host.who()
        );
    }
}

/// Whether `module` is `d3d12.dll`.
fn is_d3d12(module: HMODULE) -> bool {
    mhw_win::module_path(module.0 as usize).is_some_and(|path| {
        path.rsplit(['\\', '/'])
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case("d3d12.dll"))
    })
}

/// A renderer or NGX symbol: the game's own code is running.
fn renderer_symbol(name: &str) -> bool {
    name.starts_with("NVSDK_NGX_D3D12")
        || name.starts_with("D3D12")
        || name.starts_with("CreateDXGIFactory")
}

unsafe extern "system" fn get_proc_address(module: HMODULE, name: PCSTR) -> FARPROC {
    // SAFETY: stored by `patch_iat` before the slot changed; the import's
    // signature.
    let original: GetProcAddressFn =
        unsafe { std::mem::transmute(GET_PROC_ADDRESS.load(Ordering::Acquire)) };
    // SAFETY: the game's own arguments, forwarded.
    let result = unsafe { original(module, name) };
    let Some(found) = result else {
        return result;
    };
    let address = found as usize;
    // A value of 0xFFFF or less is an ordinal, not a name.
    if (name.0 as usize) <= 0xFFFF {
        let ordinal = name.0 as usize as u16;
        if ordinal == D3D12_CREATE_DEVICE_ORDINAL && is_d3d12(module) {
            if !boot::wait_active() {
                return result;
            }
            mhw_render::set_original("D3D12CreateDevice", address);
            if let Some(replacement) = mhw_render::intercept_ordinal("d3d12.dll", ordinal) {
                log::info!(target: "boot", "d3d12.dll #101 resolved: the mod's D3D12CreateDevice");
                // SAFETY: a function address of the same signature.
                return unsafe { std::mem::transmute::<usize, FARPROC>(replacement) };
            }
        }
        return result;
    }
    // SAFETY: a name pointer above 0xFFFF is a NUL-terminated string.
    let Ok(symbol) = unsafe { CStr::from_ptr(name.0.cast()) }.to_str() else {
        return result;
    };
    if !renderer_symbol(symbol) {
        return result;
    }
    if !boot::wait_active() {
        // The mod is not active (disabled, or its boot did not finish in
        // time); the `renderer` libraries are the launcher's and still load
        // at their moment.
        if symbol == "CreateDXGIFactory2" {
            load_at_renderer("the game resolving CreateDXGIFactory2");
        }
        return result;
    }
    log::debug!(target: "boot", "GetProcAddress {symbol}");
    gate::nudge();
    if mhw_ngx::session::set_original(symbol, module.0 as usize, address) {
        warn_of_dlisp_beside_host();
        if let Some(replacement) = mhw_ngx::session::intercept(symbol) {
            // SAFETY: ngx's replacement has the export's signature.
            return unsafe { std::mem::transmute::<usize, FARPROC>(replacement) };
        }
    }
    if let Some(replacement) = mhw_render::intercept(symbol) {
        if symbol == "CreateDXGIFactory2" {
            load_at_renderer("the game resolving CreateDXGIFactory2");
        }
        mhw_render::set_original(symbol, address);
        // SAFETY: render's replacement has the export's signature.
        return unsafe { std::mem::transmute::<usize, FARPROC>(replacement) };
    }
    result
}

/// After a successful load of `name`.
fn loaded(name: &str, module: HMODULE) {
    log::trace!(target: "boot", "loaded {name}");
    mhw_ngx::session::note_core_load(name, module.0 as usize);
    let file = name.rsplit(['\\', '/']).next().unwrap_or(name);
    if file.eq_ignore_ascii_case("d3d12.dll") || file.eq_ignore_ascii_case("d3d12") {
        // Whether frame generation runs (the loads wait for its
        // initialisation) is known once the boot is done.
        boot::wait_active();
        load_at_renderer("the game's own load of d3d12.dll");
        signal_renderer_ready();
    }
}

unsafe extern "system" fn load_library_ex_w(path: PCWSTR, file: HANDLE, flags: u32) -> HMODULE {
    // SAFETY: stored before the slot changed; the import's signature.
    let original: LoadLibraryExWFn =
        unsafe { std::mem::transmute(LOAD_LIBRARY_EX_W.load(Ordering::Acquire)) };
    // SAFETY: forwarded unchanged.
    let module = unsafe { original(path, file, flags) };
    if !module.is_invalid() && !path.is_null() {
        // SAFETY: a NUL-terminated wide string from the game.
        if let Ok(name) = unsafe { path.to_string() } {
            loaded(&name, module);
        }
    }
    module
}

unsafe extern "system" fn load_library_ex_a(path: PCSTR, file: HANDLE, flags: u32) -> HMODULE {
    // SAFETY: stored before the slot changed; the import's signature.
    let original: LoadLibraryExAFn =
        unsafe { std::mem::transmute(LOAD_LIBRARY_EX_A.load(Ordering::Acquire)) };
    // SAFETY: forwarded unchanged.
    let module = unsafe { original(path, file, flags) };
    if !module.is_invalid() && !path.is_null() {
        // SAFETY: a NUL-terminated string from the game.
        if let Ok(name) = unsafe { path.to_string() } {
            loaded(&name, module);
        }
    }
    module
}

/// Tell the launcher the renderer is loaded (its `after` entries wait for
/// this). Started without the launcher, nobody listens.
fn signal_renderer_ready() {
    static SIGNALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if SIGNALLED.swap(true, Ordering::AcqRel) {
        return;
    }
    if boot::signal_event(RENDERER_READY_EVENT) {
        log::info!(target: "boot", "renderer loaded; launcher signalled");
    } else {
        log::info!(target: "boot", "renderer loaded; no launcher listening");
    }
}

/// Load what the launcher's chain file lists as `renderer`, once, now, on
/// the game's thread (between its `d3d12.dll` load and its first factory).
fn load_at_renderer(trigger: &str) {
    if AT_RENDERER
        .compare_exchange(IDLE, LOADING, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let Some(listed) = std::env::var_os(AT_RENDERER_VARIABLE) else {
        AT_RENDERER.store(DONE, Ordering::Release);
        return;
    };
    let paths = parse_renderer_list(&listed.to_string_lossy());
    if paths.len() > MAX_AT_RENDERER {
        log::warn!(
            target: "boot",
            "{} `renderer` libraries listed; only the first {MAX_AT_RENDERER} are loaded",
            paths.len()
        );
    }
    if runtime::fg_started() {
        let waiting = Instant::now();
        if !mhw_fg::wait_settled(SETTLE_TIMEOUT) {
            log::warn!(
                target: "boot",
                "Streamline had not finished initialising after {} ms; loading the renderer \
                 libraries anyway",
                waiting.elapsed().as_millis()
            );
        }
    }
    // ReShade loads only for executables it has a configuration for unless
    // told otherwise; being listed in the chain file is that permission.
    std::env::set_var("RESHADE_DISABLE_LOADING_CHECK", "1");
    for path in paths.iter().take(MAX_AT_RENDERER) {
        let wide: Vec<u16> = path
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain([0])
            .collect();
        let started = Instant::now();
        // SAFETY: a NUL-terminated path; not under the loader lock (called
        // from the game's code after its own load returned).
        match unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) } {
            Ok(module) => log::info!(
                target: "boot",
                "loaded {} at {trigger} (module {:#x}, {} ms)",
                path.display(),
                module.0 as usize,
                started.elapsed().as_millis()
            ),
            Err(error) => log::warn!(
                target: "boot",
                "{} not loaded at {trigger}: {error}; the game carries on without it",
                path.display()
            ),
        }
    }
    mhw_render::detect_reshade();
    AT_RENDERER.store(DONE, Ordering::Release);
}
