//! The version resource (`VS_VERSIONINFO`) of a PE image: which DLSS library
//! is actually loaded, or lies in the mod folder.
//!
//! Pure: works over any [`RvaSource`] — the loaded module, or a file read
//! from disk through [`FileImage`].

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use mhw_win::pe::{self, DataDirectory, Headers, RvaSource};

/// `IMAGE_DIRECTORY_ENTRY_RESOURCE`.
const RESOURCE_DIRECTORY: usize = 2;
/// `RT_VERSION`.
const RT_VERSION: u32 = 16;
/// `VS_FIXEDFILEINFO.dwSignature`.
const FIXED_SIGNATURE: u32 = 0xFEEF_04BD;
/// `sizeof(VS_FIXEDFILEINFO)`.
const FIXED_LEN: usize = 52;
/// Entries read from one resource directory at most.
const MAX_ENTRIES: u32 = 4096;
/// Largest version resource accepted.
const MAX_BLOB: usize = 64 * 1024;

/// A four-part version number, `major.minor.build.revision`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileVersion {
    /// High word of the most significant half.
    pub major: u16,
    /// Low word of the most significant half.
    pub minor: u16,
    /// High word of the least significant half.
    pub build: u16,
    /// Low word of the least significant half.
    pub revision: u16,
}

impl FileVersion {
    /// From the two `DWORD`s of `VS_FIXEDFILEINFO`.
    pub const fn from_words(ms: u32, ls: u32) -> FileVersion {
        FileVersion {
            major: (ms >> 16) as u16,
            minor: ms as u16,
            build: (ls >> 16) as u16,
            revision: ls as u16,
        }
    }
}

impl fmt::Display for FileVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{}.{}.{}",
            self.major, self.minor, self.build, self.revision
        )
    }
}

/// The fixed part of a version resource.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct VersionInfo {
    /// `dwFileVersionMS/LS` (e.g. `310.4.0.0` for DLSS 310.4).
    pub file: FileVersion,
    /// `dwProductVersionMS/LS`.
    pub product: FileVersion,
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    let b = bytes.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Parse a `VS_VERSIONINFO` block: the key `VS_VERSION_INFO`, then the
/// `VS_FIXEDFILEINFO` on the next 32-bit boundary.
pub fn parse_version_info(blob: &[u8]) -> Option<VersionInfo> {
    let length = usize::from(u16_at(blob, 0)?);
    let value_length = usize::from(u16_at(blob, 2)?);
    let blob = blob.get(..length.min(blob.len()))?;
    let key: Vec<u16> = "VS_VERSION_INFO\0".encode_utf16().collect();
    let key_end = 6 + key.len() * 2;
    for (index, unit) in key.iter().enumerate() {
        if u16_at(blob, 6 + index * 2)? != *unit {
            return None;
        }
    }
    let value = (key_end + 3) & !3;
    if value_length < FIXED_LEN || u32_at(blob, value)? != FIXED_SIGNATURE {
        return None;
    }
    Some(VersionInfo {
        file: FileVersion::from_words(u32_at(blob, value + 8)?, u32_at(blob, value + 12)?),
        product: FileVersion::from_words(u32_at(blob, value + 16)?, u32_at(blob, value + 20)?),
    })
}

/// One `IMAGE_RESOURCE_DIRECTORY_ENTRY`: `(name or id, offset to data)`.
fn entries(
    src: &(impl RvaSource + ?Sized),
    root: u32,
    directory: u32,
) -> Option<impl Iterator<Item = (u32, u32)> + '_> {
    let header = root.checked_add(directory)?;
    let bytes = src.bytes(header, 16)?;
    let named = u32::from(u16_at(bytes, 12)?);
    let ids = u32::from(u16_at(bytes, 14)?);
    let count = (named + ids).min(MAX_ENTRIES);
    let first = header.checked_add(16)?;
    Some((0..count).map_while(move |index| {
        let at = first.checked_add(index.checked_mul(8)?)?;
        let entry = src.bytes(at, 8)?;
        Some((u32_at(entry, 0)?, u32_at(entry, 4)?))
    }))
}

const HIGH_BIT: u32 = 0x8000_0000;

