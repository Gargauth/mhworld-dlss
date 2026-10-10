//! Shader bytecode reflection.
//!
//! MHW emits no PIX markers, so passes cannot be named from the command
//! stream. The shaders name themselves instead: MT Framework is a DX11-era
//! engine shipping DXBC compiled by fxc 10.0.10011.0, and the RDEF reflection
//! chunk survives in the shipped bytecode.
//!
//! Three things are read out of it, each needed for a different reason:
//!
//!   * **bound resources** identify the pass (`tBaseMap`, `rwOutput`, ...);
//!   * their **register slots** make a descriptor table interpretable -- a
//!     table arrives as one opaque base handle, and the only way to know the
//!     third descriptor is the depth buffer is to know it is bound at `t2`;
//!   * **constant buffer variables** turn a root CBV from an anonymous blob
//!     into named fields, which is what settles whether `fProj` differs from
//!     `fNoJitterProj` -- the sub-pixel jitter DLSS needs.

use std::fmt;

/// Engine symbols that mark a pipeline as the temporal pass.
const WANTED: &[&str] = &["TemporalAA", "TemporalUpsample", "NoJitter", "Halton"];

/// Shader input class, as encoded by `D3D_SHADER_INPUT_TYPE`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    ConstantBuffer,
    Texture,
    Sampler,
    Unordered,
    Other(u32),
}

impl Kind {
    fn decode(raw: u32) -> Self {
        match raw {
            0 | 1 => Kind::ConstantBuffer,
            2 => Kind::Texture,
            3 => Kind::Sampler,
            4..=11 => Kind::Unordered,
            other => Kind::Other(other),
        }
    }

    /// The HLSL register prefix, which is also how the slot is logged.
    pub fn prefix(self) -> char {
        match self {
            Kind::ConstantBuffer => 'b',
            Kind::Texture => 't',
            Kind::Sampler => 's',
            Kind::Unordered => 'u',
            Kind::Other(_) => '?',
        }
    }
}

pub struct Binding {
    pub name: String,
    pub kind: Kind,
    pub slot: u32,
    pub count: u32,
}

impl fmt::Display for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}:{}", self.kind.prefix(), self.slot, self.name)
    }
}

/// One field of a constant buffer, with its byte offset and size.
pub struct Variable {
    pub name: String,
    pub offset: u32,
    pub size: u32,
}

pub struct ConstantBuffer {
    pub name: String,
    pub size: u32,
    pub variables: Vec<Variable>,
}

#[derive(Default)]
pub struct Shader {
    pub chunks: Vec<String>,
    pub bindings: Vec<Binding>,
    pub buffers: Vec<ConstantBuffer>,
    /// Names recovered by raw scan when RDEF could not be parsed.
    pub fallback_names: Vec<String>,
    pub hits: Vec<&'static str>,
}

impl Shader {
    /// Resources of one class, ordered by register slot -- the order a
    /// descriptor table lays them out in.
    pub fn by_slot(&self, kind: Kind) -> Vec<&Binding> {
        let mut found: Vec<&Binding> = self.bindings.iter().filter(|b| b.kind == kind).collect();
        found.sort_by_key(|b| b.slot);
        found
    }

    /// The constant buffer bound at register `b{slot}`, if the shader declares
    /// its contents.
    pub fn buffer_at(&self, slot: u32) -> Option<&ConstantBuffer> {
        let name = self
            .bindings
            .iter()
            .find(|b| b.kind == Kind::ConstantBuffer && b.slot == slot)
            .map(|b| b.name.as_str())?;
        self.buffers.iter().find(|buffer| buffer.name == name)
    }

