//! Object creation shared by the crate's own passes: shader compilation,
//! root signatures, textures, buffers, descriptor heaps, views.
//!
//! Shaders are compiled with `D3DCompile` at first use; `d3dcompiler_47.dll`
//! is loaded then, from System32, so the crate carries no load-time
//! dependency on it. Everything is created on the native device (a ReShade
//! proxy unwrapped) and created while this thread is marked as recording our
//! own work, so the pipeline hooks do not take our shaders for the engine's.

use std::ffi::c_void;
use std::sync::LazyLock;

use mhw_core::frame::Size;
use windows::core::{s, w, Interface, HRESULT, PCSTR, PCWSTR};
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

use super::alive;
use super::record::Guard;
use super::registry::STATES;
use super::reshade::native_interface;
use crate::states::State;

type D3DCompileFn = unsafe extern "system" fn(
    *const c_void,
    usize,
    PCSTR,
    *const c_void,
    *mut c_void,
    PCSTR,
    PCSTR,
    u32,
    u32,
    *mut *mut c_void,
    *mut *mut c_void,
) -> HRESULT;

/// `D3DCOMPILE_OPTIMIZATION_LEVEL3`.
const OPTIMIZATION_LEVEL3: u32 = 1 << 15;

/// `D3DCompile`'s address, resolved at first use.
static COMPILER: LazyLock<Result<usize, String>> = LazyLock::new(|| {
    // SAFETY: a System32 load of a system library, outside the loader lock
    // (first use is on a render thread).
    unsafe {
        let module = LoadLibraryExW(w!("d3dcompiler_47.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
            .map_err(|error| format!("d3dcompiler_47.dll: {error}"))?;
        GetProcAddress(module, s!("D3DCompile"))
            .map(|f| f as usize)
            .ok_or_else(|| "d3dcompiler_47.dll has no D3DCompile".to_string())
    }
});

fn compiler() -> Result<D3DCompileFn, String> {
    let address = COMPILER.clone()?;
    // SAFETY: the address of `D3DCompile`, whose signature is `D3DCompileFn`.
    Ok(unsafe { std::mem::transmute::<usize, D3DCompileFn>(address) })
}

/// Compile HLSL `source` (`entry` and `target` NUL-terminated, e.g.
/// `b"main\0"`, `b"cs_5_0\0"`) to DXBC; the error carries the compiler's
/// text.
pub(crate) fn compile(
    source: &str,
    name: &[u8],
    entry: &[u8],
    target: &[u8],
) -> Result<Vec<u8>, String> {
    for text in [name, entry, target] {
        if text.last() != Some(&0) {
            return Err("compile arguments must be NUL-terminated".into());
        }
    }
    let compile = compiler()?;
    let (mut code, mut errors): (*mut c_void, *mut c_void) =
        (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: a source buffer and NUL-terminated names that outlive the
    // call; the blobs returned are owned and released on drop.
    unsafe {
        let result = compile(
            source.as_ptr().cast(),
            source.len(),
            PCSTR(name.as_ptr()),
            std::ptr::null(),
            std::ptr::null_mut(),
            PCSTR(entry.as_ptr()),
            PCSTR(target.as_ptr()),
            OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            &mut errors,
        );
        let text = (!errors.is_null()).then(|| blob_bytes(&ID3DBlob::from_raw(errors)));
        if result.is_err() || code.is_null() {
            if !code.is_null() {
                drop(ID3DBlob::from_raw(code));
            }
            let text = text.map_or_else(String::new, |t| String::from_utf8_lossy(&t).into_owned());
            return Err(format!("{result:?} {}", text.trim()));
        }
        Ok(blob_bytes(&ID3DBlob::from_raw(code)))
    }
}

fn blob_bytes(blob: &ID3DBlob) -> Vec<u8> {
    // SAFETY: a live blob's buffer is readable for its size.
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
            .to_vec()
    }
}

/// `"what: error"`, for `map_err`.
pub(crate) fn failed(what: &'static str) -> impl Fn(windows::core::Error) -> String {
    move |error| format!("{what}: {error}")
}

/// The native device `resource` lives on.
///
/// # Safety
/// `resource` must be live.
pub(crate) unsafe fn device_of(resource: &ID3D12Resource) -> Result<ID3D12Device, String> {
    let mut device: Option<ID3D12Device> = None;
    // SAFETY: a live resource.
    unsafe { resource.GetDevice(&mut device) }.map_err(failed("GetDevice"))?;
    let device = device.ok_or("GetDevice returned nothing")?;
    // SAFETY: a live device.
    Ok(unsafe { native_interface(device) })
}

/// A resource from a raw pointer, borrowed.
///
/// # Safety
/// `raw` must be null or a live `ID3D12Resource`.
pub(crate) unsafe fn resource(raw: &*mut c_void) -> Option<&ID3D12Resource> {
    // SAFETY: caller's contract.
    unsafe { ID3D12Resource::from_raw_borrowed(raw) }
}

/// One descriptor range.
pub(crate) fn range(
    kind: D3D12_DESCRIPTOR_RANGE_TYPE,
    register: u32,
    space: u32,
    offset: u32,
) -> D3D12_DESCRIPTOR_RANGE {
    D3D12_DESCRIPTOR_RANGE {
        RangeType: kind,
        NumDescriptors: 1,
        BaseShaderRegister: register,
        RegisterSpace: space,
        OffsetInDescriptorsFromTableStart: offset,
    }
}

/// A root signature of a table over `ranges` (root 0) and, if `constants`
/// is `Some((register, space, dwords))`, root constants (root 1).
pub(crate) fn root_signature(
    device: &ID3D12Device,
    ranges: &[D3D12_DESCRIPTOR_RANGE],
    constants: Option<(u32, u32, u32)>,
    samplers: &[D3D12_STATIC_SAMPLER_DESC],
    visibility: D3D12_SHADER_VISIBILITY,
) -> Result<ID3D12RootSignature, String> {
    let mut parameters = vec![D3D12_ROOT_PARAMETER {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
        Anonymous: D3D12_ROOT_PARAMETER_0 {
            DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                NumDescriptorRanges: ranges.len() as u32,
                pDescriptorRanges: ranges.as_ptr(),
            },
        },
        ShaderVisibility: visibility,
    }];
    if let Some((register, space, dwords)) = constants {
        parameters.push(D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Constants: D3D12_ROOT_CONSTANTS {
                    ShaderRegister: register,
                    RegisterSpace: space,
                    Num32BitValues: dwords,
                },
            },
            ShaderVisibility: visibility,
        });
    }
    let description = D3D12_ROOT_SIGNATURE_DESC {
        NumParameters: parameters.len() as u32,
        pParameters: parameters.as_ptr(),
        NumStaticSamplers: samplers.len() as u32,
        pStaticSamplers: if samplers.is_empty() {
            std::ptr::null()
        } else {
            samplers.as_ptr()
        },
        Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
    };
    // SAFETY: a description whose arrays outlive the calls; a live device.
    unsafe {
        let mut blob: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        if let Err(error) = D3D12SerializeRootSignature(
            &description,
            D3D_ROOT_SIGNATURE_VERSION_1,
            &mut blob,
            Some(&mut errors),
        ) {
            let text = errors.map(|e| String::from_utf8_lossy(&blob_bytes(&e)).into_owned());
            return Err(format!(
                "root signature: {error} {}",
                text.unwrap_or_default()
            ));
        }
        let blob = blob.ok_or("root signature: no blob")?;
        let bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
        let _guard = Guard::enter();
        device
            .CreateRootSignature(0, bytes)
            .map_err(failed("root signature"))
    }
}

