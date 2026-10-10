//! Standing between the engine and its swapchain, changing nothing.
//!
//! Frame generation has to present frames the engine never rendered, so
//! something of ours has to sit on the presentation path. This is that
//! something, installed and proven to be transparent before anything is
//! inserted through it: every hook here calls straight through, and the only
//! other thing any of them does is count and describe what passed.
//!
//! ## Why vtable slots and not a wrapper object
//!
//! The obvious shape is a COM wrapper: substitute `CreateDXGIFactory2`, hand
//! the engine an object of ours, forward every method. It is not used, for
//! three reasons that are specific to this game rather than stylistic:
//!
//! * **The engine calls through interfaces it never asked for.** It requests
//!   `IDXGIFactory2` and then calls `IDXGIFactory5::CheckFeatureSupport`
//!   (slot 28) on the same pointer. DXGI gets away with that because all of
//!   `IDXGIFactory` through `IDXGIFactory7` are one derivation chain served by
//!   one vtable. A wrapper has to reproduce that whole vtable by hand -- about
//!   seventy thunks across the factory and the swapchain, each with a
//!   signature that is right or corrupts the stack -- and get it right before
//!   anything can be tested at all.
//! * **The swapchain is destroyed and recreated from inside `Present`**, on
//!   resolution and fullscreen changes, re-entrantly through any hook on that
//!   path. A wrapper has to keep an identity across that. Patched slots do
//!   not: the vtable is shared, so a swapchain created a moment ago already
//!   runs through the same hooks, with nothing to re-wrap and no identity to
//!   maintain.
//! * It is the technique the rest of this library already uses for D3D12, and
//!   `vtable::hook` derives every slot index with `offset_of!`, so an
//!   interface gaining a method cannot silently shift one.
//!
//! What it gives up is substitution: we cannot hand the engine a swapchain
//! What it gives up is substitution: this module cannot hand the engine a
//! swapchain that is not DXGI's. Frame generation does need one, and gets it
//! from Streamline, whose proxy factory creates the chain (`streamline.rs`);
//! the hooks here then sit on Streamline's vtables as they do on DXGI's.
//! ## What is shared, and the consequence
//!
//! Because the vtables are shared process-wide, these hooks see every
//! swapchain in the process -- ours, an overlay's, ReShade's. So the engine's
//! is identified rather than assumed: a swapchain is only interesting if it
//! came out of the `CreateSwapChainForHwnd` hook, and the set of those is
//! recorded. Everything else is passed through without even a count.

use parking_lot::Mutex;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use windows::core::{Interface, HRESULT};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, IDXGIFactory2, IDXGIFactory2_Vtbl, IDXGISwapChain1, IDXGISwapChain1_Vtbl,
    IDXGISwapChain2, IDXGISwapChain2_Vtbl, IDXGISwapChain_Vtbl, DXGI_CREATE_FACTORY_FLAGS,
    DXGI_PRESENT_PARAMETERS, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FULLSCREEN_DESC,
};

use crate::slot;

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
type Present1Fn =
    unsafe extern "system" fn(*mut c_void, u32, u32, *const DXGI_PRESENT_PARAMETERS) -> HRESULT;
type ResizeBuffersFn =
    unsafe extern "system" fn(*mut c_void, u32, u32, u32, DXGI_FORMAT, u32) -> HRESULT;
type GetBufferFn = unsafe extern "system" fn(
    *mut c_void,
    u32,
    *const windows::core::GUID,
    *mut *mut c_void,
) -> HRESULT;

static O_CREATE_SWAPCHAIN: AtomicUsize = AtomicUsize::new(0);

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// One patched slot of one swapchain vtable, and what it held.
///
/// Per vtable, because there is more than one: every DXGI swapchain shares
/// one, but Streamline's proxy is a different implementation with a vtable
/// of its own. Slots patched once, on the first swapchain's vtable, would
/// leave whichever implementation came second unhooked and its presents
/// unseen.
struct Patched {
    vtable: AtomicUsize,
    slot: AtomicUsize,
    original: AtomicUsize,
}

/// Room for four implementations' worth of the five slots.
const PATCHES: usize = 20;
static PATCHED: [Patched; PATCHES] = [const {
    Patched {
        vtable: AtomicUsize::new(0),
        slot: AtomicUsize::new(0),
        original: AtomicUsize::new(0),
    }
}; PATCHES];
/// Held while a patch is being added, so two creations cannot take one entry.
static PATCHING: Mutex<()> = Mutex::new(());

/// What the slot `slot` of the vtable `this` is using held before it was
/// patched. A few relaxed loads, once per call of a hook that runs at most
/// a few times a frame.
///
/// # Safety
/// `this` must be a live COM object.
unsafe fn original_of(this: *mut c_void, slot: usize) -> usize {
    let vtable = *(this as *const usize);
    for patch in &PATCHED {
        let known = patch.vtable.load(Ordering::Relaxed);
        if known == 0 {
            break;
        }
        if known == vtable && patch.slot.load(Ordering::Relaxed) == slot {
            return patch.original.load(Ordering::Relaxed);
        }
    }
    // Entered through a vtable this table does not know: something above
    // took a copy of a patched slot into a table of its own. The same slot
    // of the first vtable patched is then the best answer there is.
    PATCHED
        .iter()
        .take_while(|patch| patch.vtable.load(Ordering::Relaxed) != 0)
        .find(|patch| patch.slot.load(Ordering::Relaxed) == slot)
        .map(|patch| patch.original.load(Ordering::Relaxed))
        .unwrap_or(0)
}

