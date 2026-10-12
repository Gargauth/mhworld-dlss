//! `resolve_all` against synthetic images built from real sites of
//! `docs/re/signatures.json`.

use std::ops::Range;

use super::*;

/// An image whose RVAs are indices into a buffer.
struct Buffer {
    bytes: Vec<u8>,
    text: Option<Range<usize>>,
}

impl Image for Buffer {
    fn section(&self, name: &str) -> Option<Range<usize>> {
        (name == ".text").then(|| self.text.clone()).flatten()
    }

    fn read(&self, rva: usize, len: usize) -> Option<&[u8]> {
        self.bytes.get(rva..rva.checked_add(len)?)
    }
}

const TEXT: Range<usize> = 0x1000..0x3000;

impl Buffer {
    fn new() -> Buffer {
        let mut bytes = vec![0x90; 0x4000];
        bytes[..0x1000].fill(0);
        Buffer {
            bytes,
            text: Some(TEXT),
        }
    }

    /// Write `pattern` at `rva`, its wildcards taken from `fill` in order.
    fn place(&mut self, rva: usize, pattern: &str, fill: &[u8]) {
        let mut fill = fill.iter().copied();
        for (offset, byte) in Pattern::parse(pattern).unwrap().bytes().iter().enumerate() {
            self.bytes[rva + offset] = byte.unwrap_or_else(|| fill.next().unwrap_or(0x11));
        }
    }
}

const CONFIG_LOAD: &str =
    "48 8B 05 ?? ?? ?? ?? 48 8B D9 48 8B 90 98 01 00 00 48 8B CA 48 C1 E9 20 85 D2";
const READ_X: &str =
    "F3 0F 10 48 38 66 0F 6E C2 0F 5B C0 F3 0F 5E C8 66 41 0F 6E C0 F3 0F 11 8D F0 00 00 00";
const CAMERA_SETUP: &str =
    "40 55 53 57 48 8D AC 24 60 FE FF FF 48 81 EC A0 02 00 00 80 79 20 00 48 8B FA";
const BANK_ONE_SOURCE: &str = "F3 0F 10 05 ?? ?? ?? ?? 44 89 6C 24 48 E8 ?? ?? ?? ?? F3 44 0F 10 1D ?? ?? ?? ?? 41 0F 57 C3 F3 0F 11 44 24 4C F3 0F 10 05 ?? ?? ?? ??";

/// The global CONFIG_LOAD points at, as an RVA in the known build.
const CONFIG_DISP: u32 = 0x02f3_4053;

fn sites() -> Vec<Site> {
    vec![
        Site {
            name: "CONFIG_LOAD",
            pattern: CONFIG_LOAD,
            to_target: 0,
            known_rva: 0x1100,
            verify: &[
                0x48, 0x8b, 0x05, 0x53, 0x40, 0xf3, 0x02, 0x48, 0x8b, 0xd9, 0x48, 0x8b, 0x90, 0x98,
                0x01, 0x00, 0x00,
            ],
            wild: 0..0,
        },
        Site {
            // Found by the part after the struct offset, to exercise a
            // negative `to_target`.
            name: "READ_X",
            pattern: "66 0F 6E C2 0F 5B C0 F3 0F 5E C8 66 41 0F 6E C0 F3 0F 11 8D F0 00 00 00",
            to_target: -5,
            known_rva: 0x1200,
            verify: &[
                0xf3, 0x0f, 0x10, 0x48, 0x38, 0x66, 0x0f, 0x6e, 0xc2, 0x0f, 0x5b, 0xc0, 0xf3, 0x0f,
                0x5e, 0xc8,
            ],
            wild: 0..0,
        },
        Site {
            name: "CAMERA_SETUP",
            pattern: CAMERA_SETUP,
            to_target: 0,
            known_rva: 0x1300,
            verify: &[
                0x40, 0x55, 0x53, 0x57, 0x48, 0x8d, 0xac, 0x24, 0x60, 0xfe, 0xff, 0xff, 0x48, 0x81,
                0xec, 0xa0, 0x02, 0x00, 0x00,
            ],
            wild: 0..0,
        },
        Site {
            name: "BANK_ONE_SOURCE",
            pattern: BANK_ONE_SOURCE,
            to_target: 0,
            known_rva: 0x1400,
            verify: &[
                0xf3, 0x0f, 0x10, 0x05, 0x62, 0x7f, 0xb3, 0x00, 0x44, 0x89, 0x6c, 0x24, 0x48, 0xe8,
            ],
            wild: 4..8,
        },
    ]
}

/// The four sites at `known_rva + shift`, with RIP displacements adjusted
/// so they still reach the same targets.
fn build(shift: usize) -> Buffer {
    let mut image = Buffer::new();
    let disp = (CONFIG_DISP - shift as u32).to_le_bytes();
    image.place(0x1100 + shift, CONFIG_LOAD, &disp);
    image.place(0x1200 + shift, READ_X, &[]);
    image.place(0x1300 + shift, CAMERA_SETUP, &[]);
    let bank = (0x00b3_7f62u32 - shift as u32).to_le_bytes();
    image.place(
        0x1400 + shift,
        BANK_ONE_SOURCE,
        &[bank[0], bank[1], bank[2], bank[3], 1, 2, 3, 4],
    );
    image
}

fn problems(result: Result<Resolved, GateFailure>) -> Vec<SiteProblem> {
    match result {
        Err(GateFailure::Sites(problems)) => problems,
        other => panic!("expected site problems, got {other:?}"),
    }
}

