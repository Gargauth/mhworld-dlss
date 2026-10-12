//! Installing the vtable hooks through a device of our own.
//!
//! D3D12 objects are implemented by the runtime and every object of a class
//! shares one vtable, so patching a slot through objects this crate creates
//! redirects the engine's calls too: no game address is involved. What
//! matters is *when*: before the engine creates its device, so no view,
//! mapping or pipeline is missed. The debug layer (if asked for) is enabled
//! first, so our objects wear the same layer the game's will.
//!
//! The objects are kept for the life of the process: released, the engine's
//! own device could be allocated at the same address, and "the same
//! pointer" would stop meaning "the same object" in the log.

use std::ffi::c_void;
use std::fmt;
use std::sync::atomic::AtomicUsize;

use mhw_win::{patch_vtable, slot, vtable_of, VtableError};
use parking_lot::Mutex;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};

use super::vt::*;
use super::{creation, device_hooks as dev, diag, list_hooks as list, pso, queue, reshade};

/// Why the hooks could not be installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    /// `D3D12CreateDevice` could not be resolved.
    NoD3d12,
    /// One of our objects could not be created (text from D3D12).
    Create(&'static str, String),
    /// A slot could not be patched.
    Patch(&'static str, VtableError),
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstallError::NoD3d12 => f.write_str("D3D12CreateDevice not found"),
            InstallError::Create(what, error) => write!(f, "could not create our {what}: {error}"),
            InstallError::Patch(slot, error) => write!(f, "could not patch {slot}: {error}"),
        }
    }
}

impl std::error::Error for InstallError {}

/// Our objects, held for the process; their presence means installed.
struct Objects {
    _device: ID3D12Device,
    _list: ID3D12GraphicsCommandList,
    _allocator: ID3D12CommandAllocator,
    _queue: ID3D12CommandQueue,
    _buffer: ID3D12Resource,
}

// SAFETY: D3D12 objects are free-threaded; these are only held.
unsafe impl Send for Objects {}

static INSTALLED: Mutex<Option<Objects>> = Mutex::new(None);

/// Whether the hooks are installed.
pub fn installed() -> bool {
    INSTALLED.lock().is_some()
}

/// Install every vtable hook, through a device of our own (default adapter).
/// Idempotent; call from `mod`'s boot thread before the game creates its
/// device (the device replacement calls it too, as a fallback). Not under
/// the loader lock.
pub fn install() -> Result<(), InstallError> {
    let mut installed = INSTALLED.lock();
    if installed.is_some() {
        return Ok(());
    }
    diag::enable(creation::options());
    // SAFETY: our own objects on our own device; the patched replacements
    // have exactly the signatures of their slots (vt.rs).
    let objects = unsafe { create()? };
    // SAFETY: as above.
    unsafe { patch_all(&objects)? };
    *installed = Some(objects);
    log::info!(target: "render", "render hooks installed");
    Ok(())
}

/// # Safety
/// Not under the loader lock.
unsafe fn create() -> Result<Objects, InstallError> {
    let create_device = creation::real_create_device().ok_or(InstallError::NoD3d12)?;
    let failed = |what: &'static str| {
        move |error: windows::core::Error| InstallError::Create(what, error.to_string())
    };
    let mut raw: *mut c_void = std::ptr::null_mut();
    // SAFETY: the real D3D12CreateDevice (not our replacement, so this is
    // not counted as the game's) on the default adapter.
    let device = unsafe {
        create_device(
            std::ptr::null_mut(),
            D3D_FEATURE_LEVEL_11_0.0,
            &ID3D12Device::IID,
            &mut raw,
        )
        .ok()
        .map_err(failed("device"))?;
        if raw.is_null() {
            return Err(InstallError::Create("device", "no device returned".into()));
        }
        ID3D12Device::from_raw(raw)
    };
    // A layer that hooked the export (ReShade) hands back its proxy; the
    // vtables wanted are the native ones.
    reshade::detect_reshade();
    // SAFETY: a live device.
    let device = unsafe { reshade::native_interface(device) };
    // SAFETY: plain object creation on a live device.
    unsafe {
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .map_err(failed("command allocator"))?;
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .map_err(failed("command list"))?;
        list.Close().map_err(failed("command list"))?;
        // Nothing is ever submitted to it; it is how ExecuteCommandLists's
        // slot is reached.
        let queue: ID3D12CommandQueue = device
            .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })
            .map_err(failed("command queue"))?;
        // The cheapest way to ID3D12Resource's Map/Unmap.
        let mut buffer: Option<ID3D12Resource> = None;
        device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_UPLOAD,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Alignment: 0,
                    Width: 256,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: DXGI_FORMAT_UNKNOWN,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    Flags: D3D12_RESOURCE_FLAG_NONE,
                },
                D3D12_RESOURCE_STATE_GENERIC_READ,
                None,
                &mut buffer,
            )
            .map_err(failed("buffer"))?;
        let buffer =
            buffer.ok_or_else(|| InstallError::Create("buffer", "none returned".into()))?;
        log::info!(target: "render", "own device {:p} for the render hooks", device.as_raw());
        Ok(Objects {
            _device: device,
            _list: list,
            _allocator: allocator,
            _queue: queue,
            _buffer: buffer,
        })
    }
}

