//! The HUD-less conversion: the copied scene target into the swapchain's
//! format, on a command list of our own executed behind the engine's.
//!
//! A [`Converter`] is made per (device, size, scene format, transform shape)
//! and holds the copy texture (`hold`, resting in `COPY_DEST`), the pipeline
//! and three [`Slot`]s, each an output texture (resting in
//! [`OUTPUT_STATE`]) with its own command list. A slot is reused only once
//! its last conversion has completed on the GPU, so the image handed to
//! frame generation stays unmodified until it is presented.

use std::sync::Arc;

use mhw_core::frame::Size;
use parking_lot::Mutex;
use windows::core::{w, Interface};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

use super::gpu::{self, failed, Explicit};
use super::pso::pass_bytecode;
use super::record::Guard;
use super::submit::{Fence, OwnList};
use crate::dxbc;
use crate::hudless::encode::{plan_engine, plan_own, OutputTransform, RootPlan, OWN_HLSL};
use crate::recognise::PassKind;
use crate::signature::{fullscreen_vs, input_signature};
use crate::states;

/// Output textures (and lists) cycled through.
pub(crate) const SLOTS: usize = 3;
/// `PIXEL_SHADER_RESOURCE | NON_PIXEL_SHADER_RESOURCE`: the output's state
/// between conversions, as frame generation reads it.
pub(crate) const OUTPUT_STATE: states::State =
    states::PIXEL_SHADER_RESOURCE | states::NON_PIXEL_SHADER_RESOURCE;

/// Which pixel shader encodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoder {
    /// The engine's own `System_CopyToBackBuffer*` shader (FNV-1a hash).
    Engine(u64),
    /// This crate's gamma/PQ shader.
    Own,
}

/// What a converter was made for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Key {
    pub device: usize,
    pub size: Size,
    pub scene_format: u32,
    pub out_format: u32,
    pub pq: bool,
}

/// Objects every slot's recording uses.
pub(crate) struct Shared {
    pub hold: ID3D12Resource,
    srv_heap: ID3D12DescriptorHeap,
    rtv_heap: ID3D12DescriptorHeap,
    rtv_stride: usize,
    signature: ID3D12RootSignature,
    pipeline: ID3D12PipelineState,
    plan: RootPlan,
}

/// One output and the list that writes it.
pub(crate) struct Slot {
    pub output: ID3D12Resource,
    pub own: Mutex<OwnList>,
}

// SAFETY: D3D12 objects are free-threaded; recording state is in the mutex.
unsafe impl Send for Shared {}
// SAFETY: as above; nothing in `Shared` is mutated after creation.
unsafe impl Sync for Shared {}
// SAFETY: as above.
unsafe impl Send for Slot {}
// SAFETY: as above; the list is behind its mutex.
unsafe impl Sync for Slot {}

/// A converter.
pub(crate) struct Converter {
    pub key: Key,
    pub encoder: Encoder,
    pub shared: Arc<Shared>,
    pub slots: [Arc<Slot>; SLOTS],
    next: usize,
}

impl std::fmt::Debug for Converter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Converter")
            .field("key", &self.key)
            .field("encoder", &self.encoder)
            .finish()
    }
}

/// A pixel shader, the vertex shader that feeds it, and its root plan.
struct Shaders {
    vs: Vec<u8>,
    ps: Vec<u8>,
    plan: RootPlan,
    encoder: Encoder,
}

fn with_vertex_shader(ps: Vec<u8>, plan: RootPlan, encoder: Encoder) -> Result<Shaders, String> {
    let inputs = input_signature(&ps).map_err(|e| e.to_string())?;
    let source = fullscreen_vs(&inputs).map_err(|e| e.to_string())?;
    let vs = gpu::compile(&source, b"mhw_hudless_vs\0", b"main\0", b"vs_5_0\0")?;
    Ok(Shaders {
        vs,
        ps,
        plan,
        encoder,
    })
}

fn engine_shaders(pq: bool) -> Result<Shaders, String> {
    let kind = if pq {
        PassKind::CopyToBackBufferPq
    } else {
        PassKind::CopyToBackBuffer
    };
    let code = pass_bytecode(kind).ok_or_else(|| format!("{} not seen", kind.name()))?;
    let reflection = dxbc::reflect(&code).map_err(|e| e.to_string())?;
    let plan = plan_engine(&reflection)?;
    with_vertex_shader(code.to_vec(), plan, Encoder::Engine(dxbc::fnv1a(&code)))
}

fn own_shaders() -> Result<Shaders, String> {
    let ps = gpu::compile(OWN_HLSL, b"mhw_hudless_ps\0", b"main\0", b"ps_5_0\0")?;
    with_vertex_shader(ps, plan_own(), Encoder::Own)
}

