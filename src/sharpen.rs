//! Sharpening the resolved picture, after DLSS and before the interface.
//!
//! DLSS has not sharpened since 2.5.1, and the engine's own sharpeners do not
//! run under this mod (see `tune.rs`), so what reaches the screen is the
//! resolve as it comes: softer than the engine's own anti-aliasing, which
//! sharpens after itself. A sharpening effect in ReShade does run, but at
//! `Present`, on the finished frame: it sharpens the interface with the
//! scene, and with frame generation the generated frames too.
//!
//! So the sharpening is done here, where the resolve is made. With `sharpen`
//! above zero DLSS is handed a texture of this module's own to write instead
//! of the engine's target, and one compute pass then reads that texture and
//! writes the engine's target. Everything the engine does afterwards -- its
//! remaining passes, the interface, and frame generation's inputs -- gets the
//! sharpened picture, and nothing drawn later is touched.
//!
//! The filter is the one from FidelityFX FSR 1, RCAS: a five-tap cross whose
//! strength is limited per pixel so that it cannot ring. It is written for a
//! picture in a perceptual encoding with values up to one, and the picture
//! at this point is linear and high-range, so each tap is brought into that
//! range by a tonemap and then square-rooted, and both are undone on the way
//! out. Run on the linear values it did next to nothing below the
//! highlights, which is where textures are. FSR's optional noise term is
//! left out, as FSR leaves it by default: it halves the strength exactly
//! where a pixel differs from its neighbours, which is fine detail.
//!
//! With `sharpen = 0`, which is how the mod ships, none of this exists:
//! `evaluate` is `dlss::evaluate`, and no texture, shader or pipeline is
//! made. Anything that stops the pass being built -- a target format this
//! module does not know, a shader that will not compile -- is said once and
//! the resolve goes straight into the engine's target, as without it.
//!
//! Only this module's own texture is moved between states, as in `mv.rs`,
//! and it is put back before the function returns, so its state is the same
//! at rest on every list.

use parking_lot::Mutex;
use std::ffi::c_void;
use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

/// Descriptor pairs cycled through, so a frame never rewrites descriptors the
/// GPU may still be reading from an earlier one. The engine's target rotates.
const RING: usize = 16;
/// Descriptors per ring slot, in table order: t0 the resolve, u0 the target.
const SLOT_DESCRIPTORS: usize = 2;

/// The formats a target may have for this pass to write it: the ones a
/// float4 can be stored to through an unordered access view of the same
/// format. `R11G11B10_FLOAT`, which is what this engine's targets are, then
/// `R16G16B16A16_FLOAT`, `R10G10B10A2_UNORM` and `R8G8B8A8_UNORM`.
const FORMATS: [i32; 4] = [26, 10, 24, 28];

/// How many further frames a replaced sharpener is kept for, because the
/// lists that used it may not have been executed yet. The GPU runs at most
/// two or three frames behind.
const RETIRE_AFTER: u32 = 8;

const SHADER: &str = r#"
Texture2D<float4>   Resolved : register(t0);
RWTexture2D<float4> Target   : register(u0);

cbuffer Params : register(b0)
{
    float Amount;
    float3 Pad;
};

float peak(float3 c)
{
    return max(c.r, max(c.g, c.b));
}

// A tap, brought into [0, 1) and then into a perceptual encoding: the filter
// below is written for both, and the picture here is linear and high-range.
// Undone before the write. A tap that is not a number is taken as black, so
// it cannot spread to its neighbours.
float3 Tap(int2 at, int2 last)
{
    float3 c = Resolved[clamp(at, int2(0, 0), last)].rgb;
    bool finite = isfinite(c.r) && isfinite(c.g) && isfinite(c.b);
    c = finite ? max(c, 0.0) : float3(0.0, 0.0, 0.0);
    return sqrt(c / (1.0 + peak(c)));
}

