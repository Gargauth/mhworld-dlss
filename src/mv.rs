//! Decoding the engine's motion vectors into something DLSS can read.
//!
//! `tGBuffer4` is `R32_UINT`: two sixteen-bit components packed into one word.
//! DLSS wants a two-channel floating point texture with signed values, so the
//! packed form has to be unpacked first. That is a compute shader's worth of
//! work, and writing it here rather than hunting for the engine's own decode
//! has a useful side effect: the packing, the sign and the scale all become
//! parameters, so they can be tuned against the picture while the game runs
//! instead of guessed at.
//!
//! Only this module's own texture is transitioned between states. The source
//! is read exactly as the engine's own pass reads it, in whatever state the
//! engine already put it in, which is what makes this safe where an earlier
//! attempt at copying that texture was not.

use parking_lot::Mutex;
use std::ffi::c_void;
use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R11G11B10_FLOAT, DXGI_FORMAT_R16G16_FLOAT, DXGI_FORMAT_R32_UINT, DXGI_SAMPLE_DESC,
};

/// Descriptor pairs cycled through, so a frame never rewrites descriptors the
/// GPU may still be reading from an earlier one.
const RING: usize = 16;

/// Sixteen sampled points of four words each, then the correction the shader
/// was given. The shader records that itself so that both come from the same
/// frame: read from the processor, the correction would belong to a
/// different frame than the samples and the two could not be compared.
const PROBE_WORDS: usize = 74;
const PROBE_DELTA: usize = 64;
/// Where the shader tallies colour pixels it considers unusable.
const PROBE_NONFINITE: usize = 66;
const PROBE_NEGATIVE: usize = 67;
/// Pixels with a channel above one, and the largest channel seen. Together
/// these say whether the colour is linear high-range or already tonemapped,
/// which is what the IsHDR flag claims and nothing has yet checked.
const PROBE_BRIGHT: usize = 68;
const PROBE_PEAK: usize = 69;
/// Summed luminance of the colour, and the count that went into it.
///
/// The decisive question about the flashing is whether the input is already
/// dark when it happens. If it is, the engine darkened it and DLSS can stop
/// being investigated; if it is not, the fault is downstream. Everything
/// measured so far has been about whether the inputs were handed over
/// correctly, not about whether they were right.
const PROBE_LUMA: usize = 70;
const PROBE_LUMA_COUNT: usize = 71;
/// Pixels that are all but black.
///
/// An average over the frame's centre cannot see one column going dark --
/// eight thousand samples absorb it. Counting the near-black pixels is
/// sensitive to exactly that, and costs an atomic only where it fires.
const PROBE_BLACK: usize = 72;
/// Motion vectors longer than anything plausible.
///
/// The input carries no black at all, so the black is made downstream. A
/// vector of thousands of pixels sends DLSS looking for history far outside
/// the frame, and what it finds there is nothing -- which would blacken
/// exactly one mesh, the one carrying the bad vector.
const PROBE_WILD: usize = 73;
const PROBE_BYTES: u64 = (PROBE_WORDS * 4) as u64;
/// Descriptors per ring slot, in table order: t0 source, t1 colour,
/// u0 target, u1 probe, u2 cleaned colour.
const SLOT_DESCRIPTORS: usize = 5;

const SHADER_TEMPLATE: &str = r#"
Texture2D<uint>     Source : register(t0);
// The colour the temporal pass was going to read. Screen-space reflections
// and specular highlights produce infinities and NaNs at the edges of their
// validity, the engine's own pass very likely clamped them, and replacing
// that pass removed the clamp. One NaN spreads across a contiguous region
// through temporal accumulation, which is what a whole surface turning black
// looks like.
Texture2D<float4>   Colour : register(t1);
RWTexture2D<float2> Target : register(u0);
// A handful of sampled words, read back on the processor. Four kinds of
// guesswork about this encoding all looked plausible against the picture, so
// the bits themselves are the only way to settle it.
RWBuffer<uint>      Probe  : register(u1);
// A cleaned copy, written only when asked for. Writing into the engine's own
// colour texture would be cheaper, but it would have to be moved to unordered
// access and back around this dispatch, and resource states are where this
// library has done its real damage. A texture we own has states we set.
RWTexture2D<float4> CleanColour : register(u2);