#[test]
fn the_known_build_takes_the_fast_path() {
    let resolved = resolve_all(&build(0), &sites()).unwrap();
    assert!(!resolved.scanned);
    assert_eq!(resolved.rva("CONFIG_LOAD"), Some(0x1100));
    assert_eq!(resolved.rva("READ_X"), Some(0x1200));
    assert!(resolved.sites.iter().all(|site| site.moved == 0));
    assert_eq!(resolved.rva("NOPE"), None);
}

#[test]
fn bytes_the_mod_rewrites_do_not_break_the_fast_path() {
    let mut image = build(0);
    // tune rewrites BANK_ONE_SOURCE's displacement.
    image.bytes[0x1404..0x1408].copy_from_slice(&[9, 9, 9, 9]);
    assert!(!resolve_all(&image, &sites()).unwrap().scanned);
}

#[test]
fn a_moved_build_is_found_by_scanning() {
    let image = build(0x180);
    let resolved = resolve_all(&image, &sites()).unwrap();
    assert!(resolved.scanned);
    for site in &resolved.sites {
        assert_eq!(site.moved, 0x180, "{}", site.name);
    }
    // The global is read from the moved instruction and is the same one.
    let rva = resolved.rva("CONFIG_LOAD").unwrap();
    let insn = image.read(rva, 7).unwrap();
    assert_eq!(
        rip_target(rva, 3, 7, insn),
        Some(0x1100 + 7 + CONFIG_DISP as usize)
    );
    // And so is the struct offset.
    let read_x = resolved.rva("READ_X").unwrap();
    assert_eq!(
        displacement(image.read(read_x, 5).unwrap(), 4, 1),
        Some(0x38)
    );
}

#[test]
fn one_missing_site_fails_the_whole_table() {
    let mut image = build(0x40);
    image.bytes[0x1340] = 0xC3; // CAMERA_SETUP's first byte
    let problems = problems(resolve_all(&image, &sites()));
    assert_eq!(
        problems,
        vec![SiteProblem {
            site: "CAMERA_SETUP",
            problem: Problem::Missing
        }]
    );
}

#[test]
fn a_pattern_found_twice_is_ambiguous() {
    let mut image = build(0x40);
    image.place(0x2800, CONFIG_LOAD, &[1, 2, 3, 4]);
    let problems = problems(resolve_all(&image, &sites()));
    assert_eq!(
        problems,
        vec![SiteProblem {
            site: "CONFIG_LOAD",
            problem: Problem::Ambiguous {
                count: 2,
                rvas: vec![0x1140, 0x2800]
            }
        }]
    );
    let text = GateFailure::Sites(problems).to_string();
    assert_eq!(text, "CONFIG_LOAD: pattern found 2 times at 0x1140 0x2800");
}

#[test]
fn a_changed_struct_offset_is_refused_even_though_the_pattern_matches() {
    let mut image = build(0x40);
    image.bytes[0x1244] = 0x40; // READ_X: [rax+0x38] became [rax+0x40]
    let problems = problems(resolve_all(&image, &sites()));
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].site, "READ_X");
    assert!(matches!(
        problems[0].problem,
        Problem::Differs { rva: 0x1240, .. }
    ));
    assert!(GateFailure::Sites(problems)
        .to_string()
        .starts_with("READ_X: bytes at 0x1240 differ: f30f104840"));
}

#[test]
fn a_site_outside_the_image_is_unreadable() {
    let mut table = sites();
    table[2].to_target = -0x2000;
    let problems = problems(resolve_all(&build(0x40), &table));
    assert_eq!(problems.len(), 1);
    assert!(matches!(problems[0].problem, Problem::Unreadable { .. }));
}

#[test]
fn broken_table_entries_are_reported_even_in_the_known_build() {
    let mut table = sites();
    table[0].pattern = "48 8B ZZ";
    table[1].verify = &[];
    table[3].wild = 10..20;
    let problems = problems(resolve_all(&build(0), &table));
    let names: Vec<&str> = problems.iter().map(|p| p.site).collect();
    assert_eq!(names, ["CONFIG_LOAD", "READ_X", "BANK_ONE_SOURCE"]);
    assert!(matches!(problems[0].problem, Problem::BadPattern(_)));
    assert!(matches!(problems[1].problem, Problem::BadSite(_)));
}

#[test]
fn without_a_code_section_a_moved_build_cannot_resolve() {
    let mut image = build(0x40);
    image.text = None;
    assert_eq!(resolve_all(&image, &sites()), Err(GateFailure::NoCode));
    // Unreadable code is the same.
    image.text = Some(0x3000..0x5000);
    assert_eq!(resolve_all(&image, &sites()), Err(GateFailure::NoCode));
}

#[test]
fn scanning_ignores_pattern_wildcards_but_not_fixed_bytes() {
    let site = &sites()[0];
    let pattern = Pattern::parse(site.pattern).unwrap();
    let mut moved = site.verify.to_vec();
    moved[3..7].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
    assert!(!site.verifies(&moved));
    assert!(site.verifies_scanned(&moved, &pattern));
    moved[9] = 0xD8;
    assert!(!site.verifies_scanned(&moved, &pattern));
    assert!(!site.verifies(&site.verify[..5]));
}

#[test]
fn every_pattern_in_the_re_table_parses() {
    let json = include_str!("../../../../docs/re/signatures.json");
    let mut count = 0;
    for line in json.lines() {
        let line = line.trim();
        for key in ["\"pattern\": \"", "\"pattern_min\": \""] {
            if let Some(rest) = line.strip_prefix(key) {
                let text = rest.trim_end_matches(',').trim_end_matches('"');
                let pattern = Pattern::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
                assert_eq!(pattern.to_string(), text.to_ascii_uppercase());
                count += 1;
            }
        }
    }
    // 16 gate sites with two patterns each, at least.
    assert!(count >= 32, "{count}");
}
