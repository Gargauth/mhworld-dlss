//! Sharing the process with ReShade, and saying which stack resulted.
//!
//! ReShade does not patch D3D12's vtables. It hands the engine *proxy*
//! objects -- device, command queues, command lists, the DXGI factory and the
//! swapchain -- each forwarding to the native one. This library meets the
//! engine's objects in two ways, and with ReShade loaded they stop agreeing:
//!
//! * vtable hooks (`gpu.rs`, `swap.rs`) sit on the native implementation and
//!   are entered with **native** `this` pointers;
//! * engine detours (`hijack.rs`) and creation calls we own are handed
//!   whatever the engine holds, which is ReShade's **proxy**.
//!
//! So a command list recorded under one address is looked up under another,
//! the engine's graphics queue is never recognised, and NGX is given a list
//! that is not D3D12's. Every ReShade proxy answers `QueryInterface` for one
//! private IID with the object it wraps, and that is the cure, applied at the
//! boundaries and nowhere else.
//!
//! ## What is unwrapped and what deliberately is not
//!
//! Unwrapped: the command list given to NGX and to Streamline's tags, the
//! queue recorded as the engine's, the device our own shaders are built on,
//! the device the game passes to NGX's initialisation, and any device or
//! factory we read a vtable from.
//!
//! **Not** unwrapped: the device given to `slSetD3DDevice` and the factory
//! given to `slUpgradeInterface`. ReShade wraps a swapchain only when it is
//! created with one of ReShade's proxy queues, and Streamline creates the real
//! chain with a queue it makes from the device it was given. A native device
//! there means a native queue, and ReShade then drops out of the presentation
//! path altogether. That Streamline makes its queue from that device is an
//! inference about a closed plugin; `report_stack` is what shows whether it
//! held.
//!
//! ## Cost when ReShade is absent
//!
//! `present()` is one relaxed load, and every unwrap returns its argument
//! after it. Nothing here is called from a hook that runs per draw, per
//! descriptor copy or per constant buffer bind, and nothing may be added
//! there: those run tens of thousands of times a frame.

use parking_lot::Mutex;
use std::ffi::{c_void, CStr};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use windows::core::{Interface, GUID, HRESULT, PCSTR};
use windows::Win32::Foundation::{BOOL, HANDLE, HMODULE};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GetProcAddress, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::Threading::GetCurrentProcess;

/// `IID_UnwrappedObject`, from ReShade's `source/com_utils.hpp`. Present in
/// 6.8.0. Every proxy answers it with the object it wraps, with a reference
/// added.
pub const IID_UNWRAPPED: GUID = GUID::from_u128(0x7f2c9a11_3b4e_4d6a_812f_5e9cd37a1b42);

/// How many layers any walk here will follow before stopping.
///
/// One is expected. The bound is for the case nobody planned: a proxy that
/// answers with another proxy, or with itself.
const MAX_LAYERS: usize = 4;

/// How many modules a walk looks at. ReShade's own check uses the same cap.
const MAX_MODULES: usize = 1024;

static PRESENT: AtomicBool = AtomicBool::new(false);
static MODULE: AtomicUsize = AtomicUsize::new(0);
static VERSION: Mutex<String> = Mutex::new(String::new());

// The kernel32 variant, declared by hand: it is safe under the loader lock,
// and the `windows` crate keeps it behind a feature this crate does not
// otherwise need.
#[link(name = "kernel32")]
extern "system" {
    fn K32EnumProcessModules(
        process: HANDLE,
        modules: *mut HMODULE,
        bytes: u32,
        needed: *mut u32,
    ) -> BOOL;
}

type QueryFn = unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;

/// Whether a ReShade module has been seen in this process.
#[inline]
pub fn present() -> bool {
    PRESENT.load(Ordering::Relaxed)
}

/// ReShade's module handle, or zero.
pub fn module() -> usize {
    MODULE.load(Ordering::Relaxed)
}

/// Every module in the process, up to the cap.
unsafe fn modules() -> Vec<HMODULE> {
    let mut found = vec![HMODULE::default(); MAX_MODULES];
    let mut needed = 0u32;
    let bytes = (found.len() * std::mem::size_of::<HMODULE>()) as u32;
    if !K32EnumProcessModules(GetCurrentProcess(), found.as_mut_ptr(), bytes, &mut needed).as_bool()
    {
        return Vec::new();
    }
    found.truncate((needed as usize / std::mem::size_of::<HMODULE>()).min(MAX_MODULES));
    found
}

unsafe fn path_of(module: HMODULE) -> String {
    let mut buffer = [0u16; 512];
    let length = GetModuleFileNameW(module, &mut buffer) as usize;
    if length == 0 {
        return format!("<module {:#x}>", module.0 as usize);
    }
    String::from_utf16_lossy(&buffer[..length])
}

fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// Look for ReShade among the loaded modules, and remember it.
///
/// ReShade is recognised the way ReShade recognises itself: a module that
/// exports `ReShadeVersion`. Not by file name, which is the user's choice.
///
/// A module walk, so it is called only where one is affordable: after this
/// library loads ReShade itself, when the engine creates a factory, and when
/// it creates a swapchain. Never per frame.
///
/// # Safety
/// Reads the export of another module; that module must stay loaded, which
/// ReShade does for the life of the process.
pub unsafe fn detect() -> bool {
    for module in modules() {
        let Some(export) = GetProcAddress(module, PCSTR(b"ReShadeVersion\0".as_ptr())) else {
            continue;
        };
        if MODULE.swap(module.0 as usize, Ordering::Relaxed) == module.0 as usize {
            return true;
        }
        // The export is a `const char *` variable, so its address holds the
        // address of the text.
        let text = *(export as usize as *const *const i8);
        let version = if text.is_null() {
            "unknown".to_string()
        } else {
            CStr::from_ptr(text).to_string_lossy().into_owned()
        };
        crate::log!(
            "[cmp] ReShade is loaded: {}, ReShadeVersion \"{version}\", module {:#x}",
            path_of(module),
            module.0 as usize
        );
        *VERSION.lock() = version;
        PRESENT.store(true, Ordering::Relaxed);
        return true;
    }
    present()
}

/// One layer down, if this object is a ReShade proxy.
///
/// Balanced: ReShade adds a reference to what it returns and it is released
/// here at once. The proxy holds the native object for as long as the proxy
/// lives, so the pointer is good for as long as the argument was.
unsafe fn peel(pointer: *mut c_void) -> Option<*mut c_void> {
    if pointer.is_null() {
        return None;
    }
    let vtable = *(pointer as *const *const usize);
    let query: QueryFn = std::mem::transmute(*vtable);
    let mut inner: *mut c_void = std::ptr::null_mut();
    if query(pointer, &IID_UNWRAPPED, &mut inner).is_err() || inner.is_null() {
        return None;
    }
    let release: ReleaseFn = std::mem::transmute(*(*(inner as *const *const usize)).add(2));
    release(inner);
    Some(inner)
}

/// The native object behind a ReShade proxy, or the argument itself.
///
/// With ReShade absent this is one atomic load and a return.
///
/// **Never call this on something that may be Streamline's proxy.** Streamline
/// forwards an IID it does not know to the object it wraps, so unwrapping a
/// Streamline swapchain that sits on a ReShade one answers with DXGI's object
/// and silently skips two layers. `report_stack` walks layers by asking who
/// implements each one instead.
///
/// # Safety
/// `pointer` must be null or a live COM object.
pub unsafe fn unwrap(pointer: *mut c_void) -> (*mut c_void, bool) {
    if !present() || pointer.is_null() {
        return (pointer, false);
    }
    let mut current = pointer;
    let mut peeled = false;
    for _ in 0..MAX_LAYERS {
        match peel(current) {
            Some(inner) if inner != current => {
                current = inner;
                peeled = true;
            }
            _ => break,
        }
    }
    (current, peeled)
}

/// A boundary where proxies are unwrapped, and what it has said so far.
///
/// Each boundary reports once for each of the two things it can see while
/// ReShade is loaded: that it was handed a proxy, or that it was handed a
/// native object. The second matters as much as the first -- it is the case
/// where the stack is not the one that was planned.
pub struct Boundary {
    what: &'static str,
    then: &'static str,
    said: AtomicU8,
}

impl Boundary {
    pub const fn new(what: &'static str, then: &'static str) -> Boundary {
        Boundary {
            what,
            then,
            said: AtomicU8::new(0),
        }
    }

    /// The native object, with a line the first time each case is seen.
    ///
    /// # Safety
    /// As `unwrap`.
    pub unsafe fn native(&self, pointer: *mut c_void) -> *mut c_void {
        if !present() || pointer.is_null() {
            return pointer;
        }
        let (native, peeled) = unwrap(pointer);
        let bit = if peeled { 1 } else { 2 };
        if self.said.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
            if peeled {
                crate::log!(
                    "[cmp] {} {:#x} is a ReShade proxy over {:#x}; {}",
                    self.what,
                    pointer as usize,
                    native as usize,
                    self.then
                );
            } else {
                crate::log!(
                    "[cmp] {} {:#x} is native ({}) although ReShade is loaded",
                    self.what,
                    pointer as usize,
                    owner(pointer)
                );
            }
        }
        native
    }

    /// The same for a typed interface.
    ///
    /// # Safety
    /// As `unwrap`; the native object must implement `I`, which it does when
    /// `I` is the interface the proxy was standing in for.
    pub unsafe fn native_interface<I: Interface>(&self, object: I) -> I {
        if !present() {
            return object;
        }
        let raw = self.native(object.as_raw());
        if raw == object.as_raw() {
            return object;
        }
        I::from_raw_borrowed(&raw).cloned().unwrap_or(object)
    }
}

/// A place where an object is handed on *without* being unwrapped, on purpose.
///
/// The device given to `slSetD3DDevice` and the factory given to
/// `slUpgradeInterface`: see the module comment for why. It says what it
/// handed over, once for each case, so that a run in which ReShade did not
/// end up in the presentation path can be read back to the reason.
pub struct Kept {
    what: &'static str,
    said: AtomicU8,
}

impl Kept {
    pub const fn new(what: &'static str) -> Kept {
        Kept {
            what,
            said: AtomicU8::new(0),
        }
    }

