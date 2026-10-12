//! DXBC input signatures (`ISGN`) and the fullscreen vertex shader that
//! feeds a given pixel shader.
//!
//! The HUD-less conversion reuses the engine's own `System_CopyToBackBuffer`
//! pixel shader (its output encode is not in the executable, `ReFramePresent`
//! E6). A pixel shader links only to a vertex shader whose outputs match its
//! inputs register for register, so the vertex shader is generated from the
//! pixel shader's `ISGN`: `SV_Position` from a fullscreen triangle, every
//! other float input from the texture coordinate `(u, v, 0, 1)` (top-left
//! origin, `v` down), integer inputs zero. Inputs the rasteriser generates
//! (`SV_IsFrontFace`, `SV_SampleIndex`, `SV_PrimitiveID`) are not outputs.

use std::fmt::Write as _;

use crate::dxbc::{find_chunk, DxbcError};

/// `D3D_NAME_POSITION`.
pub const SV_POSITION: u32 = 1;
/// `D3D_NAME_PRIMITIVE_ID`.
pub const SV_PRIMITIVE_ID: u32 = 7;
/// `D3D_NAME_IS_FRONT_FACE`.
pub const SV_IS_FRONT_FACE: u32 = 9;
/// `D3D_NAME_SAMPLE_INDEX`.
pub const SV_SAMPLE_INDEX: u32 = 10;

/// `D3D_REGISTER_COMPONENT_TYPE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentType {
    /// `uint`.
    Uint,
    /// `int`.
    Sint,
    /// `float`.
    Float,
    /// Anything else, raw.
    Other(u32),
}

impl ComponentType {
    fn decode(raw: u32) -> ComponentType {
        match raw {
            1 => ComponentType::Uint,
            2 => ComponentType::Sint,
            3 => ComponentType::Float,
            other => ComponentType::Other(other),
        }
    }
}

/// One element of an input signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    /// Semantic name (`TEXCOORD`, `SV_Position`).
    pub name: String,
    /// Semantic index.
    pub index: u32,
    /// `D3D_NAME` system value (0 for none).
    pub system_value: u32,
    /// Component type.
    pub component: ComponentType,
    /// Register.
    pub register: u32,
    /// Component mask (`0b0011` = `.xy`).
    pub mask: u8,
}

impl Element {
    /// Components declared (the mask's population).
    pub fn width(&self) -> u32 {
        (self.mask & 0xf).count_ones()
    }
}

/// Why a signature could not be read or a shader not generated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureError {
    /// The container itself.
    Dxbc(DxbcError),
    /// No `ISGN` chunk (`ISG1`, shader model 5.1, is not read).
    Missing,
    /// The chunk is inconsistent.
    Malformed(&'static str),
    /// An input the generator cannot feed; the text names it.
    Unsupported(String),
}

impl std::fmt::Display for SignatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignatureError::Dxbc(error) => error.fmt(f),
            SignatureError::Missing => f.write_str("no ISGN chunk"),
            SignatureError::Malformed(what) => write!(f, "ISGN malformed: {what}"),
            SignatureError::Unsupported(what) => write!(f, "unsupported input {what}"),
        }
    }
}

impl std::error::Error for SignatureError {}

/// More elements than any real signature has.
const MAX_ELEMENTS: usize = 32;
/// Bytes per `ISGN` element.
const ELEMENT_SIZE: usize = 24;

/// The input signature of a DXBC shader.
pub fn input_signature(bytecode: &[u8]) -> Result<Vec<Element>, SignatureError> {
    let chunk = find_chunk(bytecode, *b"ISGN")
        .map_err(SignatureError::Dxbc)?
        .ok_or(SignatureError::Missing)?;
    let at = |offset: usize| -> Result<u32, SignatureError> {
        let bytes = chunk
            .get(offset..offset.saturating_add(4))
            .ok_or(SignatureError::Malformed("truncated"))?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };
    let count = at(0)? as usize;
    let table = at(4)? as usize;
    if count > MAX_ELEMENTS {
        return Err(SignatureError::Malformed("element count"));
    }
    let mut elements = Vec::with_capacity(count);
    for slot in 0..count {
        let entry = table + slot * ELEMENT_SIZE;
        let name_at = at(entry)? as usize;
        let name = chunk
            .get(name_at..)
            .and_then(|tail| {
                let end = tail.iter().take(64).position(|&b| b == 0)?;
                std::str::from_utf8(&tail[..end]).ok()
            })
            .ok_or(SignatureError::Malformed("name"))?
            .to_owned();
        let mask = *chunk
            .get(entry + 20)
            .ok_or(SignatureError::Malformed("mask"))?;
        elements.push(Element {
            name,
            index: at(entry + 4)?,
            system_value: at(entry + 8)?,
            component: ComponentType::decode(at(entry + 12)?),
            register: at(entry + 16)?,
            mask,
        });
    }
    Ok(elements)
}

