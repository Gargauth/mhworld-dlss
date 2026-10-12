//! Reading a pipeline state stream (`ID3D12Device2::CreatePipelineState`).
//!
//! The engine never uses the stream form. ReShade does, on its behalf: with
//! an add-on listening for pipeline events, its proxy device rebuilds every
//! `CreateComputePipelineState`/`CreateGraphicsPipelineState` as a stream
//! and calls `CreatePipelineState` on the native device (`d3d12_device.cpp`,
//! v6.8.0). Without this the temporal pass is never seen.
//!
//! A stream is a sequence of `alignas(void*) { u32 type; T data; }`
//! sub-objects. Sizes are fixed by the x64 ABI and listed here; an unknown
//! type ends the walk rather than being guessed at, because a wrong size
//! would read the next tag out of the middle of a structure.

/// `D3D12_SHADER_BYTECODE` as found in a stream: the address and length of
/// the bytecode (the caller owns that memory for the duration of the call).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bytecode {
    /// `pShaderBytecode`.
    pub address: usize,
    /// `BytecodeLength`.
    pub len: usize,
}

/// What a stream describes, as far as this crate cares.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamDesc {
    /// The compute shader, for a compute pipeline.
    pub cs: Option<Bytecode>,
    /// The pixel shader, for a graphics pipeline.
    pub ps: Option<Bytecode>,
    /// `RTVFormats[0]` when `NumRenderTargets > 0`.
    pub rtv0: Option<u32>,
    /// Offset of the compute shader sub-object, when there is one.
    pub cs_at: Option<usize>,
    /// Offset of the cached-pipeline sub-object, when there is one.
    pub cached_at: Option<usize>,
    /// Whether the walk reached the end of the stream (`false`: it stopped
    /// at an unknown or truncated sub-object; what was found before stands).
    pub complete: bool,
}

const ROOT_SIGNATURE: u32 = 0;
const PS: u32 = 2;
const CS: u32 = 6;
const RENDER_TARGET_FORMATS: u32 = 15;
const CACHED_PSO: u32 = 19;

/// Size of one sub-object of type `kind` on x64, padding included.
pub fn subobject_size(kind: u32) -> Option<usize> {
    Some(match kind {
        ROOT_SIGNATURE => 16,            // tag, pad, pointer
        1..=6 | 24 | 25 => 24,           // tag, pad, {pointer, length}: VS PS DS HS GS CS AS MS
        7 => 40,                         // D3D12_STREAM_OUTPUT_DESC (32)
        8 => 336,                        // D3D12_BLEND_DESC (328), 4-aligned
        9 | 13 | 14 | 16 | 18 | 20 => 8, // u32/enum: sample mask, strip cut, topology, DSV format, node mask, flags
        10 => 48,                        // D3D12_RASTERIZER_DESC (44)
        11 => 56,                        // D3D12_DEPTH_STENCIL_DESC (52)
        12 => 24,                        // D3D12_INPUT_LAYOUT_DESC {pointer, u32}
        RENDER_TARGET_FORMATS => 40,     // D3D12_RT_FORMAT_ARRAY (8 formats + count)
        17 => 16,                        // DXGI_SAMPLE_DESC
        19 => 24,                        // D3D12_CACHED_PIPELINE_STATE {pointer, size}
        21 => 64,                        // D3D12_DEPTH_STENCIL_DESC1 (56)
        22 => 32,                        // D3D12_VIEW_INSTANCING_DESC
        _ => return None,
    })
}

/// More sub-objects than any real stream has.
const MAX_SUBOBJECTS: usize = 64;