cbuffer Params : register(b0)
{
    float2 Scale;
    uint   Mode;
    // Counting costs an atomic per offending pixel, so it is asked for rather
    // than always paid.
    uint   Probing;
    // The engine jitters the current frame's projection but keeps the previous
    // one unjittered, so its vectors come out as
    //   (f_prev - f_curr) - J_curr
    // when DLSS wants the jitter-free (f_prev - f_curr). Adding J_curr back is
    // the whole correction, and it makes a still point read exactly zero --
    // which the probe below checks rather than takes on trust.
    float2 Correction;
    float  CorrectionScale;
    uint   Sanitise;
    float  Clamp;
    float3 Pad3;
};

// The debug layer corroborates the float reading from a second direction,
// warning 677 against the engine's own pipeline:
//
//   CreateGraphicsPipelineState: Pixel Shader output 'SV_Target4' has float
//   type, while the corresponding RenderTarget slot [4] has format (R32_UINT)
//
// Slot 4 is tGBuffer4, so the engine is deliberately dumping raw float bits
// into an integer target. Mode 2 reads them back the same way.
//
// Mode 0: two unsigned shorts holding a biased value, the usual way an engine
//         stores signed motion in a UNORM-ish target.
// Mode 1: two signed shorts.
// Mode 2: two half floats, horizontal in the HIGH half and vertical in the
//         low one. The order is the opposite of the obvious guess and was
//         measured, not assumed: with the jitter confined to one axis, the
//         low half moved and the high half stayed at exactly zero.
float peak_all(float3 rgb)
{
    return max(rgb.r, max(rgb.g, rgb.b));
}

