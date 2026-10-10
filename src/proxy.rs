//! The `d3d12.dll` export set, forwarded to the system's library.
//!
//! The mod is not loaded as a proxy library in this game. Load order rules
//! every candidate out, established by experiment:
//!
//! | library       | loaded from | why |
//! | ------------- | ----------- | --- |
//! | `dinput8.dll` | game folder | the game asks for it with a plain LoadLibrary -- but only at module ~101 |
//! | `winmm.dll`   | System32    | the Steam overlay pulls System32's copy in first |
//! | `version.dll` | System32    | loaded by Steam/DRM with a restricted search path |
//! | `d3d12.dll`   | System32    | the game itself requests it with LOAD_LIBRARY_SEARCH_SYSTEM32 |
//!
//! So the only replaceable library loads 47 modules after the renderer, by
//! which point the main render targets' views exist and the constant buffer
//! upload heap is already mapped. That is what the launcher is for: it
//! creates the process suspended and injects before the first instruction.
//!
//! What this file is used for is two creation calls. `hooks.rs` answers the
//! game's `GetProcAddress` for `D3D12CreateDevice` and `CreateDXGIFactory2`
//! with the functions here, so the mod is inside both calls and owns their
//! out pointers. The rest of the d3d12 export set is forwarded unchanged; it
//! keeps this library usable as a plain `d3d12.dll` proxy in a host that
//! loads one from its own folder, which this game does not.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::core::Interface;
use windows::core::{GUID, HRESULT, PCSTR, PCWSTR};
use windows::Win32::Foundation::{FARPROC, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;

static MODULES: Mutex<Option<HashMap<&'static str, usize>>> = Mutex::new(None);
static INSTALLED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// True while this thread is inside one of our own forwarders.
    ///
    /// These exports are reachable from three directions: the game, which is
    /// the point; this library's own bootstrap, which creates a device and a
    /// factory of its own; and Streamline, whose plugin startup enumerates
    /// adapters. Only the first should ever wait for Streamline to initialise.
    /// The second is what cost a run -- our bootstrap blocked on `slInit` for
    /// 1.3 seconds and the render hooks landed after the engine had built its
    /// pipeline -- and the third would be worse, because a DXGI call from
    /// inside `slInit` arriving at an export that waits for `slInit` is a
    /// deadlock rather than a delay.
    static FORWARDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Hold for the duration of a forwarder, so nested calls pass straight through.
struct Forwarding;

impl Forwarding {
    fn enter() -> Option<Forwarding> {
        if FORWARDING.with(|flag| flag.get()) {
            return None;
        }
        FORWARDING.with(|flag| flag.set(true));
        Some(Forwarding)
    }
}

impl Drop for Forwarding {
    fn drop(&mut self) {
        FORWARDING.with(|flag| flag.set(false));
    }
}

/// How long a creation call will wait for Streamline to finish initialising.
///
/// Generous, because it is paid at most twice in a session and the alternative
/// is losing the swapchain; bounded, because a Streamline that never settles
/// should cost a slow start and not a hung game.
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How many devices the game has asked for.
static DEVICES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// How many factories the game has asked this library's `CreateDXGIFactory2`
/// for. That export is substituted only with `fg = 1`, so with it off this
/// stays at zero whatever the game does.
static FACTORIES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Calls that have reached this library's two creation exports so far.
pub fn created_so_far() -> (u32, u32) {
    (
        FACTORIES.load(Ordering::Relaxed),
        DEVICES.load(Ordering::Relaxed),
    )
}

/// Load a System32 library by absolute path.
///
/// The absolute path matters twice over: it rules out loading ourselves when
/// our file carries the same name, and it ignores the search-path rules that
/// made every proxy attempt here so fragile. Never called from DllMain --
/// LoadLibrary under the loader lock is how proxies deadlock.
fn system_module(file: &'static str) -> HMODULE {
    let mut guard = MODULES.lock();
    let cache = guard.get_or_insert_with(HashMap::new);
    if let Some(handle) = cache.get(file) {
        return HMODULE(*handle as *mut c_void);
    }

    let handle = unsafe {
        let mut buffer = [0u16; 260];
        let length = GetSystemDirectoryW(Some(&mut buffer)) as usize;
        let mut path: Vec<u16> = buffer[..length].to_vec();
        path.push(b'\\' as u16);
        path.extend(file.encode_utf16());
        path.push(0);
        let module = LoadLibraryW(PCWSTR(path.as_ptr())).unwrap_or_default();
        crate::log!("[proxy] real {file} = {:?}", module.0);
        module
    };
    cache.insert(file, handle.0 as usize);
    handle
}

pub(crate) unsafe fn entry(file: &'static str, name: &[u8]) -> FARPROC {
    GetProcAddress(system_module(file), PCSTR(name.as_ptr()))
}

/// Forward an export by calling through to the real implementation.
///
/// Signatures are reproduced exactly rather than type-erased, so every
/// argument lands in the register or stack slot the real function reads.
macro_rules! forward {
    ($module:expr, $name:ident ( $($arg:ident : $ty:ty),* ) -> $ret:ty, $symbol:expr) => {
        /// Forwarded unchanged to the system library's export of this name.
        ///
        /// # Safety
        /// The same contract as the function it forwards to.
        #[no_mangle]
        pub unsafe extern "system" fn $name($($arg: $ty),*) -> $ret {
            type Signature = unsafe extern "system" fn($($ty),*) -> $ret;
            match entry($module, $symbol) {
                Some(address) => {
                    let target: Signature = std::mem::transmute(address);
                    target($($arg),*)
                }
                None => {
                    // Said once per export: a caller that polls one of these
                    // would otherwise write a line per call.
                    static SAID: AtomicBool = AtomicBool::new(false);
                    if !SAID.swap(true, Ordering::Relaxed) {
                        crate::log!(
                            "[proxy] {} has no {}; the call is answered with a failure",
                            $module,
                            String::from_utf8_lossy(&$symbol[..$symbol.len() - 1])
                        );
                    }
                    <$ret as Missing>::missing()
                }
            }
        }
    };
}

/// What a forwarder answers when the real library has no such export.
///
/// This was `Default::default()`, which for an `HRESULT` is `S_OK`: a caller
/// asking a d3d12.dll that lacks, say, `D3D12GetInterface` was told the call
/// had succeeded and left to use an out pointer nobody had written.
trait Missing {
    fn missing() -> Self;
}

impl Missing for HRESULT {
    fn missing() -> Self {
        PROC_NOT_FOUND
    }
}

impl Missing for usize {
    fn missing() -> Self {
        0
    }
}

impl Missing for u64 {
    fn missing() -> Self {
        0
    }
}

impl Missing for *mut c_void {
    fn missing() -> Self {
        std::ptr::null_mut()
    }
}

/// `HRESULT_FROM_WIN32(ERROR_PROC_NOT_FOUND)`.
const PROC_NOT_FOUND: HRESULT = HRESULT(0x8007_007Fu32 as i32);

// ---------------------------------------------------------------------------
// d3d12.dll
// ---------------------------------------------------------------------------

/// The one export that is not a pure forward.
///
/// The real call runs first, then the render hooks are installed from the
/// device it produced -- synchronously, before the game gets the pointer back
/// and starts creating resources and views with it. Also used as the
/// replacement returned from the `GetProcAddress` hook when the mod was
/// injected before d3d12.dll was even loaded.
///
/// # Safety
/// The contract of `D3D12CreateDevice`: `out`, when not null, must be valid
/// for a pointer write.
#[no_mangle]
pub unsafe extern "system" fn D3D12CreateDevice(
    adapter: *mut c_void,
    minimum_feature_level: i32,
    riid: *const GUID,
    device: *mut *mut c_void,
) -> HRESULT {
    type Signature =
        unsafe extern "system" fn(*mut c_void, i32, *const GUID, *mut *mut c_void) -> HRESULT;

    // Before the device, and before anything it builds. The game's DLSS path
    // expects its features to exist by the time the render nodes are
    // configured: one of them allocates the dense motion target only if they
    // do, and another binds that target without checking. Installing at the
    // first submission was far too late, and the null bind was an access
    // violation a few hundred frames in. `lib.rs` has normally started this
    // already; it does nothing the second time. The mip bias, which also has
    // to be in before the device, is written by the same step.
    crate::gate::start();

    let Some(address) = entry("d3d12.dll", b"D3D12CreateDevice\0") else {
        crate::log!("[proxy] D3D12CreateDevice missing from the real module");
        return HRESULT(-2147467259); // E_FAIL
    };
    // Breadcrumbs have to be switched on before the device exists, which is
    // the whole reason this forwarder is not a plain pass-through.
    if crate::config::get().dred {
        crate::dred::enable();
    }
    // Also before the device: validation cannot be retrofitted onto one that
    // already exists.
    if crate::config::get().debug_layer {
        crate::validate::enable();
    }

    let target: Signature = std::mem::transmute(address);
    let result = target(adapter, minimum_feature_level, riid, device);

    let produced = !device.is_null() && !(*device).is_null();
    // Numbered, and the pointer printed: this runs more than once in a
    // session (the mod's own bootstrap, then the engine), and which device
    // the render hooks were installed from and which one Streamline is given
    // has to be readable from the log.
    let which = DEVICES.fetch_add(1, Ordering::Relaxed) + 1;
    // The thread, because whether a given `#N` is our own bootstrap's call or
    // the engine's is otherwise guessed at from the order of unrelated log
    // lines -- and it was guessed at, twice, in opposite directions.
    let thread = windows::Win32::System::Threading::GetCurrentThreadId();
    crate::log!(
        "[proxy] D3D12CreateDevice #{which}(level={minimum_feature_level:#x}) on thread \
         {thread} -> {:#010x}{}",
        result.0,
        if produced {
            format!("  device {:#x}", (*device) as usize)
        } else {
            "  (no device)".to_string()
        }
    );

    if result.is_ok() && produced {
        // Order matters here and the cost of getting it wrong is invisible.
        // Our render hooks are installed from this pointer, and they must be
        // installed against the *real* device: patching Streamline's proxy
        // vtable instead would hook Streamline's own calls and miss the
        // engine's. So the native device goes to `install_from_device` first,
        // and only then is the engine's copy replaced.
        //
        // Nor ReShade's. This forwarder calls System32's export by address,
        // and ReShade hooks that export's body, so with ReShade loaded what
        // comes back is ReShade's proxy device.
        crate::reshade::detect();
        static CREATED: crate::reshade::Boundary = crate::reshade::Boundary::new(
            "the device D3D12CreateDevice returned",
            "the render hooks and the record of native devices use the native one",
        );
        let unwrapped = CREATED.native(*device);
        if !INSTALLED.swap(true, Ordering::SeqCst) {
            crate::gpu::install_from_device(unwrapped);
        }
        // Outside that latch deliberately. The hooks can also be installed
        // from `bootstrap_scratch`, on a thread, through a device of our own --
        // and if that thread wins the race this block would be skipped
        // entirely and the engine would keep a device Streamline has never
        // seen. The upgrade has to happen on this call whoever installed the
        // hooks, because this is the only place the engine's own pointer can
        // be replaced.
        let native = unwrapped;
        crate::streamline::note_native_device(which, native);
        // Bound to a variable: `Forwarding::enter().is_none()` would create
        // the guard and release it at the end of the statement, before
        // anything could nest inside it.
        //
        // Streamline does not get its device here. The device seen by this
        // function may be the mod's own scratch device; the engine's is
        // named by the queue it presents with, which
        // `swap::adopt_engine_device` reads before the swapchain exists.
        // What stays here is the record of which devices were created.
        //
        // The native device, not the proxy: breadcrumbs and validation are
        // about the real one, and `*device` may by now be Streamline's.
        if let Some(handle) =
            windows::Win32::Graphics::Direct3D12::ID3D12Device::from_raw_borrowed(&native)
        {
            if crate::config::get().dred {
                crate::dred::watch(handle.clone());
            }
            if crate::config::get().debug_layer {
                crate::validate::watch(handle.clone());
            }
        }
    }
    result
}

/// The other creation call worth owning.
///
/// The game resolves `CreateDXGIFactory2` early and creates its swapchain
/// soon after, so a patch on slot 15 of the *native* factory's vtable can
/// arrive after the chain exists; the chain is then DXGI's and Streamline
/// has nothing to attach to. Owning the creation call and its out pointer
/// does not have that race, and `slUpgradeInterface` is specified for
/// exactly this: *"called IMMEDIATELY after base interface is created"*.
///
/// Substituted through the `GetProcAddress` hook in `hooks.rs`; the log
/// shows `[sym] dxgi.dll CreateDXGIFactory2 -> ok` when it was.
///
/// # Safety
/// The contract of `CreateDXGIFactory2`: `out` must be valid for a pointer
/// write.
#[no_mangle]
pub unsafe extern "system" fn CreateDXGIFactory2(
    flags: u32,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    type Signature = unsafe extern "system" fn(u32, *const GUID, *mut *mut c_void) -> HRESULT;
    let Some(address) = entry("dxgi.dll", b"CreateDXGIFactory2\0") else {
        crate::log!("[proxy] CreateDXGIFactory2 missing from the real module");
        return HRESULT(-2147467259); // E_FAIL
    };
    let target: Signature = std::mem::transmute(address);
    let result = target(flags, riid, out);
    let produced = !out.is_null() && !(*out).is_null();
    crate::log!(
        "[proxy] CreateDXGIFactory2(flags={flags:#x}) -> {:#010x}{}",
        result.0,
        if produced { "" } else { "  (no factory)" }
    );
    if !result.is_ok() || !produced {
        return result;
    }
    // Only with frame generation on. With it off this is a pass-through and
    // the engine receives DXGI's own factory, untouched.
    if !crate::config::get().fg {
        return result;
    }
    // And only for the game's own call. A nested one is this library's
    // bootstrap or Streamline's adapter enumeration, and neither should wait
    // on anything.
    let Some(_forwarding) = Forwarding::enter() else {
        return result;
    };
    FACTORIES.fetch_add(1, Ordering::Relaxed);
    // Whose factory the engine was just handed. The slot below is patched on
    // *this* object's vtable and deliberately not on what it may wrap: when it
    // is ReShade's proxy, a hook on it runs before ReShade, which is what puts
    // Streamline above ReShade and leaves ReShade presenting every frame.
    crate::reshade::detect();
    crate::reshade::note_engine_factory(*out);
    // The hook, always, because it is what makes the creation observable and
    // it has to be in place before the engine builds its swapchain. The
    // *upgrade* is not done here: this game creates its factories before its
    // device -- three of them -- and Streamline refuses to upgrade anything
    // until the device has been set. So the decision is deferred to the
    // first `CreateSwapChainForHwnd`, by which time the device exists and it
    // is known whether DLSS-G initialised. See
    // `swap::hook_create_swapchain_for_hwnd`.
    crate::streamline::wait_until_ready(READY_TIMEOUT);
    crate::swap::watch_factory(*out);
    result
}

forward!("d3d12.dll", D3D12CoreCreateLayeredDevice(a: *mut c_void, b: u32, c: *mut c_void, d: *const GUID, e: *mut *mut c_void) -> HRESULT, b"D3D12CoreCreateLayeredDevice\0");
forward!("d3d12.dll", D3D12CoreGetLayeredDeviceSize(a: *mut c_void, b: u32) -> usize, b"D3D12CoreGetLayeredDeviceSize\0");
forward!("d3d12.dll", D3D12CoreRegisterLayers(a: *mut c_void, b: u32) -> HRESULT, b"D3D12CoreRegisterLayers\0");
forward!("d3d12.dll", D3D12CreateRootSignatureDeserializer(blob: *const c_void, size: usize, riid: *const GUID, out: *mut *mut c_void) -> HRESULT, b"D3D12CreateRootSignatureDeserializer\0");
forward!("d3d12.dll", D3D12CreateVersionedRootSignatureDeserializer(blob: *const c_void, size: usize, riid: *const GUID, out: *mut *mut c_void) -> HRESULT, b"D3D12CreateVersionedRootSignatureDeserializer\0");
forward!("d3d12.dll", D3D12DeviceRemovedExtendedData(a: *mut c_void, b: *mut c_void) -> HRESULT, b"D3D12DeviceRemovedExtendedData\0");
forward!("d3d12.dll", D3D12EnableExperimentalFeatures(count: u32, iids: *const GUID, structs: *const c_void, sizes: *const u32) -> HRESULT, b"D3D12EnableExperimentalFeatures\0");
forward!("d3d12.dll", D3D12GetDebugInterface(riid: *const GUID, out: *mut *mut c_void) -> HRESULT, b"D3D12GetDebugInterface\0");
forward!("d3d12.dll", D3D12GetInterface(clsid: *const GUID, riid: *const GUID, out: *mut *mut c_void) -> HRESULT, b"D3D12GetInterface\0");
forward!("d3d12.dll", D3D12PIXEventsReplaceBlock(a: *mut c_void, b: bool) -> u64, b"D3D12PIXEventsReplaceBlock\0");
forward!("d3d12.dll", D3D12PIXGetThreadInfo(a: *mut c_void) -> *mut c_void, b"D3D12PIXGetThreadInfo\0");
forward!("d3d12.dll", D3D12PIXNotifyWakeFromFenceSignal(a: *mut c_void) -> HRESULT, b"D3D12PIXNotifyWakeFromFenceSignal\0");
forward!("d3d12.dll", D3D12PIXReportCounter(a: *mut c_void, b: *const u16, c: f32) -> HRESULT, b"D3D12PIXReportCounter\0");
forward!("d3d12.dll", D3D12SerializeRootSignature(desc: *const c_void, version: u32, blob: *mut *mut c_void, error: *mut *mut c_void) -> HRESULT, b"D3D12SerializeRootSignature\0");
forward!("d3d12.dll", D3D12SerializeVersionedRootSignature(desc: *const c_void, blob: *mut *mut c_void, error: *mut *mut c_void) -> HRESULT, b"D3D12SerializeVersionedRootSignature\0");
forward!("d3d12.dll", GetBehaviorValue(a: u32, b: *mut u32) -> HRESULT, b"GetBehaviorValue\0");
forward!("d3d12.dll", SetAppCompatStringPointer(a: usize, b: PCSTR) -> HRESULT, b"SetAppCompatStringPointer\0");

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing export is a failure to the caller, never `S_OK`.
    #[test]
    fn a_missing_export_is_reported_as_a_failure() {
        assert!(<HRESULT as Missing>::missing().is_err());
        assert_eq!(<usize as Missing>::missing(), 0);
        assert!(<*mut c_void as Missing>::missing().is_null());
    }
}
