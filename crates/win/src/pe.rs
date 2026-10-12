//! PE32+ parsing over any byte source addressed by RVA.
//!
//! Pure: nothing here touches the process. The live module (`Module`) and the
//! host tests (a sparse copy of `MonsterHunterWorld.exe`) both implement
//! [`RvaSource`], so the parser that patches the game's import tables is the
//! one the tests exercise.
//!
//! Every field read comes out of headers the code does not control, so every
//! read is bounds-checked by the source and every table walk is capped: a
//! malformed image is an error, never a fault or an endless loop.

use std::fmt;

/// A byte source laid out like a mapped image: offset = RVA.
pub trait RvaSource {
    /// The longest contiguous readable run of bytes starting at `rva`, at most
    /// `max` long; `None` when the byte at `rva` itself is not available.
    fn bytes_at(&self, rva: u32, max: usize) -> Option<&[u8]>;

    /// Exactly `len` bytes at `rva`, or `None`.
    fn bytes(&self, rva: u32, len: usize) -> Option<&[u8]> {
        self.bytes_at(rva, len).filter(|run| run.len() == len)
    }
}

/// A whole mapped image in one slice (offset 0 = image base).
impl RvaSource for [u8] {
    fn bytes_at(&self, rva: u32, max: usize) -> Option<&[u8]> {
        let start = usize::try_from(rva).ok()?;
        let rest = self.get(start..).filter(|rest| !rest.is_empty())?;
        Some(&rest[..rest.len().min(max)])
    }
}

/// Why an image could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeError {
    /// A structure the parser needed lies (partly) outside the source.
    Truncated {
        /// What was being read.
        what: &'static str,
        /// Its RVA.
        rva: u32,
    },
    /// The DOS or NT signature is wrong.
    BadSignature,
    /// Not an x86-64 PE32+ image.
    NotPe32Plus {
        /// `FileHeader.Machine`.
        machine: u16,
        /// `OptionalHeader.Magic`.
        magic: u16,
    },
    /// A table is longer than any sane image would make it.
    Unterminated {
        /// Which table.
        what: &'static str,
    },
    /// The matching import descriptor has no name table, so its slots cannot
    /// be told apart in a loaded image (the IAT holds addresses by then).
    NoNameTable {
        /// The DLL named by the descriptor.
        dll: String,
    },
    /// A delay-import descriptor uses the obsolete VA-based layout
    /// (`Attributes` bit 0 clear); only RVA-based descriptors are supported.
    VaBasedDelayImport {
        /// The DLL named by the descriptor.
        dll: String,
    },
}

impl fmt::Display for PeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PeError::Truncated { what, rva } => write!(f, "{what} at rva {rva:#x} is unreadable"),
            PeError::BadSignature => f.write_str("not a PE image (bad MZ/PE signature)"),
            PeError::NotPe32Plus { machine, magic } => {
                write!(
                    f,
                    "not an x64 PE32+ image (machine {machine:#06x}, magic {magic:#06x})"
                )
            }
            PeError::Unterminated { what } => write!(f, "{what} has no terminator"),
            PeError::NoNameTable { dll } => write!(f, "imports of {dll} have no name table"),
            PeError::VaBasedDelayImport { dll } => {
                write!(
                    f,
                    "delay imports of {dll} use the unsupported VA-based layout"
                )
            }
        }
    }
}

impl std::error::Error for PeError {}

/// `IMAGE_SCN_MEM_EXECUTE`.
const SCN_MEM_EXECUTE: u32 = 0x2000_0000;
/// `IMAGE_SCN_CNT_CODE`.
const SCN_CNT_CODE: u32 = 0x0000_0020;

/// One entry of the section table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section {
    /// Raw name, NUL-padded.
    pub name: [u8; 8],
    /// `VirtualAddress`.
    pub rva: u32,
    /// `VirtualSize` (the mapped extent; see [`Section::mapped_len`]).
    pub virtual_size: u32,
    /// `PointerToRawData`.
    pub raw_offset: u32,
    /// `SizeOfRawData`.
    pub raw_size: u32,
    /// `Characteristics`.
    pub characteristics: u32,
}

impl Section {
    /// The name without padding; empty when it is not UTF-8.
    pub fn name(&self) -> &str {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(8);
        std::str::from_utf8(&self.name[..end]).unwrap_or("")
    }

    /// Bytes the section occupies once mapped. A zero `VirtualSize` (some
    /// linkers) means the raw size.
    pub fn mapped_len(&self) -> u32 {
        if self.virtual_size == 0 {
            self.raw_size
        } else {
            self.virtual_size
        }
    }

    /// Whether `rva` falls inside the mapped section.
    pub fn contains(&self, rva: u32) -> bool {
        rva >= self.rva && u64::from(rva) < u64::from(self.rva) + u64::from(self.mapped_len())
    }