float2 Unpack(uint packed)
{
    if (Mode == 0)
    {
        float2 raw = float2(packed & 0xFFFF, packed >> 16) / 65535.0;
        return raw * 2.0 - 1.0;
    }
    if (Mode == 1)
    {
        int2 raw = int2((int)(packed << 16) >> 16, (int)packed >> 16);
        return float2(raw) / 32767.0;
    }
    return float2(f16tof32(packed >> 16), f16tof32(packed & 0xFFFF));
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
    uint packed = Source[id.xy];
    float2 value = Unpack(packed);
    float2 pixels = value * Scale + Correction * CorrectionScale;

    if (Probing != 0 && (abs(pixels.x) > 200.0 || abs(pixels.y) > 200.0))
    {
        InterlockedAdd(Probe[PROBE_WILD_SLOT], 1);
    }
    // A vector no larger than the screen. Anything beyond that cannot be a
    // real displacement, and letting it through costs a whole surface.
    if (Clamp > 0.0)
    {
        pixels = clamp(pixels, -Clamp, Clamp);
    }

    Target[id.xy] = pixels;

    if (Probing != 0 || Sanitise != 0)
    {
        float4 colour = Colour[id.xy];
        float3 rgb = colour.rgb;
        // isfinite is false for both NaN and infinity, and a NaN compares
        // false against everything, so this catches the sign-of-NaN case too.
        bool finite = isfinite(rgb.r) && isfinite(rgb.g) && isfinite(rgb.b);
        bool negative = rgb.r < 0.0 || rgb.g < 0.0 || rgb.b < 0.0;

        if (Probing != 0)
        {
            // Only offending pixels are counted. A per-pixel total would be
            // eight million atomics a frame for a number the processor can
            // work out from the dimensions.
            if (!finite)
            {
                InterlockedAdd(Probe[PROBE_NONFINITE_SLOT], 1);
            }
            if (negative)
            {
                InterlockedAdd(Probe[PROBE_NEGATIVE_SLOT], 1);
            }
            // Whether anything exceeds one decides whether this buffer is
            // linear and high-range or already tonemapped. Only the pixels
            // that do are counted, so a tonemapped frame costs no atomics at
            // all -- which is itself the answer.
            // Luminance over a central patch, accumulated in thousandths so
            // an integer atomic can carry it. A patch rather than the frame:
            // a whole-frame average would bury a single surface going dark.
            //
            // Every sixteenth pixel on each axis, which is some eight thousand
            // samples rather than two million. Every pixel of the patch
            // would be two million threads contending for one address, which
            // hangs the GPU; the non-finite count above avoids the same thing
            // by counting only the pixels that fire.
            if ((id.x & 15) == 0 && (id.y & 15) == 0 &&
                id.x >= width / 4 && id.x < width * 3 / 4 &&
                id.y >= height / 4 && id.y < height * 3 / 4)
            {
                float luma = dot(rgb, float3(0.2126, 0.7152, 0.0722));
                InterlockedAdd(Probe[PROBE_LUMA_SLOT], (uint)(saturate(luma) * 1000.0));
                InterlockedAdd(Probe[PROBE_LUMA_COUNT_SLOT], 1);
            }

            // Near-black, over the whole frame rather than a sample of it.
            if (peak_all(rgb) < 0.004)
            {
                InterlockedAdd(Probe[PROBE_BLACK_SLOT], 1);
            }

            float peak = max(rgb.r, max(rgb.g, rgb.b));
            if (finite && peak > 1.0)
            {
                InterlockedAdd(Probe[PROBE_BRIGHT_SLOT], 1);
                // Positive floats order the same as their bit patterns, so an
                // integer maximum over asuint gives the largest value.
                InterlockedMax(Probe[PROBE_PEAK_SLOT], asuint(peak));
            }
        }

        if (Sanitise != 0)
        {
            // Black, not the neighbourhood's colour. A plausible-looking
            // replacement would hide how often this fires; black next to a
            // lit surface does not.
            float3 clean = finite ? max(rgb, 0.0) : float3(0.0, 0.0, 0.0);
            CleanColour[id.xy] = float4(clean, colour.a);
        }
    }

    if (id.x == 0 && id.y == 0)
    {
        Probe[PROBE_DELTA_SLOT + 0] = asuint(Correction.x);
        Probe[PROBE_DELTA_SLOT + 1] = asuint(Correction.y);
    }

    // Sixteen points spread over the frame, so a single reading shows both
    // what sits in the buffer and how it varies across the screen.
    uint2 step = uint2(width / 4, height / 4);
    if (step.x == 0 || step.y == 0)
    {
        return;
    }
    if (id.x % step.x == 0 && id.y % step.y == 0)
    {
        uint sx = id.x / step.x;
        uint sy = id.y / step.y;
        if (sx < 4 && sy < 4)
        {
            uint slot = (sy * 4 + sx) * 4;
            Probe[slot + 0] = packed;
            Probe[slot + 1] = asuint(value.x);
            Probe[slot + 2] = asuint(value.y);
            Probe[slot + 3] = (id.x << 16) | (id.y & 0xFFFF);
        }
    }
}
"#;

/// The shader source with the probe's layout filled in, so the slot number
/// cannot drift apart from the one the processor reads.
fn shader_source() -> String {
    SHADER_TEMPLATE
        .replace("PROBE_DELTA_SLOT", &PROBE_DELTA.to_string())
        .replace("PROBE_NONFINITE_SLOT", &PROBE_NONFINITE.to_string())
        .replace("PROBE_NEGATIVE_SLOT", &PROBE_NEGATIVE.to_string())
        .replace("PROBE_BRIGHT_SLOT", &PROBE_BRIGHT.to_string())
        .replace("PROBE_PEAK_SLOT", &PROBE_PEAK.to_string())
        .replace("PROBE_LUMA_COUNT_SLOT", &PROBE_LUMA_COUNT.to_string())
        .replace("PROBE_BLACK_SLOT", &PROBE_BLACK.to_string())
        .replace("PROBE_WILD_SLOT", &PROBE_WILD.to_string())
        .replace("PROBE_LUMA_SLOT", &PROBE_LUMA.to_string())
}

