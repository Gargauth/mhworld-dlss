//! Work that must follow an engine command list on its queue: a fence
//! signal (capture completion), or a command list of our own (the HUD-less
//! conversion).
//!
//! Something recorded on an engine list is registered against that list;
//! the `ExecuteCommandLists` hook runs it right after the original call, on
//! the same queue, so it is ordered behind the list in GPU time without
//! touching the engine's recording. Our own lists are executed through the
//! original `ExecuteCommandLists` (the hook, the submit observer and frame
//! generation's submit counting are not re-entered) and their barriers are
//! committed to the state tracker here.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;

use super::registry::STATES;
use super::vt::{self, original};

/// What runs after a list was submitted, with the queue it went to.
pub(crate) type Action = Box<dyn FnOnce(&ID3D12CommandQueue) + Send>;

/// Actions waiting for their list, oldest first.
static PENDING: Mutex<Vec<(usize, Action)>> = Mutex::new(Vec::new());
/// `PENDING.len()`, for the lock-free check on every submission.
static COUNT: AtomicUsize = AtomicUsize::new(0);
/// Actions kept at most: a list that is closed and never executed would
/// otherwise hold its actions forever; the oldest are dropped.
const MAX_PENDING: usize = 64;
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Run `action` right after `list` (native) is next submitted.
pub(crate) fn after_submit(list: usize, action: Action) {
    let mut pending = PENDING.lock();
    if pending.len() >= MAX_PENDING {
        drop(pending.remove(0));
        DROPPED.fetch_add(1, Ordering::Relaxed);
        mhw_core::log_once!(
            Warn,
            "render",
            "after-submit-dropped",
            "work waiting for an engine list was dropped: {MAX_PENDING} lists were never submitted"
        );
    }
    pending.push((list, action));
    COUNT.store(pending.len(), Ordering::Release);
}

/// From the `ExecuteCommandLists` hook, after the original: run what waited
/// for any of `lists`, in registration order.
///
/// # Safety
/// `queue` must be the live native queue the lists were just submitted to.
pub(crate) unsafe fn run_after(queue: *mut c_void, lists: &[*mut c_void]) {
    if COUNT.load(Ordering::Acquire) == 0 {
        return;
    }
    let due: Vec<Action> = {
        let mut pending = PENDING.lock();
        let mut due = Vec::new();
        let mut kept = Vec::with_capacity(pending.len());
        for (list, action) in pending.drain(..) {
            if lists.iter().any(|submitted| *submitted as usize == list) {
                due.push(action);
            } else {
                kept.push((list, action));
            }
        }
        *pending = kept;
        COUNT.store(pending.len(), Ordering::Release);
        due
    };
    if due.is_empty() {
        return;
    }
    // SAFETY: caller's contract.
    let Some(queue) = (unsafe { ID3D12CommandQueue::from_raw_borrowed(&queue) }) else {
        return;
    };
    for action in due {
        action(queue);
    }
}

/// Execute our own closed `list` on `queue`, through the original.
///
/// # Safety
/// `queue` live; `list` a closed native list of ours.
pub(crate) unsafe fn execute_own(queue: &ID3D12CommandQueue, list: &ID3D12GraphicsCommandList) {
    let raw = list.as_raw();
    STATES.lock().submit([raw as usize]);
    // SAFETY: the original ExecuteCommandLists with one live list.
    unsafe {
        original::<vt::ExecuteFn>(&vt::O_EXECUTE)(queue.as_raw(), 1, &raw);
    }
}

/// A fence of this crate and the values signalled on it.
pub(crate) struct Fence {
    fence: ID3D12Fence,
    device: usize,
    next: AtomicU64,
}

impl std::fmt::Debug for Fence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fence")
            .field("device", &self.device)
            .field("next", &self.next)
            .finish()
    }
}

// SAFETY: fences are free-threaded.
unsafe impl Send for Fence {}
// SAFETY: as above; the counter is atomic.
unsafe impl Sync for Fence {}

impl Fence {
    /// Signal the next value on `queue`; that value, or `None` if the
    /// signal failed.
    pub(crate) fn signal(&self, queue: &ID3D12CommandQueue) -> Option<u64> {
        let value = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        // SAFETY: a live queue and fence.
        unsafe { queue.Signal(&self.fence, value) }.ok()?;
        Some(value)
    }

    /// The highest value the GPU has reached.
    pub(crate) fn completed(&self) -> u64 {
        // SAFETY: a live fence.
        unsafe { self.fence.GetCompletedValue() }
    }
}

static FENCE: Mutex<Option<Arc<Fence>>> = Mutex::new(None);

