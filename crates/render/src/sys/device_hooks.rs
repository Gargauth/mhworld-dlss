//! Device and resource hooks: creation records, views, descriptor copies,
//! root signatures and mapped buffers.
//!
//! These record and forward; none changes what the engine gets.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Graphics::Direct3D12::*;

use super::registry::{
    self, DESCRIPTORS, DSV_HANDLES, ROOT_SIGNATURES, RTV_HANDLES, STATES, UPLOAD,
};
use super::vt::{self, original};
use super::{alive, record};
use crate::descriptors::ViewKind;
use crate::rootsig::{Layout, Param, RangeDesc, RangeKind};

static RESOURCES_CREATED: AtomicU64 = AtomicU64::new(0);
static ROOT_SIGNATURES_SEEN: AtomicU64 = AtomicU64::new(0);

/// `(resources created, root signatures deserialised)`.
pub(crate) fn counts() -> (u64, u64) {
    (
        RESOURCES_CREATED.load(Ordering::Relaxed),
        ROOT_SIGNATURES_SEEN.load(Ordering::Relaxed),
    )
}

pub(crate) unsafe extern "system" fn create_committed(
    this: *mut c_void,
    heap: *const D3D12_HEAP_PROPERTIES,
    flags: D3D12_HEAP_FLAGS,
    desc: *const D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
    clear: *const D3D12_CLEAR_VALUE,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: the original with the caller's own arguments.
    let result = unsafe {
        original::<vt::CreateCommittedFn>(&vt::O_CREATE_COMMITTED)(
            this, heap, flags, desc, state, clear, riid, out,
        )
    };
    // SAFETY: on success `out` holds the new resource; `desc` is the
    // caller's description, valid for the call.
    unsafe {
        if result.is_err() || desc.is_null() || out.is_null() || (*out).is_null() {
            return result;
        }
        let (resource, desc) = (*out, &*desc);
        RESOURCES_CREATED.fetch_add(1, Ordering::Relaxed);
        if desc.Width >= 256 && desc.Height >= 256 {
            registry::described(resource as usize, desc);
            // The state it starts in, so nothing a destroyed resource at
            // this address left behind is taken for its own.
            STATES.lock().created(resource as usize, state.0 as u32);
        }
        if registry::frame_sized(desc.Width, desc.Height) {
            alive::watch(resource);
        }
    }
    result
}

/// A view of `resource` was created at `handle`.
fn view_created(resource: *mut c_void, handle: usize, view: ViewKind) {
    if record::reentrant() {
        return;
    }
    let serial = registry::worth_following(resource as usize);
    DESCRIPTORS.created(handle, resource as usize, view, serial);
}

pub(crate) unsafe extern "system" fn create_srv(
    this: *mut c_void,
    resource: *mut c_void,
    desc: *const D3D12_SHADER_RESOURCE_VIEW_DESC,
    handle: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    view_created(resource, handle.ptr, ViewKind::Srv);
    // SAFETY: the original with the caller's own arguments.
    unsafe { original::<vt::CreateSrvFn>(&vt::O_CREATE_SRV)(this, resource, desc, handle) }
}

pub(crate) unsafe extern "system" fn create_uav(
    this: *mut c_void,
    resource: *mut c_void,
    counter: *mut c_void,
    desc: *const D3D12_UNORDERED_ACCESS_VIEW_DESC,
    handle: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    view_created(resource, handle.ptr, ViewKind::Uav);
    // SAFETY: the original with the caller's own arguments.
    unsafe { original::<vt::CreateUavFn>(&vt::O_CREATE_UAV)(this, resource, counter, desc, handle) }
}

pub(crate) unsafe extern "system" fn create_rtv(
    this: *mut c_void,
    resource: *mut c_void,
    desc: *const c_void,
    handle: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    // The slot no longer holds whatever view was recorded for it.
    DESCRIPTORS.forget(handle.ptr);
    RTV_HANDLES.lock().insert(handle.ptr, resource as usize);
    // SAFETY: the original with the caller's own arguments.
    unsafe { original::<vt::CreateTargetViewFn>(&vt::O_CREATE_RTV)(this, resource, desc, handle) }
}

pub(crate) unsafe extern "system" fn create_dsv(
    this: *mut c_void,
    resource: *mut c_void,
    desc: *const c_void,
    handle: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    DESCRIPTORS.forget(handle.ptr);
    DSV_HANDLES.lock().insert(handle.ptr, resource as usize);
    // SAFETY: the original with the caller's own arguments.
    unsafe { original::<vt::CreateTargetViewFn>(&vt::O_CREATE_DSV)(this, resource, desc, handle) }
}

pub(crate) unsafe extern "system" fn create_cbv(
    this: *mut c_void,
    desc: *const c_void,
    handle: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    DESCRIPTORS.forget(handle.ptr);
    // SAFETY: the original with the caller's own arguments.
    unsafe { original::<vt::CreateDescViewFn>(&vt::O_CREATE_CBV)(this, desc, handle) }
}