struct Decoder {
    texture: ID3D12Resource,
    heap: ID3D12DescriptorHeap,
    signature: ID3D12RootSignature,
    pipeline: ID3D12PipelineState,
    stride: usize,
    size: (u32, u32),
    /// Current state of our own texture, which we alone move between states.
    readable: bool,
    /// The command list that last transitioned it. A state belongs to the
    /// list recording it, and claiming one list's transition applies to
    /// another is the mistake states.rs exists to stop.
    readable_on: usize,
    next: usize,
    /// One pixel, bound where the shader's cleaned-colour output goes, so
    /// every descriptor in the table is a real one. The shader only writes it
    /// when asked to sanitise, which nothing asks.
    placeholder: ID3D12Resource,
    /// A non-shader-visible view of the probe, which is what
    /// ClearUnorderedAccessViewUint requires alongside the visible one.
    clear_heap: ID3D12DescriptorHeap,
    /// Where the shader writes its sampled words, and the mapped copy the
    /// processor reads them from a frame or two later.
    probe: ID3D12Resource,
    readback: ID3D12Resource,
    mapped: *mut u32,
}

unsafe impl Send for Decoder {}
unsafe impl Sync for Decoder {}

static DECODER: Mutex<Option<Decoder>> = Mutex::new(None);
/// Darkest and brightest the frame's centre has been since the last report.
static LUMA_RANGE: Mutex<Option<(f64, f64)>> = Mutex::new(None);
/// Fewest and most near-black pixels seen since the last report.
static BLACK_RANGE: Mutex<Option<(u64, u64)>> = Mutex::new(None);

unsafe fn compile() -> windows::core::Result<ID3DBlob> {
    let source = shader_source();
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let result = D3DCompile(
        source.as_ptr() as *const c_void,
        source.len(),
        PCSTR(b"mv_decode\0".as_ptr()),
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
            "[mv ] compiler said: {}",
            String::from_utf8_lossy(text).trim()
        );
    }
    result?;
    code.ok_or_else(windows::core::Error::from_win32)
}

