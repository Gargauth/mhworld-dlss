//! Hooks on the executable's import table: `GetProcAddress` and the four
//! `LoadLibrary` variants.
//!
//! They log each library the game asks for,
//! once, and every renderer or NGX symbol it resolves. Three of them also
//! act:
//!
//! * `GetProcAddress` hands the game this library's stand-ins for three NGX
//!   entry points (`ngx.rs`), for `D3D12CreateDevice`, and with `fg = 1` for
//!   `CreateDXGIFactory2` (`proxy.rs`);
//! * the `LoadLibraryEx` pair loads companions listed as `renderer` at the
//!   moment the game loads d3d12.dll, and signals the launcher;
//! * all four answer the game's bare-name request for `dinput8.dll` with
//!   System32's when the one in the game folder is an old copy of this mod.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::sync::atomic::{AtomicUsize, Ordering};
use windows::core::{PCSTR, PCWSTR};
use windows::Win32::Foundation::{FARPROC, HANDLE, HMODULE};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;

type GetProcAddressFn = unsafe extern "system" fn(HMODULE, PCSTR) -> FARPROC;
type LoadLibraryWFn = unsafe extern "system" fn(PCWSTR) -> HMODULE;
type LoadLibraryAFn = unsafe extern "system" fn(PCSTR) -> HMODULE;
type LoadLibraryExWFn = unsafe extern "system" fn(PCWSTR, HANDLE, u32) -> HMODULE;
type LoadLibraryExAFn = unsafe extern "system" fn(PCSTR, HANDLE, u32) -> HMODULE;

static O_GETPROC: AtomicUsize = AtomicUsize::new(0);
static O_LLW: AtomicUsize = AtomicUsize::new(0);
static O_LLA: AtomicUsize = AtomicUsize::new(0);
static O_LLEXW: AtomicUsize = AtomicUsize::new(0);
static O_LLEXA: AtomicUsize = AtomicUsize::new(0);

static MODULE_NAMES: Mutex<Option<HashMap<usize, String>>> = Mutex::new(None);

include!("event_name.rs");

static ANNOUNCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Tell the launcher the renderer's library is loaded.
unsafe fn announce_renderer(name: &str) {
    let interesting = name.to_ascii_lowercase();
    if !interesting.ends_with("d3d12.dll") {
        return;
    }
    if ANNOUNCED.swap(true, Ordering::Relaxed) {
        return;
    }
    let wide: Vec<u16> = RENDERER_READY_EVENT
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    match windows::Win32::System::Threading::OpenEventW(
        windows::Win32::System::Threading::EVENT_MODIFY_STATE,
        false,
        PCWSTR(wide.as_ptr()),
    ) {
        Ok(event) => {
            let _ = windows::Win32::System::Threading::SetEvent(event);
            let _ = windows::Win32::Foundation::CloseHandle(event);
            crate::log!("[load] renderer ready; signalled the launcher");
        }
        // Started without the launcher, or without the chain. Nothing to tell.
        Err(_) => crate::log!("[load] renderer ready; no launcher listening"),
    }
}

/// Symbols worth a log line. GetProcAddress runs thousands of times during
/// startup, so the filter keeps the log readable.
fn interesting_symbol(name: &str) -> bool {
    name.starts_with("NVSDK_NGX")
        || name.starts_with("D3D12")
        || name.starts_with("D3D11")
        || name.starts_with("CreateDXGIFactory")
        || name.starts_with("DXGI")
        || name.starts_with("NvAPI")
        || name.contains("DLSS")
}

fn module_name(module: HMODULE) -> String {
    let key = module.0 as usize;
    let mut guard = MODULE_NAMES.lock();
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(cached) = map.get(&key) {
        return cached.clone();
    }
    let mut buf = [0u16; 512];
    let name = unsafe {
        let len = GetModuleFileNameW(module, &mut buf) as usize;
        if len == 0 {
            format!("<hmodule {key:#x}>")
        } else {
            let full = String::from_utf16_lossy(&buf[..len]);
            full.rsplit('\u{5c}').next().unwrap_or(&full).to_string()
        }
    };
    map.insert(key, name.clone());
    name
}

