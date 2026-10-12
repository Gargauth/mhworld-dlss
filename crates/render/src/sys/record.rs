//! Recording our own work on an engine command list.
//!
//! Our calls go through the very vtable slots we patched. While this thread
//! records for us, the state-recording hooks stand aside (the record stays
//! the engine's, which is what gets restored); barriers are still observed,
//! because they are real state changes.

use std::cell::Cell;
use std::ffi::c_void;
use std::mem::ManuallyDrop;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;

use super::registry::{self, STATES};
use super::reshade::unwrap_reshade;
use super::{liststate, proxylist};
use crate::states::{self, Move, State};

thread_local! {
    static REENTRANT: Cell<bool> = const { Cell::new(false) };
}

/// Whether this thread is recording our own work.
pub(crate) fn reentrant() -> bool {
    REENTRANT.with(Cell::get)
}

/// Marks this thread as recording our own work until dropped.
pub(crate) struct Guard(bool);

impl Guard {
    pub(crate) fn enter() -> Guard {
        Guard(REENTRANT.with(|flag| flag.replace(true)))
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        REENTRANT.with(|flag| flag.set(self.0));
    }
}

/// The state `resource` is in at this point of `list` (submission-order
/// model; see [`crate::states`]). `list` may be a ReShade proxy.
///
/// # Safety
/// `list` must be null or a live command list.
pub unsafe fn resource_state(list: *mut c_void, resource: usize) -> Option<State> {
    // SAFETY: caller guarantees a live object or null.
    let native = unsafe { unwrap_reshade(list) };
    STATES.lock().known(native as usize, resource)
}

/// Transitions planned for one list, and their inverse.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transitions {
    moves: Vec<Move>,
}

impl Transitions {
    /// Plan bringing each `(resource, state)` to its state on `list`,
    /// leaving those already there; `None` if any state is unknown
    /// (guessing a `StateBefore` is undefined behaviour).
    ///
    /// # Safety
    /// `list` must be a live command list (a ReShade proxy is unwrapped).
    pub unsafe fn plan(list: *mut c_void, wanted: &[(usize, State)]) -> Option<Transitions> {
        // SAFETY: caller guarantees a live object.
        let native = unsafe { unwrap_reshade(list) };
        let moves = STATES.lock().plan(native as usize, wanted)?;
        Some(Transitions { moves })
    }

    /// The moves, forward.
    pub fn moves(&self) -> &[Move] {
        &self.moves
    }

    /// Whether nothing moves.
    pub fn is_empty(&self) -> bool {
        self.moves.is_empty()
    }

    /// Record the forward barriers.
    ///
    /// # Safety
    /// `list` must be open, and every planned resource alive.
    pub unsafe fn forward(&self, list: &ID3D12GraphicsCommandList) {
        // SAFETY: forwarded contract.
        unsafe { record_moves(list, self.moves.iter().copied()) }
    }

    /// Record the barriers that put everything back.
    ///
    /// # Safety
    /// As [`Transitions::forward`], after it.
    pub unsafe fn back(&self, list: &ID3D12GraphicsCommandList) {
        // SAFETY: forwarded contract.
        unsafe { record_moves(list, self.moves.iter().rev().map(|m| m.reversed())) }
    }
}

/// A whole-resource transition barrier. Non-owning: the pointer is not
/// AddRef'd and the barrier is never dropped as an owner.
pub(crate) fn barrier(step: Move) -> D3D12_RESOURCE_BARRIER {
    let raw = step.resource as *mut c_void;
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: `ManuallyDrop<Option<ID3D12Resource>>` is one
                // nullable interface pointer that is never released.
                pResource: unsafe { std::mem::transmute_copy(&raw) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: D3D12_RESOURCE_STATES(step.from as i32),
                StateAfter: D3D12_RESOURCE_STATES(step.to as i32),
            }),
        },
    }
}

/// # Safety
/// `list` open, resources alive.
unsafe fn record_moves(list: &ID3D12GraphicsCommandList, moves: impl Iterator<Item = Move>) {
    let barriers: Vec<D3D12_RESOURCE_BARRIER> = moves.map(barrier).collect();
    if !barriers.is_empty() {
        // SAFETY: caller's contract; through the hook, which observes them.
        unsafe { list.ResourceBarrier(&barriers) };
    }
}

