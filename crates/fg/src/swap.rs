//! The factory and swapchain interception.
//!
//! The engine creates its swapchain **once**, with
//! `IDXGIFactory5::CreateSwapChainForHwnd` on its graphics queue
//! (`0x1423a63ab`: B8G8R8A8, 3 buffers, flip-discard, waitable, tearing when
//! supported), and from then on changes mode, size and HDR with
//! `ResizeBuffers` on the same chain (docs/re/ReFramePresent.md Q1). So:
//!
//! * [`upgrade_factory`] patches `CreateSwapChainForHwnd` on each factory
//!   the engine is handed (the mod calls it from render's factory
//!   registration point). Nothing is upgraded there: the game creates its
//!   factories before its device, and Streamline upgrades nothing before
//!   `slSetD3DDevice`.
//! * At the engine's creation call the device is taken from the queue (the
//!   first argument; its `GetDevice` is the engine's device by
//!   construction), given to Streamline, DLSS-G loaded and its functions
//!   bound, and only then is a counted copy of the factory upgraded and the
//!   chain created through the proxy. Upgrading when DLSS-G cannot run
//!   would make the engine render off-screen for nothing: a black screen.
//!   Any failure leaves the engine with DXGI's own chain.
//! * `Present` and `ResizeBuffers` are patched on the proxy chain's vtable.
//!   `ResizeBuffers` turns generation off first, then records the new size
//!   and format (HDR switches to R10G10B10A2) and resets history.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use mhw_core::frame::Size;
use mhw_win::slot;
use parking_lot::Mutex;
use windows::core::{Interface, HRESULT};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, ID3D12Device};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_UNKNOWN};
use windows::Win32::Graphics::Dxgi::{
    IDXGIFactory2, IDXGIFactory2_Vtbl, IDXGISwapChain1, IDXGISwapChain_Vtbl,
    DXGI_ERROR_INVALID_CALL, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FULLSCREEN_DESC,
};

use crate::abi;
use crate::frame;
use crate::sl;
use crate::status::ChainInfo;

const TARGET: &str = "swap";

/// How long the engine's factory creation waits for `slInit` started at
/// load (measured ~1.3 s); bounded so a Streamline that never settles costs
/// a slow start, not a hung game.
const READY_TIMEOUT: Duration = Duration::from_secs(8);

type CreateSwapChainForHwndFn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    HWND,
    *const DXGI_SWAP_CHAIN_DESC1,
    *const DXGI_SWAP_CHAIN_FULLSCREEN_DESC,
    *mut c_void,
    *mut *mut c_void,
) -> HRESULT;
type PresentFn = unsafe extern "system" fn(*mut c_void, u32, u32) -> HRESULT;
type ResizeBuffersFn =
    unsafe extern "system" fn(*mut c_void, u32, u32, u32, DXGI_FORMAT, u32) -> HRESULT;

/// One patched slot of one vtable and the function it held. Per vtable,
/// because a factory the engine is handed may be DXGI's or a proxy layer's
/// (ReShade), each with its own originals.
struct Patched {
    vtable: AtomicUsize,
    slot: AtomicUsize,
    original: AtomicUsize,
}

const PATCHES: usize = 16;
static PATCHED: [Patched; PATCHES] = [const {
    Patched {
        vtable: AtomicUsize::new(0),
        slot: AtomicUsize::new(0),
        original: AtomicUsize::new(0),
    }
}; PATCHES];
static PATCHING: Mutex<()> = Mutex::new(());

/// The original of `slot` for the vtable `this` uses; for an unknown
/// vtable (a layer that copied a patched slot), the first one patched.
///
/// # Safety
/// `this` must be a live COM object.
unsafe fn original_of(this: *mut c_void, slot: usize) -> usize {
    // SAFETY: a live COM object starts with its vtable pointer.
    let vtable = unsafe { mhw_win::vtable_of(this) } as usize;
    let known = || {
        PATCHED
            .iter()
            .take_while(|p| p.vtable.load(Ordering::Acquire) != 0)
    };
    known()
        .find(|p| {
            p.vtable.load(Ordering::Acquire) == vtable && p.slot.load(Ordering::Acquire) == slot
        })
        .or_else(|| known().find(|p| p.slot.load(Ordering::Acquire) == slot))
        .map_or(0, |p| p.original.load(Ordering::Acquire))
}