/// Patch one slot of a swapchain vtable, unless it already is.
///
/// The entry is filled in before the slot is written, original first, so the
/// hook finds its way on from its very first call.
unsafe fn patch_slot(vtable: *mut *mut c_void, slot: usize, hook: *const c_void, name: &str) {
    let _one_at_a_time = PATCHING.lock();
    let mut free = None;
    for patch in &PATCHED {
        let known = patch.vtable.load(Ordering::Relaxed);
        if known == 0 {
            free = Some(patch);
            break;
        }
        if known == vtable as usize && patch.slot.load(Ordering::Relaxed) == slot {
            return;
        }
    }
    let Some(patch) = free else {
        crate::log!(
            "[swap] no room to record another patched swapchain slot ({name} on vtable \
             {vtable:p}); it is left unhooked"
        );
        return;
    };
    if std::ptr::eq(*vtable.add(slot), hook) {
        // Ours already, by a route this table did not record. Nothing here
        // knows its original, so it is not touched.
        return;
    }
    // The original and the slot number first, the vtable last: `original_of`
    // reads an entry only once its vtable is set.
    patch.slot.store(slot, Ordering::SeqCst);
    patch
        .original
        .store(*vtable.add(slot) as usize, Ordering::SeqCst);
    patch.vtable.store(vtable as usize, Ordering::SeqCst);
    if crate::vtable::hook(vtable, slot, hook, &patch.original) {
        crate::log!("[swap] hooked slot {slot:>3}  {name} (vtable {vtable:p})");
    } else {
        crate::log!("[swap] FAILED slot {slot:>3}  {name} (vtable {vtable:p})");
    }
}

/// Swapchains that came out of the factory hook, with the window each was
/// made for, so an overlay's is not mistaken for the engine's.
///
/// A window has one swapchain at a time, so creating one for a window means
/// the one before it is gone, and it is forgotten here. A set that only
/// grew would keep the address of a swapchain the engine had destroyed, and
/// whatever was next allocated there -- Streamline's own inner chain, an
/// overlay's -- would be taken for the engine's.
static OURS: Mutex<Vec<(usize, isize)>> = Mutex::new(Vec::new());

/// The one the engine is presenting, and what it was made for.
static CURRENT: Mutex<Option<Chain>> = Mutex::new(None);

#[derive(Clone, Copy)]
pub struct Chain {
    pub swapchain: usize,
    pub window: isize,
    pub size: (u32, u32),
    pub format: DXGI_FORMAT,
    pub buffers: u32,
    pub flags: u32,
    pub swap_effect: i32,
}

static PRESENTS: AtomicU64 = AtomicU64::new(0);
static CREATED: AtomicU64 = AtomicU64::new(0);
static RESIZES: AtomicU64 = AtomicU64::new(0);
/// Set while a `Present` of ours is on the stack, because the engine destroys
/// and recreates the swapchain from inside its own present path and that
/// re-enters every hook here.
static IN_PRESENT: AtomicU64 = AtomicU64::new(0);

/// What the engine is presenting into, once it has made one.
pub fn chain() -> Option<Chain> {
    *CURRENT.lock()
}

/// Whether a swapchain is the engine's rather than someone else's.
fn is_ours(swapchain: usize) -> bool {
    OURS.lock().iter().any(|(known, _)| *known == swapchain)
}

/// Record the engine's new swapchain for `window`, forgetting any it had
/// there before. Returns how many were forgotten.
fn remember(swapchain: usize, window: isize) -> usize {
    let mut ours = OURS.lock();
    let before = ours.len();
    ours.retain(|(known, its_window)| *its_window != window && *known != swapchain);
    let forgotten = before - ours.len();
    ours.push((swapchain, window));
    forgotten
}

/// Put ourselves on the presentation path.
///
/// A factory of our own is made purely to read the vtable out of: DXGI serves
/// the whole `IDXGIFactory` chain from one vtable shared by every instance, so
/// the slot patched through ours is the slot the engine calls. That also means
/// this does not have to beat the engine to `CreateDXGIFactory2`, which is
/// resolved dynamically and cached in a field of the engine's device -- a
/// pointer we would otherwise have to find and rewrite.
pub fn install() -> bool {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return O_CREATE_SWAPCHAIN.load(Ordering::Relaxed) != 0;
    }
    // This is the diagnostic path, and it races the engine: the game resolves
    // `CreateDXGIFactory2` early and may well have created its swapchain
    // before this runs, in which case the creation goes unseen and only the
    // presents are counted. Frame generation does not use it -- it owns the
    // creation call instead, through `proxy::CreateDXGIFactory2` and
    // `watch_factory`.
    unsafe {
        let factory: IDXGIFactory2 = match CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) {
            Ok(factory) => factory,
            Err(error) => {
                crate::log!("[swap] no factory of our own to read a vtable from: {error}");
                return false;
            }
        };
        // DXGI's vtable, whatever answered. This runs ten milliseconds after
        // injection, long before anything else is in the process, so the
        // factory is DXGI's own -- but if something did get in first and
        // handed back a proxy, patching the proxy's vtable would put this hook
        // on a different layer than every run so far has had.
        crate::reshade::detect();
        static OWN_FACTORY: crate::reshade::Boundary = crate::reshade::Boundary::new(
            "the factory of our own that the swapchain slot is read from",
            "the slot is patched on the native factory's vtable",
        );
        let factory = OWN_FACTORY.native_interface(factory);
        let vtable = crate::vtable::of(&factory);
        let index = slot!(IDXGIFactory2_Vtbl, CreateSwapChainForHwnd);
        if crate::vtable::hook(
            vtable,
            index,
            hook_create_swapchain_for_hwnd as *const c_void,
            &O_CREATE_SWAPCHAIN,
        ) {
            crate::log!(
                "[swap] hooked slot {index} Factory2::CreateSwapChainForHwnd (vtable {vtable:p})"
            );
            true
        } else {
            crate::log!("[swap] FAILED to hook Factory2::CreateSwapChainForHwnd");
            false
        }
    }
}