unsafe fn build(device: &ID3D12Device, size: (u32, u32)) -> windows::core::Result<Decoder> {
    // One table holding the source SRV and our target UAV, plus the handful of
    // constants that make the decoding adjustable.
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
            NumDescriptors: 3,
            BaseShaderRegister: 0,
            RegisterSpace: 0,
            OffsetInDescriptorsFromTableStart: 2,
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
                    Num32BitValues: 8,
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

    let heap_properties = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let texture_description = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Alignment: 0,
        Width: size.0 as u64,
        Height: size.1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_R16G16_FLOAT,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
    };
    let mut texture: Option<ID3D12Resource> = None;
    device.CreateCommittedResource(
        &heap_properties,
        D3D12_HEAP_FLAG_NONE,
        &texture_description,
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        None,
        &mut texture,
    )?;
    let texture = texture.ok_or_else(windows::core::Error::from_win32)?;

    // The probe lives on the default heap because the shader writes it, and is
    // copied into a readback buffer that stays mapped for the life of the
    // decoder -- readback heaps allow that, and it keeps the per-frame cost to
    // a buffer copy.
    let buffer_description = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Alignment: 0,
        Width: PROBE_BYTES,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
    };
    let mut probe: Option<ID3D12Resource> = None;
    device.CreateCommittedResource(
        &heap_properties,
        D3D12_HEAP_FLAG_NONE,
        &buffer_description,
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        None,
        &mut probe,
    )?;
    let probe = probe.ok_or_else(windows::core::Error::from_win32)?;

    let readback_properties = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let readback_description = D3D12_RESOURCE_DESC {
        Flags: D3D12_RESOURCE_FLAG_NONE,
        ..buffer_description
    };
    let mut readback: Option<ID3D12Resource> = None;
    device.CreateCommittedResource(
        &readback_properties,
        D3D12_HEAP_FLAG_NONE,
        &readback_description,
        D3D12_RESOURCE_STATE_COPY_DEST,
        None,
        &mut readback,
    )?;
    let readback = readback.ok_or_else(windows::core::Error::from_win32)?;
    let mut mapped: *mut c_void = std::ptr::null_mut();
    readback.Map(0, None, Some(&mut mapped))?;

    let heap: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
        Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
        NumDescriptors: (RING * SLOT_DESCRIPTORS) as u32,
        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
        NodeMask: 0,
    })?;

    let stride =
        device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;

    // Our own texture and the probe are fixed for the decoder's life, so
    // their views are written once here. Only the source SRV has to be
    // rewritten per frame, because the engine rotates that texture -- which is
    // what the ring of slots is for.
    let base = heap.GetCPUDescriptorHandleForHeapStart();
    let mut uav_description = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R16G16_FLOAT,
        ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
        ..Default::default()
    };
    uav_description.Anonymous.Texture2D = D3D12_TEX2D_UAV {
        MipSlice: 0,
        PlaneSlice: 0,
    };

    let mut probe_description = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_UINT,
        ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
        ..Default::default()
    };
    probe_description.Anonymous.Buffer = D3D12_BUFFER_UAV {
        FirstElement: 0,
        NumElements: PROBE_WORDS as u32,
        StructureByteStride: 0,
        CounterOffsetInBytes: 0,
        Flags: D3D12_BUFFER_UAV_FLAG_NONE,
    };

    // One pixel, so the cleaned-colour slot always points at a real view even
    // when no cleaning is being done. An unwritten descriptor is only safe for
    // as long as nothing reads it, and that is a promise about shader control
    // flow rather than about the descriptor.
    let placeholder_description = D3D12_RESOURCE_DESC {
        Width: 1,
        Height: 1,
        Format: DXGI_FORMAT_R11G11B10_FLOAT,
        ..texture_description
    };
    let mut placeholder: Option<ID3D12Resource> = None;
    device.CreateCommittedResource(
        &heap_properties,
        D3D12_HEAP_FLAG_NONE,
        &placeholder_description,
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        None,
        &mut placeholder,
    )?;
    let placeholder = placeholder.ok_or_else(windows::core::Error::from_win32)?;

    let mut colour_description = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R11G11B10_FLOAT,
        ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
        ..Default::default()
    };
    colour_description.Anonymous.Texture2D = D3D12_TEX2D_UAV {
        MipSlice: 0,
        PlaneSlice: 0,
    };

    for slot in 0..RING {
        let srv = base.ptr + slot * SLOT_DESCRIPTORS * stride;
        device.CreateUnorderedAccessView(
            &texture,
            None,
            Some(&uav_description),
            D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: srv + 2 * stride,
            },
        );
        device.CreateUnorderedAccessView(
            &probe,
            None,
            Some(&probe_description),
            D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: srv + 3 * stride,
            },
        );
        device.CreateUnorderedAccessView(
            &placeholder,
            None,
            Some(&colour_description),
            D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: srv + 4 * stride,
            },
        );
    }

    // Clearing a buffer needs a view of it in a heap the shaders cannot see,
    // alongside the one they can.
    let clear_heap: ID3D12DescriptorHeap =
        device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            NumDescriptors: 1,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
            NodeMask: 0,
        })?;
    device.CreateUnorderedAccessView(
        &probe,
        None,
        Some(&probe_description),
        clear_heap.GetCPUDescriptorHandleForHeapStart(),
    );

    crate::log!("[mv ] decoder ready at {}x{}", size.0, size.1);
    Ok(Decoder {
        texture,
        heap,
        signature,
        pipeline,
        stride,
        size,
        readable: false,
        readable_on: 0,
        next: 0,
        placeholder,
        clear_heap,
        probe,
        readback,
        mapped: mapped as *mut u32,
    })
}

