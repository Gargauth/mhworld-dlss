//! Command list hooks: root state, heaps, targets, barriers, close, the
//! temporal dispatch, and draws (for the HUD-less detection only).
//!
//! The state hooks record what the engine binds (not while this thread
//! records our own work) and forward. They run tens of thousands of times a
//! frame: one uncontended per-list lock each, no allocation in the steady
//! state.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::core::{Interface, HRESULT};
use windows::Win32::Foundation::BOOL;
use windows::Win32::Graphics::Direct3D12::*;

use super::registry::{self, DEPTHS, DESCRIPTORS, DSV_HANDLES, STATES};
use super::vt::{self, original};
use super::{alive, hudless, liststate, pass, pso, record};
use crate::descriptors::Heap;
use crate::recognise::PassKind;

static DISPATCHES: AtomicU64 = AtomicU64::new(0);
static UPSAMPLE_DISPATCHES: AtomicU64 = AtomicU64::new(0);
static CLOSED: AtomicU64 = AtomicU64::new(0);
static CLOSE_FAILURES: AtomicU64 = AtomicU64::new(0);

/// `(dispatches, temporal-upsample dispatches, lists closed, closes failed)`.
pub(crate) fn counts() -> (u64, u64, u64, u64) {
    (
        DISPATCHES.load(Ordering::Relaxed),
        UPSAMPLE_DISPATCHES.load(Ordering::Relaxed),
        CLOSED.load(Ordering::Relaxed),
        CLOSE_FAILURES.load(Ordering::Relaxed),
    )
}

pub(crate) unsafe extern "system" fn set_pipeline(this: *mut c_void, pipeline: *mut c_void) {
    if !record::reentrant() {
        liststate::with(this, |state| state.pipeline = pipeline as usize);
    }
    // SAFETY: the original with the caller's own arguments.
    unsafe { original::<vt::SetObjectFn>(&vt::O_SET_PIPELINE)(this, pipeline) }
}

pub(crate) unsafe extern "system" fn set_heaps(
    this: *mut c_void,
    count: u32,
    heaps: *const *mut c_void,
) {
    if !heaps.is_null() && !record::reentrant() {
        // SAFETY: the caller passes `count` live heaps.
        let bound = unsafe { std::slice::from_raw_parts(heaps, count.min(2) as usize) };
        for &heap in bound {
            // SAFETY: a live descriptor heap.
            unsafe { note_heap(heap) };
        }
        liststate::with(this, |state| {
            state.heaps.clear();
            state.heaps.extend(bound.iter().map(|heap| *heap as usize));
        });
    }
    // SAFETY: the original with the caller's own arguments.
    unsafe { original::<vt::SetHeapsFn>(&vt::O_SET_HEAPS)(this, count, heaps) }
}

/// Record a heap's geometry the first time it is seen with its base (a
/// recycled heap address has a different base).
///
/// # Safety
/// `heap` must be null or a live descriptor heap.
unsafe fn note_heap(heap: *mut c_void) {
    // SAFETY: caller's contract.
    let Some(object) = (unsafe { ID3D12DescriptorHeap::from_raw_borrowed(&heap) }) else {
        return;
    };
    // SAFETY: a live heap; the call has no preconditions.
    let base = unsafe { object.GetCPUDescriptorHandleForHeapStart() }.ptr;
    if DESCRIPTORS.heap_known(heap as usize, base) {
        return;
    }
    // SAFETY: a live heap. The GPU start is asked only of a shader-visible
    // heap, where it is valid.
    unsafe {
        let desc = object.GetDesc();
        let mut stride = registry::known_stride(desc.Type);
        if stride == 0 {
            let mut device: Option<ID3D12Device> = None;
            if object.GetDevice(&mut device).is_ok() {
                if let Some(device) = device {
                    stride = registry::stride(device.as_raw(), desc.Type);
                }
            }
        }
        let shader_visible = desc.Flags.0 & D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE.0 != 0;
        DESCRIPTORS.heap(
            heap as usize,
            Heap {
                kind: desc.Type.0 as u32,
                cpu_start: base,
                gpu_start: if shader_visible {
                    object.GetGPUDescriptorHandleForHeapStart().ptr
                } else {
                    0
                },
                stride,
                count: desc.NumDescriptors,
                shader_visible,
            },
        );
    }
}

pub(crate) unsafe extern "system" fn set_root_signature(this: *mut c_void, signature: *mut c_void) {
    if !record::reentrant() {
        liststate::with(this, |state| state.set_signature(signature as usize));
    }
    // SAFETY: the original with the caller's own arguments.
    unsafe { original::<vt::SetObjectFn>(&vt::O_SET_ROOT_SIG)(this, signature) }
}