[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    uint width, height;
    Target.GetDimensions(width, height);
    if (id.x >= width || id.y >= height)
    {
        return;
    }
    int2 at = int2(id.xy);
    int2 last = int2(width - 1, height - 1);
    float4 centre = Resolved[at];
    // A pixel that is not a number is passed on as it is.
    if (!(isfinite(centre.r) && isfinite(centre.g) && isfinite(centre.b)))
    {
        Target[at] = centre;
        return;
    }

    //    b
    //  d e f
    //    h
    float3 b = Tap(at + int2(0, -1), last);
    float3 d = Tap(at + int2(-1, 0), last);
    float3 e = Tap(at, last);
    float3 f = Tap(at + int2(1, 0), last);
    float3 h = Tap(at + int2(0, 1), last);

    // The most negative lobe that keeps the result from clipping, per
    // channel, from the smallest and the largest of the ring. The centre is
    // not part of either, as in FSR 1.0 and in the RCAS people run in
    // ReShade: the later variant that includes it sharpens least where the
    // centre stands out from its neighbours, which is the detail.
    float3 smallest = min(min(b, d), min(f, h));
    float3 largest = max(max(b, d), max(f, h));
    float3 hit_min = smallest / max(4.0 * largest, 1e-5);
    float3 hit_max = (1.0 - largest) / min(4.0 * smallest - 4.0, -1e-5);
    float3 lobes = max(-hit_min, hit_max);
    float lobe = max(-0.1875, min(peak(lobes), 0.0)) * Amount;

    float3 sharp = (lobe * (b + d + f + h) + e) / (4.0 * lobe + 1.0);
    // Out of the perceptual encoding, then out of the tonemap.
    sharp = clamp(sharp, 0.0, 0.9999);
    sharp *= sharp;
    Target[at] = float4(sharp / (1.0 - peak(sharp)), centre.a);
}
"#;

struct Sharpener {
    device: ID3D12Device,
    /// What DLSS writes while this pass is on. At rest in unordered access.
    texture: ID3D12Resource,
    heap: ID3D12DescriptorHeap,
    signature: ID3D12RootSignature,
    pipeline: ID3D12PipelineState,
    stride: usize,
    size: (u32, u32),
    format: DXGI_FORMAT,
    next: usize,
}

unsafe impl Send for Sharpener {}
unsafe impl Sync for Sharpener {}

static SHARPENER: Mutex<Option<Sharpener>> = Mutex::new(None);
/// Sharpeners that have been replaced, with the frames each is still kept.
static RETIRED: Mutex<Vec<(u32, Sharpener)>> = Mutex::new(Vec::new());
/// The size and format the pass could not be built for, so that it is not
/// tried again every frame.
static REFUSED: Mutex<Option<((u32, u32), i32)>> = Mutex::new(None);

unsafe fn compile() -> windows::core::Result<ID3DBlob> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let result = D3DCompile(
        SHADER.as_ptr() as *const c_void,
        SHADER.len(),
        PCSTR(b"sharpen\0".as_ptr()),
        None,
        None,
        PCSTR(b"main\0".as_ptr()),
        PCSTR(b"cs_5_0\0".as_ptr()),
        D3DCOMPILE_OPTIMIZATION_LEVEL3,
        0,
        &mut code,
        Some(&mut errors),
    );
    if let Some(errors) = errors {
        let text = std::slice::from_raw_parts(
            errors.GetBufferPointer() as *const u8,
            errors.GetBufferSize(),
        );
        crate::log!(
            "[shp] compiler said: {}",
            String::from_utf8_lossy(text).trim()
        );
    }
    result?;
    code.ok_or_else(windows::core::Error::from_win32)
}

unsafe fn build(
    device: &ID3D12Device,
    size: (u32, u32),
    format: DXGI_FORMAT,
) -> windows::core::Result<Sharpener> {
    let ranges = [
        D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
            NumDescriptors: 1,
            BaseShaderRegister: 0,
            RegisterSpace: 0,
            OffsetInDescriptorsFromTableStart: 0,
        },
        D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
            NumDescriptors: 1,
            BaseShaderRegister: 0,
            RegisterSpace: 0,
            OffsetInDescriptorsFromTableStart: 1,
        },
    ];
    let parameters = [
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                    NumDescriptorRanges: ranges.len() as u32,
                    pDescriptorRanges: ranges.as_ptr(),
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Constants: D3D12_ROOT_CONSTANTS {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                    Num32BitValues: 4,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
    ];
    let description = D3D12_ROOT_SIGNATURE_DESC {
        NumParameters: parameters.len() as u32,
        pParameters: parameters.as_ptr(),
        NumStaticSamplers: 0,
        pStaticSamplers: std::ptr::null(),
        Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
    };
    let mut blob: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    D3D12SerializeRootSignature(
        &description,
        D3D_ROOT_SIGNATURE_VERSION_1,
        &mut blob,
        Some(&mut errors),
    )?;
    let blob = blob.ok_or_else(windows::core::Error::from_win32)?;
    let signature: ID3D12RootSignature = device.CreateRootSignature(
        0,
        std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()),
    )?;

    let code = compile()?;
    let pipeline: ID3D12PipelineState =
        device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&signature),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: code.GetBufferPointer(),
                BytecodeLength: code.GetBufferSize(),
            },
            NodeMask: 0,
            CachedPSO: D3D12_CACHED_PIPELINE_STATE::default(),
            Flags: D3D12_PIPELINE_STATE_FLAG_NONE,
        })?;

    let mut texture: Option<ID3D12Resource> = None;
    device.CreateCommittedResource(
        &D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        },
        D3D12_HEAP_FLAG_NONE,
        &D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 0,
            Width: size.0 as u64,
            Height: size.1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        },
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        None,
        &mut texture,
    )?;
    let texture = texture.ok_or_else(windows::core::Error::from_win32)?;

    let heap: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
        Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
        NumDescriptors: (RING * SLOT_DESCRIPTORS) as u32,
        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
        NodeMask: 0,
    })?;
    let stride =
        device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;

    crate::log!(
        "[shp] sharpening pass ready at {}x{}, format {}",
        size.0,
        size.1,
        format.0
    );
    Ok(Sharpener {
        device: device.clone(),
        texture,
        heap,
        signature,
        pipeline,
        stride,
        size,
        format,
        next: 0,
    })
}