/// A linear-clamp static sampler at `(register, space)`.
pub(crate) fn linear_clamp(register: u32, space: u32) -> D3D12_STATIC_SAMPLER_DESC {
    D3D12_STATIC_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MipLODBias: 0.0,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
        BorderColor: D3D12_STATIC_BORDER_COLOR_TRANSPARENT_BLACK,
        MinLOD: 0.0,
        MaxLOD: f32::MAX,
        ShaderRegister: register,
        RegisterSpace: space,
        ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
    }
}

/// A compute pipeline of `code` with `signature`.
pub(crate) fn compute_pipeline(
    device: &ID3D12Device,
    signature: &ID3D12RootSignature,
    code: &[u8],
) -> Result<ID3D12PipelineState, String> {
    let _guard = Guard::enter();
    // SAFETY: a description that outlives the call; the root signature is
    // borrowed for it without a reference count change.
    unsafe {
        device
            .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(signature),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.as_ptr().cast(),
                    BytecodeLength: code.len(),
                },
                NodeMask: 0,
                CachedPSO: D3D12_CACHED_PIPELINE_STATE::default(),
                Flags: D3D12_PIPELINE_STATE_FLAG_NONE,
            })
            .map_err(failed("compute pipeline"))
    }
}

/// A 2D texture of this crate, tracked from creation (state, liveness) so
/// its transitions can be planned like the engine's.
pub(crate) fn texture(
    device: &ID3D12Device,
    size: Size,
    format: u32,
    flags: D3D12_RESOURCE_FLAGS,
    state: State,
    name: PCWSTR,
) -> Result<ID3D12Resource, String> {
    let mut texture: Option<ID3D12Resource> = None;
    let _guard = Guard::enter();
    // SAFETY: descriptions that outlive the call on a live device.
    unsafe {
        device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                    Alignment: 0,
                    Width: u64::from(size.w),
                    Height: size.h,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: DXGI_FORMAT(format as i32),
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
                    Flags: flags,
                },
                D3D12_RESOURCE_STATES(state as i32),
                None,
                &mut texture,
            )
            .map_err(failed("texture"))?;
        let texture = texture.ok_or("texture: none returned")?;
        let _ = texture.SetName(name);
        track(&texture, state);
        Ok(texture)
    }
}

