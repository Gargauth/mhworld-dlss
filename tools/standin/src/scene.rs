//! The renderer start-up, in the engine's order and through the engine's
//! calls.

use std::ffi::c_void;

use windows::core::{s, w, Interface, GUID, HRESULT, PCSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, WS_OVERLAPPEDWINDOW};

use crate::{context, Result, Say};

/// Reflects like `CSTemporalUpsample`: the three names the mod recognises
/// the pass by, spelling mistake included.
const SHADER: &str = "
Texture2D<float4>   tInput   : register(t0);
Texture2D<float4>   tHistory : register(t1);
RWTexture2D<float4> rwOuput  : register(u0);
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    rwOuput[id.xy] = tInput[id.xy] * 0.5 + tHistory[id.xy] * 0.5;
}
";

/// What the start-up made.
pub struct Scene {
    pub device: ID3D12Device,
    pub queue: ID3D12CommandQueue,
    pub signature: ID3D12RootSignature,
    pub pipeline: Option<ID3D12PipelineState>,
    pub swapchain: Option<IDXGISwapChain1>,
}

type CreateFactory = unsafe extern "system" fn(u32, *const GUID, *mut *mut c_void) -> HRESULT;
type CreateDevice =
    unsafe extern "system" fn(*mut c_void, i32, *const GUID, *mut *mut c_void) -> HRESULT;

/// Steps 1 to 5: libraries, factory, device, queue, pipeline, swapchain.
pub fn start(say: &mut Say<'_>) -> Result<Scene> {
    // SAFETY: system libraries by name; the entry points are transmuted to
    // their documented signatures and called with valid out-pointers.
    let (factory, device) = unsafe {
        let dxgi = context(
            LoadLibraryExW(w!("dxgi.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32),
            "load dxgi.dll",
        )?;
        let d3d12 = context(
            LoadLibraryExW(w!("d3d12.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32),
            "load d3d12.dll",
        )?;
        let create_factory = GetProcAddress(dxgi, s!("CreateDXGIFactory2"))
            .ok_or("dxgi.dll has no CreateDXGIFactory2")?;
        let create_factory: CreateFactory = std::mem::transmute(create_factory);
        let mut raw = std::ptr::null_mut();
        context(
            create_factory(0, &IDXGIFactory2::IID, &mut raw).ok(),
            "CreateDXGIFactory2",
        )?;
        let factory = IDXGIFactory2::from_raw(raw);
        say(format!("factory {:p}", factory.as_raw()));

        let create_device = GetProcAddress(d3d12, s!("D3D12CreateDevice"))
            .ok_or("d3d12.dll has no D3D12CreateDevice")?;
        let create_device: CreateDevice = std::mem::transmute(create_device);
        let mut raw = std::ptr::null_mut();
        context(
            create_device(
                std::ptr::null_mut(),
                D3D_FEATURE_LEVEL_11_0.0,
                &ID3D12Device::IID,
                &mut raw,
            )
            .ok(),
            "D3D12CreateDevice",
        )?;
        (factory, ID3D12Device::from_raw(raw))
    };
    say(format!("device {:p}", device.as_raw()));

    // SAFETY: a live device and a valid description.
    let queue: ID3D12CommandQueue = context(
        unsafe {
            device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })
        },
        "CreateCommandQueue",
    )?;
    say(format!("queue {:p}", queue.as_raw()));

    let signature = root_signature(&device)?;
    let pipeline = match pipeline(&device, &signature) {
        Ok(pipeline) => {
            say(format!("compute pipeline {:p}", pipeline.as_raw()));
            Some(pipeline)
        }
        Err(error) => {
            say(format!("compute pipeline failed: {error}"));
            None
        }
    };
    let swapchain = match swapchain(&factory, &queue) {
        Ok(chain) => {
            say(format!("swapchain {:p}", chain.as_raw()));
            Some(chain)
        }
        Err(error) => {
            say(format!("swapchain failed: {error}"));
            None
        }
    };
    Ok(Scene {
        device,
        queue,
        signature,
        pipeline,
        swapchain,
    })
}

/// One SRV table of two textures and one UAV, as the temporal pass binds.
fn root_signature(device: &ID3D12Device) -> Result<ID3D12RootSignature> {
    let ranges = [
        D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
            NumDescriptors: 2,
            BaseShaderRegister: 0,
            RegisterSpace: 0,
            OffsetInDescriptorsFromTableStart: 0,
        },
        D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
            NumDescriptors: 1,
            BaseShaderRegister: 0,
            RegisterSpace: 0,
            OffsetInDescriptorsFromTableStart: 2,
        },
    ];
    let parameters = [D3D12_ROOT_PARAMETER {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
        Anonymous: D3D12_ROOT_PARAMETER_0 {
            DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                NumDescriptorRanges: ranges.len() as u32,
                pDescriptorRanges: ranges.as_ptr(),
            },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
    }];
    let mut blob: Option<ID3DBlob> = None;
    // SAFETY: the description points at the arrays above, alive for the call.
    context(
        unsafe {
            D3D12SerializeRootSignature(
                &D3D12_ROOT_SIGNATURE_DESC {
                    NumParameters: parameters.len() as u32,
                    pParameters: parameters.as_ptr(),
                    ..Default::default()
                },
                D3D_ROOT_SIGNATURE_VERSION_1_0,
                &mut blob,
                None,
            )
        },
        "D3D12SerializeRootSignature",
    )?;
    let blob = blob.ok_or("no serialised root signature")?;
    // SAFETY: the blob's pointer and size describe its own live buffer.
    context(
        unsafe {
            device.CreateRootSignature(
                0,
                std::slice::from_raw_parts(
                    blob.GetBufferPointer() as *const u8,
                    blob.GetBufferSize(),
                ),
            )
        },
        "CreateRootSignature",
    )
}