/// Patch `slot` of `vtable` unless it already is. The entry is complete
/// before the slot is written, so the hook finds its original from its very
/// first call.
///
/// # Safety
/// `vtable` must be a live vtable with more than `slot` slots and `hook` a
/// function with that method's exact signature.
unsafe fn patch(vtable: *mut usize, slot: usize, hook: *const c_void, name: &str) -> bool {
    let _one = PATCHING.lock();
    let mut free = None;
    for entry in &PATCHED {
        let known = entry.vtable.load(Ordering::Acquire);
        if known == 0 {
            free = Some(entry);
            break;
        }
        if known == vtable as usize && entry.slot.load(Ordering::Acquire) == slot {
            return true;
        }
    }
    let Some(entry) = free else {
        log::warn!(target: TARGET, "no room to record {name} on vtable {vtable:p}; left unhooked");
        return false;
    };
    // SAFETY: the caller guarantees the slot exists.
    let current = unsafe { vtable.add(slot).read() };
    if current == hook as usize {
        return true;
    }
    entry.slot.store(slot, Ordering::Release);
    entry.original.store(current, Ordering::Release);
    entry.vtable.store(vtable as usize, Ordering::Release);
    // SAFETY: as the caller guarantees; `original` already holds `current`.
    match unsafe { mhw_win::patch_vtable(vtable, slot, hook, &entry.original) } {
        Ok(()) => {
            log::info!(target: TARGET, "hooked {name} (slot {slot}, vtable {vtable:p})");
            true
        }
        Err(error) => {
            entry.vtable.store(0, Ordering::Release);
            log::warn!(target: TARGET, "could not hook {name}: {error}");
            false
        }
    }
}

/// The engine's swapchain.
static CHAIN: Mutex<Option<ChainInfo>> = Mutex::new(None);
/// Streamline's proxy of the engine's factory, and the factory it wraps.
static PROXY_FACTORY: Mutex<Option<(usize, usize)>> = Mutex::new(None);
/// The device was offered to Streamline (once).
static ADOPTED: AtomicBool = AtomicBool::new(false);

/// The engine's swapchain, once created.
pub fn chain() -> Option<ChainInfo> {
    *CHAIN.lock()
}

fn is_engine_chain(this: *mut c_void) -> bool {
    CHAIN
        .lock()
        .is_some_and(|chain| chain.swapchain == this as usize)
}

/// Put frame generation on the engine's swapchain creation: patch
/// `CreateSwapChainForHwnd` on `factory`'s vtable. Called by the mod for
/// every factory the engine is handed (render's factory registration
/// point), on the engine's thread, before the engine uses it.
///
/// Waits (bounded) for the `slInit` begun by [`start`](crate::start). Does
/// nothing and returns `false` when Streamline was not started or did not
/// initialise; the engine then keeps DXGI's chain.
///
/// # Safety
/// `factory` must be a live `IDXGIFactory2` (or later) object.
pub unsafe fn upgrade_factory(factory: *mut c_void) -> bool {
    if factory.is_null() || sl::init_state() == crate::status::InitState::NotStarted {
        return false;
    }
    if !sl::wait_settled(READY_TIMEOUT) {
        log::warn!(target: TARGET, "Streamline did not finish initialising in {READY_TIMEOUT:?}; the swapchain stays DXGI's");
        return false;
    }
    if !sl::ready() {
        return false;
    }
    let index = slot!(IDXGIFactory2_Vtbl, CreateSwapChainForHwnd);
    // SAFETY: a live factory; slot 15 is CreateSwapChainForHwnd on every
    // IDXGIFactory2+ implementation and the hook has its signature.
    unsafe {
        patch(
            mhw_win::vtable_of(factory),
            index,
            create_swap_chain_for_hwnd as *const c_void,
            "IDXGIFactory2::CreateSwapChainForHwnd",
        )
    }
}

