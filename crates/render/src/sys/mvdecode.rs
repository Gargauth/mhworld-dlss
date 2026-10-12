//! The motion vector decode pass: `tGBuffer4` (packed half2 in `R32_UINT`)
//! to `R16G16_FLOAT` render pixels, recorded on the engine's list at the
//! temporal dispatch.
//!
//! The shader ([`crate::mvpack::hlsl`]) is compiled with `D3DCompile` at
//! first use; `d3dcompiler_47.dll` is loaded then, from System32, so the
//! crate carries no load-time dependency on it. The decoder's objects live
//! on the device that owns the source texture (the engine's, a ReShade proxy
//! unwrapped).
//!
//! The decoded texture rests in `NON_PIXEL_SHADER_RESOURCE` between decodes:
//! each decode moves it to `UNORDERED_ACCESS`, dispatches, and moves it
//! back, all on the same list, so whoever reads it later on the engine's
//! queue finds it readable. Descriptors rotate through a ring so a frame
//! never rewrites one the GPU may still read.

use std::ffi::c_void;
use std::sync::LazyLock;

use mhw_core::frame::Size;
use parking_lot::Mutex;
use windows::core::{w, Interface};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32_UINT;

use super::alive;
use super::gpu;
use super::record::Transitions;
use super::registry;
use super::reshade::native_interface;
use crate::mvpack::{self, DecodeConstants, DecodeParams};
use crate::{names, states};

/// Descriptor slots cycled through (one SRV + one UAV each).
const RING: usize = 16;
const SLOT_DESCRIPTORS: usize = 2;

/// Why a decode did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The shader could not be compiled (text from the compiler).
    Compile(String),
    /// A D3D12 object could not be created (text from the call).
    Build(String),
    /// The source or the list is not what it should be.
    Invalid(&'static str),
    /// The decoded texture's state is unknown on this list.
    UnknownState,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Compile(text) => write!(f, "decode shader did not compile: {text}"),
            DecodeError::Build(text) => write!(f, "decoder objects not created: {text}"),
            DecodeError::Invalid(what) => write!(f, "invalid decode input: {what}"),
            DecodeError::UnknownState => f.write_str("decoded texture state unknown on this list"),
        }
    }
}

/// The decoded texture of one decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Decoded {
    pub resource: usize,
    pub serial: u64,
}

struct Decoder {
    device: usize,
    size: Size,
    texture: ID3D12Resource,
    heap: ID3D12DescriptorHeap,
    signature: ID3D12RootSignature,
    pipeline: ID3D12PipelineState,
    stride: usize,
    next: usize,
}

// SAFETY: D3D12 objects are free-threaded; the decoder is only used under
// its mutex.
unsafe impl Send for Decoder {}

static DECODER: Mutex<Option<Decoder>> = Mutex::new(None);
/// Replaced decoders, with the pass clock at replacement: GPU work recorded
/// with their heap and texture may still be in flight, so they are released
/// a few frames later.
static RETIRED: Mutex<Vec<(u64, Decoder)>> = Mutex::new(Vec::new());
/// Frames a replaced decoder is kept.
const RETIRE_FRAMES: u64 = 4;
/// The compiled shader: compiled on first use, once.
static SHADER: LazyLock<Result<Vec<u8>, String>> =
    LazyLock::new(|| gpu::compile(&mvpack::hlsl(), b"mhw_mv_decode\0", b"main\0", b"cs_5_0\0"));
/// The `(device, size)` a build last failed for, so it is not retried
/// every frame.
static FAILED_FOR: Mutex<Option<(usize, Size)>> = Mutex::new(None);

fn build(device: &ID3D12Device, size: Size) -> Result<Decoder, DecodeError> {
    let code = SHADER
        .as_ref()
        .map_err(|text| DecodeError::Compile(text.clone()))?;
    let ranges = [
        gpu::range(D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 0, 0, 0),
        gpu::range(D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 0, 0, 1),
    ];
    let signature = gpu::root_signature(
        device,
        &ranges,
        Some((0, 0, DecodeConstants::DWORDS)),
        &[],
        D3D12_SHADER_VISIBILITY_ALL,
    )
    .map_err(DecodeError::Build)?;
    let pipeline = gpu::compute_pipeline(device, &signature, code).map_err(DecodeError::Build)?;
    // At rest it is readable; see the module comment. Tracked whatever the
    // render size (the creation hook only tracks frame-sized textures).
    let texture = gpu::texture(
        device,
        size,
        names::R16G16_FLOAT,
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        states::NON_PIXEL_SHADER_RESOURCE,
        w!("mhw-dlss decoded motion vectors"),
    )
    .map_err(DecodeError::Build)?;
    let (heap, stride) = gpu::heap(
        device,
        D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
        RING * SLOT_DESCRIPTORS,
        true,
    )
    .map_err(DecodeError::Build)?;
    // The target's view is fixed per slot; only the source SRV rotates.
    // SAFETY: a live heap of `RING * SLOT_DESCRIPTORS` descriptors and a
    // live texture.
    unsafe {
        let base = heap.GetCPUDescriptorHandleForHeapStart().ptr;
        let uav = gpu::uav(names::R16G16_FLOAT);
        for slot in 0..RING {
            device.CreateUnorderedAccessView(
                &texture,
                None,
                Some(&uav),
                D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: base + (slot * SLOT_DESCRIPTORS + 1) * stride,
                },
            );
        }
    }
    log::info!(target: "inputs", "motion vector decoder ready at {}x{}", size.w, size.h);
    Ok(Decoder {
        device: device.as_raw() as usize,
        size,
        texture,
        heap,
        signature,
        pipeline,
        stride,
        next: 0,
    })
}

