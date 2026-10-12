//! Owning the game's creation calls: `D3D12CreateDevice`,
//! `CreateDXGIFactory2` and `CreateDXGIFactory1`.
//!
//! `mod` owns the single `GetProcAddress` hook and the delay-import patch of
//! `d3d12.dll` ordinal #101 (MHW imports `D3D12CreateDevice` by ordinal
//! only, `ReLoadInitDx11` §2.2, so a by-name lookup never sees it). For each
//! lookup it asks [`intercept`] / [`intercept_ordinal`] for a replacement and
//! tells this crate what the game would have got with [`set_original`]. An
//! original never set is resolved the way the delay-load helper does
//! (`GetModuleHandle`, else `LoadLibraryExW(name, 0, 0)`, then
//! `GetProcAddress`), so the game's own search order decides which
//! `d3d12.dll`/`dxgi.dll` it is.
//!
//! The device replacement turns the debug layer and DRED on first (they
//! cannot be retrofitted onto an existing device), makes sure the vtable
//! hooks are in ([`super::install`]) before the game's device can create
//! anything, then calls the original. It tolerates `out == NULL` (three of
//! the game's four call sites only probe). The factory replacements call
//! the original and then the registered [`FactoryHook`], which may replace
//! the returned factory (frame generation upgrades it). Calls nested inside
//! one of these (Streamline enumerating adapters inside its hook, our own
//! device) pass straight through.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use parking_lot::Mutex;
use windows::core::{Interface, GUID, HRESULT, PCSTR, PCWSTR};
use windows::Win32::Foundation::{ERROR_PROC_NOT_FOUND, HANDLE};
use windows::Win32::Graphics::Direct3D12::ID3D12Device;
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleW, GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_FLAGS,
};

use super::{diag, install, proxylist, reshade};

/// The ordinal `d3d12.dll` exports `D3D12CreateDevice` under.
pub const D3D12_CREATE_DEVICE_ORDINAL: u16 = 101;

/// Device creation options; `mod` policy (`[Dev] DebugLayer`, `Dred`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeviceOptions {
    /// Enable the D3D12 debug layer and drain its messages into the log.
    pub debug_layer: bool,
    /// Enable DRED breadcrumbs and page-fault reporting, reported on
    /// device removal.
    pub dred: bool,
}

static OPTIONS: Mutex<DeviceOptions> = Mutex::new(DeviceOptions {
    debug_layer: false,
    dred: false,
});

/// Set the device options. Call before [`super::install`] and before the
/// game creates its device; later changes apply to later devices only.
pub fn configure_device(options: DeviceOptions) {
    *OPTIONS.lock() = options;
}

pub(crate) fn options() -> DeviceOptions {
    *OPTIONS.lock()
}

/// Which factory entry point was called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactoryEntry {
    /// `CreateDXGIFactory1(riid, out)` (the game's DX11 path).
    Factory1,
    /// `CreateDXGIFactory2(flags, riid, out)` (the game's DX12 path).
    Factory2,
}

/// A factory the game just created.
#[derive(Debug)]
pub struct FactoryCreated {
    /// Entry point.
    pub entry: FactoryEntry,
    /// `Flags` (0 for `CreateDXGIFactory1`).
    pub flags: u32,
    /// The interface the game asked for.
    pub riid: *const GUID,
    /// Where the factory was returned; the hook may replace `*out` with an
    /// object implementing `riid` (it then owns the reference it replaced).
    pub out: *mut *mut c_void,
}

/// Called after every successful outermost factory creation by the game,
/// on the game's thread, before the factory is returned to it.
pub type FactoryHook = fn(&FactoryCreated);

static FACTORY_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Install (or with `None` remove) the factory hook.
pub fn set_factory_hook(hook: Option<FactoryHook>) {
    FACTORY_HOOK.store(hook.map_or(0, |f| f as usize), Ordering::Release);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Entry {
    Device,
    Factory2,
    Factory1,
}

impl Entry {
    fn of(name: &str) -> Option<Entry> {
        match name {
            "D3D12CreateDevice" => Some(Entry::Device),
            "CreateDXGIFactory2" => Some(Entry::Factory2),
            "CreateDXGIFactory1" => Some(Entry::Factory1),
            _ => None,
        }
    }

    fn module(self) -> PCWSTR {
        match self {
            Entry::Device => windows::core::w!("d3d12.dll"),
            Entry::Factory2 | Entry::Factory1 => windows::core::w!("dxgi.dll"),
        }
    }

    fn symbol(self) -> PCSTR {
        match self {
            Entry::Device => windows::core::s!("D3D12CreateDevice"),
            Entry::Factory2 => windows::core::s!("CreateDXGIFactory2"),
            Entry::Factory1 => windows::core::s!("CreateDXGIFactory1"),
        }
    }

    fn slot(self) -> &'static AtomicUsize {
        static ORIGINALS: [AtomicUsize; 3] = [const { AtomicUsize::new(0) }; 3];
        &ORIGINALS[self as usize]
    }

    fn replacement(self) -> usize {
        match self {
            Entry::Device => d3d12_create_device as *const () as usize,
            Entry::Factory2 => create_dxgi_factory2 as *const () as usize,
            Entry::Factory1 => create_dxgi_factory1 as *const () as usize,
        }
    }
}