    /// # Safety
    /// As `unwrap`.
    pub unsafe fn note(&self, pointer: *mut c_void) {
        if !present() || pointer.is_null() {
            return;
        }
        let (native, peeled) = unwrap(pointer);
        let bit = if peeled { 1 } else { 2 };
        if self.said.fetch_or(bit, Ordering::Relaxed) & bit != 0 {
            return;
        }
        if peeled {
            crate::log!(
                "[cmp] {} is given {:#x}, a ReShade proxy over {:#x}, and not the native \
                 object: ReShade joins only a swapchain created through its own proxies",
                self.what,
                pointer as usize,
                native as usize
            );
        } else {
            crate::log!(
                "[cmp] {} is given {:#x}, which is native ({}) although ReShade is loaded, \
                 so ReShade is not expected in this chain's presentation path",
                self.what,
                pointer as usize,
                owner(pointer)
            );
        }
    }
}

/// The file name of the module that implements an object.
///
/// Read off the object: its vtable lives in the module whose code it points
/// at. This is how a layer is named without trusting any configuration -- a
/// factory "is ReShade's" if its vtable is in ReShade's module.
///
/// # Safety
/// `pointer` must be null or a live COM object.
pub unsafe fn owner(pointer: *mut c_void) -> String {
    match owner_module(pointer) {
        Some(module) => file_name(&path_of(module)).to_string(),
        None => "an unknown module".to_string(),
    }
}

unsafe fn owner_module(pointer: *mut c_void) -> Option<HMODULE> {
    if pointer.is_null() {
        return None;
    }
    let vtable = *(pointer as *const *const u16);
    let mut module = HMODULE::default();
    GetModuleHandleExW(
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        windows::core::PCWSTR(vtable),
        &mut module,
    )
    .ok()?;
    Some(module)
}

/// Every layer of an object, outermost first, as far as it can be followed.
///
/// A Streamline proxy is asked through `slGetNativeInterface`; a ReShade
/// proxy through its IID; anything else ends the walk. Bounded.
unsafe fn layers(pointer: *mut c_void) -> (String, bool) {
    let mut parts: Vec<String> = Vec::new();
    let mut through_reshade = false;
    let mut current = pointer;
    for _ in 0..=MAX_LAYERS {
        let module = owner_module(current);
        let name = match module {
            Some(module) => file_name(&path_of(module)).to_string(),
            None => "an unknown module".to_string(),
        };
        if module.is_some_and(|module| module.0 as usize == self::module() && self::module() != 0) {
            through_reshade = true;
        }
        parts.push(format!("{:#x} ({name})", current as usize));
        let next = if name.eq_ignore_ascii_case("sl.interposer.dll") {
            crate::streamline::native_interface(current)
        } else if present() {
            peel(current)
        } else {
            None
        };
        match next {
            Some(inner) if inner != current && !inner.is_null() => current = inner,
            _ => break,
        }
    }
    (parts.join(" over "), through_reshade)
}

/// A module that would answer NGX calls in the driver's place.
///
/// That is what OptiScaler is to this library whatever file name it was
/// installed under: something exporting `NVSDK_NGX_D3D12_Init` that is
/// neither the driver's NGX nor an NGX snippet.
///
/// Snippets are excluded: `nvngx_dlssg.dll` exports the same entry point,
/// as every snippet does, and NVIDIA's own frame generation snippet is not a
/// stand-in for the driver. Snippets are always named `nvngx_<feature>.dll`;
/// NGX finds them by that name.
///
/// So is the game's own executable. It links NVIDIA's NGX client library
/// statically and that library's entry points show up among its exports.
/// The game is the NGX *client*; it is not something standing in for the
/// driver.
unsafe fn ngx_stand_in() -> Option<String> {
    let game =
        windows::Win32::System::LibraryLoader::GetModuleHandleW(windows::core::PCWSTR::null())
            .unwrap_or_default();
    for module in modules() {
        if module.0 == game.0 {
            continue;
        }
        if GetProcAddress(module, PCSTR(b"NVSDK_NGX_D3D12_Init\0".as_ptr())).is_none() {
            continue;
        }
        let path = path_of(module);
        if !is_nvidias_ngx(&path) {
            return Some(path);
        }
    }
    None
}

/// Whether a module exporting the NGX surface is NVIDIA's: the driver's core
/// in the driver store, or a snippet.
fn is_nvidias_ngx(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("\\driverstore\\") || file_name(&lower).starts_with("nvngx_")
}

// ---------------------------------------------------------------------------
// Loading a companion at a fixed moment
// ---------------------------------------------------------------------------

/// Idle, loading, done. The whole of the re-entrancy guard: it leaves idle
/// before anything is loaded and never returns to it, so however this is
/// reached again -- from the second trigger, from another thread, or from
/// inside the load itself -- it returns at once. One attempt per process.
static LOAD_STATE: AtomicU8 = AtomicU8::new(0);
const IDLE: u8 = 0;
const LOADING: u8 = 1;
const DONE: u8 = 2;

/// More entries than this in the launcher's list are ignored, and said so.
const MAX_AT_RENDERER: usize = 4;