type Hook = (&'static str, usize, *const c_void, &'static AtomicUsize);

/// # Safety
/// Each replacement must match its slot's signature.
unsafe fn patch_all(objects: &Objects) -> Result<(), InstallError> {
    let device_hooks: [Hook; 16] = [
        (
            "Device::CreateCommittedResource",
            slot!(ID3D12Device_Vtbl, CreateCommittedResource),
            dev::create_committed as *const c_void,
            &O_CREATE_COMMITTED,
        ),
        (
            "Device::CreateShaderResourceView",
            slot!(ID3D12Device_Vtbl, CreateShaderResourceView),
            dev::create_srv as *const c_void,
            &O_CREATE_SRV,
        ),
        (
            "Device::CreateUnorderedAccessView",
            slot!(ID3D12Device_Vtbl, CreateUnorderedAccessView),
            dev::create_uav as *const c_void,
            &O_CREATE_UAV,
        ),
        (
            "Device::CreateRenderTargetView",
            slot!(ID3D12Device_Vtbl, CreateRenderTargetView),
            dev::create_rtv as *const c_void,
            &O_CREATE_RTV,
        ),
        (
            "Device::CreateDepthStencilView",
            slot!(ID3D12Device_Vtbl, CreateDepthStencilView),
            dev::create_dsv as *const c_void,
            &O_CREATE_DSV,
        ),
        (
            "Device::CreateConstantBufferView",
            slot!(ID3D12Device_Vtbl, CreateConstantBufferView),
            dev::create_cbv as *const c_void,
            &O_CREATE_CBV,
        ),
        (
            "Device::CreateSampler",
            slot!(ID3D12Device_Vtbl, CreateSampler),
            dev::create_sampler as *const c_void,
            &O_CREATE_SAMPLER,
        ),
        (
            "Device::CopyDescriptors",
            slot!(ID3D12Device_Vtbl, CopyDescriptors),
            dev::copy_descriptors as *const c_void,
            &O_COPY_DESCRIPTORS,
        ),
        (
            "Device::CopyDescriptorsSimple",
            slot!(ID3D12Device_Vtbl, CopyDescriptorsSimple),
            dev::copy_descriptors_simple as *const c_void,
            &O_COPY_DESCRIPTORS_SIMPLE,
        ),
        (
            "Device::CreateRootSignature",
            slot!(ID3D12Device_Vtbl, CreateRootSignature),
            dev::create_root_signature as *const c_void,
            &O_CREATE_ROOT_SIG,
        ),
        (
            "Device::CreateComputePipelineState",
            slot!(ID3D12Device_Vtbl, CreateComputePipelineState),
            pso::create_compute_pso as *const c_void,
            &O_CREATE_COMPUTE_PSO,
        ),
        (
            "Device::CreateGraphicsPipelineState",
            slot!(ID3D12Device_Vtbl, CreateGraphicsPipelineState),
            pso::create_graphics_pso as *const c_void,
            &O_CREATE_GRAPHICS_PSO,
        ),
        (
            "Device2::CreatePipelineState",
            slot!(ID3D12Device2_Vtbl, CreatePipelineState),
            pso::create_pipeline_state as *const c_void,
            &O_CREATE_PIPELINE_STATE,
        ),
        (
            "Resource::Map",
            slot!(ID3D12Resource_Vtbl, Map),
            dev::map as *const c_void,
            &O_MAP,
        ),
        (
            "Resource::Unmap",
            slot!(ID3D12Resource_Vtbl, Unmap),
            dev::unmap as *const c_void,
            &O_UNMAP,
        ),
        (
            "CommandQueue::ExecuteCommandLists",
            slot!(ID3D12CommandQueue_Vtbl, ExecuteCommandLists),
            queue::execute as *const c_void,
            &O_EXECUTE,
        ),
    ];
    let list_hooks: [Hook; 14] = [
        (
            "CommandList::SetPipelineState",
            slot!(ID3D12GraphicsCommandList_Vtbl, SetPipelineState),
            list::set_pipeline as *const c_void,
            &O_SET_PIPELINE,
        ),
        (
            "CommandList::SetDescriptorHeaps",
            slot!(ID3D12GraphicsCommandList_Vtbl, SetDescriptorHeaps),
            list::set_heaps as *const c_void,
            &O_SET_HEAPS,
        ),
        (
            "CommandList::SetComputeRootSignature",
            slot!(ID3D12GraphicsCommandList_Vtbl, SetComputeRootSignature),
            list::set_root_signature as *const c_void,
            &O_SET_ROOT_SIG,
        ),
        (
            "CommandList::SetComputeRootDescriptorTable",
            slot!(
                ID3D12GraphicsCommandList_Vtbl,
                SetComputeRootDescriptorTable
            ),
            list::set_root_table as *const c_void,
            &O_SET_ROOT_TABLE,
        ),
        (
            "CommandList::SetComputeRootConstantBufferView",
            slot!(
                ID3D12GraphicsCommandList_Vtbl,
                SetComputeRootConstantBufferView
            ),
            list::set_root_cbv as *const c_void,
            &O_SET_ROOT_CBV,
        ),
        (
            "CommandList::SetComputeRootShaderResourceView",
            slot!(
                ID3D12GraphicsCommandList_Vtbl,
                SetComputeRootShaderResourceView
            ),
            list::set_root_srv as *const c_void,
            &O_SET_ROOT_SRV,
        ),
        (
            "CommandList::SetComputeRootUnorderedAccessView",
            slot!(
                ID3D12GraphicsCommandList_Vtbl,
                SetComputeRootUnorderedAccessView
            ),
            list::set_root_uav as *const c_void,
            &O_SET_ROOT_UAV,
        ),
        (
            "CommandList::SetComputeRoot32BitConstants",
            slot!(ID3D12GraphicsCommandList_Vtbl, SetComputeRoot32BitConstants),
            list::set_root_constants as *const c_void,
            &O_SET_ROOT_CONSTANTS,
        ),
        (
            "CommandList::ResourceBarrier",
            slot!(ID3D12GraphicsCommandList_Vtbl, ResourceBarrier),
            list::resource_barrier as *const c_void,
            &O_BARRIER,
        ),
        (
            "CommandList::OMSetRenderTargets",
            slot!(ID3D12GraphicsCommandList_Vtbl, OMSetRenderTargets),
            list::om_set_targets as *const c_void,
            &O_OM_SET_TARGETS,
        ),
        (
            "CommandList::Close",
            slot!(ID3D12GraphicsCommandList_Vtbl, Close),
            list::close as *const c_void,
            &O_CLOSE,
        ),
        // Draws matter only to the HUD-less detection, which they skip in
        // one relaxed load while it is off.
        (
            "CommandList::DrawInstanced",
            slot!(ID3D12GraphicsCommandList_Vtbl, DrawInstanced),
            list::draw as *const c_void,
            &O_DRAW,
        ),
        (
            "CommandList::DrawIndexedInstanced",
            slot!(ID3D12GraphicsCommandList_Vtbl, DrawIndexedInstanced),
            list::draw_indexed as *const c_void,
            &O_DRAW_INDEXED,
        ),
        // Last: everything a dispatch reads is being recorded by now.
        (
            "CommandList::Dispatch",
            slot!(ID3D12GraphicsCommandList_Vtbl, Dispatch),
            list::dispatch as *const c_void,
            &O_DISPATCH,
        ),
    ];
    // SAFETY: live objects; their first word is their vtable.
    let (device, resource, queue, list) = unsafe {
        (
            vtable_of(objects._device.as_raw()),
            vtable_of(objects._buffer.as_raw()),
            vtable_of(objects._queue.as_raw()),
            vtable_of(objects._list.as_raw()),
        )
    };
    let device2 = objects
        ._device
        .cast::<ID3D12Device2>()
        // SAFETY: a live interface.
        .map(|two| unsafe { vtable_of(two.as_raw()) })
        .ok();
    for (name, index, replacement, original) in device_hooks.iter().chain(list_hooks.iter()) {
        let vtable = match *name {
            n if n.starts_with("Device2::") => match device2 {
                Some(vtable) => vtable,
                None => {
                    log::warn!(target: "render", "no ID3D12Device2: stream-created pipelines are not seen");
                    continue;
                }
            },
            n if n.starts_with("Device::") => device,
            n if n.starts_with("Resource::") => resource,
            n if n.starts_with("CommandQueue::") => queue,
            _ => list,
        };
        // SAFETY: `index` comes from the interface's own vtable layout and
        // the replacement has that slot's signature.
        unsafe { patch_vtable(vtable, *index, *replacement, original) }
            .map_err(|error| InstallError::Patch(name, error))?;
        log::debug!(target: "render", "hooked slot {index:>3} {name}");
    }
    Ok(())
}