    /// Whether the section holds code (executable or `CNT_CODE`).
    pub fn is_code(&self) -> bool {
        self.characteristics & (SCN_MEM_EXECUTE | SCN_CNT_CODE) != 0
    }
}

/// One data-directory entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DataDirectory {
    /// RVA of the table.
    pub rva: u32,
    /// Its size in bytes.
    pub size: u32,
}

/// Data-directory indices used by this crate.
pub mod dir {
    /// `IMAGE_DIRECTORY_ENTRY_EXPORT`.
    pub const EXPORT: usize = 0;
    /// `IMAGE_DIRECTORY_ENTRY_IMPORT`.
    pub const IMPORT: usize = 1;
    /// `IMAGE_DIRECTORY_ENTRY_IAT`.
    pub const IAT: usize = 12;
    /// `IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT`.
    pub const DELAY_IMPORT: usize = 13;
}

/// The parsed headers of a PE32+ image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Headers {
    /// `FileHeader.TimeDateStamp`: the build identity the gate logs.
    pub timestamp: u32,
    /// `OptionalHeader.ImageBase` (preferred base).
    pub image_base: u64,
    /// `OptionalHeader.SizeOfImage`.
    pub size_of_image: u32,
    /// `OptionalHeader.SizeOfHeaders`.
    pub size_of_headers: u32,
    /// `OptionalHeader.AddressOfEntryPoint`.
    pub entry_point: u32,
    /// The section table, in file order.
    pub sections: Vec<Section>,
    /// The data directories (missing trailing ones are zero).
    pub directories: [DataDirectory; 16],
}

impl Headers {
    /// The first section called `name`.
    pub fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name() == name)
    }

    /// The section containing `rva`.
    pub fn section_containing(&self, rva: u32) -> Option<&Section> {
        self.sections.iter().find(|s| s.contains(rva))
    }

    /// Data directory `index` (see [`dir`]); zero when absent.
    pub fn directory(&self, index: usize) -> DataDirectory {
        self.directories.get(index).copied().unwrap_or_default()
    }
}

const MACHINE_AMD64: u16 = 0x8664;
const MAGIC_PE32_PLUS: u16 = 0x20b;
/// Section tables longer than this are corrupt (the format caps it at 96).
const MAX_SECTIONS: u16 = 96;
/// Caps on table walks; the game has 189 static and ~400 delay imports.
const MAX_DESCRIPTORS: u32 = 4096;
const MAX_THUNKS: u32 = 65_536;
/// Longest DLL or function name accepted.
const MAX_NAME: usize = 512;

fn read_u16(src: &(impl RvaSource + ?Sized), rva: u32, what: &'static str) -> Result<u16, PeError> {
    let b = src.bytes(rva, 2).ok_or(PeError::Truncated { what, rva })?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn read_u32(src: &(impl RvaSource + ?Sized), rva: u32, what: &'static str) -> Result<u32, PeError> {
    let b = src.bytes(rva, 4).ok_or(PeError::Truncated { what, rva })?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn read_u64(src: &(impl RvaSource + ?Sized), rva: u32, what: &'static str) -> Result<u64, PeError> {
    let b = src.bytes(rva, 8).ok_or(PeError::Truncated { what, rva })?;
    let mut word = [0u8; 8];
    word.copy_from_slice(b);
    Ok(u64::from_le_bytes(word))
}

/// `rva + offset`, with overflow reported as truncation.
fn at(rva: u32, offset: u32, what: &'static str) -> Result<u32, PeError> {
    rva.checked_add(offset)
        .ok_or(PeError::Truncated { what, rva })
}

/// A NUL-terminated string at `rva`, at most [`MAX_NAME`] bytes, as raw
/// bytes (import names are ASCII; no UTF-8 requirement is imposed here).
pub fn c_str<'a>(
    src: &'a (impl RvaSource + ?Sized),
    rva: u32,
    what: &'static str,
) -> Result<&'a [u8], PeError> {
    let run = src
        .bytes_at(rva, MAX_NAME + 1)
        .ok_or(PeError::Truncated { what, rva })?;
    let end = run
        .iter()
        .position(|&b| b == 0)
        .ok_or(PeError::Truncated { what, rva })?;
    Ok(&run[..end])
}