pub(crate) unsafe extern "system" fn set_root_table(
    this: *mut c_void,
    root: u32,
    base: D3D12_GPU_DESCRIPTOR_HANDLE,
) {
    if !record::reentrant() {
        liststate::with(this, |state| state.set_table(root, base.ptr));
    }
    // SAFETY: the original with the caller's own arguments.
    unsafe { original::<vt::SetRootTableFn>(&vt::O_SET_ROOT_TABLE)(this, root, base) }
}

pub(crate) unsafe extern "system" fn set_root_cbv(this: *mut c_void, root: u32, address: u64) {
    if !record::reentrant() {
        liststate::with(this, |state| state.set_cbv(root, address));
    }
    // SAFETY: the original with the caller's own arguments.
    unsafe { original::<vt::SetRootAddressFn>(&vt::O_SET_ROOT_CBV)(this, root, address) }
}

pub(crate) unsafe extern "system" fn set_root_srv(this: *mut c_void, root: u32, address: u64) {
    if !record::reentrant() {
        liststate::with(this, |state| state.set_view(root, address, false));
    }
    // SAFETY: the original with the caller's own arguments.
    unsafe { original::<vt::SetRootAddressFn>(&vt::O_SET_ROOT_SRV)(this, root, address) }
}

pub(crate) unsafe extern "system" fn set_root_uav(this: *mut c_void, root: u32, address: u64) {
    if !record::reentrant() {
        liststate::with(this, |state| state.set_view(root, address, true));
    }
    // SAFETY: the original with the caller's own arguments.
    unsafe { original::<vt::SetRootAddressFn>(&vt::O_SET_ROOT_UAV)(this, root, address) }
}

pub(crate) unsafe extern "system" fn set_root_constants(
    this: *mut c_void,
    root: u32,
    count: u32,
    data: *const c_void,
    offset: u32,
) {
    if !data.is_null() && !record::reentrant() && count <= 64 {
        // SAFETY: the caller passes `count` dwords at `data`.
        let values = unsafe { std::slice::from_raw_parts(data.cast::<u32>(), count as usize) };
        liststate::with(this, |state| state.set_constants(root, offset, values));
    }
    // SAFETY: the original with the caller's own arguments.
    unsafe {
        original::<vt::SetRootConstantsFn>(&vt::O_SET_ROOT_CONSTANTS)(
            this, root, count, data, offset,
        )
    }
}

pub(crate) unsafe extern "system" fn resource_barrier(
    this: *mut c_void,
    count: u32,
    barriers: *const D3D12_RESOURCE_BARRIER,
) {
    // Observed even for our own barriers: they are real state changes.
    if !barriers.is_null() && count > 0 {
        // SAFETY: the caller passes `count` barriers; the transition member
        // is read only for transition barriers.
        unsafe {
            let batch = std::slice::from_raw_parts(barriers, count as usize);
            let mut states = STATES.lock();
            for barrier in batch {
                if barrier.Type != D3D12_RESOURCE_BARRIER_TYPE_TRANSITION {
                    continue;
                }
                let transition = &barrier.Anonymous.Transition;
                let resource = transition
                    .pResource
                    .as_ref()
                    .map_or(0, |resource| resource.as_raw() as usize);
                states.transition(
                    this as usize,
                    resource,
                    transition.StateAfter.0 as u32,
                    transition.Subresource == D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                );
            }
        }
    }
    // SAFETY: the original with the caller's own arguments.
    unsafe { original::<vt::BarrierFn>(&vt::O_BARRIER)(this, count, barriers) }
}

pub(crate) unsafe extern "system" fn om_set_targets(
    this: *mut c_void,
    count: u32,
    rtvs: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    single_range: BOOL,
    depth: *const D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    if !record::reentrant() {
        // With a single range, `rtvs` points at one handle standing for
        // `count` contiguous ones; reading `count` entries ran off a stack
        // variable once.
        let supplied = if single_range.as_bool() {
            1
        } else {
            count.min(8) as usize
        };
        // SAFETY: the caller passes `supplied` handles (or null) and one
        // depth handle (or null).
        let (bound, dsv) = unsafe {
            (
                if rtvs.is_null() || count == 0 {
                    &[][..]
                } else {
                    std::slice::from_raw_parts(rtvs, supplied)
                },
                depth.as_ref().copied(),
            )
        };
        let dsv_resource = dsv
            .and_then(|handle| DSV_HANDLES.lock().get(&handle.ptr).copied())
            .unwrap_or(0);
        liststate::with(this, |state| {
            let targets = &mut state.targets;
            targets.rtvs.clear();
            targets.rtvs.extend_from_slice(bound);
            targets.count = count;
            targets.single_range = single_range.as_bool();
            targets.dsv = dsv;
            targets.dsv_resource = dsv_resource;
        });
        note_depth(dsv_resource);
        if hudless::active() {
            hudless::on_bind(this, bound.first().map(|handle| handle.ptr));
        }
    }
    // SAFETY: the original with the caller's own arguments.
    unsafe {
        original::<vt::OmSetTargetsFn>(&vt::O_OM_SET_TARGETS)(
            this,
            count,
            rtvs,
            single_range,
            depth,
        )
    }
}

