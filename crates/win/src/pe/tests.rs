use super::*;

/// The parts of `MonsterHunterWorld.exe` (Steam build 15539686) the import
/// parser reads, in mapped layout; see `fixtures/extract_mhw.py`.
static GAME: &[u8] = include_bytes!("../../fixtures/mhw-imports.bin");

/// A sparse image: chunks of bytes at their RVAs, nothing in between.
struct Sparse(Vec<(u32, &'static [u8])>);

impl Sparse {
    fn game() -> Sparse {
        let mut chunks = Vec::new();
        let mut rest = GAME;
        while !rest.is_empty() {
            let rva = u32::from_le_bytes(rest[..4].try_into().unwrap());
            let len = u32::from_le_bytes(rest[4..8].try_into().unwrap()) as usize;
            chunks.push((rva, &rest[8..8 + len]));
            rest = &rest[8 + len..];
        }
        Sparse(chunks)
    }
}

impl RvaSource for Sparse {
    fn bytes_at(&self, rva: u32, max: usize) -> Option<&[u8]> {
        self.0
            .iter()
            .filter(|(start, bytes)| rva >= *start && ((rva - start) as usize) < bytes.len())
            .map(|(start, bytes)| &bytes[(rva - start) as usize..])
            .max_by_key(|run| run.len())
            .map(|run| &run[..run.len().min(max)])
    }
}

const GAME_BASE: u64 = 0x1_4000_0000;
const GAME_SIZE: u64 = 0x055d_3000;

#[test]
fn the_games_headers_identify_the_known_build() {
    let headers = parse_headers(&Sparse::game()).unwrap();
    assert_eq!(headers.timestamp, 0x66d1_3cb6);
    assert_eq!(headers.image_base, GAME_BASE);
    assert_eq!(u64::from(headers.size_of_image), GAME_SIZE);
    assert_eq!(headers.entry_point, 0x0274_2a48);
    let names: Vec<&str> = headers.sections.iter().map(Section::name).collect();
    assert_eq!(
        names,
        [".text", ".rdata", ".data", ".pdata", "_RDATA", ".msvcjmc", ".rsrc", ".bind"]
    );
    let text = headers.section(".text").unwrap();
    assert_eq!((text.rva, text.mapped_len()), (0x1000, 0x02e1_102c));
    assert!(text.is_code());
    assert!(!headers.section(".rdata").unwrap().is_code());
    assert_eq!(
        headers.section_containing(0x0228_f3b0).unwrap().name(),
        ".text"
    );
    assert_eq!(
        headers.section_containing(0x04c9_7a50).unwrap().name(),
        ".data"
    );
    assert!(headers.section_containing(0x0700_0000).is_none());
    assert_eq!(
        headers.directory(dir::DELAY_IMPORT),
        DataDirectory {
            rva: 0x03e6_f2d0,
            size: 0x2a0
        }
    );
    assert_eq!(headers.directory(99), DataDirectory::default());
}

#[test]
fn get_proc_address_is_one_static_slot() {
    let src = Sparse::game();
    let headers = parse_headers(&src).unwrap();
    // docs/re: IAT KERNEL32!GetProcAddress at 0x142e13118.
    let expected = vec![ImportSlot {
        table: ImportTable::Static,
        slot_rva: 0x02e1_3118,
        module_handle_rva: None,
    }];
    let found = find_import(
        &src,
        &headers,
        "KERNEL32.dll",
        ImportRef::Name("GetProcAddress"),
    );
    assert_eq!(found.unwrap(), expected);
    // DLL names are case-insensitive, function names are not.
    let found = find_import(
        &src,
        &headers,
        "kernel32.DLL",
        ImportRef::Name("GetProcAddress"),
    );
    assert_eq!(found.unwrap(), expected);
    let found = find_import(
        &src,
        &headers,
        "KERNEL32.dll",
        ImportRef::Name("getprocaddress"),
    );
    assert_eq!(found.unwrap(), vec![]);
}

#[test]
fn d3d12_create_device_is_delay_imported_by_ordinal_101() {
    let src = Sparse::game();
    let headers = parse_headers(&src).unwrap();
    // docs/re/ReLoadInitDx11.md §2.2: INT 0x143e6fb18 = ordinal 101,
    // IAT slot 0x144c97a50.
    let found = find_import(&src, &headers, "d3d12.dll", ImportRef::Ordinal(101)).unwrap();
    assert_eq!(
        found,
        vec![ImportSlot {
            table: ImportTable::Delay,
            slot_rva: 0x04c9_7a50,
            module_handle_rva: Some(0x051d_7a40),
        }]
    );
    // Looking it up by name finds nothing: that is the defect of the old
    // GetProcAddress-only hook.
    let by_name = find_import(
        &src,
        &headers,
        "d3d12.dll",
        ImportRef::Name("D3D12CreateDevice"),
    );
    assert_eq!(by_name.unwrap(), vec![]);
}

#[test]
fn delay_imports_by_name_are_found_beside_the_ordinal() {
    let src = Sparse::game();
    let headers = parse_headers(&src).unwrap();
    let found = find_import(
        &src,
        &headers,
        "D3D12.DLL",
        ImportRef::Name("D3D12SerializeRootSignature"),
    )
    .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].slot_rva, 0x04c9_7a48);
    assert_eq!(found[0].table, ImportTable::Delay);
}