/// The temporal-pass look-alike, made the way the engine makes its own:
/// `ID3D12Device::CreateComputePipelineState` on whatever device it holds.
/// `STANDIN_PLAIN_SHADER=1` renames the resources, to see what the mod
/// says when no temporal pass exists.
fn pipeline(device: &ID3D12Device, signature: &ID3D12RootSignature) -> Result<ID3D12PipelineState> {
    let source = if std::env::var_os("STANDIN_PLAIN_SHADER").is_some() {
        SHADER
            .replace("tInput", "first")
            .replace("tHistory", "second")
            .replace("rwOuput", "third")
    } else {
        SHADER.to_owned()
    };
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: `source` is alive for the call; out-pointers are valid.
    let compiled = unsafe {
        D3DCompile(
            source.as_ptr() as *const c_void,
            source.len(),
            PCSTR::null(),
            None,
            None,
            s!("main"),
            s!("cs_5_0"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    context(compiled, "D3DCompile")?;
    let code = code.ok_or("no compiled shader")?;
    // SAFETY: the root signature is borrowed for the call only
    // (`transmute_copy` makes a non-owning copy of the interface pointer);
    // the bytecode blob is alive for the call.
    context(
        unsafe {
            device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(signature),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.GetBufferPointer(),
                    BytecodeLength: code.GetBufferSize(),
                },
                ..Default::default()
            })
        },
        "CreateComputePipelineState",
    )
}

/// A window and a swapchain with the game's flags (waitable object, mode
/// switch, tearing: 0x842).
fn swapchain(factory: &IDXGIFactory2, queue: &ID3D12CommandQueue) -> Result<IDXGISwapChain1> {
    // SAFETY: a system window class and static strings.
    let window: HWND = context(
        unsafe {
            CreateWindowExW(
                Default::default(),
                w!("STATIC"),
                w!("mhw-dlss stand-in"),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                640,
                360,
                None,
                None,
                None,
                None,
            )
        },
        "CreateWindowExW",
    )?;
    let description = DXGI_SWAP_CHAIN_DESC1 {
        Width: 640,
        Height: 360,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 3,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        Flags: (DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0
            | DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH.0
            | DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0) as u32,
        ..Default::default()
    };
    // SAFETY: live factory, queue and window; a valid description.
    context(
        unsafe { factory.CreateSwapChainForHwnd(queue, window, &description, None, None) },
        "CreateSwapChainForHwnd",
    )
}

