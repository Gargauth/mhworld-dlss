//! Hooked vtable slots: the originals, their signatures, and calls to them.
//!
//! Each `O_*` holds the function a slot held before it was patched; it is
//! stored before the slot is written (`mhw_win::patch_vtable`), so a hook
//! never finds it zero.

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows::core::{GUID, HRESULT};
use windows::Win32::Graphics::Direct3D12::*;

macro_rules! originals {
    ($($name:ident),* $(,)?) => {
        $(pub(crate) static $name: AtomicUsize = AtomicUsize::new(0);)*
    };
}

originals!(
    O_CREATE_COMMITTED,
    O_CREATE_SRV,
    O_CREATE_UAV,
    O_CREATE_RTV,
    O_CREATE_DSV,
    O_CREATE_CBV,
    O_CREATE_SAMPLER,
    O_COPY_DESCRIPTORS,
    O_COPY_DESCRIPTORS_SIMPLE,
    O_CREATE_ROOT_SIG,
    O_CREATE_COMPUTE_PSO,
    O_CREATE_GRAPHICS_PSO,
    O_CREATE_PIPELINE_STATE,
    O_MAP,
    O_UNMAP,
    O_EXECUTE,
    O_SET_PIPELINE,
    O_SET_HEAPS,
    O_SET_ROOT_SIG,
    O_SET_ROOT_TABLE,
    O_SET_ROOT_CBV,
    O_SET_ROOT_SRV,
    O_SET_ROOT_UAV,
    O_SET_ROOT_CONSTANTS,
    O_DISPATCH,
    O_CLOSE,
    O_BARRIER,
    O_OM_SET_TARGETS,
    O_DRAW,
    O_DRAW_INDEXED,
);

/// The original in `slot`, as `F`.
///
/// # Safety
/// `slot` must hold a function of signature `F` (non-zero: it is filled
/// before the hook that reads it can run).
pub(crate) unsafe fn original<F: Copy>(slot: &AtomicUsize) -> F {
    let address = slot.load(Ordering::Relaxed);
    debug_assert!(address != 0, "hook reached before its original was stored");
    debug_assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<usize>());
    // SAFETY: caller guarantees `F` is the slot's function pointer type.
    unsafe { std::mem::transmute_copy::<usize, F>(&address) }
}

pub(crate) type CreateCommittedFn = unsafe extern "system" fn(
    *mut c_void,
    *const D3D12_HEAP_PROPERTIES,
    D3D12_HEAP_FLAGS,
    *const D3D12_RESOURCE_DESC,
    D3D12_RESOURCE_STATES,
    *const D3D12_CLEAR_VALUE,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;
pub(crate) type CreateSrvFn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    *const D3D12_SHADER_RESOURCE_VIEW_DESC,
    D3D12_CPU_DESCRIPTOR_HANDLE,
);
pub(crate) type CreateUavFn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    *mut c_void,
    *const D3D12_UNORDERED_ACCESS_VIEW_DESC,
    D3D12_CPU_DESCRIPTOR_HANDLE,
);
pub(crate) type CreateTargetViewFn =
    unsafe extern "system" fn(*mut c_void, *mut c_void, *const c_void, D3D12_CPU_DESCRIPTOR_HANDLE);
pub(crate) type CreateDescViewFn =
    unsafe extern "system" fn(*mut c_void, *const c_void, D3D12_CPU_DESCRIPTOR_HANDLE);
pub(crate) type CopyDescriptorsFn = unsafe extern "system" fn(
    *mut c_void,
    u32,
    *const D3D12_CPU_DESCRIPTOR_HANDLE,
    *const u32,
    u32,
    *const D3D12_CPU_DESCRIPTOR_HANDLE,
    *const u32,
    D3D12_DESCRIPTOR_HEAP_TYPE,
);
pub(crate) type CopyDescriptorsSimpleFn = unsafe extern "system" fn(
    *mut c_void,
    u32,
    D3D12_CPU_DESCRIPTOR_HANDLE,
    D3D12_CPU_DESCRIPTOR_HANDLE,
    D3D12_DESCRIPTOR_HEAP_TYPE,
);
pub(crate) type CreateRootSigFn = unsafe extern "system" fn(
    *mut c_void,
    u32,
    *const c_void,
    usize,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;
pub(crate) type CreateComputePsoFn = unsafe extern "system" fn(
    *mut c_void,
    *const D3D12_COMPUTE_PIPELINE_STATE_DESC,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;
pub(crate) type CreateGraphicsPsoFn = unsafe extern "system" fn(
    *mut c_void,
    *const D3D12_GRAPHICS_PIPELINE_STATE_DESC,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;
pub(crate) type CreatePipelineStateFn = unsafe extern "system" fn(
    *mut c_void,
    *const D3D12_PIPELINE_STATE_STREAM_DESC,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;
pub(crate) type MapFn =
    unsafe extern "system" fn(*mut c_void, u32, *const D3D12_RANGE, *mut *mut c_void) -> HRESULT;
pub(crate) type UnmapFn = unsafe extern "system" fn(*mut c_void, u32, *const D3D12_RANGE);
pub(crate) type ExecuteFn = unsafe extern "system" fn(*mut c_void, u32, *const *mut c_void);
pub(crate) type SetObjectFn = unsafe extern "system" fn(*mut c_void, *mut c_void);
pub(crate) type SetHeapsFn = unsafe extern "system" fn(*mut c_void, u32, *const *mut c_void);
pub(crate) type SetRootTableFn =
    unsafe extern "system" fn(*mut c_void, u32, D3D12_GPU_DESCRIPTOR_HANDLE);
pub(crate) type SetRootAddressFn = unsafe extern "system" fn(*mut c_void, u32, u64);
pub(crate) type SetRootConstantsFn =
    unsafe extern "system" fn(*mut c_void, u32, u32, *const c_void, u32);
pub(crate) type DispatchFn = unsafe extern "system" fn(*mut c_void, u32, u32, u32);
pub(crate) type DrawFn = unsafe extern "system" fn(*mut c_void, u32, u32, u32, u32);
pub(crate) type DrawIndexedFn = unsafe extern "system" fn(*mut c_void, u32, u32, u32, i32, u32);
pub(crate) type CloseFn = unsafe extern "system" fn(*mut c_void) -> HRESULT;
pub(crate) type BarrierFn =
    unsafe extern "system" fn(*mut c_void, u32, *const D3D12_RESOURCE_BARRIER);
pub(crate) type OmSetTargetsFn = unsafe extern "system" fn(
    *mut c_void,
    u32,
    *const D3D12_CPU_DESCRIPTOR_HANDLE,
    windows::Win32::Foundation::BOOL,
    *const D3D12_CPU_DESCRIPTOR_HANDLE,
);