thread_local! {
    /// Our nesting on the creation path: Streamline's proxy creates its own
    /// chain through the native factory, whose slot is ours, and that inner
    /// call must go straight to DXGI.
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

struct Nesting {
    outermost: bool,
}

impl Nesting {
    fn enter() -> Nesting {
        let depth = DEPTH.with(|d| {
            let next = d.get().saturating_add(1);
            d.set(next);
            next
        });
        Nesting {
            outermost: depth == 1,
        }
    }
}

impl Drop for Nesting {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Give Streamline the engine's device, load DLSS-G and bind its
/// functions, then upgrade a counted copy of `factory`. `None` (the engine
/// keeps DXGI's chain) whenever any step fails.
///
/// # Safety
/// `factory` must be a live `IDXGIFactory2`, `queue` the engine's live
/// `ID3D12CommandQueue` (or null).
unsafe fn streamline_factory(factory: *mut c_void, queue: *mut c_void) -> Option<*mut c_void> {
    if !sl::ready() {
        return None;
    }
    if !ADOPTED.swap(true, Ordering::SeqCst) {
        // SAFETY: the engine passes its live queue as the first argument.
        let queue = unsafe { ID3D12CommandQueue::from_raw_borrowed(&queue) }?;
        let mut found: Option<ID3D12Device> = None;
        // SAFETY: a live queue and a valid out slot.
        let device = match unsafe { queue.GetDevice(&mut found) } {
            Ok(()) => found,
            Err(error) => {
                log::warn!(target: TARGET, "the engine's queue has no device ({error}); no frame generation");
                return None;
            }
        }?;
        // SAFETY: a live device, kept alive below for the process.
        if !unsafe { sl::set_device(device.as_raw()) } {
            return None;
        }
        // Streamline keeps the pointer for the life of the process.
        std::mem::forget(device);
        sl::load_dlssg();
    }
    if !sl::device_set() || !sl::bind_features() {
        mhw_core::log_once!(
            Warn,
            TARGET,
            "no-dlssg",
            "DLSS-G is not available (see the [sl] lines), so the factory is not upgraded and \
             the engine presents through DXGI"
        );
        return None;
    }
    if let Some((proxy, native)) = *PROXY_FACTORY.lock() {
        if native == factory as usize {
            return Some(proxy as *mut c_void);
        }
    }
    // SAFETY: a live factory.
    let borrowed = unsafe { IDXGIFactory2::from_raw_borrowed(&factory) }?;
    // A reference of our own: only the copy is upgraded (in place), the
    // engine keeps its factory for everything else.
    let mut upgraded = borrowed.clone().into_raw();
    // SAFETY: `upgraded` holds a reference we own.
    let result = unsafe { sl::upgrade(&mut upgraded) };
    log::info!(target: TARGET, "slUpgradeInterface(factory {factory:p}) -> {} ({upgraded:p})", abi::result_name(result));
    if result != abi::OK || upgraded.is_null() {
        return None;
    }
    *PROXY_FACTORY.lock() = Some((upgraded as usize, factory as usize));
    Some(upgraded)
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn create_swap_chain_for_hwnd(
    this: *mut c_void,
    queue: *mut c_void,
    window: HWND,
    desc: *const DXGI_SWAP_CHAIN_DESC1,
    fullscreen: *const DXGI_SWAP_CHAIN_FULLSCREEN_DESC,
    output: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    let index = slot!(IDXGIFactory2_Vtbl, CreateSwapChainForHwnd);
    // SAFETY: entered through a patched vtable, so `this` is a live factory.
    let original = unsafe { original_of(this, index) };
    if original == 0 {
        return DXGI_ERROR_INVALID_CALL;
    }
    // SAFETY: the function this slot held, with this exact signature.
    let original: CreateSwapChainForHwndFn = unsafe { std::mem::transmute(original) };
    let nesting = Nesting::enter();
    if !nesting.outermost {
        // SAFETY: forwarding the caller's own arguments.
        return unsafe { original(this, queue, window, desc, fullscreen, output, out) };
    }
    // SAFETY: the engine's live factory and queue.
    let proxy = unsafe { streamline_factory(this, queue) };
    let (result, proxied) = match proxy {
        Some(proxy) => {
            // SAFETY: a live proxy factory; same slot, same signature.
            let result = unsafe {
                let create: CreateSwapChainForHwndFn =
                    std::mem::transmute(mhw_win::vtable_of(proxy).add(index).read());
                create(proxy, queue, window, desc, fullscreen, output, out)
            };
            if result.is_ok() {
                (result, true)
            } else {
                log::warn!(target: TARGET, "Streamline's CreateSwapChainForHwnd failed ({result:?}); creating DXGI's chain");
                // SAFETY: forwarding the caller's own arguments.
                (
                    unsafe { original(this, queue, window, desc, fullscreen, output, out) },
                    false,
                )
            }
        }
        // SAFETY: forwarding the caller's own arguments.
        None => (
            unsafe { original(this, queue, window, desc, fullscreen, output, out) },
            false,
        ),
    };
    if result.is_err() || out.is_null() || desc.is_null() {
        return result;
    }
    // SAFETY: on success `out` holds the new chain; `desc` is the caller's.
    let (made, desc) = unsafe { (*out, *desc) };
    if made.is_null() {
        return result;
    }
    let info = ChainInfo {
        swapchain: made as usize,
        window: window.0 as isize,
        size: Size::new(desc.Width, desc.Height),
        format: desc.Format.0 as u32,
        buffers: desc.BufferCount,
        flags: desc.Flags,
        proxied,
    };
    log::info!(
        target: TARGET,
        "swapchain {made:p}: {}x{} format {} buffers {} flags {:#x}, {}",
        info.size.w,
        info.size.h,
        info.format,
        info.buffers,
        info.flags,
        if proxied {
            "Streamline's proxy: frame generation presents"
        } else {
            "DXGI's own: the engine presents"
        }
    );
    *CHAIN.lock() = Some(info);
    if proxied {
        // SAFETY: a live swapchain; the hooks have the methods' signatures.
        unsafe {
            let vtable = mhw_win::vtable_of(made);
            patch(
                vtable,
                slot!(IDXGISwapChain_Vtbl, Present),
                present as *const c_void,
                "IDXGISwapChain::Present",
            );
            patch(
                vtable,
                slot!(IDXGISwapChain_Vtbl, ResizeBuffers),
                resize_buffers as *const c_void,
                "IDXGISwapChain::ResizeBuffers",
            );
        }
    }
    frame::activate(proxied);
    result
}

/// Set while the engine's `Present` is on the stack: a nested present
/// (from a mode change inside it) is not a frame boundary.
static IN_PRESENT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "system" fn present(this: *mut c_void, interval: u32, flags: u32) -> HRESULT {
    // SAFETY: entered through the patched vtable of a live swapchain.
    let original = unsafe { original_of(this, slot!(IDXGISwapChain_Vtbl, Present)) };
    if original == 0 {
        return DXGI_ERROR_INVALID_CALL;
    }
    // SAFETY: the function this slot held, with this exact signature.
    let original: PresentFn = unsafe { std::mem::transmute(original) };
    let chain = chain().filter(|c| c.swapchain == this as usize);
    let outermost = IN_PRESENT.fetch_add(1, Ordering::AcqRel) == 0;
    let frame = outermost && chain.is_some();
    if let (true, Some(chain)) = (frame, chain) {
        frame::before_present(chain.window, chain.size, chain.format);
    }
    // SAFETY: forwarding the caller's own arguments.
    let result = unsafe { original(this, interval, flags) };
    if frame {
        frame::after_present();
    }
    IN_PRESENT.fetch_sub(1, Ordering::AcqRel);
    result
}

unsafe extern "system" fn resize_buffers(
    this: *mut c_void,
    count: u32,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    flags: u32,
) -> HRESULT {
    // SAFETY: entered through the patched vtable of a live swapchain.
    let original = unsafe { original_of(this, slot!(IDXGISwapChain_Vtbl, ResizeBuffers)) };
    if original == 0 {
        return DXGI_ERROR_INVALID_CALL;
    }
    // SAFETY: the function this slot held, with this exact signature.
    let original: ResizeBuffersFn = unsafe { std::mem::transmute(original) };
    let engine = is_engine_chain(this);
    if engine {
        frame::before_resize();
    }
    // SAFETY: forwarding the caller's own arguments.
    let result = unsafe { original(this, count, width, height, format, flags) };
    if engine {
        log::info!(
            target: TARGET,
            "ResizeBuffers({count}, {width}x{height}, format {}, flags {flags:#x}) -> {result:?}",
            format.0
        );
        if result.is_ok() {
            // Zero sizes and UNKNOWN format mean "keep / take the window's":
            // read back what the chain now is rather than what was asked.
            // SAFETY: a live swapchain (this call just succeeded on it).
            let described = unsafe {
                IDXGISwapChain1::from_raw_borrowed(&this).and_then(|chain| chain.GetDesc1().ok())
            };
            if let Some(chain) = CHAIN.lock().as_mut() {
                match described {
                    Some(desc) => {
                        chain.size = Size::new(desc.Width, desc.Height);
                        chain.format = desc.Format.0 as u32;
                        chain.buffers = desc.BufferCount;
                        chain.flags = desc.Flags;
                    }
                    None => {
                        if width != 0 && height != 0 {
                            chain.size = Size::new(width, height);
                        }
                        if format != DXGI_FORMAT_UNKNOWN {
                            chain.format = format.0 as u32;
                        }
                        chain.flags = flags;
                    }
                }
            }
        }
    }
    result
}
