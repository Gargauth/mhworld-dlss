//! The engine's packed motion vectors (`tGBuffer4`) and their decode.
//!
//! `tGBuffer4` (GBuffer set slot 4, `0x14257c6e5`) is `R32_UINT` holding two
//! half floats: **x in the high 16 bits, y in the low 16 bits** (measured:
//! with the jitter on one axis the low half moved and the high half stayed
//! zero). The pixel shader writes float bits into the integer target on
//! purpose (debug layer warning 677). Values are NDC deltas at render
//! resolution, so pixels are `v · (−w/2, +h/2)`; they are jitter-free (the
//! previous view-projection carries the current jitter, `ReJitterCamera`).
//!
//! The lowest bit of y may be a flag (MHWSS masks it with `0xFFFE`); that is
//! unconfirmed, so it is a parameter ([`YMask`]).
//!
//! The decode pass writes `R16G16_FLOAT` pixels. Its constants are generated
//! from one table so the HLSL `cbuffer` and the root signature's constant
//! count cannot disagree again (the previous decoder declared twelve dwords
//! against eight root constants and read its clamp from beyond them).

use mhw_core::frame::Size;
use mhw_core::math::Vec2;

/// What to do with the lowest bit of the packed y half.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum YMask {
    /// Keep all sixteen bits (`0xFFFF`), as the shipped decoder did.
    #[default]
    Full,
    /// Clear bit 0 (`0xFFFE`), treating it as a flag as MHWSS does.
    DropFlagBit,
}

impl YMask {
    /// The mask applied to the low half.
    pub const fn bits(self) -> u32 {
        match self {
            YMask::Full => 0xFFFF,
            YMask::DropFlagBit => 0xFFFE,
        }
    }
}

/// Policy for the decode, chosen by `mod`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecodeParams {
    /// Low-bit policy for y.
    pub y_mask: YMask,
    /// Multiplier on the NDC-to-pixel conversion (1, 1 as measured).
    pub scale: Vec2,
    /// Added to every vector after scaling, in render pixels (0, 0: the
    /// engine's vectors are jitter-free).
    pub offset_px: Vec2,
    /// Clamp each component to ±this many pixels; 0 disables.
    pub clamp_px: f32,
}

impl Default for DecodeParams {
    fn default() -> Self {
        DecodeParams {
            y_mask: YMask::Full,
            scale: Vec2::new(1.0, 1.0),
            offset_px: Vec2::new(0.0, 0.0),
            clamp_px: 0.0,
        }
    }
}

/// IEEE half to single precision.
pub fn f16_to_f32(half: u16) -> f32 {
    let sign = u32::from(half >> 15) << 31;
    let exponent = u32::from((half >> 10) & 0x1f);
    let mantissa = u32::from(half & 0x3ff);
    let bits = match (exponent, mantissa) {
        (0, 0) => sign,
        (0, _) => {
            // Subnormal: normalise.
            let shift = mantissa.leading_zeros() - 21;
            let mantissa = (mantissa << shift) & 0x3ff;
            sign | ((113 - shift) << 23) | (mantissa << 13)
        }
        (0x1f, 0) => sign | 0x7f80_0000,
        (0x1f, _) => sign | 0x7fc0_0000 | (mantissa << 13),
        _ => sign | ((exponent + 112) << 23) | (mantissa << 13),
    };
    f32::from_bits(bits)
}

/// The packed word's NDC vector `(x, y)`.
pub fn unpack(packed: u32, y_mask: YMask) -> Vec2 {
    let x = f16_to_f32((packed >> 16) as u16);
    let y = f16_to_f32((packed & y_mask.bits()) as u16);
    Vec2::new(x, y)
}

/// Pixels per NDC unit at `render`: `(−w/2, +h/2)`.
pub fn ndc_to_pixels(render: Size) -> Vec2 {
    Vec2::new(-(render.w as f32) / 2.0, render.h as f32 / 2.0)
}

/// The decode shader's root constants, in `cbuffer` order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecodeConstants {
    /// NDC to pixels, policy multiplier included.
    pub scale: Vec2,
    /// Pixels added after scaling.
    pub offset_px: Vec2,
    /// [`YMask::bits`].
    pub y_mask: u32,
    /// Component clamp in pixels, 0 = off.
    pub clamp_px: f32,
}

/// The `cbuffer` fields: `(HLSL type, name, dwords)`. The single source of
/// both [`HLSL`]'s declaration and [`DecodeConstants::DWORDS`].
const FIELDS: [(&str, &str, u32); 4] = [
    ("float2", "Scale", 2),
    ("float2", "OffsetPx", 2),
    ("uint", "YMask", 1),
    ("float", "ClampPx", 1),
];

const fn field_dwords() -> u32 {
    let mut total = 0;
    let mut index = 0;
    while index < FIELDS.len() {
        total += FIELDS[index].2;
        index += 1;
    }
    total
}

impl DecodeConstants {
    /// Root constants the shader reads; the root signature declares exactly
    /// this many.
    pub const DWORDS: u32 = field_dwords();

    /// For `render` under `params`.
    pub fn new(render: Size, params: &DecodeParams) -> DecodeConstants {
        let base = ndc_to_pixels(render);
        DecodeConstants {
            scale: Vec2::new(base.x * params.scale.x, base.y * params.scale.y),
            offset_px: params.offset_px,
            y_mask: params.y_mask.bits(),
            clamp_px: params.clamp_px.max(0.0),
        }
    }