/// Walk a stream's bytes.
pub fn parse(stream: &[u8]) -> StreamDesc {
    let mut desc = StreamDesc::default();
    let mut offset = 0usize;
    for _ in 0..MAX_SUBOBJECTS {
        if offset == stream.len() {
            desc.complete = true;
            return desc;
        }
        let Some(kind) = u32_at(stream, offset) else {
            return desc;
        };
        let Some(size) = subobject_size(kind) else {
            return desc;
        };
        let Some(body) = stream.get(offset..offset + size) else {
            return desc;
        };
        match kind {
            CS | PS => {
                // A pointer-holding struct sits at the next pointer boundary.
                let (Some(address), Some(len)) = (u64_at(body, 8), u64_at(body, 16)) else {
                    return desc;
                };
                let code = (address != 0 && len != 0).then_some(Bytecode {
                    address: address as usize,
                    len: len as usize,
                });
                if kind == CS {
                    desc.cs = code;
                    desc.cs_at = Some(offset);
                } else {
                    desc.ps = code;
                }
            }
            RENDER_TARGET_FORMATS => {
                let count = u32_at(body, 4 + 8 * 4).unwrap_or(0);
                desc.rtv0 = if count > 0 { u32_at(body, 4) } else { None };
            }
            CACHED_PSO => desc.cached_at = Some(offset),
            _ => {}
        }
        offset += size;
    }
    desc
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(slice.try_into().ok()?))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    let slice = bytes.get(at..at.checked_add(8)?)?;
    Some(u64::from_le_bytes(slice.try_into().ok()?))
}