/// A buffer of `bytes` on `heap`.
#[cfg(feature = "dev")]
pub(crate) fn buffer(
    device: &ID3D12Device,
    bytes: u64,
    heap: D3D12_HEAP_TYPE,
    flags: D3D12_RESOURCE_FLAGS,
    state: State,
) -> Result<ID3D12Resource, String> {
    let mut buffer: Option<ID3D12Resource> = None;
    let _guard = Guard::enter();
    // SAFETY: descriptions that outlive the call on a live device.
    unsafe {
        device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: heap,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Alignment: 0,
                    Width: bytes.max(1),
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_UNKNOWN,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    Flags: flags,
                },
                D3D12_RESOURCE_STATES(state as i32),
                None,
                &mut buffer,
            )
            .map_err(failed("buffer"))?;
    }
    let buffer = buffer.ok_or("buffer: none returned")?;
    track(&buffer, state);
    Ok(buffer)
}

/// Record `resource`'s state and watch its liveness (the creation hook does
/// both only for frame-sized textures, and not for our own).
fn track(resource: &ID3D12Resource, state: State) {
    let raw = resource.as_raw();
    STATES.lock().created(raw as usize, state);
    if alive::serial_of(raw as usize).is_none() {
        // SAFETY: a live resource of ours.
        unsafe { alive::watch(raw) };
    }
}

/// A descriptor heap and its increment.
pub(crate) fn heap(
    device: &ID3D12Device,
    kind: D3D12_DESCRIPTOR_HEAP_TYPE,
    count: usize,
    shader_visible: bool,
) -> Result<(ID3D12DescriptorHeap, usize), String> {
    // SAFETY: a description that outlives the call on a live device.
    unsafe {
        let heap: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: kind,
                NumDescriptors: count as u32,
                Flags: if shader_visible {
                    D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE
                } else {
                    D3D12_DESCRIPTOR_HEAP_FLAG_NONE
                },
                NodeMask: 0,
            })
            .map_err(failed("descriptor heap"))?;
        let stride = device.GetDescriptorHandleIncrementSize(kind) as usize;
        Ok((heap, stride))
    }
}

/// A 2D texture SRV of `format`.
pub(crate) fn srv(format: u32) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    let mut view = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT(format as i32),
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        ..Default::default()
    };
    view.Anonymous.Texture2D = D3D12_TEX2D_SRV {
        MostDetailedMip: 0,
        MipLevels: 1,
        PlaneSlice: 0,
        ResourceMinLODClamp: 0.0,
    };
    view
}

/// A 2D texture UAV of `format`.
pub(crate) fn uav(format: u32) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut view = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT(format as i32),
        ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
        ..Default::default()
    };
    view.Anonymous.Texture2D = D3D12_TEX2D_UAV {
        MipSlice: 0,
        PlaneSlice: 0,
    };
    view
}

/// A whole-buffer raw (byte address) UAV of `bytes`.
#[cfg(feature = "dev")]
pub(crate) fn raw_uav(bytes: u64) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut view = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
        ..Default::default()
    };
    view.Anonymous.Buffer = D3D12_BUFFER_UAV {
        FirstElement: 0,
        NumElements: (bytes / 4) as u32,
        StructureByteStride: 0,
        CounterOffsetInBytes: 0,
        Flags: D3D12_BUFFER_UAV_FLAG_RAW,
    };
    view
}

/// Explicit transitions from caller-stated states: `(resource, from, to)`,
/// recorded forward and later back. Nothing is recorded for a move to the
/// state a resource is already in.
#[derive(Debug, Default)]
pub(crate) struct Explicit {
    moves: Vec<crate::states::Move>,
}

impl Explicit {
    /// Plan moving `resource` from `from` to `to`.
    pub(crate) fn add(&mut self, resource: usize, from: State, to: State) {
        if from != to {
            self.moves.push(crate::states::Move { resource, from, to });
        }
    }

    /// Record the forward barriers.
    ///
    /// # Safety
    /// `list` open; every resource alive.
    pub(crate) unsafe fn forward(&self, list: &ID3D12GraphicsCommandList) {
        let barriers: Vec<D3D12_RESOURCE_BARRIER> = self
            .moves
            .iter()
            .copied()
            .map(super::record::barrier)
            .collect();
        if !barriers.is_empty() {
            // SAFETY: caller's contract.
            unsafe { list.ResourceBarrier(&barriers) };
        }
    }

    /// Record the barriers back.
    ///
    /// # Safety
    /// As [`Explicit::forward`], after it.
    pub(crate) unsafe fn back(&self, list: &ID3D12GraphicsCommandList) {
        let barriers: Vec<D3D12_RESOURCE_BARRIER> = self
            .moves
            .iter()
            .rev()
            .map(|m| super::record::barrier(m.reversed()))
            .collect();
        if !barriers.is_empty() {
            // SAFETY: caller's contract.
            unsafe { list.ResourceBarrier(&barriers) };
        }
    }
}

/// A UAV barrier on `resource` (all UAV writes before it complete).
#[cfg(feature = "dev")]
pub(crate) fn uav_barrier(resource: &ID3D12Resource) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                // SAFETY: one nullable interface pointer, never released
                // through the barrier.
                pResource: unsafe { std::mem::transmute_copy(resource) },
            }),
        },
    }
}