unsafe extern "system" fn hook_get_proc_address(module: HMODULE, name: PCSTR) -> FARPROC {
    let original: GetProcAddressFn = std::mem::transmute(O_GETPROC.load(Ordering::Relaxed));
    let result = original(module, name);

    // A value of 0xFFFF or less is an ordinal, not a name pointer.
    if !name.is_null() && (name.0 as usize) > 0xFFFF {
        if let Ok(symbol) = CStr::from_ptr(name.0 as *const i8).to_str() {
            if interesting_symbol(symbol) {
                crate::log!(
                    "[sym] {:<24} {:<40} -> {}",
                    module_name(module),
                    symbol,
                    if result.is_some() { "ok" } else { "NULL" }
                );
                // The game looks its renderer and NGX up by name from its own
                // code, so one of these lookups means that code is running
                // and the executable is unpacked.
                crate::gate::note_game_running("the game looked up a renderer entry point");
            }

            // When the mod is injected before d3d12.dll is even loaded, this
            // is how the device gets intercepted: hand back our wrapper so the
            // render hooks are installed from inside the create call, with no
            // thread racing the renderer for position.
            // The game is a working NGX client, so the arguments it passes
            // through the public entry points are correct by construction:
            // observing them is how the calling convention of this old SDK
            // version is confirmed against the public headers.
            if symbol.starts_with("NVSDK_NGX_D3D11_")
                || symbol == "D3D11CreateDevice"
                || symbol == "D3D11CreateDeviceAndSwapChain"
            {
                crate::gate::note_directx11_lookup(symbol);
            }

            if symbol == "NVSDK_NGX_D3D12_Init" {
                if let Some(entry) = result {
                    // Whoever answered this resolve serves NGX for the process.
                    crate::ngxhost::decide(module);
                    crate::ngx::GAME_INIT.store(entry as usize, Ordering::Relaxed);
                    crate::log!("[sym] standing in for the game's NVSDK_NGX_D3D12_Init");
                    return Some(std::mem::transmute(
                        crate::ngx::observe_game_init as *const c_void,
                    ));
                }
            }

            if symbol == "NVSDK_NGX_D3D12_GetParameters" {
                if let Some(entry) = result {
                    crate::ngx::GAME_GET_PARAMETERS.store(entry as usize, Ordering::Relaxed);
                    crate::log!("[sym] standing in for the game's NVSDK_NGX_D3D12_GetParameters");
                    return Some(std::mem::transmute(
                        crate::ngx::observe_game_get_parameters as *const c_void,
                    ));
                }
            }

            if symbol == "NVSDK_NGX_D3D12_ReleaseFeature" {
                if let Some(entry) = result {
                    crate::ngx::GAME_RELEASE_FEATURE.store(entry as usize, Ordering::Relaxed);
                    crate::log!("[sym] standing in for the game's NVSDK_NGX_D3D12_ReleaseFeature");
                    return Some(std::mem::transmute(
                        crate::ngx::observe_game_release_feature as *const c_void,
                    ));
                }
            }

            if symbol == "D3D12CreateDevice" && result.is_some() {
                crate::log!("[sym] substituting our D3D12CreateDevice");
                return Some(std::mem::transmute(
                    crate::proxy::D3D12CreateDevice as *const c_void,
                ));
            }

            // The factory, and only with frame generation on, so that a
            // default configuration never sees a substituted entry point.
            //
            // The creation call itself is taken, and not slot 15 of the
            // factory's vtable afterwards: a vtable patch installed after
            // the engine has created its swapchain leaves the chain DXGI's,
            // with nothing for Streamline to attach to. Owning the creation
            // call means owning the out pointer, which is what
            // `slUpgradeInterface` is specified to take.
            //
            // Before that decision, the second of the two moments at which a
            // companion listed as `renderer` is loaded -- the first is the
            // game's own load of d3d12.dll, below. Whichever comes first does
            // it and the other returns at once. Without such an entry this is
            // one compare-and-swap the first time and a load ever after.
            if symbol == "CreateDXGIFactory2" {
                crate::reshade::load_at_renderer("the game resolving CreateDXGIFactory2");
            }
            if symbol == "CreateDXGIFactory2" && result.is_some() && crate::config::get().fg {
                crate::log!("[sym] substituting our CreateDXGIFactory2");
                return Some(std::mem::transmute(
                    crate::proxy::CreateDXGIFactory2 as *const c_void,
                ));
            }
        }
    }
    result
}

/// Load what the chain file marked `renderer`, when the game loads d3d12.dll.
///
/// Before the launcher is signalled, so that anything it injects on that
/// signal arrives after what was loaded here. Entered after the game's own
/// `LoadLibraryEx` has returned, so not under the loader lock.
unsafe fn load_companions(name: &str) {
    if name.to_ascii_lowercase().ends_with("d3d12.dll") {
        crate::reshade::load_at_renderer("the game's own load of d3d12.dll");
    }
}