unsafe fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: std::mem::transmute_copy(resource),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// Whether a target of this size and format is one the pass can write, and
/// if not, why.
fn writable(size: (u32, u32), display: (u32, u32), format: i32) -> Result<(), String> {
    if size != display || size.0 == 0 || size.1 == 0 {
        return Err(format!(
            "the target is {}x{} and the resolve is {}x{}",
            size.0, size.1, display.0, display.1
        ));
    }
    if !FORMATS.contains(&format) {
        return Err(format!(
            "the target's format is {format}, which is not one this pass writes"
        ));
    }
    Ok(())
}

/// The sharpener for this frame's target, built or rebuilt if it has to be.
unsafe fn ready<'a>(
    slot: &'a mut Option<Sharpener>,
    frame: &crate::dlss::Frame,
) -> Option<&'a mut Sharpener> {
    // One frame nearer to letting go of whatever was replaced.
    RETIRED.lock().retain_mut(|(left, _)| {
        *left = left.saturating_sub(1);
        *left > 0
    });

    let info = crate::gpu::described(frame.output as usize)?;
    let size = (info.Width as u32, info.Height);
    let refuse = |why: String| {
        let mut refused = REFUSED.lock();
        if *refused != Some((size, info.Format.0)) {
            *refused = Some((size, info.Format.0));
            crate::log!(
                "[shp] not sharpening: {why}. DLSS writes the engine's target directly, as with \
                 sharpen = 0"
            );
        }
    };
    if let Err(why) = writable(size, frame.display, info.Format.0) {
        refuse(why);
        return None;
    }
    if slot
        .as_ref()
        .is_some_and(|sharpener| sharpener.size != size || sharpener.format != info.Format)
    {
        if let Some(old) = slot.take() {
            RETIRED.lock().push((RETIRE_AFTER, old));
        }
    }
    if slot.is_none() {
        if *REFUSED.lock() == Some((size, info.Format.0)) {
            return None;
        }
        // ReShade answers `GetDevice` with its proxy; what is built here is
        // this library's own and belongs on the native device, as in `mv.rs`.
        static NATIVE_DEVICE: crate::reshade::Boundary = crate::reshade::Boundary::new(
            "the device the engine's full-size target reports",
            "the sharpening pass is built on the native device",
        );
        let output = ID3D12Resource::from_raw_borrowed(&frame.output)?;
        let mut device: Option<ID3D12Device> = None;
        output.GetDevice(&mut device).ok()?;
        let device = NATIVE_DEVICE.native_interface(device?);
        match build(&device, size, info.Format) {
            Ok(sharpener) => *slot = Some(sharpener),
            Err(error) => {
                refuse(format!("the pass could not be built ({error})"));
                return None;
            }
        }
    }
    slot.as_mut()
}