/// Unpack the engine's motion vectors into our own texture and return it.
///
/// # Safety
/// `list` must be open for recording and `source` a live `ID3D12Resource`.
/// Returns the texture holding the decoded vectors, in pixels.
pub unsafe fn decode(
    list_raw: *mut c_void,
    source: usize,
    colour: usize,
    size: (u32, u32),
    scale: (f32, f32),
    mode: u32,
    correction: (f32, f32),
    strength: f32,
) -> Option<*mut c_void> {
    let probing = crate::config::get().mv_probe;
    let on_list = list_raw as usize;
    let list = list_raw;
    let raw_source = source as *mut c_void;
    let raw_colour = colour as *mut c_void;
    let list = ID3D12GraphicsCommandList::from_raw_borrowed(&list)?;
    let source = ID3D12Resource::from_raw_borrowed(&raw_source)?;
    let colour = ID3D12Resource::from_raw_borrowed(&raw_colour);

    // ReShade patches `ID3D12Resource::GetDevice` to answer with its proxy
    // device. The decoder's pipeline, root signature and textures are ours and
    // belong on the native device: built through the proxy they would be shown
    // to every add-on as if the game had made them.
    static NATIVE_DEVICE: crate::reshade::Boundary = crate::reshade::Boundary::new(
        "the device an engine resource reports",
        "the motion vector decoder is built on the native device",
    );

    let mut guard = DECODER.lock();
    if guard.as_ref().is_some_and(|decoder| decoder.size != size) {
        *guard = None;
    }
    if guard.is_none() {
        let mut device: Option<ID3D12Device> = None;
        source.GetDevice(&mut device).ok()?;
        let device = NATIVE_DEVICE.native_interface(device?);
        match build(&device, size) {
            Ok(decoder) => *guard = Some(decoder),
            Err(error) => {
                crate::log!("[mv ] could not build the decoder: {error}");
                return None;
            }
        }
    }
    let decoder = guard.as_mut()?;

    let mut device: Option<ID3D12Device> = None;
    source.GetDevice(&mut device).ok()?;
    let device = NATIVE_DEVICE.native_interface(device?);

    // A fresh descriptor pair each frame: the source texture rotates, and
    // rewriting a descriptor the GPU is still reading would be a race.
    let slot = decoder.next;
    decoder.next = (decoder.next + 1) % RING;
    let base = decoder.heap.GetCPUDescriptorHandleForHeapStart();
    let srv = D3D12_CPU_DESCRIPTOR_HANDLE {
        ptr: base.ptr + slot * SLOT_DESCRIPTORS * decoder.stride,
    };

    let mut srv_description = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32_UINT,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: 0x1688, // D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING
        ..Default::default()
    };
    srv_description.Anonymous.Texture2D = D3D12_TEX2D_SRV {
        MostDetailedMip: 0,
        MipLevels: 1,
        PlaneSlice: 0,
        ResourceMinLODClamp: 0.0,
    };
    device.CreateShaderResourceView(source, Some(&srv_description), srv);

    // The colour rotates with the frame exactly as the source does, so its
    // view is rewritten per slot too. A null resource gives the shader a view
    // that reads as zero rather than an unpopulated descriptor.
    let mut colour_srv = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R11G11B10_FLOAT,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: 0x1688,
        ..Default::default()
    };
    colour_srv.Anonymous.Texture2D = D3D12_TEX2D_SRV {
        MostDetailedMip: 0,
        MipLevels: 1,
        PlaneSlice: 0,
        ResourceMinLODClamp: 0.0,
    };
    device.CreateShaderResourceView(
        colour,
        Some(&colour_srv),
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: srv.ptr + decoder.stride,
        },
    );

    let mut clean_uav = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R11G11B10_FLOAT,
        ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
        ..Default::default()
    };
    clean_uav.Anonymous.Texture2D = D3D12_TEX2D_UAV {
        MipSlice: 0,
        PlaneSlice: 0,
    };
    device.CreateUnorderedAccessView(
        &decoder.placeholder,
        None,
        Some(&clean_uav),
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: srv.ptr + 4 * decoder.stride,
        },
    );

    // Our textures are handed to DLSS as shader resources, so they have to
    // come back to unordered access before they can be written again. We own
    // them, so the state is known -- but only on the list that set it. A
    // transition recorded into a different list says nothing here.
    if decoder.readable && decoder.readable_on == on_list {
        list.ResourceBarrier(&[transition(
            &decoder.texture,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )]);
    }

    let gpu_base = decoder.heap.GetGPUDescriptorHandleForHeapStart();
    let table = D3D12_GPU_DESCRIPTOR_HANDLE {
        ptr: gpu_base.ptr + (slot * SLOT_DESCRIPTORS * decoder.stride) as u64,
    };

    list.SetDescriptorHeaps(&[Some(decoder.heap.clone())]);

    // Start the tally from nothing -- without this the counters would be the
    // sum over every frame since the decoder was built -- and only after our
    // heap is the one bound. ClearUnorderedAccessViewUint takes a GPU handle,
    // and a GPU handle is only meaningful against the heap currently set on
    // the list; doing this before the bind addressed our descriptor through
    // the engine's heap, which the validation layer reported nine thousand
    // times in a single session.
    if probing {
        let zero = [0u32; 4];
        list.ClearUnorderedAccessViewUint(
            D3D12_GPU_DESCRIPTOR_HANDLE {
                ptr: decoder.heap.GetGPUDescriptorHandleForHeapStart().ptr
                    + (slot * SLOT_DESCRIPTORS * decoder.stride + 3 * decoder.stride) as u64,
            },
            decoder.clear_heap.GetCPUDescriptorHandleForHeapStart(),
            &decoder.probe,
            &zero,
            &[],
        );
    }

    list.SetComputeRootSignature(&decoder.signature);
    list.SetPipelineState(&decoder.pipeline);
    list.SetComputeRootDescriptorTable(0, table);
    let constants = [
        scale.0.to_bits(),
        scale.1.to_bits(),
        mode,
        u32::from(probing),
        correction.0.to_bits(),
        correction.1.to_bits(),
        strength.to_bits(),
        // `Sanitise`, the shader's switch for writing a cleaned copy of the
        // colour. Off: the engine's colour holds no non-finite pixels
        // (measured, 0 of 8 294 400).
        0,
    ];
    list.SetComputeRoot32BitConstants(1, 8, constants.as_ptr() as *const c_void, 0);
    list.Dispatch(size.0.div_ceil(8), size.1.div_ceil(8), 1);

    list.ResourceBarrier(&[transition(
        &decoder.texture,
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
    )]);
    decoder.readable = true;
    decoder.readable_on = on_list;

    // The shader always writes its sixteen samples -- sixty-eight words, and
    // keeping one code path is worth more than saving them -- but the copy
    // back and the two barriers around it only happen when someone is going
    // to read them.
    if probing {
        // The probe is ours, so both ends of this are known rather than
        // assumed.
        list.ResourceBarrier(&[transition(
            &decoder.probe,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        list.CopyBufferRegion(&decoder.readback, 0, &decoder.probe, 0, PROBE_BYTES);
        list.ResourceBarrier(&[transition(
            &decoder.probe,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )]);

        report(decoder, mode, scale, strength);
    }

    Some(decoder.texture.as_raw())
}

/// Log the sampled words, with every interpretation of them side by side.
///
/// The reading is a frame or two behind, which does not matter: the question
/// is what the encoding is, not what this exact frame held.
unsafe fn report(decoder: &Decoder, mode: u32, scale: (f32, f32), strength: f32) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let now = NEXT.fetch_add(1, Ordering::Relaxed);
    if decoder.mapped.is_null() {
        return;
    }

    // The luminance is read every frame and only reported every so often. A
    // three-second snapshot misses a flash entirely, which is what a single
    // reading of 0.013 among a settled 0.25 actually was: luck. The question
    // needs the darkest the frame got in between, not a sample of it.
    {
        let words = std::slice::from_raw_parts(decoder.mapped, PROBE_WORDS);
        let count = words[PROBE_LUMA_COUNT] as f64;
        if count > 0.0 {
            let mean = words[PROBE_LUMA] as f64 / count / 1000.0;
            let mut extremes = LUMA_RANGE.lock();
            let (low, high) = extremes.get_or_insert((mean, mean));
            *low = low.min(mean);
            *high = high.max(mean);
        }
        let black = words[PROBE_BLACK] as u64;
        let mut extremes = BLACK_RANGE.lock();
        let (fewest, most) = extremes.get_or_insert((black, black));
        *fewest = (*fewest).min(black);
        *most = (*most).max(black);
    }

    if !now.is_multiple_of(180) {
        return;
    }

    let words = std::slice::from_raw_parts(decoder.mapped, PROBE_WORDS);
    let correction = (
        f32::from_bits(words[PROBE_DELTA]),
        f32::from_bits(words[PROBE_DELTA + 1]),
    );
    crate::log!(
        "[mv ] mode {mode}, correction ({:+.3},{:+.3}) px at strength {strength:+}",
        correction.0,
        correction.1
    );
    crate::log!("[mv ] standing still, the corrected vector must be zero on both axes");

    // The colour tally. A non-finite count above zero is the whole
    // explanation for a surface flashing black, and the engine's colour is
    // R11G11B10_FLOAT, which has no sign bit -- so a negative count above
    // zero would mean the format this code believes in is not the one in use.
    let pixels = decoder.size.0 as u64 * decoder.size.1 as u64;
    let nonfinite = words[PROBE_NONFINITE] as u64;
    let negative = words[PROBE_NEGATIVE] as u64;
    crate::log!(
        "[mv ] colour: {nonfinite} of {pixels} pixels not finite, {negative} negative{}",
        if negative > 0 {
            "  <-- impossible in R11G11B10_FLOAT; the format is wrong"
        } else {
            ""
        }
    );

    // The range decides whether IsHDR is a true claim. A linear high-range
    // buffer has plenty above one; a tonemapped one has none at all, and
    // telling DLSS it is high-range then has it judging exposure against a
    // scale that does not exist.
    let bright = words[PROBE_BRIGHT] as u64;
    let peak = f32::from_bits(words[PROBE_PEAK]);
    if let Some((fewest, most)) = BLACK_RANGE.lock().take() {
        crate::log!(
            "[mv ] near-black pixels in the input: {fewest} at fewest, {most} at most{}",
            if most > fewest.saturating_mul(2) + 10000 {
                "  <-- something in the input goes dark"
            } else {
                ""
            }
        );
    }
    if let Some((low, high)) = LUMA_RANGE.lock().take() {
        crate::log!(
            "[mv ] colour luminance over the centre: {low:.4} at its darkest,              {high:.4} at its brightest{}",
            if high > 0.0 && low < high * 0.5 {
                "  <-- the input itself is going dark"
            } else {
                ""
            }
        );
    }
    crate::log!(
        "[mv ] colour range: {bright} pixels above 1.0, peak {peak:.3} -> {}",
        if bright == 0 {
            "tonemapped, so the IsHDR flag is a lie"
        } else {
            "linear and high-range, so IsHDR is right"
        }
    );
    let mut nonzero = 0;
    for sample in 0..16 {
        let packed = words[sample * 4];
        let decoded = (
            f32::from_bits(words[sample * 4 + 1]),
            f32::from_bits(words[sample * 4 + 2]),
        );
        let position = words[sample * 4 + 3];
        if packed != 0 {
            nonzero += 1;
        }
        // Every point, not one in five. The question now is whether parts of
        // the frame carry no motion while others do -- foliage that writes no
        // velocity reads zero while the camera pans, and the surrounding
        // geometry does not -- and a sample of four cannot show that.
        let moved = (
            decoded.0 * scale.0 + correction.0 * strength,
            decoded.1 * scale.1 + correction.1 * strength,
        );
        crate::log!(
            "[mv ]   {:>4},{:<4} uv=({:+.6},{:+.6}) -> ({:+8.3},{:+8.3}) px  |v|={:7.3}{}",
            position >> 16,
            position & 0xFFFF,
            decoded.0,
            decoded.1,
            moved.0,
            moved.1,
            (moved.0 * moved.0 + moved.1 * moved.1).sqrt(),
            if packed == 0 {
                "  <-- no velocity written here"
            } else {
                ""
            }
        );
    }
    crate::log!("[mv ]   {nonzero} of 16 samples hold a non-zero word");
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