/// Parse the DOS, NT and section headers.
pub fn parse_headers(src: &(impl RvaSource + ?Sized)) -> Result<Headers, PeError> {
    if src.bytes(0, 2) != Some(b"MZ") {
        return Err(PeError::BadSignature);
    }
    let nt = read_u32(src, 0x3c, "e_lfanew")?;
    if src.bytes(nt, 4) != Some(b"PE\0\0") {
        return Err(PeError::BadSignature);
    }
    let file = at(nt, 4, "file header")?;
    let machine = read_u16(src, file, "file header")?;
    let section_count = read_u16(src, at(file, 2, "file header")?, "file header")?;
    let timestamp = read_u32(src, at(file, 4, "file header")?, "file header")?;
    let optional_size = read_u16(src, at(file, 16, "file header")?, "file header")?;

    let optional = at(file, 20, "optional header")?;
    let magic = read_u16(src, optional, "optional header")?;
    if machine != MACHINE_AMD64 || magic != MAGIC_PE32_PLUS {
        return Err(PeError::NotPe32Plus { machine, magic });
    }
    let field = |offset| at(optional, offset, "optional header");
    let entry_point = read_u32(src, field(16)?, "optional header")?;
    let image_base = read_u64(src, field(24)?, "optional header")?;
    let size_of_image = read_u32(src, field(56)?, "optional header")?;
    let size_of_headers = read_u32(src, field(60)?, "optional header")?;
    let rva_count = read_u32(src, field(108)?, "optional header")?;

    let mut directories = [DataDirectory::default(); 16];
    // A directory past what the optional header declares, or past its end,
    // is absent rather than an error.
    let declared = (rva_count as usize).min(16);
    let fits = (usize::from(optional_size).saturating_sub(112)) / 8;
    for (index, entry) in directories.iter_mut().enumerate().take(declared.min(fits)) {
        let rva = field(112 + 8 * index as u32)?;
        entry.rva = read_u32(src, rva, "data directory")?;
        entry.size = read_u32(src, at(rva, 4, "data directory")?, "data directory")?;
    }

    if section_count > MAX_SECTIONS {
        return Err(PeError::Unterminated {
            what: "section table",
        });
    }
    let table = at(optional, u32::from(optional_size), "section table")?;
    let mut sections = Vec::with_capacity(usize::from(section_count));
    for index in 0..u32::from(section_count) {
        let entry = at(table, 40 * index, "section table")?;
        let raw = src.bytes(entry, 40).ok_or(PeError::Truncated {
            what: "section table",
            rva: entry,
        })?;
        let word = |o: usize| u32::from_le_bytes([raw[o], raw[o + 1], raw[o + 2], raw[o + 3]]);
        let mut name = [0u8; 8];
        name.copy_from_slice(&raw[..8]);
        sections.push(Section {
            name,
            virtual_size: word(8),
            rva: word(12),
            raw_size: word(16),
            raw_offset: word(20),
            characteristics: word(36),
        });
    }

    Ok(Headers {
        timestamp,
        image_base,
        size_of_image,
        size_of_headers,
        entry_point,
        sections,
        directories,
    })
}

/// Which function of a DLL an import refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportRef<'a> {
    /// Imported by name (case-sensitive, as the linker wrote it).
    Name(&'a str),
    /// Imported by ordinal. Matches only entries the image imports *by
    /// ordinal*; a by-name entry for the same export is not recognised (that
    /// would need the exporting DLL's export table).
    Ordinal(u16),
}

impl fmt::Display for ImportRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImportRef::Name(name) => f.write_str(name),
            ImportRef::Ordinal(ordinal) => write!(f, "#{ordinal}"),
        }
    }
}

/// Which import table a slot belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportTable {
    /// The regular import table, bound by the loader before the image runs.
    Static,
    /// A delay-import table: the slot initially holds a thunk into the
    /// importing image, which calls `__delayLoadHelper2` on first use and
    /// overwrites the slot with the resolved address.
    Delay,
}

/// One IAT slot that imports the requested function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportSlot {
    /// Static or delay.
    pub table: ImportTable,
    /// RVA of the pointer-sized slot the code calls through.
    pub slot_rva: u32,
    /// Delay imports: RVA of the descriptor's module-handle slot.
    pub module_handle_rva: Option<u32>,
}

/// `IMAGE_ORDINAL_FLAG64`.
const ORDINAL_FLAG: u64 = 1 << 63;

/// Whether the name-table entry `thunk` imports `import`.
fn thunk_matches(
    src: &(impl RvaSource + ?Sized),
    thunk: u64,
    import: ImportRef<'_>,
) -> Result<bool, PeError> {
    if thunk & ORDINAL_FLAG != 0 {
        return Ok(matches!(import, ImportRef::Ordinal(o) if u64::from(o) == thunk & 0xffff));
    }
    let ImportRef::Name(wanted) = import else {
        return Ok(false);
    };
    // IMAGE_IMPORT_BY_NAME: Hint u16, then the name. Bits 31..62 are zero in
    // a valid entry; masking keeps a corrupt one inside the u32 RVA space.
    let by_name = (thunk & 0x7fff_ffff) as u32;
    let name = c_str(src, at(by_name, 2, "import name")?, "import name")?;
    Ok(name == wanted.as_bytes())
}