/// The replacement for a procedure the game looks up by name, if this crate
/// intercepts it: `D3D12CreateDevice`, `CreateDXGIFactory2`,
/// `CreateDXGIFactory1`.
pub fn intercept(name: &str) -> Option<usize> {
    Entry::of(name).map(Entry::replacement)
}

/// The replacement for an import by ordinal: `("d3d12.dll", 101)` is
/// `D3D12CreateDevice`. `dll` is matched case-insensitively.
pub fn intercept_ordinal(dll: &str, ordinal: u16) -> Option<usize> {
    (dll.eq_ignore_ascii_case("d3d12.dll") && ordinal == D3D12_CREATE_DEVICE_ORDINAL)
        .then(|| Entry::Device.replacement())
}

/// Record what the game would have called for `name` (the real export, or
/// whatever a layer above put there). Ignored for names not intercepted, for
/// 0, and for this crate's own replacement. Returns whether it was taken.
pub fn set_original(name: &str, address: usize) -> bool {
    match Entry::of(name) {
        Some(entry) if address != 0 && address != entry.replacement() => {
            entry.slot().store(address, Ordering::SeqCst);
            true
        }
        _ => false,
    }
}

/// The original for `entry`, resolved like the delay-load helper if unset.
fn original(entry: Entry) -> Option<usize> {
    let known = entry.slot().load(Ordering::SeqCst);
    if known != 0 {
        return Some(known);
    }
    // SAFETY: NUL-terminated literals; an already loaded module is used as
    // is, otherwise loaded with the standard search order, as the game's
    // helper would.
    let address = unsafe {
        let module = match GetModuleHandleW(entry.module()) {
            Ok(module) => module,
            Err(_) => {
                LoadLibraryExW(entry.module(), HANDLE::default(), LOAD_LIBRARY_FLAGS(0)).ok()?
            }
        };
        GetProcAddress(module, entry.symbol())? as usize
    };
    if address == entry.replacement() {
        return None;
    }
    entry.slot().store(address, Ordering::SeqCst);
    Some(address)
}

thread_local! {
    /// Inside one of our creation replacements on this thread.
    static INSIDE: Cell<bool> = const { Cell::new(false) };
}

/// Marks the outermost creation call on this thread.
struct Outermost;

impl Outermost {
    fn enter() -> Option<Outermost> {
        INSIDE.with(|inside| (!inside.replace(true)).then_some(Outermost))
    }
}

impl Drop for Outermost {
    fn drop(&mut self) {
        INSIDE.with(|inside| inside.set(false));
    }
}

const NOT_FOUND: HRESULT = ERROR_PROC_NOT_FOUND.to_hresult();
static DEVICES: AtomicU32 = AtomicU32::new(0);
static FACTORIES: AtomicU32 = AtomicU32::new(0);

/// `(devices created through the replacement, factories)`.
pub(crate) fn counts() -> (u32, u32) {
    (
        DEVICES.load(Ordering::Relaxed),
        FACTORIES.load(Ordering::Relaxed),
    )
}

type CreateDeviceFn =
    unsafe extern "system" fn(*mut c_void, i32, *const GUID, *mut *mut c_void) -> HRESULT;
type CreateFactory2Fn = unsafe extern "system" fn(u32, *const GUID, *mut *mut c_void) -> HRESULT;
type CreateFactory1Fn = unsafe extern "system" fn(*const GUID, *mut *mut c_void) -> HRESULT;

/// The real `D3D12CreateDevice`, for this crate's own device.
pub(crate) fn real_create_device() -> Option<CreateDeviceFn> {
    // SAFETY: the slot holds `D3D12CreateDevice` or a function with its
    // signature that a layer put there.
    original(Entry::Device)
        .map(|address| unsafe { std::mem::transmute::<usize, CreateDeviceFn>(address) })
}