/// Patch a swapchain's own slots, on whichever vtable it has.
///
/// Deferred for the same reason the factory's is not: there is no swapchain
/// to read a vtable from until the engine makes one. Called for every chain
/// the engine creates. A vtable that has been patched already is found in
/// `PATCHED` and left alone, which is the usual case: the chains the engine
/// builds when the resolution changes share the first one's.
unsafe fn hook_chain(swapchain: *mut c_void) {
    let Some(chain) = IDXGISwapChain1::from_raw_borrowed(&swapchain) else {
        crate::log!("[swap] the new swapchain is not an IDXGISwapChain1");
        return;
    };
    let vtable = crate::vtable::of(chain);
    let sites: [(&str, usize, *const c_void); 4] = [
        (
            "SwapChain::Present",
            slot!(IDXGISwapChain_Vtbl, Present),
            hook_present as *const c_void,
        ),
        (
            "SwapChain1::Present1",
            slot!(IDXGISwapChain1_Vtbl, Present1),
            hook_present1 as *const c_void,
        ),
        (
            "SwapChain::ResizeBuffers",
            slot!(IDXGISwapChain_Vtbl, ResizeBuffers),
            hook_resize_buffers as *const c_void,
        ),
        (
            "SwapChain::GetBuffer",
            slot!(IDXGISwapChain_Vtbl, GetBuffer),
            hook_get_buffer as *const c_void,
        ),
    ];
    for (name, index, replacement) in sites {
        patch_slot(vtable, index, replacement, name);
    }

    // IDXGISwapChain2's own slot, which is where the three frames of queued
    // latency come from. Through its own interface pointer: the index is
    // past IDXGISwapChain1's methods, and an implementation is free to
    // answer that interface from a different vtable.
    if let Ok(two) = chain.cast::<IDXGISwapChain2>() {
        patch_slot(
            crate::vtable::of(&two),
            slot!(IDXGISwapChain2_Vtbl, SetMaximumFrameLatency),
            hook_set_maximum_frame_latency as *const c_void,
            "SwapChain2::SetMaximumFrameLatency",
        );
    }
}

/// Shorten the engine's latency queue, when the configuration asks.
///
/// The engine asks for three, which is a reasonable choice for a game that
/// presents once per rendered frame and a poor one for anything trying to pace
/// presentation: three frames of queued work is three frames of input lag that
/// no amount of interpolation can pay back. Clamping it here rather than
/// patching `renderMgr` keeps it a one-line, reversible change.
unsafe extern "system" fn hook_set_maximum_frame_latency(
    this: *mut c_void,
    latency: u32,
) -> HRESULT {
    let original: SetMaximumFrameLatencyFn = std::mem::transmute(original_of(
        this,
        slot!(IDXGISwapChain2_Vtbl, SetMaximumFrameLatency),
    ));
    let wanted = crate::config::get().fg_max_latency;
    if wanted == 0 || !is_ours(this as usize) {
        return original(this, latency);
    }
    let clamped = latency.min(wanted);
    if clamped != latency && !SAID_LATENCY.swap(true, Ordering::Relaxed) {
        crate::log!("[swap] the engine asked for {latency} frames of latency; giving it {clamped}");
    }
    original(this, clamped)
}

static SAID_LATENCY: AtomicBool = AtomicBool::new(false);
type SetMaximumFrameLatencyFn = unsafe extern "system" fn(*mut c_void, u32) -> HRESULT;

/// Our own nesting on the swapchain-creation path.
///
/// With the factory upgraded, our hook calls Streamline's proxy; the proxy
/// calls `CreateSwapChainForHwnd` on the native factory -- whose vtable slot
/// is the one patched here -- and so this hook is entered again. Handing
/// that inner call the proxy again would go round until the stack ran out:
/// Streamline's log shows it as hundreds of
/// `swapChainData.cpp[createSwapChain]` lines microseconds apart.
///
/// Held in a thread-local because the recursion is within one call stack on
/// one thread, and a process-wide flag would make a second thread's honest
/// creation look like a re-entry.
struct Creating {
    outermost: bool,
}

thread_local! {
    static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// How deep we will go before refusing to route through Streamline at all.
///
/// The guard below already caps the depth at two, so this should never fire.
/// It is here anyway: on any path where this library calls something that
/// calls it back, a bound turns a dead process into a line in the log.
const MAX_DEPTH: u32 = 10;
static GAVE_UP: AtomicBool = AtomicBool::new(false);

impl Creating {
    fn enter() -> Creating {
        let depth = DEPTH.with(|depth| {
            let next = depth.get() + 1;
            depth.set(next);
            next
        });
        if depth > MAX_DEPTH && !GAVE_UP.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[swap] our own swapchain hook has been entered {depth} times without \
                 unwinding, which is a loop rather than a workload. Nothing more will \
                 be routed through Streamline and every creation from here on goes \
                 straight to DXGI"
            );
        }
        Creating {
            outermost: depth == 1,
        }
    }
}

