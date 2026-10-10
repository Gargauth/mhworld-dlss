//! Tracking what state each resource is in, in submission order.
//!
//! NGX reads colour, depth and motion vectors as non-pixel shader resources
//! and writes the output as an unordered access view. The engine leaves them
//! in whatever state its own passes needed, so they have to be moved first,
//! and a transition barrier has to declare the state the resource is actually
//! in. Getting that wrong is undefined behaviour, not an error return.
//!
//! D3D12 offers no query for it, so the state has to be followed, and two
//! simpler models are wrong:
//!
//! One state per resource, last writer wins across every command list. MT
//! Framework records on several threads at once, so that answer can come
//! from a list recorded beside this one and submitted after it.
//!
//! A table per command list, falling back to the first model when the list
//! has not touched the resource. The fallback is needed -- the engine
//! transitions the depth buffer on a list other than the one that
//! dispatches the temporal pass -- and the fallback is exactly the unsound
//! part: the debug layer reports the resulting barriers as
//! `D3D12 ERROR #527`, a barrier claiming `DEPTH_WRITE` for a resource
//! sitting in `COPY_SOURCE`.
//!
//! What a resource's state actually follows is **submission** order. Recording
//! a barrier changes nothing; executing the list does. So this module records
//! each list's transitions as they are recorded, holds them when the list
//! closes, and applies them to a committed table when `ExecuteCommandLists`
//! consumes that list. The committed table is then the true state as of
//! everything submitted so far, and the answer for a list being recorded is
//! its own transitions if it has any and the committed state otherwise --
//! with no third guess underneath.
//!
//! That is still an approximation in one direction: a list recorded now may be
//! submitted after a sibling recorded alongside it, whose transitions are not
//! committed yet. Nothing observable from inside the API fixes that, and it is
//! a far narrower window than "whatever any thread last recorded". The
//! remaining exposure is visible rather than hidden -- the debug layer counts
//! barrier mismatches, and `[val] barrier mismatches` reports the tally.

use crate::fasthash::AddressMap;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::ffi::c_void;
use windows::Win32::Graphics::Direct3D12::*;

/// What one command list's recording does to resource states.
///
/// `None` means the recording made the state unknowable rather than setting
/// it -- a transition of a single subresource says nothing certain about the
/// whole resource, and the barriers issued here cover all subresources.
type Transitions = AddressMap<usize, Option<i32>>;

/// Lists currently being recorded, and what they have done so far.
static RECORDING: Mutex<Option<AddressMap<usize, Transitions>>> = Mutex::new(None);
/// Closed recordings waiting to be submitted, oldest first.
///
/// A queue per list rather than a single entry: a list can be reset and
/// re-recorded before an earlier recording of it has been executed, and the
/// two must be applied in the order they were closed.
static CLOSED: Mutex<Option<AddressMap<usize, VecDeque<Transitions>>>> = Mutex::new(None);
/// The state of every resource as of everything submitted so far.
static COMMITTED: Mutex<Option<AddressMap<usize, i32>>> = Mutex::new(None);

/// A closed recording that is never executed would sit in the queue forever.
/// Keeping a few and dropping the oldest bounds it; a recording this stale is
/// of no use anyway.
const MAX_PENDING: usize = 8;

/// Transitions recorded, so "never observed" can be told apart from "the hook
/// is not seeing anything at all".
static TRACKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Lists submitted whose recording we never saw -- closed before the hooks
/// were installed, or executed twice. Their transitions are missing from the
/// committed table, so a number climbing here explains a state being unknown.
static UNSEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A resource has just been created, in this state.
///
/// The committed table is keyed by address and never forgets one, so
/// without this a resource created where a destroyed one stood would inherit
/// whatever state that one was last seen in, and a barrier planned from an
/// inherited state declares a `StateBefore` the new resource was never in.
/// The engine rebuilds every render target when the render ratio changes,
/// the allocator hands the new ones the old ones' addresses, and the first
/// frame after a rebuild is exactly when this library plans barriers for
/// them. Creation is the one moment the true state is given outright, so it
/// is taken.
pub fn created(resource: usize, state: i32) {
    if resource == 0 {
        return;
    }
    COMMITTED
        .lock()
        .get_or_insert_with(AddressMap::default)
        .insert(resource, state);
}