/// A shader-visible heap of three null descriptors (two SRVs, one UAV),
/// which D3D12 defines: reads give zero, writes go nowhere.
pub fn null_table(device: &ID3D12Device) -> Result<ID3D12DescriptorHeap> {
    // SAFETY: a live device; valid descriptions; handles inside the heap.
    unsafe {
        let heap: ID3D12DescriptorHeap = context(
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 3,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            }),
            "CreateDescriptorHeap",
        )?;
        let step = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
            as usize;
        let base = heap.GetCPUDescriptorHandleForHeapStart();
        let at = |index: usize| D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: base.ptr + index * step,
        };
        let texture = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_SRV {
                    MipLevels: 1,
                    ..Default::default()
                },
            },
        };
        device.CreateShaderResourceView(None, Some(&texture), at(0));
        device.CreateShaderResourceView(None, Some(&texture), at(1));
        device.CreateUnorderedAccessView(
            None,
            None,
            Some(&D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
                ..Default::default()
            }),
            at(2),
        );
        Ok(heap)
    }
}

/// A fresh direct command list with the temporal pass bound as the engine
/// binds it: its heap set, root signature, table and pipeline.
pub fn bound_list(
    scene: &Scene,
    pipeline: &ID3D12PipelineState,
    heap: &ID3D12DescriptorHeap,
) -> Result<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)> {
    // SAFETY: live objects; the list is recorded on this thread only.
    unsafe {
        let allocator: ID3D12CommandAllocator = context(
            scene
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT),
            "CreateCommandAllocator",
        )?;
        let list: ID3D12GraphicsCommandList = context(
            scene
                .device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None),
            "CreateCommandList",
        )?;
        list.SetDescriptorHeaps(&[Some(heap.clone())]);
        list.SetComputeRootSignature(&scene.signature);
        list.SetComputeRootDescriptorTable(0, heap.GetGPUDescriptorHandleForHeapStart());
        list.SetPipelineState(pipeline);
        Ok((allocator, list))
    }
}

/// Close `list` and execute it on the scene's queue.
pub fn submit(scene: &Scene, list: &ID3D12GraphicsCommandList) -> Result<()> {
    // SAFETY: a recording list of this device, closed once and submitted.
    unsafe {
        context(list.Close(), "Close")?;
        let list: ID3D12CommandList = context(list.cast(), "cast to ID3D12CommandList")?;
        scene.queue.ExecuteCommandLists(&[Some(list)]);
    }
    Ok(())
}

/// Step 5b: one dispatch of the pipeline, recorded and submitted the way
/// the engine records its own. Each call is a method the mod patches on the
/// native object, made on whatever the layers above handed out, so a layer
/// that re-routes one of them shows up as a counter that stays at zero.
pub fn dispatch_once(scene: &Scene, pipeline: &ID3D12PipelineState) -> Result<()> {
    let heap = null_table(&scene.device)?;
    let (allocator, list) = bound_list(scene, pipeline, &heap)?;
    // SAFETY: a recording list with a pipeline and table bound.
    unsafe { list.Dispatch(1, 1, 1) };
    submit(scene, &list)?;
    // Alive until the process exits: the queue is never waited on here.
    std::mem::forget((heap, allocator, list));
    Ok(())
}

/// Past six hundred presents: the mod logs its counters periodically, and
/// that is where the first account comes after.
pub fn present(scene: &Scene, count: u32) {
    if let Some(chain) = &scene.swapchain {
        for _ in 0..count {
            // SAFETY: a live swapchain.
            let _ = unsafe { chain.Present(0, DXGI_PRESENT(0)) };
        }
    }
}
