//! The gate on a DirectX 11 configuration, and against the reference
//! executable when it is available (`MHW_EXE`, default
//! `../../MonsterHunterWorld.exe` next to the worktrees).

use std::ops::Range;
use std::path::PathBuf;

use super::*;
use crate::renderer::Dx11Reason;

/// A PE file mapped by RVA: just its sections.
struct FileImage {
    data: Vec<u8>,
    /// `(name, rva, virtual size, raw offset, raw size)`.
    sections: Vec<(String, usize, usize, usize, usize)>,
}

fn u16_at(d: &[u8], at: usize) -> usize {
    u16::from_le_bytes([d[at], d[at + 1]]) as usize
}

fn u32_at(d: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]])
}

impl FileImage {
    fn open(path: &PathBuf) -> FileImage {
        let data = std::fs::read(path).unwrap();
        let nt = u32_at(&data, 0x3c) as usize;
        assert_eq!(&data[nt..nt + 4], b"PE\0\0");
        let count = u16_at(&data, nt + 6);
        let table = nt + 24 + u16_at(&data, nt + 20);
        let sections = (0..count)
            .map(|i| {
                let e = table + i * 40;
                let name = String::from_utf8_lossy(&data[e..e + 8])
                    .trim_end_matches('\0')
                    .to_owned();
                let field = |at: usize| u32_at(&data, e + at) as usize;
                (name, field(12), field(8), field(20), field(16))
            })
            .collect();
        FileImage { data, sections }
    }
}

impl Image for FileImage {
    fn section(&self, name: &str) -> Option<Range<usize>> {
        self.sections
            .iter()
            .find(|s| s.0 == name)
            .map(|s| s.1..s.1 + s.2.min(s.4))
    }

    fn read(&self, rva: usize, len: usize) -> Option<&[u8]> {
        let (_, start, vsize, raw, rsize) = self
            .sections
            .iter()
            .find(|s| rva >= s.1 && rva + len <= s.1 + s.2.max(s.4))?;
        let offset = rva - start;
        (offset + len <= (*vsize).min(*rsize)).then_some(())?;
        self.data.get(raw + offset..raw + offset + len)
    }
}

fn known_exe() -> Option<PathBuf> {
    let path = std::env::var_os("MHW_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../../MonsterHunterWorld.exe")
        });
    path.is_file().then_some(path)
}

/// An image with nothing in it.
struct Empty;

impl Image for Empty {
    fn section(&self, _: &str) -> Option<Range<usize>> {
        Some(0..0x10)
    }

    fn read(&self, _: usize, _: usize) -> Option<&[u8]> {
        None
    }
}

#[test]
fn directx_11_stands_down_before_anything_is_looked_at() {
    let outcome = evaluate(&Empty, 0, &Renderer::Dx11(Dx11Reason::NoKey));
    assert_eq!(outcome, Err(StandDown::NotDx12(Dx11Reason::NoKey)));
    assert!(!outcome.unwrap_err().retryable());
}

#[test]
fn an_image_without_the_sites_stands_down_retryably() {
    let outcome = evaluate(&Empty, 0, &Renderer::Dx12).unwrap_err();
    assert!(matches!(outcome, StandDown::Sites(_)));
    assert!(outcome.retryable());
}

#[test]
fn the_reference_executable_passes_the_whole_gate() {
    let Some(path) = known_exe() else {
        eprintln!("skipped: MHW_EXE not set and no MonsterHunterWorld.exe next to the worktrees");
        return;
    };
    let image = FileImage::open(&path);
    let addresses = evaluate(&image, 0x1_4000_0000, &Renderer::Dx12).unwrap();
    assert!(!addresses.scanned);
    assert_eq!(addresses.site(site::CAMERA_SETUP), 0x1_4228_f3b0);
    assert_eq!(addresses.global(global::RENDER_CONFIG), 0x1_451c_4480);
    assert_eq!(addresses.global(global::CAMERA), 0x1_451c_4400);
    assert_eq!(addresses.global(global::RENDER_SETTINGS), 0x1_451c_4368);
    assert_eq!(addresses.field(field::CFG_198), 0x198);
    assert_eq!(addresses.field(field::CAM_38), 0x38);
    assert_eq!(addresses.field(field::RS_E9E4), 0xe9e4);
    for (field, (name, offset)) in FIELDS.iter().zip(addresses.fields()) {
        assert_eq!(field.known, offset, "{name}");
    }
}