pub fn tracked() -> u64 {
    TRACKED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Submitted lists whose recording was never seen.
pub fn unseen() -> u64 {
    UNSEEN.load(std::sync::atomic::Ordering::Relaxed)
}

/// Record the transitions in a barrier batch recorded into `list`.
///
/// # Safety
/// `barriers` must point to `count` valid barrier structures.
pub unsafe fn observe(list: usize, barriers: *const D3D12_RESOURCE_BARRIER, count: u32) {
    if barriers.is_null() || count == 0 {
        return;
    }
    let mut guard = RECORDING.lock();
    let states = guard
        .get_or_insert_with(AddressMap::default)
        .entry(list)
        .or_default();

    for index in 0..count as usize {
        let barrier = &*barriers.add(index);
        if barrier.Type != D3D12_RESOURCE_BARRIER_TYPE_TRANSITION {
            continue;
        }
        let transition = &barrier.Anonymous.Transition;
        // The COM pointer's value, not the address of the field holding it.
        let resource = transition
            .pResource
            .as_ref()
            .map(|resource| windows::core::Interface::as_raw(resource) as usize);
        if let Some(resource) = resource.filter(|address| *address != 0) {
            let reached = (transition.Subresource == D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES)
                .then_some(transition.StateAfter.0);
            states.insert(resource, reached);
            TRACKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// The list has been closed: hold what it recorded until it is submitted.
///
/// The recording is kept, not discarded: its transitions only reach the
/// committed table when the list is executed, and without them the state of
/// everything the engine moved would stay unknown.
pub fn close(list: usize) {
    // Queued even when it recorded nothing. The queue is consumed one entry
    // per submission, so skipping the empty ones would let a submission pop a
    // recording that belongs to a later one.
    let recorded = RECORDING
        .lock()
        .as_mut()
        .and_then(|lists| lists.remove(&list))
        .unwrap_or_default();
    let mut guard = CLOSED.lock();
    let pending = guard
        .get_or_insert_with(AddressMap::default)
        .entry(list)
        .or_default();
    pending.push_back(recorded);
    while pending.len() > MAX_PENDING {
        pending.pop_front();
    }
}

/// Apply the transitions of each submitted list, in the order given.
///
/// # Safety
/// `lists` must point to `count` command list pointers.
pub unsafe fn submit(lists: *const *mut c_void, count: u32) {
    if lists.is_null() || count == 0 {
        return;
    }
    let mut closed = CLOSED.lock();
    let Some(closed) = closed.as_mut() else {
        UNSEEN.fetch_add(count as u64, std::sync::atomic::Ordering::Relaxed);
        return;
    };
    let mut committed = COMMITTED.lock();
    let committed = committed.get_or_insert_with(AddressMap::default);

    for index in 0..count as usize {
        let list = *lists.add(index) as usize;
        let Some(pending) = closed.get_mut(&list).and_then(VecDeque::pop_front) else {
            UNSEEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            continue;
        };
        for (resource, reached) in pending {
            match reached {
                Some(state) => {
                    committed.insert(resource, state);
                }
                // The list left the resource in a state this module cannot
                // name, so claiming to know it afterwards would be the very
                // guess being avoided.
                None => {
                    committed.remove(&resource);
                }
            }
        }
    }
    closed.retain(|_, queue| !queue.is_empty());
}

/// The state a resource will be in at this point in `list`.
///
/// The list's own recording first -- those barriers precede whatever is being
/// recorded now, whenever the list eventually runs -- and the committed state
/// otherwise. Nothing beyond those two: a third source of truth is what made
/// the previous two models wrong.
pub fn known(list: usize, resource: usize) -> Option<D3D12_RESOURCE_STATES> {
    if let Some(reached) = RECORDING
        .lock()
        .as_ref()
        .and_then(|lists| lists.get(&list))
        .and_then(|states| states.get(&resource))
        .copied()
    {
        return reached.map(D3D12_RESOURCE_STATES);
    }
    COMMITTED
        .lock()
        .as_ref()
        .and_then(|states| states.get(&resource))
        .copied()
        .map(D3D12_RESOURCE_STATES)
}

/// A transition to apply before handing a resource to NGX, and the one that
/// puts it back.
pub struct Move {
    pub resource: usize,
    pub from: D3D12_RESOURCE_STATES,
    pub to: D3D12_RESOURCE_STATES,
}

/// Work out what has to move on `list`, leaving anything already correct
/// alone.
///
/// Returns `None` when a resource's state is not known here: handing NGX a
/// resource whose state has to be guessed at is exactly what this module
/// exists to prevent.
pub fn plan(list: usize, resources: &[(usize, D3D12_RESOURCE_STATES)]) -> Option<Vec<Move>> {
    let mut moves = Vec::new();
    for (resource, wanted) in resources {
        let current = known(list, *resource)?;
        if current != *wanted {
            moves.push(Move {
                resource: *resource,
                from: current,
                to: *wanted,
            });
        }
    }
    Some(moves)
}

/// Build a barrier for one move.
///
/// # Safety
/// `resource` must be a live `ID3D12Resource`.
pub unsafe fn barrier(step: &Move, reverse: bool) -> D3D12_RESOURCE_BARRIER {
    let (before, after) = if reverse {
        (step.to, step.from)
    } else {
        (step.from, step.to)
    };
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: std::mem::transmute(step.resource as *mut c_void),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// A readable name for a state mask, for log lines that have to be understood
/// quickly.
pub fn name(state: D3D12_RESOURCE_STATES) -> String {
    const NAMES: &[(i32, &str)] = &[
        (0x1, "VERTEX_AND_CONSTANT_BUFFER"),
        (0x2, "INDEX_BUFFER"),
        (0x4, "RENDER_TARGET"),
        (0x8, "UNORDERED_ACCESS"),
        (0x10, "DEPTH_WRITE"),
        (0x20, "DEPTH_READ"),
        (0x40, "NON_PIXEL_SHADER_RESOURCE"),
        (0x80, "PIXEL_SHADER_RESOURCE"),
        (0x100, "STREAM_OUT"),
        (0x200, "INDIRECT_ARGUMENT"),
        (0x400, "COPY_DEST"),
        (0x800, "COPY_SOURCE"),
        (0x1000, "RESOLVE_DEST"),
        (0x2000, "RESOLVE_SOURCE"),
    ];
    if state.0 == 0 {
        return "COMMON".into();
    }
    let listed: Vec<&str> = NAMES
        .iter()
        .filter(|(bit, _)| state.0 & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    if listed.is_empty() {
        format!("{:#x}", state.0)
    } else {
        listed.join("|")
    }
}