fn root_signature(device: &ID3D12Device, plan: &RootPlan) -> Result<ID3D12RootSignature, String> {
    let ranges = [gpu::range(
        D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
        plan.texture.0,
        plan.texture.1,
        0,
    )];
    let samplers: Vec<D3D12_STATIC_SAMPLER_DESC> = plan
        .samplers
        .iter()
        .map(|&(register, space)| gpu::linear_clamp(register, space))
        .collect();
    let constants = plan
        .constants
        .as_ref()
        .map(|c| (c.register.0, c.register.1, c.dwords));
    gpu::root_signature(
        device,
        &ranges,
        constants,
        &samplers,
        D3D12_SHADER_VISIBILITY_PIXEL,
    )
}

fn pipeline(
    device: &ID3D12Device,
    signature: &ID3D12RootSignature,
    shaders: &Shaders,
    out_format: u32,
) -> Result<ID3D12PipelineState, String> {
    let mut formats = [DXGI_FORMAT(0); 8];
    formats[0] = DXGI_FORMAT(out_format as i32);
    let mut blend = D3D12_BLEND_DESC::default();
    blend.RenderTarget[0] = D3D12_RENDER_TARGET_BLEND_DESC {
        BlendEnable: false.into(),
        LogicOpEnable: false.into(),
        SrcBlend: D3D12_BLEND_ONE,
        DestBlend: D3D12_BLEND_ZERO,
        BlendOp: D3D12_BLEND_OP_ADD,
        SrcBlendAlpha: D3D12_BLEND_ONE,
        DestBlendAlpha: D3D12_BLEND_ZERO,
        BlendOpAlpha: D3D12_BLEND_OP_ADD,
        LogicOp: D3D12_LOGIC_OP_NOOP,
        RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };
    let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
        // SAFETY: borrowed for the call without a reference count change.
        pRootSignature: unsafe { std::mem::transmute_copy(signature) },
        VS: D3D12_SHADER_BYTECODE {
            pShaderBytecode: shaders.vs.as_ptr().cast(),
            BytecodeLength: shaders.vs.len(),
        },
        PS: D3D12_SHADER_BYTECODE {
            pShaderBytecode: shaders.ps.as_ptr().cast(),
            BytecodeLength: shaders.ps.len(),
        },
        BlendState: blend,
        SampleMask: u32::MAX,
        RasterizerState: D3D12_RASTERIZER_DESC {
            FillMode: D3D12_FILL_MODE_SOLID,
            CullMode: D3D12_CULL_MODE_NONE,
            DepthClipEnable: true.into(),
            ..Default::default()
        },
        DepthStencilState: D3D12_DEPTH_STENCIL_DESC::default(),
        PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
        NumRenderTargets: 1,
        RTVFormats: formats,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        ..Default::default()
    };
    let _guard = Guard::enter();
    // SAFETY: a description whose pointers outlive the call, on a live
    // device; our own creation (not recognised as the engine's).
    unsafe { device.CreateGraphicsPipelineState(&desc) }.map_err(failed("graphics pipeline"))
}