#[test]
fn absent_imports_are_empty_not_errors() {
    let src = Sparse::game();
    let headers = parse_headers(&src).unwrap();
    for (dll, import) in [
        ("KERNEL32.dll", ImportRef::Ordinal(101)),
        ("dxgi.dll", ImportRef::Name("CreateDXGIFactory2")),
        ("d3d12.dll", ImportRef::Ordinal(102)),
    ] {
        assert_eq!(
            find_import(&src, &headers, dll, import).unwrap(),
            vec![],
            "{dll}!{import}"
        );
    }
}

#[test]
fn the_unresolved_delay_thunk_lies_inside_the_image() {
    let src = Sparse::game();
    // The slot's initial content, as linked: the thunk 0x14273ddf7.
    let slot = src.bytes(0x04c9_7a50, 8).unwrap();
    let value = u64::from_le_bytes(slot.try_into().unwrap());
    assert_eq!(value, 0x1_4273_ddf7);
    assert!(delay_slot_unresolved(value, GAME_BASE, GAME_SIZE));
    assert!(!delay_slot_unresolved(
        0x7ffa_1234_5678,
        GAME_BASE,
        GAME_SIZE
    ));
    assert!(!delay_slot_unresolved(
        GAME_BASE + GAME_SIZE,
        GAME_BASE,
        GAME_SIZE
    ));
    assert!(!delay_slot_unresolved(GAME_BASE - 1, GAME_BASE, GAME_SIZE));
}

#[test]
fn a_slice_is_a_mapped_image() {
    let image: &[u8] = &[1, 2, 3, 4];
    assert_eq!(image.bytes_at(1, 10), Some(&[2u8, 3, 4][..]));
    assert_eq!(image.bytes(1, 3), Some(&[2u8, 3, 4][..]));
    assert_eq!(image.bytes(1, 4), None);
    assert_eq!(image.bytes_at(4, 1), None);
    assert_eq!(image.bytes_at(u32::MAX, 1), None);
}

/// A minimal PE32+ image in mapped layout, for malformed-input cases.
struct Synthetic(Vec<u8>);

const NT: usize = 0x40;
const OPTIONAL: usize = NT + 24;
const SECTIONS: usize = OPTIONAL + 240;