/// Decode `source` (the engine's `tGBuffer4`, readable on `list`) at
/// `render` into the decoder's texture, on `list`. The caller holds the
/// recording guard and restores the engine's state afterwards.
///
/// # Safety
/// `list` must be the open native list being recorded on this thread and
/// `source` a live resource.
pub(crate) unsafe fn decode(
    list: &ID3D12GraphicsCommandList,
    source: usize,
    render: Size,
    params: &DecodeParams,
) -> Result<Decoded, DecodeError> {
    if render.is_empty() {
        return Err(DecodeError::Invalid("empty render size"));
    }
    let raw = source as *mut c_void;
    // SAFETY: caller guarantees a live resource.
    let source = unsafe { ID3D12Resource::from_raw_borrowed(&raw) }
        .ok_or(DecodeError::Invalid("no source"))?;
    // ReShade answers GetDevice with its proxy; the decoder's objects are
    // ours and belong on the native device.
    let mut device: Option<ID3D12Device> = None;
    // SAFETY: a live resource.
    unsafe { source.GetDevice(&mut device) }
        .map_err(|e| DecodeError::Build(format!("GetDevice: {e}")))?;
    // SAFETY: a live device.
    let device = unsafe { native_interface(device.ok_or(DecodeError::Invalid("no device"))?) };
    let key = (device.as_raw() as usize, render);

    let mut guard = DECODER.lock();
    if guard.as_ref().is_none_or(|d| (d.device, d.size) != key) {
        let now = registry::frame();
        let mut retired = RETIRED.lock();
        retired.retain(|(at, _)| now.saturating_sub(*at) <= RETIRE_FRAMES);
        if let Some(old) = guard.take() {
            retired.push((now, old));
        }
        drop(retired);
        if *FAILED_FOR.lock() == Some(key) {
            return Err(DecodeError::Invalid("decoder build failed for this size"));
        }
        match build(&device, render) {
            Ok(decoder) => *guard = Some(decoder),
            Err(error) => {
                *FAILED_FOR.lock() = Some(key);
                log::warn!(target: "inputs", "{error}");
                return Err(error);
            }
        }
    }
    let decoder = guard.as_mut().ok_or(DecodeError::Invalid("no decoder"))?;
    let texture = decoder.texture.as_raw() as usize;
    let list_raw = list.as_raw();
    // SAFETY: a live list.
    let to_write = unsafe { Transitions::plan(list_raw, &[(texture, states::UNORDERED_ACCESS)]) }
        .ok_or(DecodeError::UnknownState)?;

    let slot = decoder.next;
    decoder.next = (decoder.next + 1) % RING;
    let mut srv = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32_UINT,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        ..Default::default()
    };
    srv.Anonymous.Texture2D = D3D12_TEX2D_SRV {
        MostDetailedMip: 0,
        MipLevels: 1,
        PlaneSlice: 0,
        ResourceMinLODClamp: 0.0,
    };
    let constants = DecodeConstants::new(render, params).to_dwords();
    // SAFETY: our own heap, signature, pipeline and texture on the open
    // engine list; the source is live and in a shader-readable state (the
    // temporal pass is about to read it).
    unsafe {
        let cpu = decoder.heap.GetCPUDescriptorHandleForHeapStart().ptr
            + slot * SLOT_DESCRIPTORS * decoder.stride;
        device.CreateShaderResourceView(
            source,
            Some(&srv),
            D3D12_CPU_DESCRIPTOR_HANDLE { ptr: cpu },
        );
        let gpu = decoder.heap.GetGPUDescriptorHandleForHeapStart().ptr
            + (slot * SLOT_DESCRIPTORS * decoder.stride) as u64;
        to_write.forward(list);
        list.SetDescriptorHeaps(&[Some(decoder.heap.clone())]);
        list.SetComputeRootSignature(&decoder.signature);
        list.SetPipelineState(&decoder.pipeline);
        list.SetComputeRootDescriptorTable(0, D3D12_GPU_DESCRIPTOR_HANDLE { ptr: gpu });
        list.SetComputeRoot32BitConstants(1, DecodeConstants::DWORDS, constants.as_ptr().cast(), 0);
        list.Dispatch(
            render.w.div_ceil(mvpack::GROUP),
            render.h.div_ceil(mvpack::GROUP),
            1,
        );
        let readable = Transitions::plan(list_raw, &[(texture, states::NON_PIXEL_SHADER_RESOURCE)])
            .ok_or(DecodeError::UnknownState)?;
        readable.forward(list);
    }
    let serial =
        alive::serial_of(texture).ok_or(DecodeError::Invalid("decoded texture unwatched"))?;
    Ok(Decoded {
        resource: texture,
        serial,
    })
}