impl Drop for Creating {
    fn drop(&mut self) {
        DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

unsafe extern "system" fn hook_create_swapchain_for_hwnd(
    this: *mut c_void,
    device: *mut c_void,
    window: HWND,
    description: *const DXGI_SWAP_CHAIN_DESC1,
    fullscreen: *const DXGI_SWAP_CHAIN_FULLSCREEN_DESC,
    restrict_to: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    let original: CreateSwapChainForHwndFn =
        std::mem::transmute(O_CREATE_SWAPCHAIN.load(Ordering::Relaxed));

    // Bound to a variable. `Creating::enter()` returns a guard whose `Drop`
    // unwinds the depth, so writing `Creating::enter().outermost` would
    // create it and release it in the same statement and guard nothing.
    let creating = Creating::enter();
    if !creating.outermost {
        // Streamline, inside the chain we asked it to build, creating its own.
        // Forwarded straight to DXGI and nothing else: this is not the
        // engine's creation, so it must not re-decide the routing, must not
        // claim the queue as the engine's, and must not be counted as a chain
        // the engine presents through.
        //
        //
        // The queue is the visible half of that. Streamline's internal
        // creation presents a queue of its own, and recording that one would
        // replace the engine's identity (`[queue] the engine's graphics queue
        // is now ..., was ...`). The engine's queue is the one from the
        // engine's own call, so the engine's call is the only one that may
        // record it.
        return original(
            this,
            device,
            window,
            description,
            fullscreen,
            restrict_to,
            out,
        );
    }

    // A module walk, once per chain the engine creates. This is the last
    // moment at which finding ReShade is still in time: the queue below has to
    // be recorded under its native address or `hook_execute_command_lists`,
    // which is entered with native pointers, never recognises it.
    crate::reshade::detect();

    // Before the call, because this is the one place the engine's graphics
    // queue is identified rather than guessed at: in D3D12 the first argument
    // is the queue the swapchain presents from. Recorded even if the creation
    // then fails, since the argument is what it is either way.
    //
    // Under its native address. With ReShade loaded the engine holds a proxy
    // queue, and `gpu::hook_execute_command_lists` sits on the native vtable
    // and is entered with the native pointer, so a proxy recorded here would
    // never be recognised and no frame would ever be marked.
    static QUEUE: crate::reshade::Boundary = crate::reshade::Boundary::new(
        "the queue the engine creates its swapchain with",
        "it is recorded as the engine's graphics queue under the native address",
    );
    let presenting = QUEUE.native(device);
    crate::queue::note_presenting(presenting);
    adopt_device_for_our_resources(presenting);

    // And the device Streamline is given, for the same reason and from the
    // same argument. Not the device seen by `proxy::D3D12CreateDevice`: the
    // engine may resolve its own creation call before the patches land, so
    // the only device that function is certain to see is this mod's own
    // scratch device.
    //
    // In D3D12 the first argument to `CreateSwapChainForHwnd` is the queue
    // the chain presents from, so its `GetDevice` is the engine's real
    // device by construction -- the same reasoning `queue.rs` uses, and the
    // same argument. And it is before the chain is created, which is the
    // ordering Streamline requires: the factory cannot be upgraded after the
    // engine already has its swapchain.
    adopt_engine_device(device);

    // And this is where frame generation is decided, rather than at factory
    // creation. Two reasons, and the second is the one that matters to the
    // picture:
    //
    //   * this game creates its factories *before* its device -- three of
    //     them -- and Streamline refuses to upgrade anything until the device
    //     has been set, so an upgrade attempted at factory creation can only
    //     ever be refused;
    //   * by now it is known whether DLSS-G actually initialised.
    //     `streamline::device_ready` is exactly that question: the device set
    //     *and* the feature functions bound. A missing DLSS-G leaves it false.
    //
    // Upgrading the factory is what puts Streamline's proxy swapchain on the
    // presentation path and makes the engine render off-screen, and doing
    // that when the feature cannot run is a black screen with a HUD. If
    // DLSS-G is not there, nothing is upgraded, the engine gets DXGI's own
    // swapchain, and the picture is exactly what it is with frame generation
    // off.
    let through = if GAVE_UP.load(Ordering::Relaxed) {
        None
    } else {
        streamline_factory(this)
    };
    let result = match through {
        Some(proxy) => {
            let create: CreateSwapChainForHwndFn = std::mem::transmute(
                *(*(proxy as *const *const *const c_void))
                    .add(slot!(IDXGIFactory2_Vtbl, CreateSwapChainForHwnd)),
            );
            create(
                proxy,
                device,
                window,
                description,
                fullscreen,
                restrict_to,
                out,
            )
        }
        None => original(
            this,
            device,
            window,
            description,
            fullscreen,
            restrict_to,
            out,
        ),
    };

    if result.is_err() || out.is_null() || (*out).is_null() || description.is_null() {
        return result;
    }
    let made = *out;
    let desc = *description;
    let count = CREATED.fetch_add(1, Ordering::Relaxed);
    // Every creation, not just the first: the engine destroys and recreates it
    // on resolution and fullscreen changes, from inside its own present path,
    // and knowing that happened is the whole reason to watch this.
    crate::log!(
        "[swap] swapchain {} at {:#x}: {}x{} {:?} buffers={} effect={} flags={:#x} hwnd={:#x}{}",
        count,
        made as usize,
        desc.Width,
        desc.Height,
        desc.Format,
        desc.BufferCount,
        desc.SwapEffect.0,
        desc.Flags,
        window.0 as usize,
        if IN_PRESENT.load(Ordering::Relaxed) > 0 {
            "  (from inside Present)"
        } else {
            ""
        }
    );

    let forgotten = remember(made as usize, window.0 as isize);
    if forgotten > 0 {
        crate::log!(
            "[swap] {forgotten} earlier swapchain of this window is no longer treated as the \
             engine's"
        );
    }
    // Observed, not intended: with frame generation on this hook is installed
    // on Streamline's proxy factory, so reaching it at all means Streamline
    // built the chain. With it off the factory is DXGI's and so is the chain.
    // The line is written from what was actually done, not from what the
    // configuration asked for.
    crate::fg::announce_swapchain(through.is_some(), this as usize);
    // A rebuilt chain has no temporal connection to the one before it, so the
    // interpolator has to be told rather than left to blend across it.
    if count > 0 {
        crate::fg::note_reset();
    }
    *CURRENT.lock() = Some(Chain {
        swapchain: made as usize,
        window: window.0 as isize,
        size: (desc.Width, desc.Height),
        format: desc.Format,
        buffers: desc.BufferCount,
        flags: desc.Flags,
        swap_effect: desc.SwapEffect.0,
    });
    hook_chain(made);
    // Last, so that it describes the chain as the engine will hold it. Every
    // clause is read off these objects; nothing in it comes from the settings.
    crate::reshade::report_stack(count, this, through, device, made);
    // The settings page, if ReShade is here to host one. Returns at once
    // without ReShade, and after its single attempt with it.
    crate::overlay::register_once();
    // And the hook that tells a native command list's proxy, which NGX needs.
    // Here because by now the engine's device exists and, with ReShade loaded
    // at the renderer, carries ReShade's proxy.
    crate::reshade::watch_proxy_lists();
    result
}

unsafe extern "system" fn hook_present(this: *mut c_void, interval: u32, flags: u32) -> HRESULT {
    let original: PresentFn =
        std::mem::transmute(original_of(this, slot!(IDXGISwapChain_Vtbl, Present)));
    if !is_ours(this as usize) {
        return original(this, interval, flags);
    }
    // Nested, if the engine is rebuilding the swapchain from inside its own
    // present path and presents again on the new one. The frame boundary
    // belongs to the outermost call only: a second PRESENT_START for the same
    // frame would make the driver's timings nonsense, and generating a frame
    // from inside a swapchain teardown would read resources that are going
    // away.
    let outermost = IN_PRESENT.fetch_add(1, Ordering::Relaxed) == 0;
    if outermost {
        before_present(interval, flags, "Present");
    }
    let result = original(this, interval, flags);
    if outermost {
        after_present();
    }
    IN_PRESENT.fetch_sub(1, Ordering::Relaxed);
    result
}

unsafe extern "system" fn hook_present1(
    this: *mut c_void,
    interval: u32,
    flags: u32,
    parameters: *const DXGI_PRESENT_PARAMETERS,
) -> HRESULT {
    let original: Present1Fn =
        std::mem::transmute(original_of(this, slot!(IDXGISwapChain1_Vtbl, Present1)));
    if !is_ours(this as usize) {
        return original(this, interval, flags, parameters);
    }
    let outermost = IN_PRESENT.fetch_add(1, Ordering::Relaxed) == 0;
    if outermost {
        before_present(interval, flags, "Present1");
    }
    let result = original(this, interval, flags, parameters);
    if outermost {
        after_present();
    }
    IN_PRESENT.fetch_sub(1, Ordering::Relaxed);
    result
}

/// The frame boundary, as seen from the presentation path.
///
/// Reflex's markers hang off this because it is the only frame boundary this
/// library can see without detouring the engine, and because the present pair
/// has to be here in any case.
fn before_present(interval: u32, flags: u32, which: &str) {
    let perf = crate::perf::start(crate::perf::Family::PresentBefore);
    let count = PRESENTS.fetch_add(1, Ordering::Relaxed);
    if count == 0 {
        crate::log!("[swap] first {which}(interval={interval}, flags={flags:#x})");
    }
    // Infrequent enough to be free, often enough that a session's log says
    // whether the interposition is actually carrying the frames or whether
    // the engine is presenting through something this never saw created.
    if count > 0 && count.is_multiple_of(600) {
        crate::log!(
            "[swap] {}, {} kept interface-free frames, {} candidates refused for their \
             format and {} for being another resource",
            summary(),
            crate::hudless::kept(),
            crate::hudless::rejected().0,
            crate::hudless::rejected().1
        );
        // Whether our hooks fire at all, which is the one thing a dead
        // per-frame path cannot be diagnosed without. `dispatches` at zero
        // means the command-list hooks are on objects the engine never uses;
        // non-zero with `recognised` at zero means they fire but no pipeline
        // was ever identified, which is what happens when the hooks are
        // installed after the engine has built its pipelines.
        let (dispatches, closes, pipelines, recognised) = crate::gpu::activity();
        crate::log!(
            "[gpu] hooks saw {dispatches} dispatches, {closes} list closes, \
             {pipelines} compute pipelines created, {recognised} of them recognised"
        );
        // And in words, once, when what those numbers mean is that DLSS
        // cannot run. A missing `[dls]` line is easy to read past.
        crate::pipeline::warn_if_no_temporal_pass(count, dispatches, pipelines);
    }
    // The clock descriptor copies age by. Here as well as at the temporal
    // dispatch, so that it runs in menus and on loading screens too.
    crate::bindings::tick();
    crate::fg::before_present();
    perf.stop();
}

/// Patch the swapchain-creation slot on a factory we have been handed.
///
/// Called from `proxy::CreateDXGIFactory2` the moment the object exists and
/// before the engine has used it: the engine resolves `CreateDXGIFactory2`
/// early and creates its chain soon after, so a patch installed later can
/// miss the creation.
///
/// The slot index is the same on Streamline's factory as on DXGI's: both
/// implement `IDXGIFactory` through `IDXGIFactory7` on one vtable, because the
/// engine calls `IDXGIFactory5::CheckFeatureSupport` through an
/// `IDXGIFactory2` pointer and any stand-in has to survive that too.
///
/// # Safety
/// `factory` must be a live `IDXGIFactory2`.
pub unsafe fn watch_factory(factory: *mut c_void) {
    if factory.is_null() || INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let vtable = *(factory as *mut *mut *mut c_void);
    let index = slot!(IDXGIFactory2_Vtbl, CreateSwapChainForHwnd);
    if crate::vtable::hook(
        vtable,
        index,
        hook_create_swapchain_for_hwnd as *const c_void,
        &O_CREATE_SWAPCHAIN,
    ) {
        crate::log!(
            "[swap] hooked slot {index} CreateSwapChainForHwnd on the factory at \
             {:#x} (vtable {vtable:p})",
            factory as usize
        );
    } else {
        crate::log!(
            "[swap] FAILED to hook CreateSwapChainForHwnd on {:#x}",
            factory as usize
        );
    }
}

/// Make the engine's own device the one this library creates its resources
/// on, taken from the queue the engine presents with.
///
/// `GetDevice` on that queue names the device that made it, which is the
/// engine's by construction. See `bindings::adopt_device`.
unsafe fn adopt_device_for_our_resources(queue: *mut c_void) {
    use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, ID3D12Device};
    if queue.is_null() {
        return;
    }
    let Some(borrowed) = ID3D12CommandQueue::from_raw_borrowed(&queue) else {
        return;
    };
    let mut found: Option<ID3D12Device> = None;
    if borrowed.GetDevice(&mut found).is_err() {
        return;
    }
    let Some(device) = found else {
        return;
    };
    let raw = device.as_raw() as usize;
    if crate::bindings::adopt_device(device) {
        crate::log!(
            "[swap] this library's own textures are created on the engine's device {raw:#x} \
             from here on, taken from its present queue; until now they would have been \
             created on the device the hooks were installed through, {:#x}",
            crate::gpu::scratch_pointer()
        );
    }
}

/// Streamline's factory, upgraded from the engine's the first time a swapchain
/// is created and only if frame generation can actually run.
///
/// `slUpgradeInterface` replaces the pointer in place, so a counted reference
/// of our own is taken first and only the copy is upgraded: the engine keeps
/// using DXGI's factory for everything that is not a swapchain creation, which
/// is what manual hooking prescribes -- only hooked APIs route through proxies.
static FACTORY: AtomicUsize = AtomicUsize::new(0);
static SAID_NO_FEATURE: AtomicBool = AtomicBool::new(false);
static SAID_NO_UPGRADE: AtomicBool = AtomicBool::new(false);

/// Latched: the engine creates more than one chain and the device is set once.
static ADOPTED: AtomicBool = AtomicBool::new(false);

/// How long to wait for the `slInit` that started at load.
///
/// Generous because the measured cost is 1.3 seconds and the alternative to
/// waiting is handing over nothing at all, but bounded because a hang here is
/// a game that never draws a frame.
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// Hand Streamline the engine's own device, and ask for frame generation.
///
/// Done at the one call site that can name the engine's device rather than
/// guess at it. The order inside is the order Streamline requires:
/// `slSetD3DDevice` first, because `slUpgradeInterface` refuses outright
/// without it, and the feature asked for before anything is upgraded.
unsafe fn adopt_engine_device(queue: *mut c_void) {
    if !crate::config::get().fg || queue.is_null() {
        return;
    }
    // Not for a game build the mod does not know. Frame generation is fed
    // from the DLSS path, which is not there, and a session the mod stands
    // down in is to present as the game does by itself.
    if crate::gate::stood_down() {
        static SAID: AtomicBool = AtomicBool::new(false);
        if !SAID.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[swap] the mod is standing down, so Streamline is not given the engine's \
                 device and the swapchain is DXGI's own: no frame generation"
            );
        }
        return;
    }
    // Latched only once something has actually been handed over, not on
    // entry. The first chain seen may not be the engine's, and a latch on
    // entry would spend the one attempt on it and then stay silent for the
    // real one -- which is indistinguishable, from the log, from never having
    // run at all.
    if ADOPTED.load(Ordering::Relaxed) {
        return;
    }
    let Some(borrowed) =
        windows::Win32::Graphics::Direct3D12::ID3D12CommandQueue::from_raw_borrowed(&queue)
    else {
        crate::log!(
            "[swap] the present queue {queue:#x} is not an ID3D12CommandQueue, so \
             Streamline gets no device",
            queue = queue as usize
        );
        return;
    };
    let mut found: Option<windows::Win32::Graphics::Direct3D12::ID3D12Device> = None;
    if let Err(error) = borrowed.GetDevice(&mut found) {
        crate::log!(
            "[swap] the present queue has no device ({error}), so Streamline gets none either"
        );
        return;
    }
    let Some(device) = found else {
        crate::log!("[swap] GetDevice succeeded without a device, so Streamline gets none");
        return;
    };
    // What was adopted and from where, every time, because the alternative
    // was a function that could not be shown to have run.
    let raw = device.as_raw() as usize;
    let scratch = crate::gpu::scratch_pointer();
    crate::log!(
        "[swap] the engine's present queue {:#x} reports device {raw:#x}{}",
        queue as usize,
        if scratch != 0 && raw == scratch {
            "  -- which is our own scratch device"
        } else if scratch != 0 {
            "  (not our scratch device, which is held elsewhere)"
        } else {
            ""
        }
    );
    // Reported, and not acted on. `GetDevice` on this queue returns the
    // device that *created* this queue, and the queue is the one the engine
    // handed to `CreateSwapChainForHwnd`. Whatever comes back is by
    // definition the device Streamline must be given to work with this chain
    // -- even if it is also the device `bootstrap_scratch` made, which only
    // means the engine ended up on that one. `bootstrap_scratch` holds its
    // device for the life of the process, so the pointer is live whoever
    // owns it.
    if scratch != 0 && raw == scratch {
        crate::log!(
            "[swap] that is also the device gpu::bootstrap_scratch made, which is worth \
             knowing but is not a reason to refuse it: GetDevice on the engine's own \
             present queue is what names the device this chain needs"
        );
    }
    // Waited for here rather than at device creation. It is a wait inside a
    // call the engine is blocked on either way, and this is the last moment
    // that is still before the chain exists.
    if !crate::streamline::wait_until_ready(READY_TIMEOUT) {
        crate::log!(
            "[swap] Streamline is not initialised (the [sl ] lines above say why), so the \
             chain is DXGI's"
        );
        return;
    }
    // Leaked on purpose: Streamline keeps the pointer for the life of the
    // process, and a device released while something else still holds its
    // address is a use-after-free.
    let handed = device.into_raw();
    // As the engine's queue reports it, which with ReShade loaded is ReShade's
    // proxy device. Not unwrapped: see `reshade.rs`.
    static DEVICE: crate::reshade::Kept = crate::reshade::Kept::new("slSetD3DDevice");
    DEVICE.note(handed);
    ADOPTED.store(true, Ordering::Relaxed);
    if crate::streamline::set_device(handed) {
        crate::streamline::load_feature();
        crate::streamline::bind_feature_functions();
    } else {
        crate::log!(
            "[swap] Streamline would not take the engine's device, so nothing is \
             upgraded and the engine presents through DXGI. The reason is on the \
             [sl ] slSetD3DDevice line above"
        );
    }
}