/// HLSL (`vs_5_0`, entry `main`, `SV_VertexID` only, draw 3 vertices) for a
/// fullscreen triangle whose outputs link to a pixel shader with `inputs`.
pub fn fullscreen_vs(inputs: &[Element]) -> Result<String, SignatureError> {
    let mut ordered: Vec<&Element> = inputs
        .iter()
        .filter(|e| {
            !matches!(
                e.system_value,
                SV_PRIMITIVE_ID | SV_IS_FRONT_FACE | SV_SAMPLE_INDEX
            )
        })
        .collect();
    ordered.sort_by_key(|e| (e.register, e.mask.trailing_zeros()));
    if !ordered.iter().any(|e| e.system_value == SV_POSITION) {
        return Err(SignatureError::Unsupported(
            "no SV_Position: the rasteriser needs one".into(),
        ));
    }
    let mut fields = String::new();
    let mut body = String::new();
    for (slot, element) in ordered.iter().enumerate() {
        let width = element.width();
        if !(1..=4).contains(&width) {
            return Err(SignatureError::Unsupported(format!(
                "{}{} with mask {:#06b}",
                element.name, element.index, element.mask
            )));
        }
        if !element
            .name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || element.name.is_empty()
        {
            return Err(SignatureError::Unsupported(format!(
                "semantic {:?}",
                element.name
            )));
        }
        let suffix = if width == 1 {
            String::new()
        } else {
            width.to_string()
        };
        match (element.system_value, element.component) {
            (SV_POSITION, _) => {
                let _ = writeln!(fields, "    float4 o{slot} : SV_Position;");
                let _ = writeln!(
                    body,
                    "    o.o{slot} = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);"
                );
            }
            (0, ComponentType::Float) => {
                let _ = writeln!(
                    fields,
                    "    float{suffix} o{slot} : {}{};",
                    element.name, element.index
                );
                let swizzle = &"xyzw"[..width as usize];
                let _ = writeln!(body, "    o.o{slot} = float4(uv, 0.0, 1.0).{swizzle};");
            }
            (0, kind @ (ComponentType::Uint | ComponentType::Sint)) => {
                let base = if kind == ComponentType::Uint {
                    "uint"
                } else {
                    "int"
                };
                let _ = writeln!(
                    fields,
                    "    nointerpolation {base}{suffix} o{slot} : {}{};",
                    element.name, element.index
                );
                let _ = writeln!(body, "    o.o{slot} = 0;");
            }
            (system, kind) => {
                return Err(SignatureError::Unsupported(format!(
                    "{}{} (system value {system}, {kind:?})",
                    element.name, element.index
                )))
            }
        }
    }
    Ok(format!(
        "struct Out\n{{\n{fields}}};\n\nOut main(uint id : SV_VertexID)\n{{\n    \
         float2 uv = float2((id << 1) & 2, id & 2);\n    Out o;\n{body}    return o;\n}}\n"
    ))
}