impl Converter {
    /// Build a converter: the engine's encode if it can drive it, else the
    /// own shader. Returns why the engine's was not used, if it was not.
    pub(crate) fn build(
        device: &ID3D12Device,
        key: Key,
    ) -> Result<(Converter, Option<String>), String> {
        let (shaders, signature, pipeline, refused) = match engine_shaders(key.pq).and_then(|s| {
            let signature = root_signature(device, &s.plan)?;
            let pipeline = pipeline(device, &signature, &s, key.out_format)?;
            Ok((s, signature, pipeline))
        }) {
            Ok((s, signature, pipeline)) => (s, signature, pipeline, None),
            Err(why) => {
                let s = own_shaders()?;
                let signature = root_signature(device, &s.plan)?;
                let pipeline = pipeline(device, &signature, &s, key.out_format)?;
                (s, signature, pipeline, Some(why))
            }
        };
        let hold = gpu::texture(
            device,
            key.size,
            key.scene_format,
            D3D12_RESOURCE_FLAG_NONE,
            states::COPY_DEST,
            w!("mhw-dlss hud-less copy"),
        )?;
        let (srv_heap, _) = gpu::heap(device, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 1, true)?;
        let (rtv_heap, rtv_stride) =
            gpu::heap(device, D3D12_DESCRIPTOR_HEAP_TYPE_RTV, SLOTS, false)?;
        let make_slot = |index: usize| -> Result<Arc<Slot>, String> {
            let output = gpu::texture(
                device,
                key.size,
                key.out_format,
                D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                OUTPUT_STATE,
                w!("mhw-dlss hud-less image"),
            )?;
            // SAFETY: a live heap with `SLOTS` descriptors; a live texture.
            unsafe {
                let handle = D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: rtv_heap.GetCPUDescriptorHandleForHeapStart().ptr + index * rtv_stride,
                };
                let _guard = Guard::enter();
                device.CreateRenderTargetView(&output, None, handle);
            }
            Ok(Arc::new(Slot {
                output,
                own: Mutex::new(OwnList::new(device)?),
            }))
        };
        let slots = [make_slot(0)?, make_slot(1)?, make_slot(2)?];
        // SAFETY: a live heap and texture.
        unsafe {
            let _guard = Guard::enter();
            device.CreateShaderResourceView(
                &hold,
                Some(&gpu::srv(key.scene_format)),
                srv_heap.GetCPUDescriptorHandleForHeapStart(),
            );
        }
        let encoder = shaders.encoder;
        log::info!(
            target: "hudless",
            "conversion ready {}x{} format {} -> {} ({}), encoder {:?}",
            key.size.w,
            key.size.h,
            key.scene_format,
            key.out_format,
            if key.pq { "PQ" } else { "SDR" },
            encoder
        );
        Ok((
            Converter {
                key,
                encoder,
                shared: Arc::new(Shared {
                    hold,
                    srv_heap,
                    rtv_heap,
                    rtv_stride,
                    signature,
                    pipeline,
                    plan: shaders.plan,
                }),
                slots,
                next: 0,
            },
            refused,
        ))
    }

    /// The next slot whose last conversion has completed.
    pub(crate) fn free_slot(
        &mut self,
        completed: u64,
        presents: u64,
    ) -> Option<(usize, Arc<Slot>)> {
        for step in 0..SLOTS {
            let index = (self.next + step) % SLOTS;
            let slot = &self.slots[index];
            if slot.own.lock().free(completed, presents) {
                self.next = (index + 1) % SLOTS;
                return Some((index, Arc::clone(slot)));
            }
        }
        None
    }

    /// Whether no conversion of this converter is still running.
    pub(crate) fn idle(&self, completed: u64) -> bool {
        self.slots
            .iter()
            .all(|slot| slot.own.lock().idle(completed))
    }
}

/// Record the conversion of `shared.hold` into slot `index` on its own list
/// (opened here and always closed); the ticket to run it with.
pub(crate) fn record(
    shared: &Shared,
    slot: &Slot,
    index: usize,
    size: Size,
    transform: &OutputTransform,
    presents: u64,
) -> Result<u64, String> {
    let mut own = slot.own.lock();
    let ticket = own.open(presents)?;
    let list = own.list();
    let hold = shared.hold.as_raw() as usize;
    let output = slot.output.as_raw() as usize;
    let mut moves = Explicit::default();
    moves.add(hold, states::COPY_DEST, states::PIXEL_SHADER_RESOURCE);
    moves.add(output, OUTPUT_STATE, states::RENDER_TARGET);
    let constants = shared.plan.values(transform);
    let _guard = Guard::enter();
    // SAFETY: our open list and our own live objects; every pointer passed
    // outlives its call.
    let closed = unsafe {
        moves.forward(list);
        list.SetDescriptorHeaps(&[Some(shared.srv_heap.clone())]);
        list.SetGraphicsRootSignature(&shared.signature);
        list.SetPipelineState(&shared.pipeline);
        list.SetGraphicsRootDescriptorTable(
            0,
            shared.srv_heap.GetGPUDescriptorHandleForHeapStart(),
        );
        if !constants.is_empty() {
            list.SetGraphicsRoot32BitConstants(
                1,
                constants.len() as u32,
                constants.as_ptr().cast(),
                0,
            );
        }
        list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        list.RSSetViewports(&[D3D12_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: size.w as f32,
            Height: size.h as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        }]);
        list.RSSetScissorRects(&[RECT {
            left: 0,
            top: 0,
            right: size.w as i32,
            bottom: size.h as i32,
        }]);
        let target = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: shared.rtv_heap.GetCPUDescriptorHandleForHeapStart().ptr
                + index * shared.rtv_stride,
        };
        list.OMSetRenderTargets(1, Some(&target), false, None);
        list.DrawInstanced(3, 1, 0, 0);
        moves.back(list);
        list.Close()
    };
    closed.map_err(failed("conversion list close"))?;
    Ok(ticket)
}

/// Run slot's recording of `ticket` behind the engine list on `queue`.
pub(crate) fn run(slot: &Slot, ticket: u64, queue: &ID3D12CommandQueue, fence: &Fence) {
    // SAFETY: the recording of `ticket` was closed by `record`; the queue
    // is the live one the engine list just went to.
    unsafe { slot.own.lock().run(ticket, queue, fence) };
}