    pub fn summary(&self) -> String {
        if self.bindings.is_empty() {
            return self.fallback_names.join(" ");
        }
        self.bindings
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Parse a DXBC container's reflection data.
///
/// # Safety
/// `data` must point to `len` readable bytes for the duration of the call.
pub unsafe fn inspect(data: *const u8, len: usize) -> Shader {
    if data.is_null() || len < 0x20 {
        return Shader::default();
    }
    let bytes = std::slice::from_raw_parts(data, len);
    let mut shader = Shader::default();

    let mut reflection: Option<&[u8]> = None;
    if &bytes[..4] == b"DXBC" {
        // Header: magic, 16-byte digest, version, total size, chunk count,
        // then one u32 offset per chunk.
        let count = u32_at(bytes, 0x1c).unwrap_or(0) as usize;
        for index in 0..count.min(32) {
            let Some(offset) = u32_at(bytes, 0x20 + index * 4).map(|v| v as usize) else {
                break;
            };
            if offset + 8 > len {
                break;
            }
            let fourcc = String::from_utf8_lossy(&bytes[offset..offset + 4]).to_string();
            let size = u32_at(bytes, offset + 4).unwrap_or(0) as usize;
            let end = (offset + 8 + size).min(len);
            if fourcc == "RDEF" && reflection.is_none() {
                reflection = Some(&bytes[offset + 8..end]);
            }
            shader.chunks.push(fourcc);
        }
    }

    match reflection {
        Some(chunk) => match parse_bindings(chunk) {
            Some(bindings) => {
                shader.bindings = bindings;
                shader.buffers = parse_buffers(chunk).unwrap_or_default();
            }
            None => shader.fallback_names = ascii_runs(chunk),
        },
        None => shader.fallback_names = ascii_runs(bytes),
    }

    let haystack = shader.summary();
    shader.hits = WANTED
        .iter()
        .copied()
        .filter(|needle| haystack.contains(needle))
        .collect();
    shader
}

/// Decode the RDEF bound-resource table.
///
/// Layout (SM5.0): constant buffer count and offset, bound resource count and
/// offset, version, flags, creator offset. Offsets are relative to the start
/// of the chunk payload. Each bound resource is eight u32 fields: name offset,
/// type, return type, dimension, sample count, bind point, bind count, flags.
fn parse_bindings(chunk: &[u8]) -> Option<Vec<Binding>> {
    let count = u32_at(chunk, 8)? as usize;
    let table = u32_at(chunk, 12)? as usize;
    if count == 0 || count > 256 {
        return None;
    }

    let mut bindings = Vec::with_capacity(count);
    for index in 0..count {
        let entry = table + index * 32;
        let name = cstr_at(chunk, u32_at(chunk, entry)? as usize)?;
        bindings.push(Binding {
            name,
            kind: Kind::decode(u32_at(chunk, entry + 4)?),
            slot: u32_at(chunk, entry + 20)?,
            count: u32_at(chunk, entry + 24)?,
        });
    }
    Some(bindings)
}

/// Decode the RDEF constant buffer table.
///
/// Each buffer is six u32 fields: name offset, variable count, variable
/// offset, size, flags, type. Each variable is ten u32 fields in SM5.0: name
/// offset, start offset, size, flags, type offset, default value offset, and
/// four texture/sampler fields.
fn parse_buffers(chunk: &[u8]) -> Option<Vec<ConstantBuffer>> {
    let count = u32_at(chunk, 0)? as usize;
    let table = u32_at(chunk, 4)? as usize;
    if count > 64 {
        return None;
    }

    let mut buffers = Vec::with_capacity(count);
    for index in 0..count {
        let entry = table + index * 24;
        let name = cstr_at(chunk, u32_at(chunk, entry)? as usize)?;
        let variable_count = u32_at(chunk, entry + 4)? as usize;
        let variable_table = u32_at(chunk, entry + 8)? as usize;
        let size = u32_at(chunk, entry + 12)?;

        let mut variables = Vec::with_capacity(variable_count.min(512));
        for slot in 0..variable_count.min(512) {
            let field = variable_table + slot * 40;
            let Some(name_offset) = u32_at(chunk, field) else {
                break;
            };
            let Some(variable_name) = cstr_at(chunk, name_offset as usize) else {
                break;
            };
            variables.push(Variable {
                name: variable_name,
                offset: u32_at(chunk, field + 4)?,
                size: u32_at(chunk, field + 8)?,
            });
        }
        buffers.push(ConstantBuffer {
            name,
            size,
            variables,
        });
    }
    Some(buffers)
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn cstr_at(bytes: &[u8], at: usize) -> Option<String> {
    let tail = bytes.get(at..)?;
    let end = tail.iter().position(|&b| b == 0)?;
    std::str::from_utf8(&tail[..end]).ok().map(str::to_owned)
}

/// Identifier-like printable runs, deduplicated, used only when RDEF is absent.
fn ascii_runs(bytes: &[u8]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut current = String::new();

    for &byte in bytes.iter().chain(std::iter::once(&0)) {
        let c = byte as char;
        if c.is_ascii_alphanumeric() || c == '_' {
            current.push(c);
        } else {
            if current.len() >= 4 && !found.contains(&current) {
                found.push(current.clone());
            }
            current.clear();
        }
    }
    found
}