unsafe fn streamline_factory(native: *mut c_void) -> Option<*mut c_void> {
    if !crate::config::get().fg || crate::gate::stood_down() || !crate::streamline::ready() {
        return None;
    }
    let cached = FACTORY.load(Ordering::Relaxed);
    if cached != 0 {
        return Some(cached as *mut c_void);
    }
    // The question that decides whether the picture survives a refusal, and
    // it has more than one "no". `device_ready` is false both when DLSS-G
    // was refused and when the device has simply not been handed over yet,
    // so the line below says which it is and names no cause it cannot see.
    if !crate::streamline::device_ready() {
        if !SAID_NO_FEATURE.swap(true, Ordering::Relaxed) {
            // Three states, each said as what can be seen from here: no
            // device offered yet, the device accepted and the feature not
            // initialised, or the device refused.
            if !ADOPTED.load(Ordering::Relaxed) {
                crate::log!(
                    "[swap] no device has been offered to Streamline yet, so the factory \
                     is left alone and the engine presents through DXGI"
                );
            } else if crate::streamline::device_is_set() {
                crate::log!(
                    "[swap] the device was accepted but frame generation did not \
                     initialise, so the factory is left alone and the engine presents \
                     through DXGI as it would with fg off. The reason is on the [sl ] \
                     and [sl*] lines above"
                );
            } else {
                crate::log!(
                    "[swap] a device was offered to Streamline and not accepted, so the \
                     factory is left alone and the engine presents through DXGI. The \
                     [sl ] slSetD3DDevice line above carries the result"
                );
            }
        }
        return None;
    }
    let borrowed = IDXGIFactory2::from_raw_borrowed(&native)?;
    // The factory our hook was entered on, which with ReShade loaded first is
    // ReShade's proxy factory. Not unwrapped: see `reshade.rs`.
    static FACTORY_GIVEN: crate::reshade::Kept = crate::reshade::Kept::new("slUpgradeInterface");
    FACTORY_GIVEN.note(native);
    let mut slot = borrowed.clone().into_raw();
    if !crate::streamline::upgrade("IDXGIFactory2", &mut slot) {
        if !SAID_NO_UPGRADE.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[swap] Streamline would not upgrade the factory, so the chain is DXGI's \
                 and frame generation cannot present"
            );
        }
        return None;
    }
    FACTORY.store(slot as usize, Ordering::Relaxed);
    Some(slot)
}

