//! DXBC shader reflection (the `RDEF` chunk).
//!
//! MHW emits no PIX markers, so passes cannot be named from the command
//! stream; the shaders name themselves. MT Framework ships DXBC compiled by
//! fxc (shader model 5.0) and the reflection chunk survives in the shipped
//! bytecode: bound resource names and registers identify a pass and make its
//! descriptor table interpretable, and constant buffer variables turn a root
//! CBV from an anonymous blob into named fields.
//!
//! Everything is read with bounds checks from a byte slice; malformed input
//! is an error, never a panic.

use std::fmt;

/// FNV-1a over a byte slice: the identity logged for a shader.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// Which register class a bound input uses (from `D3D_SHADER_INPUT_TYPE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputKind {
    /// `cbuffer` (`b` registers).
    ConstantBuffer,
    /// Texture, `tbuffer`, structured or byte-address buffer (`t`).
    ShaderResource,
    /// Sampler (`s`).
    Sampler,
    /// Any read-write resource (`u`).
    Unordered,
    /// A type this parser does not know.
    Other(u32),
}

impl InputKind {
    fn decode(raw: u32) -> InputKind {
        match raw {
            0 => InputKind::ConstantBuffer,
            1 | 2 | 5 | 7 => InputKind::ShaderResource,
            3 => InputKind::Sampler,
            4 | 6 | 8..=11 => InputKind::Unordered,
            other => InputKind::Other(other),
        }
    }

    /// HLSL register prefix.
    pub fn prefix(self) -> char {
        match self {
            InputKind::ConstantBuffer => 'b',
            InputKind::ShaderResource => 't',
            InputKind::Sampler => 's',
            InputKind::Unordered => 'u',
            InputKind::Other(_) => '?',
        }
    }
}

/// One bound input of a shader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// Name in the HLSL source (`tBaseMap`, `CBViewProjection`).
    pub name: String,
    /// Register class.
    pub kind: InputKind,
    /// First register.
    pub register: u32,
    /// Registers used (array size).
    pub count: u32,
    /// Register space (always 0 before shader model 5.1).
    pub space: u32,
}

impl fmt::Display for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}:{}", self.kind.prefix(), self.register, self.name)
    }
}

/// One field of a constant buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variable {
    /// Field name (`fProj`).
    pub name: String,
    /// Byte offset in the buffer.
    pub offset: u32,
    /// Size in bytes.
    pub size: u32,
}

/// A constant buffer declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstantBuffer {
    /// Name, equal to its binding's name.
    pub name: String,
    /// Declared size in bytes.
    pub size: u32,
    /// Fields in declaration order.
    pub variables: Vec<Variable>,
}

impl ConstantBuffer {
    /// The field called `name`.
    pub fn variable(&self, name: &str) -> Option<&Variable> {
        self.variables.iter().find(|v| v.name == name)
    }
}

/// Shader stage, from the program type in `RDEF`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Pixel shader.
    Pixel,
    /// Vertex shader.
    Vertex,
    /// Geometry shader.
    Geometry,
    /// Hull shader.
    Hull,
    /// Domain shader.
    Domain,
    /// Compute shader.
    Compute,
    /// Anything else, raw program type.
    Unknown(u16),
}

impl Stage {
    fn decode(raw: u16) -> Stage {
        match raw {
            0xFFFF => Stage::Pixel,
            0xFFFE => Stage::Vertex,
            0x4753 => Stage::Geometry,
            0x4853 => Stage::Hull,
            0x4453 => Stage::Domain,
            0x4353 => Stage::Compute,
            other => Stage::Unknown(other),
        }
    }
}

/// What the reflection chunk says about a shader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reflection {
    /// Stage.
    pub stage: Stage,
    /// Shader model `(major, minor)`.
    pub model: (u8, u8),
    /// Bound inputs, in the chunk's order.
    pub bindings: Vec<Binding>,
    /// Constant buffer declarations (`cbuffer` only).
    pub buffers: Vec<ConstantBuffer>,
}

impl Reflection {
    /// The input called `name` of class `kind`.
    pub fn binding(&self, kind: InputKind, name: &str) -> Option<&Binding> {
        self.bindings
            .iter()
            .find(|b| b.kind == kind && b.name == name)
    }

    /// Whether every `(kind, name)` is bound.
    pub fn binds_all(&self, wanted: &[(InputKind, &str)]) -> bool {
        wanted
            .iter()
            .all(|(kind, name)| self.binding(*kind, name).is_some())
    }

    /// The constant buffer called `name`.
    pub fn buffer(&self, name: &str) -> Option<&ConstantBuffer> {
        self.buffers.iter().find(|b| b.name == name)
    }