/// What is known about a `dinput8.dll` in the game folder.
///
/// Development builds of this library were installed as `dinput8.dll`. A
/// copy left in the game folder is not harmless: the game loads
/// `dinput8.dll` by bare name around its hundredth module, the game folder
/// comes first in the search, and it would get a second, older copy of this
/// mod with its own hooks on the same vtables.
///
/// Anybody else's `dinput8.dll` is left entirely alone. So the file is
/// looked at, once, off the loader lock, and only a copy that is
/// recognisably this mod's changes anything.
static BESIDE_US: AtomicUsize = AtomicUsize::new(NOT_LOOKED);
const NOT_LOOKED: usize = 0;
const NOTHING_OF_OURS: usize = 1;
const STALE_COPY_OF_OURS: usize = 2;

/// Look for an old copy of this library under its old name.
///
/// On a thread, because it reads a few megabytes and this is called from
/// `DllMain`. The game does not ask for `dinput8.dll` until long after its
/// renderer is up, and a request that did arrive before this finished is
/// simply left alone.
pub fn look_for_stale_copy(directory: std::path::PathBuf, loaded_as: String) {
    std::thread::spawn(move || {
        let candidate = directory.join("dinput8.dll");
        let state = if loaded_as.eq_ignore_ascii_case("dinput8.dll") {
            // Installed under that name. Then the game's request finds this
            // very module, which does not export DirectInput8Create.
            crate::log!(
                "[cmp] this library is loaded as dinput8.dll, the name it no longer \
                 uses and no longer serves: the game will ask it for \
                 DirectInput8Create and not find it. Install it as mhw-dlss\\mhw-dlss.dll"
            );
            NOTHING_OF_OURS
        } else if !candidate.exists() {
            NOTHING_OF_OURS
        } else if carries_our_marker(&candidate) {
            crate::log!(
                "[cmp] {} exists and IS an old build of this mod. The game's request \
                 for dinput8.dll will be answered with System32's instead, or it would \
                 load a second copy of the mod. Delete or rename that file",
                candidate.display()
            );
            STALE_COPY_OF_OURS
        } else {
            crate::log!(
                "[cmp] {} exists and is not ours; the game loads it at its usual point",
                candidate.display()
            );
            NOTHING_OF_OURS
        };
        BESIDE_US.store(state, Ordering::Relaxed);
    });
}

/// System32's `dinput8.dll`, if this request is the game's bare-name one and
/// what it would otherwise find is a stale copy of this mod.
///
/// One relaxed load in every other case. Loaded and returned here rather than
/// by rewriting the argument, so that all four variants behave alike.
unsafe fn instead_of_stale_copy(name: &str) -> Option<HMODULE> {
    if BESIDE_US.load(Ordering::Relaxed) != STALE_COPY_OF_OURS {
        return None;
    }
    // Bare name only. A request with a directory in it names a file, and is
    // none of this function's business.
    if !name.eq_ignore_ascii_case("dinput8.dll") && !name.eq_ignore_ascii_case("dinput8") {
        return None;
    }
    let mut buffer = [0u16; 260];
    let length =
        windows::Win32::System::SystemInformation::GetSystemDirectoryW(Some(&mut buffer)) as usize;
    let mut path: Vec<u16> = buffer[..length].to_vec();
    path.extend("\\dinput8.dll".encode_utf16());
    path.push(0);
    let module = windows::Win32::System::LibraryLoader::LoadLibraryW(PCWSTR(path.as_ptr()))
        .unwrap_or_default();
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !SAID.swap(true, Ordering::Relaxed) {
        crate::log!(
            "[cmp] the game asked for {name}; answered with System32's at {:#x} and not \
             the stale copy of this mod in the game folder",
            module.0 as usize
        );
    }
    Some(module)
}

/// The libraries the game has asked for so far, so each is written once.
///
/// The game asks for `kernel32.dll` by name again and again for as long as
/// it runs, so a line per call would be without bound and nearly all of them
/// would say nothing new. What these lines are for is the order of first
/// requests.
static LOADS: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);
/// How many different names are listed before the list is closed.
const LOADS_LISTED: usize = 400;