/// The `RT_VERSION` resource's bytes: type 16, its first name, its first
/// language.
pub fn version_resource(
    src: &(impl RvaSource + ?Sized),
    resources: DataDirectory,
) -> Option<&[u8]> {
    if resources.rva == 0 || resources.size == 0 {
        return None;
    }
    let root = resources.rva;
    let subdirectory = |(_, data): (u32, u32)| (data & HIGH_BIT != 0).then_some(data & !HIGH_BIT);
    let by_type = entries(src, root, 0)?
        .find(|(name, data)| *name == RT_VERSION && data & HIGH_BIT != 0)
        .and_then(subdirectory)?;
    let by_name = entries(src, root, by_type)?.find_map(subdirectory)?;
    let leaf = entries(src, root, by_name)?.find(|(_, data)| data & HIGH_BIT == 0)?;
    let data = src.bytes(root.checked_add(leaf.1)?, 8)?;
    let rva = u32_at(data, 0)?;
    let size = usize::try_from(u32_at(data, 4)?).ok()?.min(MAX_BLOB);
    src.bytes(rva, size)
}

/// The version of an image whose headers are `headers`.
pub fn image_version(src: &(impl RvaSource + ?Sized), headers: &Headers) -> Option<VersionInfo> {
    parse_version_info(version_resource(
        src,
        headers.directory(RESOURCE_DIRECTORY),
    )?)
}

/// A PE file's bytes addressed by RVA: headers in place, sections at their
/// raw offsets. Holds only the parts it was given, so a large file can be
/// read sparsely (headers and the resource section).
#[derive(Debug)]
pub struct FileImage<'a> {
    headers: Headers,
    head: &'a [u8],
    /// `(rva, raw bytes)` of each section present.
    sections: Vec<(u32, &'a [u8])>,
}

impl<'a> FileImage<'a> {
    /// Parse a whole file read into memory.
    pub fn parse(bytes: &'a [u8]) -> Option<FileImage<'a>> {
        let headers = pe::parse_headers(bytes).ok()?;
        let sections = headers
            .sections
            .iter()
            .filter_map(|s| {
                let start = usize::try_from(s.raw_offset).ok()?;
                let end = start.checked_add(usize::try_from(s.raw_size).ok()?)?;
                let raw = bytes.get(start..end.min(bytes.len()))?;
                Some((s.rva, raw))
            })
            .collect();
        Some(FileImage {
            headers,
            head: bytes,
            sections,
        })
    }

    /// The parsed headers.
    pub fn headers(&self) -> &Headers {
        &self.headers
    }

    /// The version resource.
    pub fn version(&self) -> Option<VersionInfo> {
        image_version(self, &self.headers)
    }
}

impl RvaSource for FileImage<'_> {
    fn bytes_at(&self, rva: u32, max: usize) -> Option<&[u8]> {
        let start = usize::try_from(rva).ok()?;
        let run = if rva < self.headers.size_of_headers {
            let end = usize::try_from(self.headers.size_of_headers).ok()?;
            self.head.get(start..end.min(self.head.len()))?
        } else {
            self.sections.iter().find_map(|&(base, raw)| {
                let into = usize::try_from(rva.checked_sub(base)?).ok()?;
                raw.get(into..).filter(|rest| !rest.is_empty())
            })?
        };
        let run = &run[..max.min(run.len())];
        (!run.is_empty()).then_some(run)
    }
}

/// The version of a PE file's bytes; `None` when it is not a PE32+ image or
/// has no version resource.
pub fn file_version(bytes: &[u8]) -> Option<VersionInfo> {
    FileImage::parse(bytes)?.version()
}

/// Largest resource section [`read_file_version`] reads.
const MAX_RESOURCE_SECTION: u32 = 16 * 1024 * 1024;

/// The version of the PE file at `path`, reading only its headers and its
/// resource section (a 310.x `nvngx_dlss.dll` is tens of megabytes).
/// `Ok(None)` for a file that is not a PE32+ image or has no version.
pub fn read_file_version(path: &Path) -> io::Result<Option<VersionInfo>> {
    let mut file = File::open(path)?;
    let mut head = Vec::new();
    (&mut file).take(0x1000).read_to_end(&mut head)?;
    let Ok(headers) = pe::parse_headers(head.as_slice()) else {
        return Ok(None);
    };
    let resources = headers.directory(RESOURCE_DIRECTORY);
    let Some(section) = headers
        .sections
        .iter()
        .find(|s| s.contains(resources.rva))
        .copied()
    else {
        return Ok(None);
    };
    let mut raw = Vec::new();
    file.seek(SeekFrom::Start(u64::from(section.raw_offset)))?;
    (&mut file)
        .take(u64::from(section.raw_size.min(MAX_RESOURCE_SECTION)))
        .read_to_end(&mut raw)?;
    let image = FileImage {
        headers,
        head: &head,
        sections: vec![(section.rva, &raw)],
    };
    Ok(image.version())
}

#[cfg(test)]
mod tests;