fn after_present() {
    let perf = crate::perf::start(crate::perf::Family::PresentAfter);
    crate::hudless::close_frame();
    crate::pathdump::at_present();
    crate::fg::after_present();
    perf.stop();
}

unsafe extern "system" fn hook_resize_buffers(
    this: *mut c_void,
    count: u32,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    flags: u32,
) -> HRESULT {
    let original: ResizeBuffersFn =
        std::mem::transmute(original_of(this, slot!(IDXGISwapChain_Vtbl, ResizeBuffers)));
    if is_ours(this as usize) {
        // Only on the HDR path in this engine -- a resolution change destroys
        // and rebuilds the swapchain instead -- so a line here is rare enough
        // to be worth having every time.
        RESIZES.fetch_add(1, Ordering::Relaxed);
        crate::log!(
            "[swap] ResizeBuffers to {width}x{height} {format:?}, {count} buffers, flags {flags:#x}"
        );
        if let Some(chain) = CURRENT.lock().as_mut() {
            if width != 0 && height != 0 {
                chain.size = (width, height);
            }
        }
    }
    original(this, count, width, height, format, flags)
}

unsafe extern "system" fn hook_get_buffer(
    this: *mut c_void,
    index: u32,
    riid: *const windows::core::GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let original: GetBufferFn =
        std::mem::transmute(original_of(this, slot!(IDXGISwapChain_Vtbl, GetBuffer)));
    let result = original(this, index, riid, out);
    // Hooked now because frame generation needs the back buffers and nothing
    // else reveals them, but it reports rather than records: holding a
    // reference to a back buffer across a swapchain rebuild is how a resize
    // fails with DXGI_ERROR_INVALID_CALL and nothing says why.
    // The latch is tested before `is_ours`, which takes a mutex: the engine
    // fetches back buffers often enough that the order matters.
    if !SAID_BUFFER.load(Ordering::Relaxed) && result.is_ok() && is_ours(this as usize) {
        SAID_BUFFER.store(true, Ordering::Relaxed);
        crate::log!("[swap] the engine fetched back buffer {index}");
    }
    result
}