/// Log a library the first time it is asked for under this name.
fn note_load(function: &str, name: &str, flags: Option<u32>) {
    {
        let mut guard = LOADS.lock();
        let seen = guard.get_or_insert_with(std::collections::HashSet::new);
        if seen.contains(name) || seen.len() > LOADS_LISTED {
            return;
        }
        seen.insert(name.to_string());
        if seen.len() > LOADS_LISTED {
            drop(guard);
            crate::log!(
                "[load] {LOADS_LISTED} different libraries have been listed; later ones are \
                 not written"
            );
            return;
        }
    }
    match flags {
        Some(flags) => crate::log!("[load] {function:<14} {name} (flags {flags:#x})"),
        None => crate::log!("[load] {function:<14} {name}"),
    }
}

unsafe extern "system" fn hook_load_library_w(path: PCWSTR) -> HMODULE {
    let original: LoadLibraryWFn = std::mem::transmute(O_LLW.load(Ordering::Relaxed));
    let name = wide(path);
    if let Some(real) = instead_of_stale_copy(&name) {
        return real;
    }
    let result = original(path);
    note_load("LoadLibraryW", &name, None);
    crate::ngxhost::note_load(&name, result);
    result
}

unsafe extern "system" fn hook_load_library_a(path: PCSTR) -> HMODULE {
    let original: LoadLibraryAFn = std::mem::transmute(O_LLA.load(Ordering::Relaxed));
    let name = narrow(path);
    if let Some(real) = instead_of_stale_copy(&name) {
        return real;
    }
    let result = original(path);
    note_load("LoadLibraryA", &name, None);
    crate::ngxhost::note_load(&name, result);
    result
}

unsafe extern "system" fn hook_load_library_ex_w(
    path: PCWSTR,
    file: HANDLE,
    flags: u32,
) -> HMODULE {
    let original: LoadLibraryExWFn = std::mem::transmute(O_LLEXW.load(Ordering::Relaxed));
    if let Some(real) = instead_of_stale_copy(&wide(path)) {
        return real;
    }
    let result = original(path, file, flags);
    let name = wide(path);
    note_load("LoadLibraryExW", &name, Some(flags));
    crate::ngxhost::note_load(&name, result);
    load_companions(&name);
    announce_renderer(&name);
    result
}

unsafe extern "system" fn hook_load_library_ex_a(path: PCSTR, file: HANDLE, flags: u32) -> HMODULE {
    let original: LoadLibraryExAFn = std::mem::transmute(O_LLEXA.load(Ordering::Relaxed));
    if let Some(real) = instead_of_stale_copy(&narrow(path)) {
        return real;
    }
    let result = original(path, file, flags);
    let name = narrow(path);
    note_load("LoadLibraryExA", &name, Some(flags));
    crate::ngxhost::note_load(&name, result);
    load_companions(&name);
    announce_renderer(&name);
    result
}

unsafe fn wide(p: PCWSTR) -> String {
    if p.is_null() {
        "<null>".into()
    } else {
        p.to_string().unwrap_or_default()
    }
}

unsafe fn narrow(p: PCSTR) -> String {
    if p.is_null() {
        "<null>".into()
    } else {
        p.to_string().unwrap_or_default()
    }
}

/// Patch the five import-table entries of the host executable.
///
/// # Safety
/// `exe` must be the base of the process image, and this must run before
/// the game's own code does: the entries are written without stopping other
/// threads, each original stored before its entry changes.
pub unsafe fn install(exe: HMODULE) {
    let targets: [(&str, *const std::ffi::c_void, &AtomicUsize); 5] = [
        (
            "GetProcAddress",
            hook_get_proc_address as *const _,
            &O_GETPROC,
        ),
        ("LoadLibraryW", hook_load_library_w as *const _, &O_LLW),
        ("LoadLibraryA", hook_load_library_a as *const _, &O_LLA),
        (
            "LoadLibraryExW",
            hook_load_library_ex_w as *const _,
            &O_LLEXW,
        ),
        (
            "LoadLibraryExA",
            hook_load_library_ex_a as *const _,
            &O_LLEXA,
        ),
    ];

    for (name, replacement, slot) in targets {
        match crate::iat::patch(exe, "KERNEL32.dll", name, replacement, slot) {
            Some(original) => {
                crate::log!("[iat] hooked KERNEL32!{name} (orig {:p})", original);
            }
            None => crate::log!("[iat] FAILED to hook KERNEL32!{name}"),
        }
    }
}