/// Walk a name table (`names`) in parallel with its IAT (`slots`), pushing
/// the IAT slot of every entry that matches.
fn scan_thunks(
    src: &(impl RvaSource + ?Sized),
    names: u32,
    slots: u32,
    import: ImportRef<'_>,
    found: &mut impl FnMut(u32),
) -> Result<(), PeError> {
    for index in 0..MAX_THUNKS {
        let offset = index * 8;
        let thunk = read_u64(
            src,
            at(names, offset, "import name table")?,
            "import name table",
        )?;
        if thunk == 0 {
            return Ok(());
        }
        if thunk_matches(src, thunk, import)? {
            found(at(slots, offset, "import address table")?);
        }
    }
    Err(PeError::Unterminated {
        what: "import name table",
    })
}

fn dll_matches(name: &[u8], wanted: &str) -> bool {
    name.eq_ignore_ascii_case(wanted.as_bytes())
}

fn lossy(name: &[u8]) -> String {
    String::from_utf8_lossy(name).into_owned()
}

/// An import descriptor, regular or delay, reduced to what lookup needs.
struct Descriptor {
    table: ImportTable,
    name: u32,
    name_table: u32,
    iat: u32,
    module_handle: Option<u32>,
}

/// Read descriptor `index` of `table`; `None` at the terminator.
fn descriptor(
    src: &(impl RvaSource + ?Sized),
    directory: u32,
    table: ImportTable,
    index: u32,
) -> Result<Option<Descriptor>, PeError> {
    let (what, size) = match table {
        ImportTable::Static => ("import descriptor", 20),
        ImportTable::Delay => ("delay descriptor", 32),
    };
    let desc = at(directory, index.saturating_mul(size), what)?;
    let word = |offset| -> Result<u32, PeError> { read_u32(src, at(desc, offset, what)?, what) };
    match table {
        // IMAGE_IMPORT_DESCRIPTOR: OriginalFirstThunk @0, Name @12,
        // FirstThunk @16; terminated by an all-zero entry.
        ImportTable::Static => {
            let (name, iat) = (word(12)?, word(16)?);
            if name == 0 && iat == 0 {
                return Ok(None);
            }
            Ok(Some(Descriptor {
                table,
                name,
                name_table: word(0)?,
                iat,
                module_handle: None,
            }))
        }
        // ImgDelayDescr: grAttrs @0, rvaDLLName @4, rvaHmod @8, rvaIAT @12,
        // rvaINT @16; terminated by a zero name.
        ImportTable::Delay => {
            let name = word(4)?;
            if name == 0 {
                return Ok(None);
            }
            if word(0)? & 1 == 0 {
                // VA-based: `name` is a VA, so it cannot even be compared.
                return Err(PeError::VaBasedDelayImport {
                    dll: format!("<va {name:#x}>"),
                });
            }
            Ok(Some(Descriptor {
                table,
                name,
                name_table: word(16)?,
                iat: word(12)?,
                module_handle: Some(word(8)?),
            }))
        }
    }
}

/// Every IAT slot through which the image imports `dll!import`, regular
/// table first, then delay-import table. DLL names compare ASCII
/// case-insensitively. An empty result means the image does not import it.
pub fn find_import(
    src: &(impl RvaSource + ?Sized),
    headers: &Headers,
    dll: &str,
    import: ImportRef<'_>,
) -> Result<Vec<ImportSlot>, PeError> {
    let mut slots = Vec::new();
    for (table, directory, what) in [
        (ImportTable::Static, dir::IMPORT, "import directory"),
        (
            ImportTable::Delay,
            dir::DELAY_IMPORT,
            "delay-import directory",
        ),
    ] {
        let directory = headers.directory(directory).rva;
        if directory == 0 {
            continue;
        }
        let mut index = 0;
        while let Some(desc) = descriptor(src, directory, table, index)? {
            index += 1;
            if index == MAX_DESCRIPTORS {
                return Err(PeError::Unterminated { what });
            }
            let this = c_str(src, desc.name, "import DLL name")?;
            if !dll_matches(this, dll) {
                continue;
            }
            if desc.name_table == 0 {
                return Err(PeError::NoNameTable { dll: lossy(this) });
            }
            scan_thunks(src, desc.name_table, desc.iat, import, &mut |slot_rva| {
                slots.push(ImportSlot {
                    table: desc.table,
                    slot_rva,
                    module_handle_rva: desc.module_handle,
                })
            })?;
        }
    }
    Ok(slots)
}

/// Whether a delay-import slot still holds its unresolved thunk: the thunk
/// lives inside the importing image, a resolved import does not.
pub fn delay_slot_unresolved(value: u64, image_base: u64, image_size: u64) -> bool {
    value >= image_base && value - image_base < image_size
}

#[cfg(test)]
mod tests;