/// The first bytes of an exported function, as text.
unsafe fn prologue(module: &'static str, symbol: &'static [u8]) -> Option<[u8; 8]> {
    let entry = crate::proxy::entry(module, symbol)? as usize as *const u8;
    let mut bytes = [0u8; 8];
    std::ptr::copy_nonoverlapping(entry, bytes.as_mut_ptr(), bytes.len());
    Some(bytes)
}

fn hex(bytes: &[u8; 8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Load what the launcher's chain file marked `renderer`, now, on this thread.
///
/// The point of doing it here rather than letting the launcher inject it is
/// order. The engine creates its first DXGI factory on the line after it
/// loads `d3d12.dll`, on the same thread, and the swapchain is made by that
/// factory. An injection from outside is asynchronous and arrives after it,
/// so ReShade then never sees a swapchain at all. Loading synchronously from
/// the engine's own thread, between its `d3d12.dll` load and its first
/// factory, puts ReShade's hooks in place before either the factory or the
/// device exists -- every time, not when a race falls the right way.
///
/// It is also late enough. The protection stub has finished: the engine is
/// executing its own renderer start-up to get here.
///
/// Called from the two hooks that can see that moment, whichever comes first.
/// Not under the loader lock: both are entered from the game's own code,
/// after the call they wrap has returned.
///
/// Say so when a library that serves NGX has `nvngx_dlisp.dll` in its own
/// folder and frame generation is asked for.
///
/// NGX looks for snippets in the folder of the module that initialises it.
/// A library that stands between Streamline and NGX -- OptiScaler does --
/// makes that folder its own, and in the game folder that finds the game's
/// 2019 `nvngx_dlisp.dll`, which raises a breakpoint when NGX initialises it
/// on a current GPU: `slSetD3DDevice` then ends in `eErrorExceptionHandler`
/// and there is no frame generation. Seen in a minidump of exactly that:
/// the breakpoint in `<game>\nvngx_dlisp.dll`, under `_nvngx.dll`,
/// `sl.common.dll` and `OptiScaler.dll`.
unsafe fn warn_of_a_dlisp_snippet_beside(module: HMODULE, path: &str) {
    if !crate::streamline::asked_for() {
        return;
    }
    if GetProcAddress(module, PCSTR(b"NVSDK_NGX_D3D12_Init\0".as_ptr())).is_none() {
        return;
    }
    let Some(folder) = std::path::Path::new(path).parent() else {
        return;
    };
    let snippet = folder.join("nvngx_dlisp.dll");
    if snippet.exists() {
        crate::log!(
            "[cmp] {} serves NGX and has {} beside it. With fg = 1 NGX may initialise that \
             snippet for Streamline, and the game's own copy of it raises a breakpoint on \
             current GPUs: if slSetD3DDevice then returns eErrorExceptionHandler, that is \
             why, and frame generation is off for the session. Keeping this library in the \
             mhw-dlss folder instead of the game folder avoids it",
            file_name(path),
            snippet.display()
        );
    }
}

/// # Safety
/// Must not be called from `DllMain`.
pub unsafe fn load_at_renderer(trigger: &str) {
    /// How long Streamline is given to finish initialising first. It takes
    /// one to two seconds.
    const SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    if LOAD_STATE
        .compare_exchange(IDLE, LOADING, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    // Absent in every configuration that has no `renderer` entry, and then
    // this is the end of it: no line, no load, no module walk.
    let Some(listed) = std::env::var_os(crate::hooks::AT_RENDERER_VARIABLE) else {
        LOAD_STATE.store(DONE, Ordering::SeqCst);
        return;
    };
    let listed = listed.to_string_lossy().into_owned();
    let paths: Vec<&str> = listed
        .split('|')
        .filter(|path| !path.trim().is_empty())
        .collect();
    if paths.len() > MAX_AT_RENDERER {
        crate::log!(
            "[cmp] the launcher listed {} libraries to load with the renderer; only the \
             first {MAX_AT_RENDERER} are loaded",
            paths.len()
        );
    }

    // Not while Streamline is initialising on its own thread. What is loaded
    // here rewrites the first bytes of functions in dxgi.dll and d3d12.dll,
    // and `slInit` calls into both. The engine is inside a library load or a
    // symbol lookup at this point and is not waiting on anything of
    // Streamline's, so the wait cannot deadlock; it is bounded all the same.
    let waiting = std::time::Instant::now();
    let settled = crate::streamline::wait_until_settled(SETTLE_TIMEOUT);
    if !settled || waiting.elapsed() > std::time::Duration::from_millis(20) {
        crate::log!(
            "[cmp] waited {} ms for Streamline to finish initialising before loading what \
             the chain file lists as `renderer`{}",
            waiting.elapsed().as_millis(),
            if settled {
                ""
            } else {
                "; it had not finished, and they are loaded now all the same"
            }
        );
    }

    let factory_before = prologue("dxgi.dll", b"CreateDXGIFactory2\0");
    let device_before = prologue("d3d12.dll", b"D3D12CreateDevice\0");

    // ReShade loads only for an executable it has a configuration for unless
    // told otherwise; being listed in the chain file is the user telling it.
    std::env::set_var("RESHADE_DISABLE_LOADING_CHECK", "1");

    for path in paths.into_iter().take(MAX_AT_RENDERER) {
        let (factories, devices) = crate::proxy::created_so_far();
        crate::log!(
            "[cmp] loading {path} on thread {} at {trigger}; this library's own \
             CreateDXGIFactory2 has been called {factories} times and its \
             D3D12CreateDevice {devices} times so far",
            windows::Win32::System::Threading::GetCurrentThreadId()
        );
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let started = std::time::Instant::now();
        match windows::Win32::System::LibraryLoader::LoadLibraryW(windows::core::PCWSTR(
            wide.as_ptr(),
        )) {
            Ok(module) => {
                crate::log!(
                    "[cmp] -> module {:#x} after {} ms",
                    module.0 as usize,
                    started.elapsed().as_millis()
                );
                warn_of_a_dlisp_snippet_beside(module, path);
            }
            Err(error) => crate::log!(
                "[cmp] -> NOT loaded after {} ms: {error}. The game carries on without it",
                started.elapsed().as_millis()
            ),
        }
    }

    // What the load did, read off the two functions ReShade has to hook for
    // any of this to work. "Loaded" alone would say what was asked for.
    for (name, before, after) in [
        (
            "dxgi.dll!CreateDXGIFactory2",
            factory_before,
            prologue("dxgi.dll", b"CreateDXGIFactory2\0"),
        ),
        (
            "d3d12.dll!D3D12CreateDevice",
            device_before,
            prologue("d3d12.dll", b"D3D12CreateDevice\0"),
        ),
    ] {
        match (before, after) {
            (Some(before), Some(after)) if before != after => crate::log!(
                "[cmp] {name} began {} before the load and {} after: its body was hooked",
                hex(&before),
                hex(&after)
            ),
            (Some(before), Some(_)) => crate::log!(
                "[cmp] {name} began {} before the load and the same after: NOT hooked",
                hex(&before)
            ),
            _ => crate::log!("[cmp] {name} could not be read"),
        }
    }
    detect();
    LOAD_STATE.store(DONE, Ordering::SeqCst);
}

static SAID_FACTORY: AtomicU8 = AtomicU8::new(0);

/// Say what the engine was handed by `CreateDXGIFactory2`, once per case.
///
/// # Safety
/// `factory` must be null or a live COM object.
pub unsafe fn note_engine_factory(factory: *mut c_void) {
    if !present() || factory.is_null() {
        return;
    }
    let (native, peeled) = unwrap(factory);
    let bit = if peeled { 1 } else { 2 };
    if SAID_FACTORY.fetch_or(bit, Ordering::Relaxed) & bit != 0 {
        return;
    }
    if peeled {
        crate::log!(
            "[cmp] the factory CreateDXGIFactory2 returned to the engine, {:#x}, is a \
             ReShade proxy over {:#x}",
            factory as usize,
            native as usize
        );
    } else {
        crate::log!(
            "[cmp] the factory CreateDXGIFactory2 returned to the engine, {:#x}, is native \
             ({}) although ReShade is loaded: ReShade's hook was not in place for this call",
            factory as usize,
            owner(factory)
        );
    }
}

/// The module that entered the hook this is called from.
///
/// A hook on DXGI's own factory cannot see what is above it by looking at the
/// objects it is given -- by then a proxy has already unwrapped them. But it
/// can see who called it. If that is ReShade's module, ReShade is above this
/// hook and receives whatever this hook returns.
///
/// The first return address outside this library, from at most eight frames.
#[inline(never)]
unsafe fn entered_from() -> Option<HMODULE> {
    let mut frames = [std::ptr::null_mut::<c_void>(); 8];
    let captured =
        windows::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace(1, &mut frames, None)
            as usize;
    let ours = module_at(entered_from as *const c_void);
    for frame in &frames[..captured.min(frames.len())] {
        let module = module_at(*frame);
        if module.is_some() && module.map(|module| module.0) != ours.map(|module| module.0) {
            return module;
        }
    }
    None
}

pub(crate) unsafe fn module_at(address: *const c_void) -> Option<HMODULE> {
    let mut module = HMODULE::default();
    GetModuleHandleExW(
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        windows::core::PCWSTR(address as *const u16),
        &mut module,
    )
    .ok()?;
    Some(module)
}

static LAST_STACK: Mutex<String> = Mutex::new(String::new());

/// Say which stack the swapchain that was just created sits on.
///
/// Observed, never configured: every clause is read off an object or off the
/// module list at this moment. Printed for the first chain and again only
/// when the answer differs, since the engine rebuilds its chain on every
/// resolution change.
///
/// # Safety
/// Must be called directly from the swapchain creation hook, because it reads
/// that hook's caller off the stack. The pointers must be the live objects of
/// the creation call being reported:
/// the factory our hook was entered on, the factory the call was routed
/// through if it was routed through Streamline, the queue argument as the
/// engine passed it, and the chain handed back.
pub unsafe fn report_stack(
    index: u64,
    factory: *mut c_void,
    routed_through: Option<*mut c_void>,
    queue: *mut c_void,
    chain: *mut c_void,
) {
    let (chain_layers, chain_through_reshade) = layers(chain);
    let (queue_layers, _) = layers(queue);
    let made_by = match routed_through {
        Some(proxy) => format!("Created through {}", layers(proxy).0),
        None => format!("Created by {}", layers(factory).0),
    };
    // Who entered our hook. Under a proxy factory the objects above cannot be
    // seen from here, but the caller can.
    let caller = entered_from();
    let caller_name = match caller {
        Some(module) => file_name(&path_of(module)).to_string(),
        None => "an unknown module".to_string(),
    };
    let called_by_reshade =
        caller.is_some_and(|module| module.0 as usize == self::module() && self::module() != 0);
    let reshade = if present() {
        format!(
            "ReShade {} is loaded and {}",
            VERSION.lock(),
            if chain_through_reshade {
                "IS a layer of the chain our hook returned"
            } else if called_by_reshade {
                "is ABOVE this hook: it made this call, so it receives this chain, and \
                 ReShade.log says whether it wrapped it"
            } else {
                "is NOT a layer of this chain and did not make this call"
            }
        )
    } else {
        "No module exporting ReShadeVersion".to_string()
    };
    let stand_in = match ngx_stand_in() {
        Some(path) => format!("A module that is neither the game, the driver nor an NVIDIA snippet exports NVSDK_NGX_D3D12_Init and may be answering NGX calls: {path}"),
        None => "Nothing stands in for the driver's NGX (only the game, the driver and NVIDIA's snippets export NVSDK_NGX_D3D12_Init)".to_string(),
    };
    let text = format!(
        "our hook was entered from {caller_name} and returned {chain_layers}. {made_by}. \
         Queue argument {queue_layers}. {reshade}. {stand_in}"
    );
    let mut last = LAST_STACK.lock();
    if *last == text {
        return;
    }
    crate::log!("[stk] swapchain {index}, observed: {text}");
    *last = text;
}

// ---------------------------------------------------------------------------
// The list as the engine holds it
// ---------------------------------------------------------------------------
//
// NGX is the one place where a native list is the wrong thing to hand over.
// NGX creates descriptor heaps and command objects on the device it holds and
// then uses them with the list it is given. With ReShade under Streamline that
// device is ReShade's proxy -- Streamline was given it, on purpose -- so what
// NGX creates are ReShade's proxies, and it then called `SetDescriptorHeaps`
// with a proxy heap on the native list this library had handed it. D3D12 was
// passed an object it did not make and faulted inside D3D12Core: the second
// run in the game, at the first DLSS frame. Measured in the stand-in in all
// four combinations: a proxy list works whichever device this library
// initialised NGX with; a native list faults or fails.
//
// So NGX must get the list from the same side of ReShade as its device. The
// difficulty is that the DLSS pass runs from a vtable hook on the *native*
// list, and nothing leads from a native list back to its proxy: ReShade keeps
// no such record. What does exist is the call stack. The engine's `Dispatch`
// goes through ReShade's proxy before it reaches ours, so one hook on the
// proxy's `Dispatch` slot notes, for the duration of that call and on that
// thread, which proxy is being dispatched on. The native hook underneath can
// then ask.

thread_local! {
    /// The ReShade proxy list whose `Dispatch` is on this thread's stack.
    static DISPATCHING: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

type DispatchFn = unsafe extern "system" fn(*mut c_void, u32, u32, u32);
type SetHeapsFn = unsafe extern "system" fn(*mut c_void, u32, *const *mut c_void);

static O_PROXY_DISPATCH: AtomicUsize = AtomicUsize::new(0);
static O_PROXY_SET_HEAPS: AtomicUsize = AtomicUsize::new(0);
static PROXY_LIST_WATCHED: AtomicBool = AtomicBool::new(false);
static PROXY_DISPATCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The heaps the engine last set on each proxy list, as the engine passed
/// them, which is as ReShade's proxies. Kept so they can be set again through
/// ReShade after NGX has replaced them: ReShade remembers the heaps of every
/// list it wraps, and a restore made on the native list underneath would
/// leave that memory pointing at NGX's heap.
static HELD_HEAPS: Mutex<Option<std::collections::HashMap<usize, (u32, [usize; 2])>>> =
    Mutex::new(None);
/// Lists are created and destroyed for the life of a session and this map is
/// keyed by address, so it is emptied when it reaches this many entries
/// rather than left to grow. The engine sets its heaps again on every list
/// every frame.
const MAX_HELD_LISTS: usize = 4096;

/// Holds the proxy for the length of one `Dispatch`, and puts back whatever
/// was there: NGX dispatches on the same proxy from inside the outer call.
pub struct Dispatching {
    previous: usize,
}

/// Say that `proxy` is the engine's list on this thread until the guard is
/// dropped, for a caller that was handed the proxy itself.
///
/// The temporal pass is reached from a hook on the native list, where the
/// proxy can only be learnt from the `Dispatch` on the stack above. The
/// engine's upscale stage is the other way round: its detour is handed the
/// list as the engine holds it, which with ReShade loaded *is* the proxy,
/// and no hooked proxy method is on the stack there. This is what lets the
/// lookup in `held_list` find it at that stage.
///
/// The answer is still checked where it is used: `held_list` unwraps the
/// proxy and requires it to answer with the native list in question.
pub fn hold(proxy: *mut c_void) -> Dispatching {
    Dispatching::enter(proxy)
}

impl Dispatching {
    fn enter(proxy: *mut c_void) -> Dispatching {
        Dispatching {
            previous: DISPATCHING.with(|current| current.replace(proxy as usize)),
        }
    }
}

impl Drop for Dispatching {
    fn drop(&mut self) {
        DISPATCHING.with(|current| current.set(self.previous));
    }
}

unsafe extern "system" fn proxy_dispatch(this: *mut c_void, x: u32, y: u32, z: u32) {
    let original: DispatchFn = std::mem::transmute(O_PROXY_DISPATCH.load(Ordering::Relaxed));
    PROXY_DISPATCHES.fetch_add(1, Ordering::Relaxed);
    // Bound to a variable, so that it lasts for the call and not for the
    // statement.
    let _dispatching = Dispatching::enter(this);
    // The stand-in's DLSS frame, if one is waiting for this dispatch. One
    // relaxed load otherwise.
    crate::selftest::at_proxy_dispatch(this);
    original(this, x, y, z);
}

unsafe extern "system" fn proxy_set_heaps(
    this: *mut c_void,
    count: u32,
    heaps: *const *mut c_void,
) {
    let original: SetHeapsFn = std::mem::transmute(O_PROXY_SET_HEAPS.load(Ordering::Relaxed));
    if !heaps.is_null() && count <= 2 {
        let mut held = [0usize; 2];
        for index in 0..count as usize {
            held[index] = *heaps.add(index) as usize;
        }
        let mut lists = HELD_HEAPS.lock();
        let lists = lists.get_or_insert_with(std::collections::HashMap::new);
        if lists.len() >= MAX_HELD_LISTS {
            lists.clear();
        }
        lists.insert(this as usize, (count, held));
    }
    original(this, count, heaps);
}

/// Whether ReShade's command list proxy is being watched.
pub fn proxy_lists_watched() -> bool {
    O_PROXY_DISPATCH.load(Ordering::Relaxed) != 0
}

/// How many dispatches have come through ReShade's proxy list.
pub fn proxy_dispatches() -> u64 {
    PROXY_DISPATCHES.load(Ordering::Relaxed)
}

/// Put the two hooks on ReShade's command list proxy, once ReShade has a
/// proxy device.
///
/// ReShade leaves a pointer to its proxy device on the native device, as
/// private data under the proxy class's own GUID. A list created through that
/// proxy is one of ReShade's, and every proxy list shares its vtable. Called
/// where a swapchain is created; it does nothing until ReShade is loaded and
/// nothing again once it has succeeded.
///
/// # Safety
/// Creates and releases a command list; must not be called under the loader
/// lock.
pub unsafe fn watch_proxy_lists() {
    use windows::Win32::Graphics::Direct3D12::{
        ID3D12CommandAllocator, ID3D12Device, ID3D12GraphicsCommandList,
        ID3D12GraphicsCommandList_Vtbl, D3D12_COMMAND_LIST_TYPE_DIRECT,
    };
    if !present() || PROXY_LIST_WATCHED.load(Ordering::Relaxed) {
        return;
    }
    let Some(native) = crate::bindings::device() else {
        return;
    };
    // `__uuidof(D3D12Device)` in ReShade's d3d12_device.hpp.
    const PROXY_DEVICE: GUID = GUID::from_u128(0x2523aff4_978b_4939_ba16_8ee876a4cb2a);
    let mut proxy: *mut c_void = std::ptr::null_mut();
    let mut size = std::mem::size_of::<*mut c_void>() as u32;
    if native
        .GetPrivateData(
            &PROXY_DEVICE,
            &mut size,
            Some(&mut proxy as *mut _ as *mut c_void),
        )
        .is_err()
        || proxy.is_null()
    {
        // ReShade is loaded and has no proxy on the engine's device: it came
        // in after the device was made. The engine's lists are native then,
        // and there is nothing to watch.
        return;
    }
    if PROXY_LIST_WATCHED.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(device) = ID3D12Device::from_raw_borrowed(&proxy) else {
        return;
    };
    let allocator: ID3D12CommandAllocator =
        match device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT) {
            Ok(allocator) => allocator,
            Err(error) => {
                crate::log!("[cmp] no command allocator from ReShade's proxy device: {error}");
                return;
            }
        };
    let list: ID3D12GraphicsCommandList =
        match device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None) {
            Ok(list) => list,
            Err(error) => {
                crate::log!("[cmp] no command list from ReShade's proxy device: {error}");
                return;
            }
        };
    let _ = list.Close();
    let (_, is_proxy) = unwrap(list.as_raw());
    if !is_proxy {
        crate::log!(
            "[cmp] a list created through ReShade's proxy device {:#x} is not a proxy ({}), \
             so the engine's lists are not either and NGX is given them as they are",
            proxy as usize,
            owner(list.as_raw())
        );
        return;
    }
    let vtable = crate::vtable::of(&list);
    for (name, index, replacement, slot) in [
        (
            "Dispatch",
            crate::slot!(ID3D12GraphicsCommandList_Vtbl, Dispatch),
            proxy_dispatch as *const c_void,
            &O_PROXY_DISPATCH,
        ),
        (
            "SetDescriptorHeaps",
            crate::slot!(ID3D12GraphicsCommandList_Vtbl, SetDescriptorHeaps),
            proxy_set_heaps as *const c_void,
            &O_PROXY_SET_HEAPS,
        ),
    ] {
        if crate::vtable::hook(vtable, index, replacement, slot) {
            crate::log!(
                "[cmp] hooked slot {index} {name} on ReShade's command list proxy (vtable \
                 {vtable:p} in {}), to know which proxy a native list belongs to",
                owner(list.as_raw())
            );
        } else {
            crate::log!("[cmp] FAILED to hook {name} on ReShade's command list proxy");
        }
    }
}

/// The proxy the engine holds for this native list, if its `Dispatch` is on
/// this thread's stack.
///
/// Checked, not assumed: the proxy on the stack is unwrapped and must answer
/// with this very list.
///
/// # Safety
/// `native` must be a live command list.
pub unsafe fn held_list(native: *mut c_void) -> Option<*mut c_void> {
    let proxy = DISPATCHING.with(|current| current.get()) as *mut c_void;
    if proxy.is_null() {
        return None;
    }
    (peel(proxy) == Some(native)).then_some(proxy)
}

/// The descriptor heaps the engine last set on this proxy list, as it passed
/// them. Taken before NGX is given the list.
pub fn held_heaps(proxy: *mut c_void) -> Option<(u32, [usize; 2])> {
    HELD_HEAPS
        .lock()
        .as_ref()
        .and_then(|lists| lists.get(&(proxy as usize)).copied())
}

/// Set the engine's own descriptor heaps on its list again, through ReShade.
///
/// After NGX has run on a proxy list, with what `held_heaps` answered
/// *before* it ran: NGX sets its own heaps through the same proxy, and those
/// calls pass through the same hook and overwrite the record. (The first
/// version read the record afterwards and faithfully restored NGX's heaps.)
/// Returns how many heaps were set; zero when the engine had set none.
///
/// # Safety
/// `proxy` must be a live ReShade command list proxy, open for recording.
pub unsafe fn restore_held_heaps(proxy: *mut c_void, saved: Option<(u32, [usize; 2])>) -> u32 {
    let Some((count, heaps)) = saved else {
        return 0;
    };
    if count == 0 {
        return 0;
    }
    // Through the proxy's own slot, which is our hook and then ReShade's
    // method: ReShade unwraps the heaps, sets them on the native list and
    // updates its record.
    let set: SetHeapsFn =
        std::mem::transmute(*(*(proxy as *const *const usize)).add(crate::slot!(
            windows::Win32::Graphics::Direct3D12::ID3D12GraphicsCommandList_Vtbl,
            SetDescriptorHeaps
        )));
    let pointers = [heaps[0] as *mut c_void, heaps[1] as *mut c_void];
    set(proxy, count, pointers.as_ptr());
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The IID, byte for byte, against ReShade's declaration:
    /// `{ 0x7f2c9a11, 0x3b4e, 0x4d6a, { 0x81, 0x2f, 0x5e, 0x9c, 0xd3, 0x7a, 0x1b, 0x42 } }`.
    #[test]
    fn the_iid_is_reshades() {
        assert_eq!(IID_UNWRAPPED.data1, 0x7f2c9a11);
        assert_eq!(IID_UNWRAPPED.data2, 0x3b4e);
        assert_eq!(IID_UNWRAPPED.data3, 0x4d6a);
        assert_eq!(
            IID_UNWRAPPED.data4,
            [0x81, 0x2f, 0x5e, 0x9c, 0xd3, 0x7a, 0x1b, 0x42]
        );
    }

    /// With no ReShade seen, an unwrap must not touch its argument at all --
    /// not even to read a vtable out of it. A pointer that would fault if
    /// dereferenced proves it.
    #[test]
    fn absent_means_untouched() {
        assert!(!present());
        let bogus = 0x10usize as *mut c_void;
        assert_eq!(unsafe { unwrap(bogus) }, (bogus, false));
        static SITE: Boundary = Boundary::new("a test pointer", "nothing");
        assert_eq!(unsafe { SITE.native(bogus) }, bogus);
        assert_eq!(
            unsafe { unwrap(std::ptr::null_mut()) },
            (std::ptr::null_mut(), false)
        );
    }

    #[test]
    fn snippets_and_the_drivers_core_are_not_a_stand_in() {
        assert!(is_nvidias_ngx(
            r"C:\Windows\System32\DriverStore\FileRepository\nv_dispi.inf_amd64_x\_nvngx.dll"
        ));
        assert!(is_nvidias_ngx(r"C:\game\mhw-dlss\nvngx_dlssg.dll"));
        assert!(is_nvidias_ngx(r"C:\game\NVNGX_DLSS.DLL"));
        assert!(!is_nvidias_ngx(r"C:\game\OptiScaler.dll"));
        assert!(!is_nvidias_ngx(r"C:\game\version.dll"));
    }

    #[test]
    fn file_names_are_cut_at_either_separator() {
        assert_eq!(file_name("C:\\a\\ReShade64.dll"), "ReShade64.dll");
        assert_eq!(file_name("C:/a/b.dll"), "b.dll");
        assert_eq!(file_name("plain.dll"), "plain.dll");
    }
}