/// The crate's fence on `device` (made at first use, kept for the session;
/// the engine has one device).
pub(crate) fn fence(device: &ID3D12Device) -> Result<Arc<Fence>, String> {
    let mut held = FENCE.lock();
    if let Some(fence) = held.as_ref() {
        if fence.device == device.as_raw() as usize {
            return Ok(Arc::clone(fence));
        }
        return Err("a second device: the fence belongs to the first".into());
    }
    // SAFETY: a live device.
    let made: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }
        .map_err(super::gpu::failed("fence"))?;
    let fence = Arc::new(Fence {
        fence: made,
        device: device.as_raw() as usize,
        next: AtomicU64::new(0),
    });
    *held = Some(Arc::clone(&fence));
    Ok(fence)
}

/// The crate's fence, once made.
pub(crate) fn current_fence() -> Option<Arc<Fence>> {
    FENCE.lock().clone()
}

/// Presents after which a recorded own list whose engine list never got
/// submitted is taken back (nothing of it was executed).
const STALE_PRESENTS: u64 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Executable again once the fence reaches `done_at`.
    Idle { done_at: u64 },
    /// Recorded under `ticket` at present `at`, waiting for its engine list.
    Recorded { ticket: u64, at: u64 },
    /// Executed but the fence signal failed: never reused.
    Lost,
}

/// A command list of our own with its allocator, executed behind an engine
/// list and reusable once the GPU has passed its fence value.
pub(crate) struct OwnList {
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    phase: Phase,
    tickets: u64,
}

impl std::fmt::Debug for OwnList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnList")
            .field("list", &self.list.as_raw())
            .field("phase", &self.phase)
            .finish()
    }
}

// SAFETY: D3D12 lists and allocators may be used from any thread, one at a
// time; an `OwnList` is always behind a mutex.
unsafe impl Send for OwnList {}

impl OwnList {
    /// A closed DIRECT list.
    pub(crate) fn new(device: &ID3D12Device) -> Result<OwnList, String> {
        // SAFETY: plain object creation on a live device.
        unsafe {
            let allocator: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .map_err(super::gpu::failed("command allocator"))?;
            let list: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                .map_err(super::gpu::failed("command list"))?;
            list.Close().map_err(super::gpu::failed("command list"))?;
            Ok(OwnList {
                allocator,
                list,
                phase: Phase::Idle { done_at: 0 },
                tickets: 0,
            })
        }
    }

    /// Whether it can be recorded again: its last execution completed, or
    /// it was recorded and its engine list never submitted.
    pub(crate) fn free(&self, completed: u64, presents: u64) -> bool {
        match self.phase {
            Phase::Idle { done_at } => done_at <= completed,
            Phase::Recorded { at, .. } => presents.saturating_sub(at) > STALE_PRESENTS,
            Phase::Lost => false,
        }
    }

    /// Whether its last execution has completed (or it never ran).
    pub(crate) fn idle(&self, completed: u64) -> bool {
        match self.phase {
            Phase::Idle { done_at } => done_at <= completed,
            Phase::Recorded { .. } => true,
            Phase::Lost => false,
        }
    }

    /// Reset and open it for recording (the caller checked [`free`] and
    /// closes it whatever happens); the ticket to [`run`] it with.
    ///
    /// [`free`]: OwnList::free
    /// [`run`]: OwnList::run
    pub(crate) fn open(&mut self, presents: u64) -> Result<u64, String> {
        // SAFETY: nothing recorded with the allocator is executing (`free`);
        // the list is closed.
        unsafe {
            self.allocator
                .Reset()
                .map_err(super::gpu::failed("allocator reset"))?;
            self.list
                .Reset(&self.allocator, None)
                .map_err(super::gpu::failed("list reset"))?;
        }
        self.tickets += 1;
        self.phase = Phase::Recorded {
            ticket: self.tickets,
            at: presents,
        };
        Ok(self.tickets)
    }

    /// The list, for recording and closing.
    pub(crate) fn list(&self) -> &ID3D12GraphicsCommandList {
        &self.list
    }

    /// Execute the recording of `ticket` on `queue` and signal `fence`
    /// behind it; nothing if the list was taken back or re-recorded since.
    ///
    /// # Safety
    /// `queue` live; the recording of `ticket` was closed.
    pub(crate) unsafe fn run(&mut self, ticket: u64, queue: &ID3D12CommandQueue, fence: &Fence) {
        if !matches!(self.phase, Phase::Recorded { ticket: t, .. } if t == ticket) {
            return;
        }
        // SAFETY: caller's contract.
        unsafe { execute_own(queue, &self.list) };
        self.phase = match fence.signal(queue) {
            Some(done_at) => Phase::Idle { done_at },
            None => {
                mhw_core::log_once!(
                    Error,
                    "render",
                    "own-list-signal",
                    "a fence signal behind our own command list failed; that list is not reused"
                );
                Phase::Lost
            }
        };
    }
}