impl Synthetic {
    fn new() -> Synthetic {
        let mut image = Synthetic(vec![0u8; 0x1000]);
        image.0[..2].copy_from_slice(b"MZ");
        image.u32(0x3c, NT as u32);
        image.0[NT..NT + 4].copy_from_slice(b"PE\0\0");
        image.u16(NT + 4, 0x8664);
        image.u16(NT + 6, 1);
        image.u32(NT + 8, 0x1234_5678);
        image.u16(NT + 20, 240);
        image.u16(OPTIONAL, 0x20b);
        image.u32(OPTIONAL + 56, 0x1000);
        image.u32(OPTIONAL + 108, 16);
        image.0[SECTIONS..SECTIONS + 5].copy_from_slice(b".text");
        image.u32(SECTIONS + 8, 0x100);
        image.u32(SECTIONS + 12, 0x400);
        image.u32(SECTIONS + 36, 0x6000_0020);
        image
    }
    fn u16(&mut self, at: usize, value: u16) {
        self.0[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }
    fn u32(&mut self, at: usize, value: u32) {
        self.0[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn u64(&mut self, at: usize, value: u64) {
        self.0[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }
    fn str(&mut self, at: usize, value: &str) {
        self.0[at..at + value.len()].copy_from_slice(value.as_bytes());
    }
    fn directory(&mut self, index: usize, rva: u32, size: u32) {
        self.u32(OPTIONAL + 112 + 8 * index, rva);
        self.u32(OPTIONAL + 116 + 8 * index, size);
    }
    /// One delay descriptor for `x.dll` at 0x600 with one by-name entry.
    fn with_delay(mut self, attributes: u32) -> Synthetic {
        self.directory(dir::DELAY_IMPORT, 0x600, 64);
        self.u32(0x600, attributes);
        self.u32(0x604, 0x700);
        self.u32(0x608, 0x710);
        self.u32(0x60c, 0x720);
        self.u32(0x610, 0x740);
        self.str(0x700, "x.dll");
        self.u64(0x740, 0x760);
        self.str(0x762, "Fn");
        self
    }
    fn parse(&self) -> Result<Headers, PeError> {
        parse_headers(&self.0[..])
    }
}

#[test]
fn a_minimal_image_parses() {
    let image = Synthetic::new();
    let headers = image.parse().unwrap();
    assert_eq!(headers.timestamp, 0x1234_5678);
    assert_eq!(headers.sections.len(), 1);
    assert_eq!(headers.sections[0].name(), ".text");
    assert!(headers.sections[0].contains(0x4ff));
    assert!(!headers.sections[0].contains(0x500));
}

#[test]
fn bad_signatures_and_foreign_machines_are_refused() {
    let mut image = Synthetic::new();
    image.0[0] = b'Z';
    assert_eq!(image.parse(), Err(PeError::BadSignature));

    let mut image = Synthetic::new();
    image.u32(0x3c, 0xffff_fff0);
    assert_eq!(image.parse(), Err(PeError::BadSignature));

    let mut image = Synthetic::new();
    image.u16(NT + 4, 0x14c);
    assert_eq!(
        image.parse(),
        Err(PeError::NotPe32Plus {
            machine: 0x14c,
            magic: 0x20b
        })
    );

    let mut image = Synthetic::new();
    image.u16(OPTIONAL, 0x10b);
    assert!(matches!(image.parse(), Err(PeError::NotPe32Plus { .. })));
}

#[test]
fn truncated_and_oversized_section_tables_are_errors() {
    let image = Synthetic::new();
    assert!(matches!(
        parse_headers(&image.0[..SECTIONS + 20]),
        Err(PeError::Truncated {
            what: "section table",
            ..
        })
    ));
    let mut image = Synthetic::new();
    image.u16(NT + 6, 97);
    assert_eq!(
        image.parse(),
        Err(PeError::Unterminated {
            what: "section table"
        })
    );
}

#[test]
fn directories_beyond_the_declared_count_are_absent() {
    let mut image = Synthetic::new();
    image.directory(dir::DELAY_IMPORT, 0x600, 64);
    image.u32(OPTIONAL + 108, 13);
    assert_eq!(
        image.parse().unwrap().directory(dir::DELAY_IMPORT),
        DataDirectory::default()
    );
}

#[test]
fn synthetic_delay_imports_resolve_and_reject_the_va_layout() {
    let image = Synthetic::new().with_delay(1);
    let headers = image.parse().unwrap();
    let found = find_import(&image.0[..], &headers, "X.DLL", ImportRef::Name("Fn")).unwrap();
    assert_eq!(
        found,
        vec![ImportSlot {
            table: ImportTable::Delay,
            slot_rva: 0x720,
            module_handle_rva: Some(0x710),
        }]
    );

    let image = Synthetic::new().with_delay(0);
    let headers = image.parse().unwrap();
    assert!(matches!(
        find_import(&image.0[..], &headers, "x.dll", ImportRef::Name("Fn")),
        Err(PeError::VaBasedDelayImport { .. })
    ));
}

#[test]
fn a_matching_descriptor_without_name_table_is_an_error() {
    let mut image = Synthetic::new().with_delay(1);
    image.u32(0x610, 0);
    let headers = image.parse().unwrap();
    assert_eq!(
        find_import(&image.0[..], &headers, "x.dll", ImportRef::Ordinal(1)),
        Err(PeError::NoNameTable {
            dll: "x.dll".into()
        })
    );
    // Other DLLs are not affected by it.
    assert_eq!(
        find_import(&image.0[..], &headers, "y.dll", ImportRef::Ordinal(1)),
        Ok(vec![])
    );
}

#[test]
fn reads_past_the_image_and_unterminated_names_are_errors() {
    let mut image = Synthetic::new().with_delay(1);
    // Name-table entry pointing outside the image.
    image.u64(0x740, 0x10_0000);
    let headers = image.parse().unwrap();
    assert!(matches!(
        find_import(&image.0[..], &headers, "x.dll", ImportRef::Name("Fn")),
        Err(PeError::Truncated {
            what: "import name",
            ..
        })
    ));

    let mut image = Synthetic::new().with_delay(1);
    // A DLL name running to the end of the image with no NUL.
    image.u32(0x604, 0xff8);
    image.0[0xff8..].fill(b'a');
    let headers = image.parse().unwrap();
    assert!(matches!(
        find_import(&image.0[..], &headers, "x.dll", ImportRef::Name("Fn")),
        Err(PeError::Truncated { .. })
    ));
}

#[test]
fn a_name_table_without_terminator_is_an_error() {
    // 65536 ordinal entries, none of them zero.
    let mut image = Synthetic::new().with_delay(1);
    image.0.resize(0x1000 + 8 * 65_536 + 8, 0);
    image.u32(0x610, 0x1000);
    for index in 0..=65_536 {
        image.u64(0x1000 + 8 * index, ORDINAL_FLAG | 7);
    }
    let headers = image.parse().unwrap();
    assert_eq!(
        find_import(&image.0[..], &headers, "x.dll", ImportRef::Ordinal(1)),
        Err(PeError::Unterminated {
            what: "import name table"
        })
    );
}
