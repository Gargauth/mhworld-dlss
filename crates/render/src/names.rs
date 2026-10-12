//! Readable names for DXGI formats and resource states, for log lines.

use std::borrow::Cow;

use crate::states::State;

/// `DXGI_FORMAT_R11G11B10_FLOAT`: the engine's colour.
pub const R11G11B10_FLOAT: u32 = 26;
/// `DXGI_FORMAT_R32_UINT`: `tGBuffer4`.
pub const R32_UINT: u32 = 42;
/// `DXGI_FORMAT_R32_TYPELESS`: a typeless resource viewable as `R32_UINT`.
pub const R32_TYPELESS: u32 = 39;
/// `DXGI_FORMAT_R16G16_FLOAT`: decoded motion vectors.
pub const R16G16_FLOAT: u32 = 34;

/// The name of a `DXGI_FORMAT`.
pub fn format(format: u32) -> Cow<'static, str> {
    let name = match format {
        0 => "UNKNOWN",
        2 => "R32G32B32A32_FLOAT",
        10 => "R16G16B16A16_FLOAT",
        11 => "R16G16B16A16_UNORM",
        16 => "R32G32_FLOAT",
        19 => "R32G8X24_TYPELESS",
        20 => "D32_FLOAT_S8X24_UINT",
        24 => "R10G10B10A2_UNORM",
        26 => "R11G11B10_FLOAT",
        27 => "R8G8B8A8_TYPELESS",
        28 => "R8G8B8A8_UNORM",
        29 => "R8G8B8A8_UNORM_SRGB",
        34 => "R16G16_FLOAT",
        35 => "R16G16_UNORM",
        37 => "R16G16_SINT",
        39 => "R32_TYPELESS",
        40 => "D32_FLOAT",
        41 => "R32_FLOAT",
        42 => "R32_UINT",
        44 => "R24G8_TYPELESS",
        45 => "D24_UNORM_S8_UINT",
        46 => "R24_UNORM_X8_TYPELESS",
        49 => "R8G8_UNORM",
        54 => "R16_FLOAT",
        56 => "R16_UNORM",
        57 => "R16_UINT",
        61 => "R8_UNORM",
        62 => "R8_UINT",
        87 => "B8G8R8A8_UNORM",
        91 => "B8G8R8A8_UNORM_SRGB",
        other => return Cow::Owned(format!("DXGI_FORMAT({other})")),
    };
    Cow::Borrowed(name)
}

/// `NON_PIXEL_SHADER_RESOURCE|PIXEL_SHADER_RESOURCE`, `COMMON`, or hex.
pub fn state(state: State) -> String {
    const NAMES: &[(u32, &str)] = &[
        (0x1, "VERTEX_AND_CONSTANT_BUFFER"),
        (0x2, "INDEX_BUFFER"),
        (0x4, "RENDER_TARGET"),
        (0x8, "UNORDERED_ACCESS"),
        (0x10, "DEPTH_WRITE"),
        (0x20, "DEPTH_READ"),
        (0x40, "NON_PIXEL_SHADER_RESOURCE"),
        (0x80, "PIXEL_SHADER_RESOURCE"),
        (0x100, "STREAM_OUT"),
        (0x200, "INDIRECT_ARGUMENT"),
        (0x400, "COPY_DEST"),
        (0x800, "COPY_SOURCE"),
        (0x1000, "RESOLVE_DEST"),
        (0x2000, "RESOLVE_SOURCE"),
    ];
    if state == 0 {
        return "COMMON".into();
    }
    let mut rest = state;
    let mut listed: Vec<&str> = Vec::new();
    for (bit, name) in NAMES {
        if state & bit != 0 {
            listed.push(name);
            rest &= !bit;
        }
    }
    let mut text = listed.join("|");
    if rest != 0 {
        if !text.is_empty() {
            text.push('|');
        }
        text.push_str(&format!("{rest:#x}"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(format(26), "R11G11B10_FLOAT");
        assert_eq!(format(200), "DXGI_FORMAT(200)");
        assert_eq!(state(0), "COMMON");
        assert_eq!(
            state(0xc0),
            "NON_PIXEL_SHADER_RESOURCE|PIXEL_SHADER_RESOURCE"
        );
        assert_eq!(state(0x8 | 0x10000), "UNORDERED_ACCESS|0x10000");
    }
}