/// A copy of `stream` (as described by `desc`, fully walked) building `code`
/// as its compute shader, with any cached pipeline blob dropped (it belongs
/// to the original shader). The copy is `u64`s: sub-objects are pointer
/// aligned. `None` for an incomplete walk or a stream without a compute
/// shader.
pub fn with_compute_shader(stream: &[u8], desc: &StreamDesc, code: Bytecode) -> Option<Vec<u64>> {
    if !desc.complete {
        return None;
    }
    let at = desc.cs_at?;
    let mut bytes = stream.to_vec();
    put_u64(&mut bytes, at + 8, code.address as u64)?;
    put_u64(&mut bytes, at + 16, code.len as u64)?;
    if let Some(cached) = desc.cached_at {
        put_u64(&mut bytes, cached + 8, 0)?;
        put_u64(&mut bytes, cached + 16, 0)?;
    }
    bytes.resize(bytes.len().div_ceil(8) * 8, 0);
    Some(
        bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|word| u64::from_le_bytes(*word))
            .collect(),
    )
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) -> Option<()> {
    bytes
        .get_mut(at..at.checked_add(8)?)?
        .copy_from_slice(&value.to_le_bytes());
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(kind: u32, body: &[u8], size: usize) -> Vec<u8> {
        let mut out = vec![0u8; size];
        out[0..4].copy_from_slice(&kind.to_le_bytes());
        let at = if size > 8 && kind != 15 && kind != 8 {
            8
        } else {
            4
        };
        out[at..at + body.len()].copy_from_slice(body);
        out
    }

    fn shader(kind: u32, address: u64, len: u64) -> Vec<u8> {
        let mut body = address.to_le_bytes().to_vec();
        body.extend_from_slice(&len.to_le_bytes());
        tag(kind, &body, 24)
    }

    /// The stream ReShade builds for a compute pipeline: root signature,
    /// compute shader, node mask, flags.
    #[test]
    fn finds_the_compute_shader_in_reshades_stream() {
        let mut stream = tag(0, &0x1000u64.to_le_bytes(), 16);
        stream.extend(shader(6, 0xdead_0000, 32));
        stream.extend(tag(18, &1u32.to_le_bytes(), 8));
        stream.extend(tag(20, &0u32.to_le_bytes(), 8));
        let desc = parse(&stream);
        assert!(desc.complete);
        assert_eq!(
            desc.cs,
            Some(Bytecode {
                address: 0xdead_0000,
                len: 32
            })
        );
        assert_eq!(desc.ps, None);
    }

    /// The replacement keeps every other sub-object, swaps the shader and
    /// drops the cached blob of the original.
    #[test]
    fn builds_a_copy_with_another_compute_shader() {
        let mut stream = tag(0, &0x1000u64.to_le_bytes(), 16);
        stream.extend(shader(6, 0xdead_0000, 32));
        stream.extend(shader(19, 0xcafe_0000, 512));
        stream.extend(tag(18, &1u32.to_le_bytes(), 8));
        let desc = parse(&stream);
        let new = Bytecode {
            address: 0xbeef_0000,
            len: 64,
        };
        let copy = with_compute_shader(&stream, &desc, new).unwrap();
        let bytes: Vec<u8> = copy.iter().flat_map(|w| w.to_le_bytes()).collect();
        assert_eq!(bytes.len(), stream.len());
        let patched = parse(&bytes);
        assert!(patched.complete);
        assert_eq!(patched.cs, Some(new));
        let cached = desc.cached_at.unwrap();
        assert_eq!(u64_at(&bytes, cached + 8), Some(0));
        assert_eq!(u64_at(&bytes, cached + 16), Some(0));
        assert_eq!(&bytes[..16], &stream[..16]);
        assert_eq!(&bytes[bytes.len() - 8..], &stream[stream.len() - 8..]);
        // Nothing to swap without a compute shader or a complete walk.
        let graphics = shader(2, 0x20, 64);
        assert_eq!(with_compute_shader(&graphics, &parse(&graphics), new), None);
        let cut = &stream[..20];
        assert_eq!(with_compute_shader(cut, &parse(cut), new), None);
    }

    #[test]
    fn finds_the_pixel_shader_and_first_target_format() {
        let mut formats = Vec::new();
        for format in [87u32, 0, 0, 0, 0, 0, 0, 0, 1] {
            formats.extend_from_slice(&format.to_le_bytes());
        }
        let mut stream = shader(1, 0x10, 8);
        stream.extend(shader(2, 0x20, 64));
        stream.extend(tag(8, &[0; 328], 336));
        stream.extend(tag(15, &formats, 40));
        let desc = parse(&stream);
        assert!(desc.complete);
        assert_eq!(desc.ps.map(|code| code.address), Some(0x20));
        assert_eq!(desc.rtv0, Some(87));
        assert_eq!(desc.cs, None);
    }

    #[test]
    fn stops_at_unknown_or_truncated_subobjects() {
        let mut stream = tag(77, &[], 8);
        stream.extend(shader(6, 0x10, 8));
        assert_eq!(parse(&stream), StreamDesc::default());
        let stream = shader(6, 0x10, 8);
        let cut = parse(&stream[..20]);
        assert!(!cut.complete);
        assert_eq!(cut.cs, None);
        // A shader sub-object without bytecode is no shader.
        assert_eq!(parse(&shader(6, 0, 0)).cs, None);
    }

    #[test]
    fn sizes_are_the_x64_headers() {
        assert_eq!(subobject_size(0), Some(16));
        assert_eq!(subobject_size(6), Some(24));
        assert_eq!(subobject_size(8), Some(336));
        assert_eq!(subobject_size(15), Some(40));
        assert_eq!(subobject_size(17), Some(16));
        assert_eq!(subobject_size(19), Some(24));
        assert_eq!(subobject_size(23), None);
        assert_eq!(subobject_size(99), None);
    }

    /// Against the SDK's own types: `{u32 tag; T data}` aligned to a pointer.
    #[cfg(windows)]
    #[test]
    fn sizes_match_the_sdk_types() {
        use std::ffi::c_void;
        use std::mem::{align_of, size_of};
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
        fn sub<T>() -> usize {
            let data_at = 4usize.div_ceil(align_of::<T>()) * align_of::<T>();
            (data_at + size_of::<T>()).div_ceil(8) * 8
        }
        let expected = [
            (0, sub::<*mut c_void>()),
            (6, sub::<D3D12_SHADER_BYTECODE>()),
            (7, sub::<D3D12_STREAM_OUTPUT_DESC>()),
            (8, sub::<D3D12_BLEND_DESC>()),
            (9, sub::<u32>()),
            (10, sub::<D3D12_RASTERIZER_DESC>()),
            (11, sub::<D3D12_DEPTH_STENCIL_DESC>()),
            (12, sub::<D3D12_INPUT_LAYOUT_DESC>()),
            (15, sub::<D3D12_RT_FORMAT_ARRAY>()),
            (17, sub::<DXGI_SAMPLE_DESC>()),
            (19, sub::<D3D12_CACHED_PIPELINE_STATE>()),
            (21, sub::<D3D12_DEPTH_STENCIL_DESC1>()),
            (22, sub::<D3D12_VIEW_INSTANCING_DESC>()),
        ];
        for (kind, size) in expected {
            assert_eq!(subobject_size(kind), Some(size), "sub-object {kind}");
        }
    }
}