/// Record our own work on an engine command list and put the engine's state
/// back afterwards: descriptor heaps, compute root signature, pipeline and
/// every root binding (and its render targets, if `unbind_depth` names the
/// depth target bound on it: a resource bound as a depth target cannot be
/// transitioned, so it is unbound for the duration and bound again).
///
/// `list` may be a ReShade proxy; `f` gets the native list. Returns `None`
/// if `list` is not a graphics command list.
///
/// When `list` is a proxy (it unwraps to a different native list), the
/// descriptor heaps the engine last set through it are snapshotted before
/// `f` and set again through the proxy's own `SetDescriptorHeaps` after it,
/// before the native state is restored: work given the proxy (NGX) sets
/// its own heaps through ReShade, which remembers them per list, and a
/// restore on the native list alone would leave ReShade's record pointing
/// at them. Nothing extra for a native list.
///
/// # Safety
/// `list` must be a live, open command list recorded on this thread; `f`
/// must not close or reset it.
pub unsafe fn record_on<R>(
    list: *mut c_void,
    unbind_depth: Option<usize>,
    f: impl FnOnce(&ID3D12GraphicsCommandList) -> R,
) -> Option<R> {
    // SAFETY: caller guarantees a live object.
    let native = unsafe { unwrap_reshade(list) };
    // SAFETY: as above; the type is checked by the borrow (a list).
    let graphics = unsafe { ID3D12GraphicsCommandList::from_raw_borrowed(&native) }?;
    let saved = liststate::with(native, |state| state.clone());
    let proxy_heaps = if native != list {
        proxylist::held_heaps(list)
    } else {
        None
    };
    let unbind =
        unbind_depth.is_some_and(|depth| depth != 0 && saved.targets.dsv_resource == depth);
    let guard = Guard::enter();
    if unbind {
        // SAFETY: the engine's own targets on its open list.
        unsafe { liststate::bind_targets(native, &saved.targets, false) };
    }
    let result = f(graphics);
    // SAFETY: the snapshot of this very list, on it; `proxy_heaps` only
    // exists when `list` is a live proxy, open on this thread.
    unsafe {
        if let Some(heaps) = proxy_heaps {
            proxylist::set_heaps_through(list, heaps);
        }
        if unbind {
            liststate::bind_targets(native, &saved.targets, true);
        }
        liststate::restore(native, &saved);
    }
    drop(guard);
    Some(result)
}

/// Why [`copy_resource`] did not copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyError {
    /// One of the two was not created while hooked, or is too small to be
    /// described.
    Undescribed,
    /// They differ in size, format or mips, or are the same resource.
    Mismatch,
    /// A state is unknown on this list.
    UnknownState,
    /// `list` is not a graphics command list.
    NotAList,
}

/// Copy `from` into `to` on `list` (`CopyResource`), moving both into the
/// copy states and back.
///
/// # Safety
/// `list` live and open on this thread; both resources alive.
pub unsafe fn copy_resource(list: *mut c_void, from: usize, to: usize) -> Result<(), CopyError> {
    let (a, b) = (
        registry::describe(from).ok_or(CopyError::Undescribed)?,
        registry::describe(to).ok_or(CopyError::Undescribed)?,
    );
    if from == to || (a.width, a.height, a.format, a.mips) != (b.width, b.height, b.format, b.mips)
    {
        return Err(CopyError::Mismatch);
    }
    // SAFETY: caller guarantees a live list.
    let native = unsafe { unwrap_reshade(list) };
    // SAFETY: as above.
    let graphics = unsafe { ID3D12GraphicsCommandList::from_raw_borrowed(&native) }
        .ok_or(CopyError::NotAList)?;
    // SAFETY: as above.
    let plan = unsafe {
        Transitions::plan(
            native,
            &[(from, states::COPY_SOURCE), (to, states::COPY_DEST)],
        )
    }
    .ok_or(CopyError::UnknownState)?;
    let (raw_from, raw_to) = (from as *mut c_void, to as *mut c_void);
    // SAFETY: caller guarantees both resources alive and the list open.
    unsafe {
        let (Some(source), Some(destination)) = (
            ID3D12Resource::from_raw_borrowed(&raw_from),
            ID3D12Resource::from_raw_borrowed(&raw_to),
        ) else {
            return Err(CopyError::Undescribed);
        };
        plan.forward(graphics);
        graphics.CopyResource(destination, source);
        plan.back(graphics);
    }
    Ok(())
}

/// The native command list behind `list` as an interface, for callers that
/// hold a raw pointer.
///
/// # Safety
/// `list` must be null or a live COM object.
pub unsafe fn native_list(list: *mut c_void) -> Option<ID3D12GraphicsCommandList> {
    // SAFETY: caller's contract.
    let native = unsafe { unwrap_reshade(list) };
    // SAFETY: as above.
    unsafe { ID3D12GraphicsCommandList::from_raw_borrowed(&native) }.cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guards_nest() {
        assert!(!reentrant());
        let outer = Guard::enter();
        {
            let _inner = Guard::enter();
            assert!(reentrant());
        }
        assert!(reentrant());
        drop(outer);
        assert!(!reentrant());
    }

    #[test]
    fn barriers_carry_the_move() {
        let step = Move {
            resource: 0x1234,
            from: states::RENDER_TARGET,
            to: states::NON_PIXEL_SHADER_RESOURCE,
        };
        let built = barrier(step);
        // SAFETY: the union member just written.
        let transition = unsafe { &built.Anonymous.Transition };
        assert_eq!(transition.StateBefore.0, 4);
        assert_eq!(transition.StateAfter.0, 0x40);
        assert_eq!(
            transition.pResource.as_ref().map(|r| r.as_raw() as usize),
            Some(0x1234)
        );
    }
}