    /// The values as root constants, in [`FIELDS`] order.
    pub fn to_dwords(&self) -> [u32; Self::DWORDS as usize] {
        [
            self.scale.x.to_bits(),
            self.scale.y.to_bits(),
            self.offset_px.x.to_bits(),
            self.offset_px.y.to_bits(),
            self.y_mask,
            self.clamp_px.to_bits(),
        ]
    }

    /// What the shader computes for one packed word, in pixels.
    pub fn decode(&self, packed: u32) -> Vec2 {
        let x = f16_to_f32((packed >> 16) as u16);
        let y = f16_to_f32((packed & self.y_mask) as u16);
        let mut px = Vec2::new(
            x * self.scale.x + self.offset_px.x,
            y * self.scale.y + self.offset_px.y,
        );
        if self.clamp_px > 0.0 {
            px = Vec2::new(
                px.x.clamp(-self.clamp_px, self.clamp_px),
                px.y.clamp(-self.clamp_px, self.clamp_px),
            );
        }
        px
    }
}

/// The decode shader body; the `cbuffer` is prepended by [`hlsl`].
const BODY: &str = r#"
Texture2D<uint>     Source : register(t0);
RWTexture2D<float2> Target : register(u0);

[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    uint width, height;
    Target.GetDimensions(width, height);
    if (id.x >= width || id.y >= height)
        return;
    uint packed = Source[id.xy];
    float2 ndc = float2(f16tof32(packed >> 16), f16tof32(packed & YMask));
    float2 px = ndc * Scale + OffsetPx;
    if (ClampPx > 0.0)
        px = clamp(px, -ClampPx, ClampPx);
    Target[id.xy] = px;
}
"#;

/// Thread group edge of the decode shader.
pub const GROUP: u32 = 8;

/// The full HLSL source (`cs_5_0`, entry `main`).
pub fn hlsl() -> String {
    let mut source = String::from("cbuffer Decode : register(b0)\n{\n");
    for (ty, name, _) in FIELDS {
        source.push_str("    ");
        source.push_str(ty);
        source.push(' ');
        source.push_str(name);
        source.push_str(";\n");
    }
    source.push_str("};\n");
    source.push_str(BODY);
    source
}

#[cfg(test)]
mod tests {
    use super::*;

    fn half(value: f32) -> u16 {
        // Exact for the values used here (small dyadic fractions).
        let bits = value.to_bits();
        let sign = ((bits >> 16) & 0x8000) as u16;
        if value == 0.0 {
            return sign;
        }
        let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
        let mantissa = ((bits >> 13) & 0x3ff) as u16;
        sign | ((exponent as u16) << 10) | mantissa
    }

    #[test]
    fn halves_convert() {
        assert_eq!(f16_to_f32(0x3c00), 1.0);
        assert_eq!(f16_to_f32(0xc000), -2.0);
        assert_eq!(f16_to_f32(0x3555), 0.333_251_95);
        assert_eq!(f16_to_f32(0x0001), 5.960_464_5e-8);
        assert_eq!(f16_to_f32(0x7c00), f32::INFINITY);
        assert!(f16_to_f32(0x7e00).is_nan());
        assert_eq!(f16_to_f32(0x8000).to_bits(), (-0.0f32).to_bits());
        for value in [0.5f32, -0.25, 0.001_953_125, 3.0] {
            assert_eq!(f16_to_f32(half(value)), value);
        }
    }

    #[test]
    fn x_is_the_high_half_and_y_the_low() {
        let packed = (u32::from(half(0.5)) << 16) | u32::from(half(-0.25));
        assert_eq!(unpack(packed, YMask::Full), Vec2::new(0.5, -0.25));
    }

    #[test]
    fn the_y_mask_clears_only_the_low_bit_of_y() {
        let y = half(-0.25) | 1;
        let packed = (u32::from(half(0.5) | 1) << 16) | u32::from(y);
        let full = unpack(packed, YMask::Full);
        let masked = unpack(packed, YMask::DropFlagBit);
        assert_eq!(masked.x, full.x, "x keeps its low bit");
        assert_eq!(masked.y, -0.25);
        assert_ne!(full.y, -0.25);
    }

    #[test]
    fn decodes_to_render_pixels() {
        let render = Size::new(2560, 1440);
        let constants = DecodeConstants::new(render, &DecodeParams::default());
        // +0.5 NDC in x is a quarter screen to the left in pixels; +0.25 in
        // y a eighth down... in the engine's convention, (−w/2, +h/2).
        let packed = (u32::from(half(0.5)) << 16) | u32::from(half(0.25));
        assert_eq!(constants.decode(packed), Vec2::new(-640.0, 180.0));
        let clamped = DecodeConstants::new(
            render,
            &DecodeParams {
                clamp_px: 100.0,
                offset_px: Vec2::new(1.0, -1.0),
                ..DecodeParams::default()
            },
        );
        assert_eq!(clamped.decode(packed), Vec2::new(-100.0, 100.0));
    }

    #[test]
    fn the_cbuffer_and_the_root_constants_agree() {
        let source = hlsl();
        let start = source.find('{').unwrap();
        let end = source.find("};").unwrap();
        let dwords: u32 = source[start + 1..end]
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .map(|ty| match ty {
                "float" | "uint" | "int" => 1,
                "float2" | "uint2" => 2,
                "float3" => 3,
                "float4" => 4,
                other => panic!("unexpected cbuffer type {other}"),
            })
            .sum();
        assert_eq!(dwords, DecodeConstants::DWORDS);
        let constants = DecodeConstants::new(Size::new(4, 2), &DecodeParams::default());
        assert_eq!(
            constants.to_dwords().len(),
            DecodeConstants::DWORDS as usize
        );
        for (_, name, _) in FIELDS {
            assert!(BODY.contains(name), "{name} unused by the shader");
        }
    }
}
