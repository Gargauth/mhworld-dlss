//! Seeing compute pipelines whichever creation call made them.
//!
//! The engine's temporal pass is recognised when its pipeline is created:
//! `gpu::hook_create_compute_pso` sits on `ID3D12Device::
//! CreateComputePipelineState`, reflects the compute shader's bytecode and
//! remembers the pipeline. Everything downstream -- noticing the pass being
//! dispatched, resolving its inputs, running DLSS in its place -- hangs off
//! that one record.
//!
//! D3D12 has a second way to create the same object, `ID3D12Device2::
//! CreatePipelineState`, which takes the description as a stream of tagged
//! sub-objects. The engine does not use it. **ReShade does, on the engine's
//! behalf.** In an add-on build, whenever an add-on listens for pipeline
//! events -- RenoDX does, it is how it replaces shaders -- ReShade's proxy
//! device rebuilds every `CreateComputePipelineState` and
//! `CreateGraphicsPipelineState` the engine makes as a stream and calls
//! `CreatePipelineState` on the native device (`d3d12_device.cpp`,
//! `invoke_create_and_init_pipeline_event`, at v6.8.0). The call this mod had
//! patched was then never made: one pipeline seen where a session has 487, no
//! temporal pass, no DLSS, and a picture of the interface over nothing.
//!
//! So the stream form is patched too and feeds the same record. Only the
//! compute shader sub-object is looked for. Graphics pipelines are not
//! recorded by either hook; nothing here keys on them.
//!
//! What reaches this hook is the engine's own bytecode. ReShade passes the
//! stream through unchanged unless an add-on replaced something in it, and in
//! that case it creates compute pipelines with the plain call again. RenoDX's
//! Monster Hunter: World add-on replaces two *pixel* shaders.
//!
//! Called a few hundred times while the game loads and never per frame.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

use crate::slot;

