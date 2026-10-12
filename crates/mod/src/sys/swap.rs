//! The engine's factory, swapchain creation and Present, seen from the mod.
//!
//! The engine creates its swapchain once (`IDXGIFactory5::CreateSwapChainForHwnd`
//! on its graphics queue) and presents it from the main loop's thread
//! (`ReFramePresent.md` §1, §3a). The mod needs three things there, with or
//! without frame generation: the queue (render's engine-queue
//! identification), the moment before each Present (HUD-less tag, wait
//! latch, render's present clock, the dev `present` point), and the moment
//! after it (the dev `frame` point: the next main-loop iteration starts
//! with the wait helper right after Present returns).
//!
//! Patched vtable slots, outermost: frame generation patches the same
//! factory slot first (`fg::upgrade_factory`) and its proxy chain's
//! Present while creating; the mod patches after it, so the mod's hooks run
//! before frame generation's. Nested creations (a proxy factory creating
//! the real chain through the patched DXGI vtable) pass straight through.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

use mhw_render::FactoryCreated;
use mhw_win::slot;
use parking_lot::Mutex;
use windows::core::HRESULT;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dxgi::{
    IDXGIFactory2_Vtbl, IDXGISwapChain_Vtbl, DXGI_ERROR_INVALID_CALL, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_CHAIN_FULLSCREEN_DESC,
};

use super::{fg_glue, overlay, runtime};

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

/// One patched slot of one vtable and the function it held (a factory or
/// chain may be DXGI's or a layer's, each with its own originals).
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

/// The engine's swapchain as the engine holds it.
static CHAIN: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static CREATING: Cell<u32> = const { Cell::new(0) };
}

/// The original of `slot` for `this`'s vtable (for an unknown vtable — a
/// layer that copied a patched slot — the first one recorded).
///
/// # Safety
/// `this` must be a live COM object.
unsafe fn original_of(this: *mut c_void, slot: usize) -> usize {
    // SAFETY: caller's contract.
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

/// Patch `slot` of `vtable` with `hook` unless done already.
///
/// # Safety
/// `vtable` must be live with more than `slot` slots; `hook` has the
/// method's exact signature.
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
        log::warn!(target: "swap", "no room to record {name} on vtable {vtable:p}; left unhooked");
        return false;
    };
    entry.slot.store(slot, Ordering::Release);
    entry.vtable.store(vtable as usize, Ordering::Release);
    // SAFETY: caller's contract; `entry.original` is filled before the slot
    // is written.
    match unsafe { mhw_win::patch_vtable(vtable, slot, hook, &entry.original) } {
        Ok(()) => {
            log::info!(target: "swap", "hooked {name} (vtable {vtable:p})");
            true
        }
        Err(error) => {
            entry.vtable.store(0, Ordering::Release);
            log::warn!(target: "swap", "could not hook {name}: {error}");
            false
        }
    }
}

/// render's factory registration point: frame generation first, then the
/// mod's own creation hook on top.
pub(crate) fn on_factory(created: &FactoryCreated) {
    // SAFETY: render hands the slot the factory was just returned in.
    let factory = unsafe { created.out.as_ref() }
        .copied()
        .unwrap_or(std::ptr::null_mut());
    if factory.is_null() {
        return;
    }
    if runtime::fg_started() {
        // SAFETY: a live factory the game asked for (IDXGIFactory2+ on the
        // DX12 path).
        let upgraded = unsafe { mhw_fg::upgrade_factory(factory) };
        log::debug!(target: "swap", "factory {factory:p}: frame generation hook {upgraded}");
    }
    if created.entry != mhw_render::FactoryEntry::Factory2 {
        return;
    }
    // SAFETY: a live IDXGIFactory2+; slot index derived from the vtable
    // layout and the hook has the method's signature.
    unsafe {
        patch(
            mhw_win::vtable_of(factory),
            slot!(IDXGIFactory2_Vtbl, CreateSwapChainForHwnd),
            create_swap_chain_for_hwnd as *const c_void,
            "IDXGIFactory2::CreateSwapChainForHwnd",
        );
    }
}

struct Creating;

