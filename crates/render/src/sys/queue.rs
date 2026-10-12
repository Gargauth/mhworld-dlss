//! Submission: committing resource states, and which queue is the engine's.
//!
//! `ExecuteCommandLists` is hooked on the vtable every queue shares: the
//! engine's graphics, copy and compute queues, and Streamline's present
//! queue once frame generation runs. Resource states are committed for all
//! of them (their barriers are real). Frame-related work needs the engine's
//! graphics queue, which is known exactly once a swapchain is created: the
//! first argument of `CreateSwapChainForHwnd` *is* the queue the engine
//! presents from (`ReLoadInitDx11` §2.3: `[Device+0x20]`).

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use windows::core::{IUnknown, Interface};
use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, D3D12_COMMAND_LIST_TYPE_DIRECT};

use super::registry::STATES;
use super::reshade::unwrap_reshade;
use super::vt::{self, original};

/// The engine's graphics queue, held so its address cannot be recycled.
static ENGINE: Mutex<Option<ID3D12CommandQueue>> = Mutex::new(None);
/// The same pointer for the lock-free comparison on every submission.
static ENGINE_PTR: AtomicUsize = AtomicUsize::new(0);
/// The observer, as a function address (0: none).
static OBSERVER: AtomicUsize = AtomicUsize::new(0);

/// One `ExecuteCommandLists` call, as shown to the observer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Submission {
    /// The native queue.
    pub queue: usize,
    /// Whether it is the engine's graphics queue (`false` while unknown).
    pub engine: bool,
    /// Lists submitted.
    pub lists: u32,
}

/// Called before each submission is forwarded, on the submitting thread.
/// Must be cheap and must not submit work itself.
pub type SubmitObserver = fn(&Submission);

/// Install (or with `None` remove) the submission observer.
pub fn set_submit_observer(observer: Option<SubmitObserver>) {
    OBSERVER.store(observer.map_or(0, |f| f as usize), Ordering::Release);
}

/// Record the queue a swapchain is being created against: the engine's
/// graphics queue. Checked, not trusted: it must answer as a DIRECT
/// `ID3D12CommandQueue` (under D3D11 that argument is a device). A ReShade
/// proxy is unwrapped. Returns whether it was accepted.
///
/// # Safety
/// `queue` must be null or a live COM object.
pub unsafe fn note_presenting_queue(queue: *mut c_void) -> bool {
    // SAFETY: caller's contract.
    let native = unsafe { unwrap_reshade(queue) };
    // SAFETY: as above; a real QueryInterface, not a reinterpretation.
    let Some(unknown) = (unsafe { IUnknown::from_raw_borrowed(&native) }) else {
        return false;
    };
    let Ok(handle) = unknown.cast::<ID3D12CommandQueue>() else {
        log::warn!(target: "render", "the swapchain's device argument {native:p} is not a D3D12 queue");
        return false;
    };
    // SAFETY: a live queue.
    if unsafe { handle.GetDesc() }.Type != D3D12_COMMAND_LIST_TYPE_DIRECT {
        log::warn!(target: "render", "the swapchain's queue {native:p} is not a DIRECT queue");
        return false;
    }
    let canonical = handle.as_raw() as usize;
    let mut held = ENGINE.lock();
    let previous = ENGINE_PTR.swap(canonical, Ordering::Relaxed);
    *held = Some(handle);
    if previous != canonical {
        log::info!(target: "render", "engine graphics queue {canonical:#x} (was {previous:#x})");
    }
    true
}

/// The engine's graphics queue, once a swapchain named it.
pub fn engine_queue() -> Option<ID3D12CommandQueue> {
    ENGINE.lock().clone()
}

/// Whether `queue` (native) is the engine's graphics queue.
pub fn is_engine_queue(queue: *mut c_void) -> bool {
    let engine = ENGINE_PTR.load(Ordering::Relaxed);
    engine != 0 && engine == queue as usize
}

pub(crate) unsafe extern "system" fn execute(
    this: *mut c_void,
    count: u32,
    lists: *const *mut c_void,
) {
    // Before the call: what these lists reach is what any list recorded
    // from now on starts from, and another thread may record at once.
    if !lists.is_null() && count > 0 {
        // SAFETY: the caller passes `count` list pointers. A list pointer
        // is the same address through every interface of its chain, so it
        // keys the recordings `ResourceBarrier` and `Close` saw.
        let submitted = unsafe { std::slice::from_raw_parts(lists, count as usize) };
        STATES
            .lock()
            .submit(submitted.iter().map(|list| *list as usize));
    }
    let observer = OBSERVER.load(Ordering::Acquire);
    if observer != 0 {
        // SAFETY: only `set_submit_observer` stores here, from a
        // `SubmitObserver`.
        let observer: SubmitObserver =
            unsafe { std::mem::transmute::<usize, SubmitObserver>(observer) };
        observer(&Submission {
            queue: this as usize,
            engine: is_engine_queue(this),
            lists: count,
        });
    }
    // SAFETY: the original with the caller's own arguments.
    unsafe { original::<vt::ExecuteFn>(&vt::O_EXECUTE)(this, count, lists) };
    // After the call: whatever our passes need behind these lists (a fence
    // signal, our own conversion list) goes on the same queue right now.
    if !lists.is_null() && count > 0 {
        // SAFETY: as above; the caller's array is valid for the call, and
        // `this` is the live native queue the lists went to.
        unsafe {
            let submitted = std::slice::from_raw_parts(lists, count as usize);
            super::submit::run_after(this, submitted);
        }
    }
}