type CreatePipelineStateFn = unsafe extern "system" fn(
    *mut c_void,
    *const D3D12_PIPELINE_STATE_STREAM_DESC,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;

static O_CREATE_PIPELINE_STATE: AtomicUsize = AtomicUsize::new(0);
static INSTALLED: AtomicBool = AtomicBool::new(false);
/// Calls to the stream form, and how many of them carried a compute shader.
static STREAMS: AtomicU64 = AtomicU64::new(0);
static STREAM_COMPUTE: AtomicU64 = AtomicU64::new(0);
static SAID_FIRST: AtomicBool = AtomicBool::new(false);

/// Stream creations seen, and compute pipelines recorded from them.
pub fn seen() -> (u64, u64) {
    (
        STREAMS.load(Ordering::Relaxed),
        STREAM_COMPUTE.load(Ordering::Relaxed),
    )
}

static SAID_NO_TEMPORAL_PASS: AtomicBool = AtomicBool::new(false);

/// Say, once and in words, that no temporal pass has been recognised.
///
/// The engine creates its temporal pipelines while the renderer starts, long
/// before the six hundredth present, so by the time this is first asked the
/// answer is final. Without one, DLSS has nothing to replace and nothing
/// resolves the scene, and the *absence* of `[dls]` lines alone is easy to
/// read past. Called from the periodic report in `swap.rs`, with that
/// report's own numbers.
pub fn warn_if_no_temporal_pass(presents: u64, dispatches: u64, pipelines: u64) {
    if SAID_NO_TEMPORAL_PASS.load(Ordering::Relaxed) || presents < 600 || dispatches == 0 {
        return;
    }
    // Not when the mod stopped recognising them on purpose: that has its own
    // line, and this one would describe a fault that is not there.
    if crate::gate::dlss_unusable() {
        return;
    }
    if crate::gpu::temporal_pipelines() > 0 || SAID_NO_TEMPORAL_PASS.swap(true, Ordering::Relaxed) {
        return;
    }
    let (streams, from_streams) = seen();
    crate::log!(
        "[gpu] NO TEMPORAL PASS RECOGNISED. After {presents} presents the hooks have seen \
         {dispatches} dispatches, so the engine is rendering, but only {pipelines} compute \
         pipelines were created through the calls this mod patches ({} by \
         CreateComputePipelineState, {from_streams} by CreatePipelineState out of {streams} \
         stream creations) and none of them is the engine's temporal pass. A normal session \
         has several hundred. DLSS has nothing to replace and will not run, and since the \
         game's own DLSS is this mod's stand-in nothing resolves the scene: expect the \
         interface over an empty picture and no [dls] lines. {}",
        pipelines.saturating_sub(from_streams),
        if crate::reshade::present() {
            "ReShade is loaded: a layer above this mod is creating the engine's pipelines \
             by a call it does not see, or is not passing them on"
        } else {
            "No companion layer was seen, so the likely cause is that the hooks went in \
             after the engine had built its pipelines: check that the game was started \
             through the launcher"
        }
    );
}

/// `D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_CS`.
const SUBOBJECT_CS: u32 = 6;

/// The size of one sub-object in a stream: a 32-bit type tag, then the data
/// at its own alignment, the whole padded to pointer size. That is what
/// `alignas(void*)` on a `{ type; T }` pair gives in the SDK's header.
const fn subobject<T>() -> usize {
    let alignment = std::mem::align_of::<T>();
    let data_at = 4_usize.div_ceil(alignment) * alignment;
    (data_at + std::mem::size_of::<T>()).div_ceil(8) * 8
}

/// The size of the sub-object with this type tag, or `None` for one this does
/// not know, which ends the walk. Never guessed: a wrong size would read the
/// next tag out of the middle of a structure.
fn subobject_size(kind: u32) -> Option<usize> {
    Some(match kind {
        0 => subobject::<*mut c_void>(), // root signature
        1..=6 | 24 | 25 => subobject::<D3D12_SHADER_BYTECODE>(), // VS PS DS HS GS CS, AS MS
        7 => subobject::<D3D12_STREAM_OUTPUT_DESC>(),
        8 => subobject::<D3D12_BLEND_DESC>(),
        9 => subobject::<u32>(), // sample mask
        10 => subobject::<D3D12_RASTERIZER_DESC>(),
        11 => subobject::<D3D12_DEPTH_STENCIL_DESC>(),
        12 => subobject::<D3D12_INPUT_LAYOUT_DESC>(),
        13 | 14 => subobject::<i32>(), // strip cut value, topology
        15 => subobject::<D3D12_RT_FORMAT_ARRAY>(),
        16 => subobject::<DXGI_FORMAT>(),
        17 => subobject::<DXGI_SAMPLE_DESC>(),
        18 => subobject::<u32>(), // node mask
        19 => subobject::<D3D12_CACHED_PIPELINE_STATE>(),
        20 => subobject::<i32>(), // flags
        21 => subobject::<D3D12_DEPTH_STENCIL_DESC1>(),
        22 => subobject::<D3D12_VIEW_INSTANCING_DESC>(),
        _ => return None,
    })
}

/// The compute shader in a pipeline state stream, if it has one.
///
/// Bounded twice: by the stream's own length, and by a count of sub-objects
/// no real stream approaches.
///
/// # Safety
/// `stream` must be readable for `length` bytes.
pub unsafe fn compute_shader(stream: *const u8, length: usize) -> Option<D3D12_SHADER_BYTECODE> {
    if stream.is_null() {
        return None;
    }
    let mut offset = 0usize;
    for _ in 0..64 {
        if offset + 8 > length {
            return None;
        }
        let kind = std::ptr::read_unaligned(stream.add(offset) as *const u32);
        let size = subobject_size(kind)?;
        if offset + size > length {
            return None;
        }
        if kind == SUBOBJECT_CS {
            // A shader's bytecode holds a pointer, so its data sits at the
            // next pointer boundary after the tag.
            let code =
                std::ptr::read_unaligned(stream.add(offset + 8) as *const D3D12_SHADER_BYTECODE);
            return (!code.pShaderBytecode.is_null() && code.BytecodeLength != 0).then_some(code);
        }
        offset += size;
    }
    None
}

unsafe extern "system" fn hook_create_pipeline_state(
    this: *mut c_void,
    desc: *const D3D12_PIPELINE_STATE_STREAM_DESC,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let original: CreatePipelineStateFn =
        std::mem::transmute(O_CREATE_PIPELINE_STATE.load(Ordering::Relaxed));
    let result = original(this, desc, riid, out);
    STREAMS.fetch_add(1, Ordering::Relaxed);
    if result.is_err() || desc.is_null() || out.is_null() || (*out).is_null() {
        return result;
    }
    let stream = &*desc;
    let Some(code) = compute_shader(
        stream.pPipelineStateSubobjectStream as *const u8,
        stream.SizeInBytes,
    ) else {
        return result;
    };
    STREAM_COMPUTE.fetch_add(1, Ordering::Relaxed);
    if !SAID_FIRST.swap(true, Ordering::Relaxed) {
        // Once, because it is the fact that explains everything after it:
        // something between the engine and D3D12 is creating the engine's
        // pipelines by the other call.
        crate::log!(
            "[pso] a compute pipeline reached D3D12 through ID3D12Device2::\
             CreatePipelineState and not CreateComputePipelineState (pso {:#x}, {} bytes of \
             shader). The engine does not use that call, so a layer above this mod is \
             making it; pipelines are recorded from both{}",
            *out as usize,
            code.BytecodeLength,
            if crate::reshade::present() {
                ". ReShade is loaded"
            } else {
                ""
            }
        );
    }
    crate::gpu::record_compute_pipeline(*out, code.pShaderBytecode, code.BytecodeLength);
    result
}

/// Patch the stream form on the native device.
///
/// Apart from the table in `gpu::install`, on purpose: it is one slot on a
/// later interface, and it is installed whether or not anything is expected
/// to use it. With nothing above this mod it is never called.
///
/// # Safety
/// `device` must be the native device, not a proxy.
pub unsafe fn install(device: &ID3D12Device) {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let two: ID3D12Device2 = match device.cast() {
        Ok(two) => two,
        Err(error) => {
            crate::log!(
                "[gpu] the device has no ID3D12Device2 ({error}), so pipelines created \
                 through CreatePipelineState cannot be seen"
            );
            return;
        }
    };
    let index = slot!(ID3D12Device2_Vtbl, CreatePipelineState);
    if crate::vtable::hook(
        crate::vtable::of(&two),
        index,
        hook_create_pipeline_state as *const c_void,
        &O_CREATE_PIPELINE_STATE,
    ) {
        crate::log!("[gpu] hooked slot {index:>3}  Device2::CreatePipelineState");
    } else {
        crate::log!("[gpu] FAILED slot {index:>3}  Device2::CreatePipelineState");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slot, counted independently: ID3D12Device ends at 43, ID3D12Device1
    /// adds three, and CreatePipelineState is ID3D12Device2's only method.
    #[test]
    fn the_slot_is_device2s_first() {
        assert_eq!(slot!(ID3D12Device_Vtbl, GetAdapterLuid), 43);
        assert_eq!(slot!(ID3D12Device2_Vtbl, CreatePipelineState), 47);
    }

    /// Sub-object sizes against the SDK's, which are fixed by the ABI on x64.
    #[test]
    fn subobject_sizes_are_the_headers() {
        assert_eq!(subobject_size(0), Some(16)); // tag, pad, pointer
        assert_eq!(subobject_size(6), Some(24)); // tag, pad, pointer + length
        assert_eq!(subobject_size(18), Some(8)); // tag, u32
        assert_eq!(subobject_size(20), Some(8));
        assert_eq!(subobject_size(19), Some(24));
        assert_eq!(subobject_size(15), Some(40)); // tag, eight formats, count
        assert_eq!(subobject_size(17), Some(16)); // tag, two u32, padded
        assert_eq!(subobject_size(8), Some(336)); // tag, 328 bytes of blend, padded
        assert_eq!(subobject_size(23), None);
        assert_eq!(subobject_size(99), None);
    }

    #[repr(C, align(8))]
    struct Tagged<T> {
        kind: u32,
        data: T,
    }

    /// The stream ReShade builds for a compute pipeline: root signature,
    /// compute shader, node mask, flags (`d3d12_device.cpp`, the
    /// `D3D12_COMPUTE_PIPELINE_STATE_DESC` overload).
    #[test]
    fn finds_the_shader_in_the_stream_reshade_builds() {
        #[repr(C)]
        struct Stream {
            root: Tagged<*mut c_void>,
            cs: Tagged<D3D12_SHADER_BYTECODE>,
            node_mask: Tagged<u32>,
            flags: Tagged<i32>,
        }
        let code = [0u8; 32];
        let stream = Stream {
            root: Tagged {
                kind: 0,
                data: 0x1000 as *mut c_void,
            },
            cs: Tagged {
                kind: 6,
                data: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.as_ptr() as *const c_void,
                    BytecodeLength: code.len(),
                },
            },
            node_mask: Tagged { kind: 18, data: 1 },
            flags: Tagged { kind: 20, data: 0 },
        };
        assert_eq!(std::mem::size_of::<Stream>(), 16 + 24 + 8 + 8);
        let found = unsafe {
            compute_shader(
                &stream as *const Stream as *const u8,
                std::mem::size_of::<Stream>(),
            )
        }
        .expect("the compute shader is in the stream");
        assert_eq!(found.pShaderBytecode, code.as_ptr() as *const c_void);
        assert_eq!(found.BytecodeLength, 32);
    }

    /// A graphics stream has no compute shader; a truncated or unknown one
    /// ends the walk instead of reading past it.
    #[test]
    fn declines_what_it_should() {
        #[repr(C)]
        struct Graphics {
            root: Tagged<*mut c_void>,
            vs: Tagged<D3D12_SHADER_BYTECODE>,
            ps: Tagged<D3D12_SHADER_BYTECODE>,
            mask: Tagged<u32>,
        }
        let code = [0u8; 8];
        let shader = D3D12_SHADER_BYTECODE {
            pShaderBytecode: code.as_ptr() as *const c_void,
            BytecodeLength: 8,
        };
        let graphics = Graphics {
            root: Tagged {
                kind: 0,
                data: std::ptr::null_mut(),
            },
            vs: Tagged {
                kind: 1,
                data: shader,
            },
            ps: Tagged {
                kind: 2,
                data: shader,
            },
            mask: Tagged { kind: 9, data: !0 },
        };
        let bytes = &graphics as *const Graphics as *const u8;
        let size = std::mem::size_of::<Graphics>();
        unsafe {
            assert!(compute_shader(bytes, size).is_none());
            // Cut off in the middle of the second sub-object.
            assert!(compute_shader(bytes, 20).is_none());
            assert!(compute_shader(std::ptr::null(), 64).is_none());
        }
        let unknown = [
            Tagged {
                kind: 77u32,
                data: 0u32,
            },
            Tagged { kind: 6, data: 0 },
        ];
        unsafe {
            assert!(compute_shader(unknown.as_ptr() as *const u8, 16).is_none());
        }
        // A compute sub-object with no bytecode is not a compute pipeline.
        let empty = Tagged {
            kind: 6u32,
            data: D3D12_SHADER_BYTECODE::default(),
        };
        unsafe {
            assert!(compute_shader(&empty as *const _ as *const u8, 24).is_none());
        }
    }
}