    /// The constant buffer bound at `b{register}` in space 0.
    pub fn buffer_at(&self, register: u32) -> Option<&ConstantBuffer> {
        let binding = self.bindings.iter().find(|b| {
            b.kind == InputKind::ConstantBuffer && b.register == register && b.space == 0
        })?;
        self.buffer(&binding.name)
    }

    /// `t0:tBaseMap u0:rwOutput b0:CBViewProjection …`, for the log.
    pub fn summary(&self) -> String {
        let listed: Vec<String> = self.bindings.iter().map(Binding::to_string).collect();
        listed.join(" ")
    }
}

/// Why bytecode could not be reflected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DxbcError {
    /// No `DXBC` magic.
    NotDxbc,
    /// The container header or chunk table runs past the end.
    Truncated,
    /// No `RDEF` chunk (stripped reflection).
    NoReflection,
    /// The `RDEF` chunk is inconsistent; the text says where.
    Malformed(&'static str),
}

impl fmt::Display for DxbcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DxbcError::NotDxbc => f.write_str("not a DXBC container"),
            DxbcError::Truncated => f.write_str("container truncated"),
            DxbcError::NoReflection => f.write_str("no RDEF chunk"),
            DxbcError::Malformed(what) => write!(f, "RDEF malformed: {what}"),
        }
    }
}

impl std::error::Error for DxbcError {}

/// More chunks than any real container has; bounds a corrupt count.
const MAX_CHUNKS: usize = 64;
/// More bound inputs / buffers / fields than any real shader has.
const MAX_BINDINGS: usize = 1024;
const MAX_BUFFERS: usize = 64;
const MAX_VARIABLES: usize = 1024;

/// Reflect a DXBC container.
pub fn reflect(bytecode: &[u8]) -> Result<Reflection, DxbcError> {
    let chunk = find_chunk(bytecode, *b"RDEF")?.ok_or(DxbcError::NoReflection)?;
    parse_rdef(chunk)
}

/// The payload of the first chunk with `fourcc`.
pub fn find_chunk(bytecode: &[u8], fourcc: [u8; 4]) -> Result<Option<&[u8]>, DxbcError> {
    if bytecode.get(0..4) != Some(b"DXBC".as_slice()) {
        return Err(DxbcError::NotDxbc);
    }
    // Header: magic, 16-byte digest, version, total size, chunk count, then
    // one u32 offset per chunk.
    let count = u32_at(bytecode, 0x1c).ok_or(DxbcError::Truncated)? as usize;
    if count > MAX_CHUNKS {
        return Err(DxbcError::Truncated);
    }
    for index in 0..count {
        let offset = u32_at(bytecode, 0x20 + index * 4).ok_or(DxbcError::Truncated)? as usize;
        let tag = bytecode
            .get(offset..offset.saturating_add(4))
            .ok_or(DxbcError::Truncated)?;
        let size = u32_at(bytecode, offset + 4).ok_or(DxbcError::Truncated)? as usize;
        if tag == fourcc {
            let start = offset + 8;
            let payload = bytecode
                .get(start..start.saturating_add(size))
                .ok_or(DxbcError::Truncated)?;
            return Ok(Some(payload));
        }
    }
    Ok(None)
}

