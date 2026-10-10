//! Which queue is the engine's graphics queue, by identity rather than by luck.
//!
//! `ExecuteCommandLists` is hooked on the `ID3D12CommandQueue` vtable, which
//! is shared by every queue in the process, so the hook fires for all of them:
//! the engine's graphics queue, its copy and compute queues, ours, and -- once
//! frame generation is loaded -- the extra graphics queue `sl.dlss_g` creates
//! to present asynchronously. Work that belongs to a frame has to be keyed on
//! the right one.
//!
//! "The first direct queue that submits" is not a definition once DLSS-G has
//! a queue of its own: which of the two submits first can differ between
//! sessions.
//!
//!
//! There is an exact answer and we already own the call that carries it. In
//! D3D12 the first argument to `IDXGIFactory2::CreateSwapChainForHwnd` is the
//! command queue the swapchain presents from -- so the queue the engine passes
//! there *is* its graphics queue, by definition rather than by inference. That
//! call is hooked for both configurations: on DXGI's factory when frame
//! generation is off, and on Streamline's proxy factory when it is on, where
//! our hook runs before Streamline sees the arguments.
//!
//! What it does not give is an answer before the swapchain exists. Submissions
//! happen during renderer setup, and the callers differ in what they want then:
//!
//! * `offset_the_engines_camera` wants to run as early as possible -- it is
//!   what applies the jitter -- so it asks `is_engine_or_unknown`, which
//!   accepts any queue until the engine's is known.
//! * The Reflex markers want the engine's frame boundary and nothing else, so
//!   they ask `is_engine` and simply do not fire before a swapchain exists.
//!   There are no frames being presented then, so there is nothing to mark.

use parking_lot::Mutex;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use windows::core::{IUnknown, Interface};
use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, D3D12_COMMAND_LIST_TYPE_DIRECT};

/// The engine's graphics queue, held as a counted reference.
///
/// A reference and not an address: the comparison in `is_engine` is on the
/// pointer, but holding the object means that pointer cannot be a stale
/// address that something else has since been allocated at -- which, for a
/// comparison that decides whether a submission is a frame, would be a
/// misidentification with no symptom at the point of the mistake.
static ENGINE: Mutex<Option<ID3D12CommandQueue>> = Mutex::new(None);
/// The same pointer, for a comparison that must not take a lock: this is read
/// on every `ExecuteCommandLists`, which is several times a frame on several
/// threads.
static ENGINE_PTR: AtomicUsize = AtomicUsize::new(0);
static SAID_WRONG_TYPE: AtomicBool = AtomicBool::new(false);

/// Record the queue a swapchain is being created against.
///
/// # Safety
/// `queue` must be the first argument of a D3D12 `CreateSwapChainForHwnd`,
/// which is an `ID3D12CommandQueue`. It is checked rather than trusted: under
/// D3D11 the same slot carries a device, and calling `GetDesc` on one of those
/// would be a vtable call at the wrong offset.
pub unsafe fn note_presenting(queue: *mut c_void) {
    if queue.is_null() {
        return;
    }
    // A real QueryInterface, not a cast. `from_raw_borrowed` would happily
    // reinterpret a device as a queue.
    let Some(unknown) = IUnknown::from_raw_borrowed(&queue) else {
        return;
    };
    let Ok(handle) = unknown.cast::<ID3D12CommandQueue>() else {
        if !SAID_WRONG_TYPE.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[queue] the swapchain was created against {:#x}, which is not an \
                 ID3D12CommandQueue -- so the engine's graphics queue stays unknown",
                queue as usize
            );
        }
        return;
    };
    let description = handle.GetDesc();
    if description.Type != D3D12_COMMAND_LIST_TYPE_DIRECT {
        if !SAID_WRONG_TYPE.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[queue] the swapchain was created against a queue of type {}, not DIRECT",
                description.Type.0
            );
        }
        return;
    }

    let mut held = ENGINE.lock();
    let previous = ENGINE_PTR.swap(queue as usize, Ordering::Relaxed);
    // The address is what is compared, so it is taken from the interface we
    // are about to hold rather than from the argument -- QueryInterface can
    // hand back a different pointer for a different interface of the same
    // object, and `ExecuteCommandLists` is called on the queue interface.
    let canonical = handle.as_raw() as usize;
    ENGINE_PTR.store(canonical, Ordering::Relaxed);
    *held = Some(handle);
    if previous == 0 {
        crate::log!(
            "[queue] the engine's graphics queue is {canonical:#x}, taken from the \
             swapchain it presents with"
        );
    } else if previous != canonical {
        // A resolution or fullscreen change rebuilds the swapchain, and
        // nothing says it has to be against the same queue.
        crate::log!("[queue] the engine's graphics queue is now {canonical:#x}, was {previous:#x}");
    }
}

/// Whether this is the engine's graphics queue.
pub fn is_engine(queue: *mut c_void) -> bool {
    let engine = ENGINE_PTR.load(Ordering::Relaxed);
    engine != 0 && engine == queue as usize
}

/// Whether this is the engine's graphics queue, or nothing is known yet.
///
/// The permissive form, for work that has to happen during renderer setup and
/// before any swapchain exists.
pub fn is_engine_or_unknown(queue: *mut c_void) -> bool {
    let engine = ENGINE_PTR.load(Ordering::Relaxed);
    engine == 0 || engine == queue as usize
}