#[cfg(test)]
pub(crate) mod fixture {
    /// `(name, index, system value, component type, register, mask)`.
    pub type FixtureElement<'a> = (&'a str, u32, u32, u32, u32, u8);

    /// A container holding only an `ISGN` chunk with `elements`.
    pub fn with_isgn(elements: &[FixtureElement<'_>]) -> Vec<u8> {
        let table = 8usize;
        let pool = table + elements.len() * 24;
        let mut chunk = vec![0u8; pool];
        let mut strings = Vec::new();
        chunk[0..4].copy_from_slice(&(elements.len() as u32).to_le_bytes());
        chunk[4..8].copy_from_slice(&(table as u32).to_le_bytes());
        for (slot, (name, index, system, component, register, mask)) in elements.iter().enumerate()
        {
            let entry = table + slot * 24;
            let name_at = (pool + strings.len()) as u32;
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);
            for (offset, value) in [
                (0, name_at),
                (4, *index),
                (8, *system),
                (12, *component),
                (16, *register),
            ] {
                chunk[entry + offset..entry + offset + 4].copy_from_slice(&value.to_le_bytes());
            }
            chunk[entry + 20] = *mask;
            chunk[entry + 21] = *mask;
        }
        chunk.extend_from_slice(&strings);
        let header = 0x20 + 4;
        let mut out = Vec::new();
        out.extend_from_slice(b"DXBC");
        out.extend_from_slice(&[0; 16]);
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&((header + 8 + chunk.len()) as u32).to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(header as u32).to_le_bytes());
        out.extend_from_slice(b"ISGN");
        out.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        out.extend_from_slice(&chunk);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::with_isgn;
    use super::*;

    fn copy_to_backbuffer_like() -> Vec<u8> {
        with_isgn(&[
            ("SV_Position", 0, 1, 3, 0, 0b1111),
            ("TEXCOORD", 0, 0, 3, 1, 0b0011),
        ])
    }

    #[test]
    fn reads_elements() {
        let elements = input_signature(&copy_to_backbuffer_like()).unwrap();
        assert_eq!(elements.len(), 2);
        assert_eq!(elements[0].name, "SV_Position");
        assert_eq!(elements[0].system_value, SV_POSITION);
        assert_eq!(elements[1].name, "TEXCOORD");
        assert_eq!((elements[1].register, elements[1].width()), (1, 2));
        assert_eq!(elements[1].component, ComponentType::Float);
    }

    #[test]
    fn generates_matching_outputs_in_register_order() {
        // Declared out of order on purpose: outputs follow registers.
        let bytes = with_isgn(&[
            ("TEXCOORD", 1, 0, 3, 2, 0b1111),
            ("SV_Position", 0, 1, 3, 0, 0b1111),
            ("TEXCOORD", 0, 0, 3, 1, 0b0011),
            ("SV_IsFrontFace", 0, 9, 1, 3, 0b0001),
            ("BLENDINDICES", 0, 0, 1, 4, 0b0001),
        ]);
        let source = fullscreen_vs(&input_signature(&bytes).unwrap()).unwrap();
        let position = source.find("SV_Position").unwrap();
        let first = source.find("float2 o1 : TEXCOORD0").unwrap();
        let second = source.find("float4 o2 : TEXCOORD1").unwrap();
        assert!(position < first && first < second, "{source}");
        assert!(source.contains("nointerpolation uint o3 : BLENDINDICES0"));
        assert!(
            !source.contains("IsFrontFace"),
            "generated by the rasteriser"
        );
        assert!(source.contains("o.o1 = float4(uv, 0.0, 1.0).xy;"));
    }

    #[test]
    fn refuses_what_it_cannot_feed() {
        let no_position = with_isgn(&[("TEXCOORD", 0, 0, 3, 0, 0b0011)]);
        assert!(matches!(
            fullscreen_vs(&input_signature(&no_position).unwrap()),
            Err(SignatureError::Unsupported(_))
        ));
        let clip = with_isgn(&[
            ("SV_Position", 0, 1, 3, 0, 0b1111),
            ("SV_ClipDistance", 0, 2, 3, 1, 1),
        ]);
        assert!(fullscreen_vs(&input_signature(&clip).unwrap()).is_err());
        let odd = with_isgn(&[
            ("SV_Position", 0, 1, 3, 0, 0b1111),
            ("TEX-COORD", 0, 0, 3, 1, 3),
        ]);
        assert!(fullscreen_vs(&input_signature(&odd).unwrap()).is_err());
    }

    #[test]
    fn never_panics_on_truncation() {
        let bytes = copy_to_backbuffer_like();
        for len in 0..bytes.len() {
            let _ = input_signature(&bytes[..len]);
        }
        assert_eq!(
            input_signature(b"nope"),
            Err(SignatureError::Dxbc(DxbcError::NotDxbc))
        );
        let mut renamed = bytes.clone();
        let at = renamed.windows(4).position(|w| w == b"ISGN").unwrap();
        renamed[at..at + 4].copy_from_slice(b"OSGN");
        assert_eq!(input_signature(&renamed), Err(SignatureError::Missing));
    }
}