impl Creating {
    fn enter() -> (Creating, bool) {
        let outer = CREATING.with(|depth| {
            let now = depth.get();
            depth.set(now + 1);
            now == 0
        });
        (Creating, outer)
    }
}

impl Drop for Creating {
    fn drop(&mut self) {
        CREATING.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

unsafe extern "system" fn create_swap_chain_for_hwnd(
    this: *mut c_void,
    device: *mut c_void,
    window: HWND,
    desc: *const DXGI_SWAP_CHAIN_DESC1,
    fullscreen: *const DXGI_SWAP_CHAIN_FULLSCREEN_DESC,
    output: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: `this` is the live factory DXGI called us for.
    let original = unsafe { original_of(this, slot!(IDXGIFactory2_Vtbl, CreateSwapChainForHwnd)) };
    if original == 0 {
        return DXGI_ERROR_INVALID_CALL;
    }
    // SAFETY: recorded from this very slot; same signature.
    let original: CreateSwapChainForHwndFn = unsafe { std::mem::transmute(original) };
    let (_guard, outer) = Creating::enter();
    if outer {
        // SAFETY: the first argument of a D3D12 chain creation is the
        // queue; render checks what it is.
        let accepted = unsafe { mhw_render::note_presenting_queue(device) };
        log::info!(target: "swap", "swapchain creation on queue {device:p} (engine queue {accepted})");
        mhw_render::detect_reshade();
        // ReShade's command-list proxies (NGX must get the proxy list);
        // render tries at device creation already, this is where 0.2 did
        // it. Needs the engine queue noted above.
        mhw_render::watch_proxy_lists();
        overlay::register_once();
    }
    // SAFETY: the game's own arguments, forwarded.
    let result = unsafe { original(this, device, window, desc, fullscreen, output, out) };
    if outer && result.is_ok() {
        // SAFETY: on success `out` holds the new chain.
        let chain = unsafe { out.as_ref() }
            .copied()
            .unwrap_or(std::ptr::null_mut());
        if !chain.is_null() {
            CHAIN.store(chain as usize, Ordering::Release);
            // SAFETY: a live swapchain; Present's index from the vtable
            // layout and the hook has its signature.
            unsafe {
                patch(
                    mhw_win::vtable_of(chain),
                    slot!(IDXGISwapChain_Vtbl, Present),
                    present as *const c_void,
                    "IDXGISwapChain::Present",
                );
            }
        }
    }
    result
}

unsafe extern "system" fn present(this: *mut c_void, interval: u32, flags: u32) -> HRESULT {
    // SAFETY: `this` is the live chain being presented.
    let original = unsafe { original_of(this, slot!(IDXGISwapChain_Vtbl, Present)) };
    if original == 0 {
        return DXGI_ERROR_INVALID_CALL;
    }
    // SAFETY: recorded from this very slot; same signature.
    let original: PresentFn = unsafe { std::mem::transmute(original) };
    if this as usize != CHAIN.load(Ordering::Acquire) {
        // SAFETY: forwarded unchanged.
        return unsafe { original(this, interval, flags) };
    }
    let submitted = fg_glue::before_present();
    // The engine's render frame: what this frame's evaluations were filed
    // under, whether or not frame generation runs (its submitted frame
    // only moves with frame generation, and was 0 without it: the retire
    // clock then counted every evaluate/Present alternation as a frame).
    let frame = render_frame().unwrap_or(submitted);
    // The DLSS retire queue ages by distinct frames, evaluated or not.
    mhw_ngx::feature::tick(frame);
    #[cfg(feature = "dev")]
    mhw_dev::on_present(frame);
    mhw_render::note_present();
    // SAFETY: forwarded unchanged.
    let result = unsafe { original(this, interval, flags) };
    #[cfg(feature = "dev")]
    mhw_dev::on_frame(render_frame().unwrap_or(frame));
    result
}

/// The engine's render frame counter, once the gate has passed.
fn render_frame() -> Option<u64> {
    mhw_engine::addresses()
        .and_then(mhw_engine::views::RenderConfig::get)
        .and_then(|config| config.render_frame())
        .map(u64::from)
}