fn parse_rdef(chunk: &[u8]) -> Result<Reflection, DxbcError> {
    let field = |at: usize| u32_at(chunk, at).ok_or(DxbcError::Malformed("header"));
    let buffer_count = field(0)? as usize;
    let buffer_table = field(4)? as usize;
    let binding_count = field(8)? as usize;
    let binding_table = field(12)? as usize;
    let minor = *chunk.get(16).ok_or(DxbcError::Malformed("header"))?;
    let major = *chunk.get(17).ok_or(DxbcError::Malformed("header"))?;
    let program = u16_at(chunk, 18).ok_or(DxbcError::Malformed("header"))?;
    if binding_count > MAX_BINDINGS || buffer_count > MAX_BUFFERS {
        return Err(DxbcError::Malformed("counts"));
    }
    // Shader model 5.1 adds register space and range id to each binding;
    // shader model 5 grew the variable entry from six fields to ten.
    let binding_size = if (major, minor) >= (5, 1) { 40 } else { 32 };
    let variable_size = if major >= 5 { 40 } else { 24 };

    let mut bindings = Vec::with_capacity(binding_count);
    for index in 0..binding_count {
        let entry = binding_table + index * binding_size;
        let at =
            |offset: usize| u32_at(chunk, entry + offset).ok_or(DxbcError::Malformed("binding"));
        bindings.push(Binding {
            name: cstr_at(chunk, at(0)? as usize).ok_or(DxbcError::Malformed("binding name"))?,
            kind: InputKind::decode(at(4)?),
            register: at(20)?,
            count: at(24)?,
            space: if binding_size == 40 { at(32)? } else { 0 },
        });
    }

    let mut buffers = Vec::with_capacity(buffer_count);
    for index in 0..buffer_count {
        let entry = buffer_table + index * 24;
        let at =
            |offset: usize| u32_at(chunk, entry + offset).ok_or(DxbcError::Malformed("buffer"));
        let name = cstr_at(chunk, at(0)? as usize).ok_or(DxbcError::Malformed("buffer name"))?;
        let variable_count = at(4)? as usize;
        let variable_table = at(8)? as usize;
        let size = at(12)?;
        let kind = at(20)?;
        if variable_count > MAX_VARIABLES {
            return Err(DxbcError::Malformed("variable count"));
        }
        let mut variables = Vec::with_capacity(variable_count);
        for slot in 0..variable_count {
            let field = variable_table + slot * variable_size;
            let at = |offset: usize| {
                u32_at(chunk, field + offset).ok_or(DxbcError::Malformed("variable"))
            };
            variables.push(Variable {
                name: cstr_at(chunk, at(0)? as usize)
                    .ok_or(DxbcError::Malformed("variable name"))?,
                offset: at(4)?,
                size: at(8)?,
            });
        }
        // Only real `cbuffer`s (D3D_CT_CBUFFER); tbuffers and interface
        // tables are not constant buffers bound at `b` registers.
        if kind == 0 {
            buffers.push(ConstantBuffer {
                name,
                size,
                variables,
            });
        }
    }

    Ok(Reflection {
        stage: Stage::decode(program),
        model: (major, minor),
        bindings,
        buffers,
    })
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    let slice = bytes.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// A NUL-terminated UTF-8 string at `at`, at most 256 bytes long.
fn cstr_at(bytes: &[u8], at: usize) -> Option<String> {
    let tail = bytes.get(at..)?;
    let end = tail.iter().take(256).position(|&b| b == 0)?;
    std::str::from_utf8(&tail[..end]).ok().map(str::to_owned)
}

/// Hand-built DXBC containers for tests (this crate's and its users').
#[cfg(test)]
pub(crate) mod fixture {
    /// A binding to put into a fixture: `(name, D3D_SHADER_INPUT_TYPE, register)`.
    pub type FixtureBinding<'a> = (&'a str, u32, u32);
    /// A cbuffer: `(name, size, [(field, offset, size)])`.
    pub type FixtureBuffer<'a> = (&'a str, u32, &'a [(&'a str, u32, u32)]);

    /// A shader model 5.0 container with one `RDEF` chunk (after a dummy
    /// `SHEX` chunk, so the chunk table is walked).
    pub fn container(
        program: u16,
        bindings: &[FixtureBinding<'_>],
        buffers: &[FixtureBuffer<'_>],
    ) -> Vec<u8> {
        let rdef = rdef(program, bindings, buffers);
        let shex = [0u8; 12];
        let header = 0x20 + 2 * 4;
        let mut out = Vec::new();
        out.extend_from_slice(b"DXBC");
        out.extend_from_slice(&[0; 16]);
        out.extend_from_slice(&1u32.to_le_bytes());
        let total = header + 8 + shex.len() + 8 + rdef.len();
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(header as u32).to_le_bytes());
        out.extend_from_slice(&((header + 8 + shex.len()) as u32).to_le_bytes());
        out.extend_from_slice(b"SHEX");
        out.extend_from_slice(&(shex.len() as u32).to_le_bytes());
        out.extend_from_slice(&shex);
        out.extend_from_slice(b"RDEF");
        out.extend_from_slice(&(rdef.len() as u32).to_le_bytes());
        out.extend_from_slice(&rdef);
        out
    }

    fn rdef(
        program: u16,
        bindings: &[FixtureBinding<'_>],
        buffers: &[FixtureBuffer<'_>],
    ) -> Vec<u8> {
        // Layout: 28-byte header, buffer table (24 each), binding table (32
        // each), variable tables (40 each), then a string pool.
        let header = 28usize;
        let buffer_table = header;
        let binding_table = buffer_table + buffers.len() * 24;
        let mut variable_tables = Vec::new();
        let mut cursor = binding_table + bindings.len() * 32;
        for (_, _, fields) in buffers {
            variable_tables.push(cursor);
            cursor += fields.len() * 40;
        }
        let mut strings: Vec<u8> = Vec::new();
        let pool = cursor;
        let mut intern = |text: &str| {
            let at = pool + strings.len();
            strings.extend_from_slice(text.as_bytes());
            strings.push(0);
            at as u32
        };

        let mut out = vec![0u8; pool];
        let put = |out: &mut Vec<u8>, at: usize, value: u32| {
            out[at..at + 4].copy_from_slice(&value.to_le_bytes());
        };
        put(&mut out, 0, buffers.len() as u32);
        put(&mut out, 4, buffer_table as u32);
        put(&mut out, 8, bindings.len() as u32);
        put(&mut out, 12, binding_table as u32);
        out[16] = 0;
        out[17] = 5;
        out[18..20].copy_from_slice(&program.to_le_bytes());
        for (index, (name, size, fields)) in buffers.iter().enumerate() {
            let entry = buffer_table + index * 24;
            let name = intern(name);
            put(&mut out, entry, name);
            put(&mut out, entry + 4, fields.len() as u32);
            put(&mut out, entry + 8, variable_tables[index] as u32);
            put(&mut out, entry + 12, *size);
            for (slot, (field, offset, len)) in fields.iter().enumerate() {
                let at = variable_tables[index] + slot * 40;
                let field = intern(field);
                put(&mut out, at, field);
                put(&mut out, at + 4, *offset);
                put(&mut out, at + 8, *len);
            }
        }
        for (index, (name, kind, register)) in bindings.iter().enumerate() {
            let entry = binding_table + index * 32;
            let name = intern(name);
            put(&mut out, entry, name);
            put(&mut out, entry + 4, *kind);
            put(&mut out, entry + 20, *register);
            put(&mut out, entry + 24, 1);
        }
        out.extend_from_slice(&strings);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::container;
    use super::*;

    const COMPUTE: u16 = 0x4353;

    fn temporal() -> Vec<u8> {
        container(
            COMPUTE,
            &[
                ("CBViewProjection", 0, 0),
                ("tBaseMap", 2, 0),
                ("tGBuffer4", 2, 4),
                ("sbData", 5, 7),
                ("rwOutput", 4, 0),
                ("sLinear", 3, 1),
            ],
            &[(
                "CBViewProjection",
                1072,
                &[("fProj", 128, 64), ("fNoJitterProj", 800, 64)],
            )],
        )
    }

    #[test]
    fn reflects_bindings_buffers_and_stage() {
        let reflection = reflect(&temporal()).unwrap();
        assert_eq!(reflection.stage, Stage::Compute);
        assert_eq!(reflection.model, (5, 0));
        assert_eq!(
            reflection.summary(),
            "b0:CBViewProjection t0:tBaseMap t4:tGBuffer4 t7:sbData u0:rwOutput s1:sLinear"
        );
        let gbuffer = reflection
            .binding(InputKind::ShaderResource, "tGBuffer4")
            .unwrap();
        assert_eq!((gbuffer.register, gbuffer.count, gbuffer.space), (4, 1, 0));
        // A structured buffer is a `t` register, not a UAV.
        assert!(reflection
            .binding(InputKind::ShaderResource, "sbData")
            .is_some());
        let camera = reflection.buffer_at(0).unwrap();
        assert_eq!(camera.size, 1072);
        assert_eq!(camera.variable("fNoJitterProj").unwrap().offset, 800);
        assert!(reflection.buffer_at(1).is_none());
        assert!(reflection.binds_all(&[
            (InputKind::ShaderResource, "tBaseMap"),
            (InputKind::Unordered, "rwOutput")
        ]));
        assert!(!reflection.binds_all(&[(InputKind::Unordered, "tBaseMap")]));
    }

    #[test]
    fn rejects_what_is_not_reflectable() {
        assert_eq!(reflect(b"nope"), Err(DxbcError::NotDxbc));
        assert_eq!(reflect(b"DXBC"), Err(DxbcError::Truncated));
        let mut bytes = temporal();
        // Rename the RDEF chunk: no reflection left.
        let at = bytes.windows(4).position(|w| w == b"RDEF").unwrap();
        bytes[at..at + 4].copy_from_slice(b"STAT");
        assert_eq!(reflect(&bytes), Err(DxbcError::NoReflection));
    }

    #[test]
    fn never_panics_on_truncation() {
        let bytes = temporal();
        for len in 0..bytes.len() {
            let _ = reflect(&bytes[..len]);
        }
        // A corrupt chunk count is bounded.
        let mut bytes = temporal();
        bytes[0x1c..0x20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(reflect(&bytes), Err(DxbcError::Truncated));
    }

    #[test]
    fn decodes_input_types_to_register_classes() {
        let classes: Vec<char> = (0..13).map(|raw| InputKind::decode(raw).prefix()).collect();
        assert_eq!(classes.iter().collect::<String>(), "bttsututuuuu?");
    }

    #[test]
    fn hash_is_fnv1a() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