impl Sharpener {
    /// Record the pass: read this module's texture, write `output`.
    ///
    /// # Safety
    /// `list_raw` must be the native list, open for recording, and `output`
    /// a live resource in the unordered access state on it.
    unsafe fn record(&mut self, list_raw: *mut c_void, output: *mut c_void, amount: f32) -> bool {
        let Some(list) = ID3D12GraphicsCommandList::from_raw_borrowed(&list_raw) else {
            return false;
        };
        let Some(output) = ID3D12Resource::from_raw_borrowed(&output) else {
            return false;
        };

        let slot = self.next;
        self.next = (self.next + 1) % RING;
        let offset = slot * SLOT_DESCRIPTORS * self.stride;
        let cpu = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.heap.GetCPUDescriptorHandleForHeapStart().ptr + offset,
        };
        let mut read = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: self.format,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
            Shader4ComponentMapping: 0x1688, // D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING
            ..Default::default()
        };
        read.Anonymous.Texture2D = D3D12_TEX2D_SRV {
            MostDetailedMip: 0,
            MipLevels: 1,
            PlaneSlice: 0,
            ResourceMinLODClamp: 0.0,
        };
        self.device
            .CreateShaderResourceView(&self.texture, Some(&read), cpu);
        let mut write = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: self.format,
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        write.Anonymous.Texture2D = D3D12_TEX2D_UAV {
            MipSlice: 0,
            PlaneSlice: 0,
        };
        self.device.CreateUnorderedAccessView(
            output,
            None,
            Some(&write),
            D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: cpu.ptr + self.stride,
            },
        );

        list.ResourceBarrier(&[transition(
            &self.texture,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        )]);
        list.SetDescriptorHeaps(&[Some(self.heap.clone())]);
        list.SetComputeRootSignature(&self.signature);
        list.SetPipelineState(&self.pipeline);
        list.SetComputeRootDescriptorTable(
            0,
            D3D12_GPU_DESCRIPTOR_HANDLE {
                ptr: self.heap.GetGPUDescriptorHandleForHeapStart().ptr + offset as u64,
            },
        );
        let constants = [amount.to_bits(), 0, 0, 0];
        list.SetComputeRoot32BitConstants(1, 4, constants.as_ptr() as *const c_void, 0);
        list.Dispatch(self.size.0.div_ceil(8), self.size.1.div_ceil(8), 1);
        // Back to where it rests, so the next list finds it as this one did.
        list.ResourceBarrier(&[transition(
            &self.texture,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )]);
        true
    }
}

/// `dlss::evaluate`, with the result sharpened on its way into
/// `frame.output` when the settings ask for that.
///
/// `output_in_unordered_access` is whether the caller has put `frame.output`
/// in that state on this list, which this pass needs and NGX is not asked
/// about; without it the resolve is not sharpened.
///
/// The caller holds `gpu::Guard` and restores the list's state afterwards,
/// as it already does for NGX: this sets a descriptor heap, a root signature
/// and a pipeline of its own.
///
/// # Safety
/// As `dlss::evaluate`; `list` is the native list.
pub unsafe fn evaluate(
    list: *mut c_void,
    frame: &crate::dlss::Frame,
    output_in_unordered_access: bool,
) -> bool {
    let amount = crate::config::get().sharpen;
    if amount <= 0.0 || !output_in_unordered_access {
        return crate::dlss::evaluate(list, frame);
    }
    let mut guard = SHARPENER.lock();
    let Some(sharpener) = ready(&mut guard, frame) else {
        drop(guard);
        return crate::dlss::evaluate(list, frame);
    };
    let resolved = crate::dlss::Frame {
        colour: frame.colour,
        output: sharpener.texture.as_raw(),
        depth: frame.depth,
        motion: frame.motion,
        render: frame.render,
        display: frame.display,
        jitter: frame.jitter,
        motion_scale: frame.motion_scale,
        reset: frame.reset,
    };
    if !crate::dlss::evaluate(list, &resolved) {
        return false;
    }
    let recorded = sharpener.record(list, frame.output, amount);
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if recorded && !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
        crate::log!(
            "[shp] DLSS resolves into this library's own texture and the sharpening pass \
             writes the engine's target {:#x} from it, at {amount:.2}",
            frame.output as usize
        );
    }
    recorded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_target_the_pass_can_write_is_sharpened() {
        assert!(writable((3840, 2160), (3840, 2160), 26).is_ok());
        assert!(writable((3840, 2160), (3840, 2160), 10).is_ok());
        // A target that is not the size DLSS resolves to.
        assert!(writable((3840, 2160), (2560, 1440), 26).is_err());
        assert!(writable((0, 0), (0, 0), 26).is_err());
        // Typeless, depth and block-compressed formats, and unknown.
        for format in [0, 27, 40, 71, 87] {
            assert!(writable((1920, 1080), (1920, 1080), format).is_err());
        }
    }

    /// The shader is what `D3DCompile` is given at the first sharpened
    /// frame, in the game. Compiled here so that a mistake in it is found by
    /// the tests and not there.
    #[test]
    fn the_shader_compiles() {
        let code = unsafe { compile() };
        assert!(code.is_ok(), "{:?}", code.err());
    }
}