pub(crate) unsafe extern "system" fn create_sampler(
    this: *mut c_void,
    desc: *const c_void,
    handle: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    DESCRIPTORS.forget(handle.ptr);
    // SAFETY: the original with the caller's own arguments.
    unsafe { original::<vt::CreateDescViewFn>(&vt::O_CREATE_SAMPLER)(this, desc, handle) }
}

pub(crate) unsafe extern "system" fn copy_descriptors_simple(
    this: *mut c_void,
    count: u32,
    destination: D3D12_CPU_DESCRIPTOR_HANDLE,
    source: D3D12_CPU_DESCRIPTOR_HANDLE,
    kind: D3D12_DESCRIPTOR_HEAP_TYPE,
) {
    if kind == D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV {
        // SAFETY: `this` is the live device the call was made on.
        let stride = unsafe { registry::stride(this, kind) };
        DESCRIPTORS.copy(destination.ptr, source.ptr, count, stride);
    }
    // SAFETY: the original with the caller's own arguments.
    unsafe {
        original::<vt::CopyDescriptorsSimpleFn>(&vt::O_COPY_DESCRIPTORS_SIMPLE)(
            this,
            count,
            destination,
            source,
            kind,
        )
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) unsafe extern "system" fn copy_descriptors(
    this: *mut c_void,
    destination_ranges: u32,
    destination_starts: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    destination_sizes: *const u32,
    source_ranges: u32,
    source_starts: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    source_sizes: *const u32,
    kind: D3D12_DESCRIPTOR_HEAP_TYPE,
) {
    if kind == D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV
        && !destination_starts.is_null()
        && !source_starts.is_null()
    {
        // SAFETY: the arrays have the counts the caller passed (D3D12
        // contract: null sizes mean ranges of one); `this` is the device.
        unsafe {
            let stride = registry::stride(this, kind);
            let size_at = |sizes: *const u32, index: usize| {
                if sizes.is_null() {
                    1
                } else {
                    *sizes.add(index)
                }
            };
            // Both sides describe one flat sequence, split differently.
            let (mut d, mut s, mut d_off, mut s_off) = (0usize, 0usize, 0u32, 0u32);
            while d < destination_ranges as usize && s < source_ranges as usize {
                let (d_size, s_size) = (size_at(destination_sizes, d), size_at(source_sizes, s));
                let run = d_size
                    .saturating_sub(d_off)
                    .min(s_size.saturating_sub(s_off));
                if run > 0 {
                    DESCRIPTORS.copy(
                        (*destination_starts.add(d)).ptr + d_off as usize * stride,
                        (*source_starts.add(s)).ptr + s_off as usize * stride,
                        run,
                        stride,
                    );
                }
                d_off += run;
                s_off += run;
                if d_off >= d_size {
                    d += 1;
                    d_off = 0;
                }
                if s_off >= s_size {
                    s += 1;
                    s_off = 0;
                }
            }
        }
    }
    // SAFETY: the original with the caller's own arguments.
    unsafe {
        original::<vt::CopyDescriptorsFn>(&vt::O_COPY_DESCRIPTORS)(
            this,
            destination_ranges,
            destination_starts,
            destination_sizes,
            source_ranges,
            source_starts,
            source_sizes,
            kind,
        )
    }
}

pub(crate) unsafe extern "system" fn create_root_signature(
    this: *mut c_void,
    node_mask: u32,
    blob: *const c_void,
    len: usize,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: the original with the caller's own arguments.
    let result = unsafe {
        original::<vt::CreateRootSigFn>(&vt::O_CREATE_ROOT_SIG)(
            this, node_mask, blob, len, riid, out,
        )
    };
    // SAFETY: on success `out` holds the new signature; `blob` is the
    // caller's `len` bytes, valid for the call.
    unsafe {
        if result.is_err() || out.is_null() || (*out).is_null() {
            return result;
        }
        let signature = *out as usize;
        match deserialise(blob, len) {
            Some(layout) => {
                ROOT_SIGNATURES_SEEN.fetch_add(1, Ordering::Relaxed);
                log::trace!(target: "render", "root signature {signature:#x}: {}", layout.summary());
                ROOT_SIGNATURES.lock().insert(signature, Arc::new(layout));
            }
            // A recycled address must not keep a dead signature's layout.
            None => {
                ROOT_SIGNATURES.lock().remove(&signature);
            }
        }
    }
    result
}

/// The parameters of a 1.0 or 1.1 root signature description (the two
/// differ only in types, not in the fields read here). Ranges of unknown
/// type are dropped; parameters never are, so root indices stay right.
macro_rules! params_of {
    ($desc:expr) => {{
        let desc = &$desc;
        let mut params = Vec::with_capacity(desc.NumParameters as usize);
        for index in 0..desc.NumParameters as usize {
            let parameter = &*desc.pParameters.add(index);
            let union = &parameter.Anonymous;
            let param = match parameter.ParameterType {
                D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE => {
                    let table = &union.DescriptorTable;
                    Param::Table(
                        (0..table.NumDescriptorRanges as usize)
                            .filter_map(|slot| {
                                let r = &*table.pDescriptorRanges.add(slot);
                                Some(RangeDesc {
                                    kind: RangeKind::from_raw(r.RangeType.0)?,
                                    count: r.NumDescriptors,
                                    base_register: r.BaseShaderRegister,
                                    space: r.RegisterSpace,
                                    offset: r.OffsetInDescriptorsFromTableStart,
                                })
                            })
                            .collect(),
                    )
                }
                D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS => Param::Constants {
                    register: union.Constants.ShaderRegister,
                    space: union.Constants.RegisterSpace,
                    count: union.Constants.Num32BitValues,
                },
                D3D12_ROOT_PARAMETER_TYPE_CBV => Param::Cbv {
                    register: union.Descriptor.ShaderRegister,
                    space: union.Descriptor.RegisterSpace,
                },
                D3D12_ROOT_PARAMETER_TYPE_SRV => Param::Srv {
                    register: union.Descriptor.ShaderRegister,
                    space: union.Descriptor.RegisterSpace,
                },
                D3D12_ROOT_PARAMETER_TYPE_UAV => Param::Uav {
                    register: union.Descriptor.ShaderRegister,
                    space: union.Descriptor.RegisterSpace,
                },
                _ => Param::Other,
            };
            params.push(param);
        }
        params
    }};
}
/// The layout of a serialised root signature, through D3D12's own
/// deserialiser.
///
/// # Safety
/// `blob` must be readable for `len` bytes.
unsafe fn deserialise(blob: *const c_void, len: usize) -> Option<Layout> {
    if blob.is_null() || len == 0 {
        return None;
    }
    let mut raw: *mut c_void = std::ptr::null_mut();
    // SAFETY: the caller's blob; `raw` receives an owned interface.
    let deserialiser = unsafe {
        D3D12CreateVersionedRootSignatureDeserializer(
            blob,
            len,
            &ID3D12VersionedRootSignatureDeserializer::IID,
            &mut raw,
        )
        .ok()?;
        if raw.is_null() {
            return None;
        }
        ID3D12VersionedRootSignatureDeserializer::from_raw(raw)
    };
    // SAFETY: the description lives as long as the deserialiser, which
    // outlives every read below.
    let versioned = unsafe { deserialiser.GetUnconvertedRootSignatureDesc().as_ref() }?;
    // SAFETY: the union member matching `Version`, whose arrays have the
    // counts it declares; each parameter's union is read through the member
    // its `ParameterType` names.
    let params = unsafe {
        match versioned.Version {
            D3D_ROOT_SIGNATURE_VERSION_1_0 => params_of!(versioned.Anonymous.Desc_1_0),
            D3D_ROOT_SIGNATURE_VERSION_1_1 | D3D_ROOT_SIGNATURE_VERSION_1_2 => {
                params_of!(versioned.Anonymous.Desc_1_1)
            }
            other => {
                mhw_core::log_once!(
                    Warn,
                    "render",
                    other.0,
                    "root signature version {} not understood",
                    other.0
                );
                return None;
            }
        }
    };
    Some(Layout::new(&params))
}

pub(crate) unsafe extern "system" fn map(
    this: *mut c_void,
    subresource: u32,
    range: *const D3D12_RANGE,
    data: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: the original with the caller's own arguments.
    let result = unsafe { original::<vt::MapFn>(&vt::O_MAP)(this, subresource, range, data) };
    // SAFETY: on success `data` holds the CPU pointer of `this`, a live
    // resource.
    unsafe {
        if result.is_ok() && subresource == 0 && !data.is_null() && !(*data).is_null() {
            if let Some(resource) = ID3D12Resource::from_raw_borrowed(&this) {
                let gpu = resource.GetGPUVirtualAddress();
                let size = resource.GetDesc().Width;
                UPLOAD.lock().mapped(gpu, size, *data as usize);
            }
        }
    }
    result
}

pub(crate) unsafe extern "system" fn unmap(
    this: *mut c_void,
    subresource: u32,
    range: *const D3D12_RANGE,
) {
    // Before the call: no window where the registry hands out a pointer
    // the runtime no longer backs.
    if subresource == 0 {
        // SAFETY: `this` is the live resource being unmapped.
        if let Some(resource) = unsafe { ID3D12Resource::from_raw_borrowed(&this) } {
            // SAFETY: a live resource; no preconditions.
            let gpu = unsafe { resource.GetGPUVirtualAddress() };
            UPLOAD.lock().unmapped(gpu);
        }
    }
    // SAFETY: the original with the caller's own arguments.
    unsafe { original::<vt::UnmapFn>(&vt::O_UNMAP)(this, subresource, range) }
}