static SAID_BUFFER: AtomicBool = AtomicBool::new(false);

/// What has passed through, for a line in the periodic report.
pub fn summary() -> String {
    format!(
        "swapchains={} presents={} resizes={}",
        CREATED.load(Ordering::Relaxed),
        PRESENTS.load(Ordering::Relaxed),
        RESIZES.load(Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slot numbers these hooks depend on, against the ones measured in
    /// the live process. `offset_of!` derives them, so this is a check that
    /// the interface definitions in use describe the same DXGI the game talks
    /// to -- and it is cheap enough to be worth having, because a vtable slot
    /// that is off by one patches an unrelated method.
    #[test]
    fn a_new_swapchain_for_a_window_replaces_the_one_before_it() {
        assert_eq!(remember(0x1000, 77), 0);
        assert!(is_ours(0x1000));
        // The engine rebuilt its chain: the old address is no longer one of
        // ours, whatever is allocated there next.
        assert_eq!(remember(0x2000, 77), 1);
        assert!(!is_ours(0x1000));
        assert!(is_ours(0x2000));
        // Another window's chain is its own.
        assert_eq!(remember(0x3000, 78), 0);
        assert!(is_ours(0x2000) && is_ours(0x3000));
    }

    #[test]
    fn each_swapchain_vtable_keeps_its_own_originals() {
        unsafe extern "system" fn of_dxgi() -> u32 {
            1
        }
        unsafe extern "system" fn of_a_proxy() -> u32 {
            2
        }
        unsafe extern "system" fn hook() -> u32 {
            3
        }
        const SLOT: usize = 5;
        let mut dxgi = [std::ptr::null_mut::<c_void>(); 8];
        let mut proxy = [std::ptr::null_mut::<c_void>(); 8];
        dxgi[SLOT] = of_dxgi as *mut c_void;
        proxy[SLOT] = of_a_proxy as *mut c_void;
        // An object is a pointer to its vtable.
        let mut dxgi_chain = dxgi.as_mut_ptr();
        let mut proxy_chain = proxy.as_mut_ptr();
        unsafe {
            patch_slot(dxgi.as_mut_ptr(), SLOT, hook as *const c_void, "test");
            patch_slot(proxy.as_mut_ptr(), SLOT, hook as *const c_void, "test");
            // A second chain on a vtable already patched changes nothing.
            patch_slot(dxgi.as_mut_ptr(), SLOT, hook as *const c_void, "test");
            assert_eq!(dxgi[SLOT] as usize, hook as *const c_void as usize);
            assert_eq!(proxy[SLOT] as usize, hook as *const c_void as usize);
            assert_eq!(
                original_of(&mut dxgi_chain as *mut _ as *mut c_void, SLOT),
                of_dxgi as *const c_void as usize
            );
            assert_eq!(
                original_of(&mut proxy_chain as *mut _ as *mut c_void, SLOT),
                of_a_proxy as *const c_void as usize
            );
        }
    }

    #[test]
    fn slots_are_where_the_game_calls_them() {
        assert_eq!(slot!(IDXGIFactory2_Vtbl, CreateSwapChainForHwnd), 15);
        assert_eq!(slot!(IDXGISwapChain_Vtbl, Present), 8);
        assert_eq!(slot!(IDXGISwapChain_Vtbl, GetBuffer), 9);
        assert_eq!(slot!(IDXGISwapChain_Vtbl, ResizeBuffers), 13);
        assert_eq!(slot!(IDXGISwapChain1_Vtbl, Present1), 22);
    }
}