unsafe extern "system" fn d3d12_create_device(
    adapter: *mut c_void,
    level: i32,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(create) = real_create_device() else {
        mhw_core::log_once!(
            Error,
            "render",
            "no-d3d12",
            "D3D12CreateDevice could not be resolved"
        );
        return NOT_FOUND;
    };
    let Some(_outermost) = Outermost::enter() else {
        // SAFETY: the original with the caller's own arguments.
        return unsafe { create(adapter, level, riid, out) };
    };
    let options = options();
    diag::enable(options);
    if !install::installed() {
        if let Err(error) = install::install() {
            log::error!(target: "render", "render hooks not installed: {error}");
        }
    }
    // SAFETY: the original with the caller's own arguments.
    let result = unsafe { create(adapter, level, riid, out) };
    // SAFETY: on success a non-null `out` holds the device.
    let device = unsafe { out.as_ref().map_or(std::ptr::null_mut(), |device| *device) };
    if result.is_ok() && !device.is_null() {
        let which = DEVICES.fetch_add(1, Ordering::Relaxed) + 1;
        reshade::detect_reshade();
        // SAFETY: a live device just created.
        let native = unsafe { reshade::unwrap_reshade(device) };
        log::info!(
            target: "render",
            "D3D12CreateDevice #{which} (level {level:#x}) -> device {native:p} on thread {}",
            mhw_win::current_thread_id()
        );
        // SAFETY: a live native device.
        unsafe { diag::watch(native, options) };
        if reshade::reshade_present() {
            // SAFETY: the live native device (every device interface
            // extends ID3D12Device); device creation is never under the
            // loader lock. A ReShade that wraps the device only after this
            // returns is caught at swapchain creation.
            unsafe {
                if let Some(device) = ID3D12Device::from_raw_borrowed(&native) {
                    proxylist::watch(device);
                }
            }
        }
    }
    result
}

fn factory_created(created: &FactoryCreated) {
    FACTORIES.fetch_add(1, Ordering::Relaxed);
    let hook = FACTORY_HOOK.load(Ordering::Acquire);
    if hook != 0 {
        // SAFETY: only `set_factory_hook` stores here, from a FactoryHook.
        let hook: FactoryHook = unsafe { std::mem::transmute::<usize, FactoryHook>(hook) };
        hook(created);
    }
}

unsafe extern "system" fn create_dxgi_factory2(
    flags: u32,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(address) = original(Entry::Factory2) else {
        mhw_core::log_once!(
            Error,
            "render",
            "no-dxgi2",
            "CreateDXGIFactory2 could not be resolved"
        );
        return NOT_FOUND;
    };
    // SAFETY: the resolved export or what a layer put in its place.
    let create = unsafe { std::mem::transmute::<usize, CreateFactory2Fn>(address) };
    let outermost = Outermost::enter();
    // SAFETY: the original with the caller's own arguments.
    let result = unsafe { create(flags, riid, out) };
    // SAFETY: on success a non-null `out` holds the factory.
    let produced = unsafe { out.as_ref().is_some_and(|factory| !factory.is_null()) };
    if outermost.is_some() && result.is_ok() && produced {
        reshade::detect_reshade();
        factory_created(&FactoryCreated {
            entry: FactoryEntry::Factory2,
            flags,
            riid,
            out,
        });
    }
    result
}

unsafe extern "system" fn create_dxgi_factory1(
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(address) = original(Entry::Factory1) else {
        mhw_core::log_once!(
            Error,
            "render",
            "no-dxgi1",
            "CreateDXGIFactory1 could not be resolved"
        );
        return NOT_FOUND;
    };
    // SAFETY: the resolved export or what a layer put in its place.
    let create = unsafe { std::mem::transmute::<usize, CreateFactory1Fn>(address) };
    let outermost = Outermost::enter();
    // SAFETY: the original with the caller's own arguments.
    let result = unsafe { create(riid, out) };
    // SAFETY: on success a non-null `out` holds the factory.
    let produced = unsafe { out.as_ref().is_some_and(|factory| !factory.is_null()) };
    if outermost.is_some() && result.is_ok() && produced {
        factory_created(&FactoryCreated {
            entry: FactoryEntry::Factory1,
            flags: 0,
            riid,
            out,
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intercepts_exactly_its_names_and_the_ordinal() {
        let device = intercept("D3D12CreateDevice").unwrap();
        assert_eq!(intercept_ordinal("D3D12.DLL", 101), Some(device));
        assert_eq!(intercept_ordinal("d3d12.dll", 102), None);
        assert_eq!(intercept_ordinal("dxgi.dll", 101), None);
        assert!(intercept("CreateDXGIFactory2").is_some());
        assert!(intercept("CreateDXGIFactory1").is_some());
        assert!(intercept("CreateDXGIFactory").is_none());
        assert!(!set_original("D3D12CreateDevice", device), "never our own");
        assert!(!set_original("D3D12CreateDevice", 0));
        assert!(!set_original("GetProcAddress", 0x1000));
    }

    #[test]
    fn only_the_outermost_call_is_marked() {
        let outer = Outermost::enter();
        assert!(outer.is_some());
        assert!(Outermost::enter().is_none());
        drop(outer);
        assert!(Outermost::enter().is_some());
    }
}