/// A depth target was bound: a scene-depth candidate if it is frame-sized.
fn note_depth(resource: usize) {
    let Some(info) = registry::describe(resource) else {
        return;
    };
    if !registry::frame_sized(info.width, info.height) {
        return;
    }
    let Some(serial) = alive::serial_of(resource) else {
        return;
    };
    DEPTHS
        .lock()
        .bound(resource, serial, info.size(), registry::frame());
}

pub(crate) unsafe extern "system" fn close(this: *mut c_void) -> HRESULT {
    CLOSED.fetch_add(1, Ordering::Relaxed);
    // Held, not discarded: its transitions take effect at submission.
    STATES.lock().close(this as usize);
    // Everything: a recycled list address must not inherit bindings.
    liststate::with(this, liststate::ListState::reset);
    // SAFETY: the original with the caller's own arguments.
    let result = unsafe { original::<vt::CloseFn>(&vt::O_CLOSE)(this) };
    if result.is_err() {
        // A list fails to close only when something recorded into it was
        // invalid; since we record into the engine's lists, say so at once.
        CLOSE_FAILURES.fetch_add(1, Ordering::Relaxed);
        mhw_core::log_once!(
            Error,
            "render",
            "close-failed",
            "a command list failed to close: {:#010x}",
            result.0
        );
    }
    result
}

pub(crate) unsafe extern "system" fn dispatch(this: *mut c_void, x: u32, y: u32, z: u32) {
    DISPATCHES.fetch_add(1, Ordering::Relaxed);
    let mut skip = false;
    if !record::reentrant() {
        let pipeline = liststate::with(this, |state| state.pipeline);
        match pso::pass_of(pipeline) {
            Some((PassKind::TemporalAa, reflection)) => {
                // SAFETY: `this` is the live, open list being recorded.
                skip = unsafe { pass::temporal_dispatch(this, [x, y, z], &reflection) };
            }
            Some((PassKind::TemporalUpsample, _)) => {
                UPSAMPLE_DISPATCHES.fetch_add(1, Ordering::Relaxed);
                mhw_core::log_once!(
                    Warn,
                    "taa",
                    "upsample-dispatch",
                    "the engine runs CSTemporalUpsample (its temporal-upsample mode): \
                     CSTemporalAA2 does not run and no inputs are identified"
                );
            }
            _ => {}
        }
    }
    if skip {
        return;
    }
    // SAFETY: the original with the caller's own arguments.
    unsafe { original::<vt::DispatchFn>(&vt::O_DISPATCH)(this, x, y, z) }
}

pub(crate) unsafe extern "system" fn draw(
    this: *mut c_void,
    vertices: u32,
    instances: u32,
    first_vertex: u32,
    first_instance: u32,
) {
    if hudless::active() && !record::reentrant() {
        // SAFETY: `this` is the live, open list being recorded.
        unsafe { hudless::on_draw(this) };
    }
    // SAFETY: the original with the caller's own arguments.
    unsafe {
        original::<vt::DrawFn>(&vt::O_DRAW)(this, vertices, instances, first_vertex, first_instance)
    }
}

pub(crate) unsafe extern "system" fn draw_indexed(
    this: *mut c_void,
    indices: u32,
    instances: u32,
    first_index: u32,
    base_vertex: i32,
    first_instance: u32,
) {
    if hudless::active() && !record::reentrant() {
        // SAFETY: `this` is the live, open list being recorded.
        unsafe { hudless::on_draw(this) };
    }
    // SAFETY: the original with the caller's own arguments.
    unsafe {
        original::<vt::DrawIndexedFn>(&vt::O_DRAW_INDEXED)(
            this,
            indices,
            instances,
            first_index,
            base_vertex,
            first_instance,
        )
    }
}
