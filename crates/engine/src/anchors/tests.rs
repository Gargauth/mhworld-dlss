//! Deriving globals and offsets from synthetic images, and the invariants of
//! the generated tables.

use std::ops::Range;

use mhw_core::sig::{resolve_all, ResolvedSite};

use super::*;
use crate::sites::{ANCHORS, FIELDS, GLOBALS, SITES};

/// An image whose RVAs are indices into a buffer.
struct Buffer {
    bytes: Vec<u8>,
    text: Range<usize>,
}

impl Image for Buffer {
    fn section(&self, name: &str) -> Option<Range<usize>> {
        (name == ".text").then(|| self.text.clone())
    }

    fn read(&self, rva: usize, len: usize) -> Option<&[u8]> {
        self.bytes.get(rva..rva.checked_add(len)?)
    }
}

impl Buffer {
    fn new() -> Buffer {
        Buffer {
            bytes: vec![0x90; 0x4000],
            text: 0x1000..0x3000,
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

/// `cfg+0x1f0`-like field: a store with a wildcarded 32-bit displacement.
const STORE: &str = "F3 0F 11 81 ?? ?? ?? ?? C3 CC CC";
/// A second, independent anchor for the same field.
const LOAD: &str = "F3 0F 10 89 ?? ?? ?? ?? 0F 2F C8";
/// An 8-bit displacement (`movss xmm1,[rax+disp8]`).
const SHORT: &str = "F3 0F 10 48 ?? 66 0F 6E C2 0F 5B C0";

/// One field of an object with unanchored offsets, one of a fully anchored
/// object.
const FIELDS_T: [FieldInfo; 2] = [
    FieldInfo {
        name: "obj+0x1f0",
        known: 0x1f0,
        layout: true,
        reflection: "",
    },
    FieldInfo {
        name: "free+0x38",
        known: 0x38,
        layout: false,
        reflection: "",
    },
];

fn anchors() -> [OffsetAnchor; 3] {
    [
        OffsetAnchor {
            field: 0,
            pattern: STORE,
            disp_at: 4,
            disp_size: 4,
            known_rva: 0x1100,
        },
        OffsetAnchor {
            field: 0,
            pattern: LOAD,
            disp_at: 4,
            disp_size: 4,
            known_rva: 0x1200,
        },
        OffsetAnchor {
            field: 1,
            pattern: SHORT,
            disp_at: 4,
            disp_size: 1,
            known_rva: 0x1300,
        },
    ]
}

fn known_image() -> Buffer {
    let mut image = Buffer::new();
    image.place(0x1100, STORE, &0x1f0u32.to_le_bytes());
    image.place(0x1200, LOAD, &0x1f0u32.to_le_bytes());
    image.place(0x1300, SHORT, &[0x38]);
    image
}

#[test]
fn the_reference_build_gives_the_known_offsets_from_the_fast_path() {
    let offsets = derive_offsets(&known_image(), &FIELDS_T, &anchors()).unwrap();
    assert_eq!(offsets, vec![0x1f0, 0x38]);
}

#[test]
fn a_moved_anchor_is_found_by_scanning() {
    let mut image = Buffer::new();
    image.place(0x2100, STORE, &0x1f0u32.to_le_bytes());
    image.place(0x1200, LOAD, &0x1f0u32.to_le_bytes());
    image.place(0x2a00, SHORT, &[0x38]);
    let offsets = derive_offsets(&image, &FIELDS_T, &anchors()).unwrap();
    assert_eq!(offsets, vec![0x1f0, 0x38]);
}

#[test]
fn a_moved_field_is_read_from_the_code() {
    let mut image = known_image();
    image.place(0x1300, SHORT, &[0x40]);
    let offsets = derive_offsets(&image, &FIELDS_T, &anchors()).unwrap();
    assert_eq!(offsets, vec![0x1f0, 0x40]);
}

#[test]
fn a_field_unanchored_offsets_depend_on_must_not_move() {
    let mut image = Buffer::new();
    image.place(0x1100, STORE, &0x1f8u32.to_le_bytes());
    image.place(0x1200, LOAD, &0x1f8u32.to_le_bytes());
    image.place(0x1300, SHORT, &[0x40]);
    let failures = derive_offsets(&image, &FIELDS_T, &anchors()).unwrap_err();
    assert_eq!(
        failures,
        vec![AnchorFailure {
            name: "obj+0x1f0",
            problem: AnchorProblem::Moved {
                found: 0x1f8,
                known: 0x1f0
            }
        }]
    );
    assert_eq!(
        failures[0].to_string(),
        "obj+0x1f0: reads 0x1f8, not 0x1f0: the object's layout changed under its unanchored \
         offsets"
    );
}

#[test]
fn anchors_of_one_field_must_agree() {
    let mut image = known_image();
    image.place(0x1200, LOAD, &0x204u32.to_le_bytes());
    let failures = derive_offsets(&image, &FIELDS_T, &anchors()).unwrap_err();
    assert_eq!(
        failures,
        vec![AnchorFailure {
            name: "obj+0x1f0",
            problem: AnchorProblem::Disagree {
                found: vec![0x1f0, 0x204]
            }
        }]
    );
}

#[test]
fn missing_and_ambiguous_anchors_fail_every_field_they_name() {
    let mut image = Buffer::new();
    image.place(0x1200, LOAD, &0x1f0u32.to_le_bytes());
    image.place(0x2000, SHORT, &[0x38]);
    image.place(0x2800, SHORT, &[0x38]);
    let failures = derive_offsets(&image, &FIELDS_T, &anchors()).unwrap_err();
    assert_eq!(failures[0].problem, AnchorProblem::Missing);
    assert_eq!(failures[1].problem, AnchorProblem::Ambiguous { count: 2 });
}

#[test]
fn a_negative_displacement_is_not_an_offset() {
    let mut image = known_image();
    image.place(0x1300, SHORT, &[0xf8]);
    let failures = derive_offsets(&image, &FIELDS_T, &anchors()).unwrap_err();
    assert_eq!(failures[0].problem, AnchorProblem::Negative { found: -8 });
}

const LOAD_GLOBAL: &str = "48 8B 05 ?? ?? ?? ?? 48 8B D9";

#[test]
fn globals_are_where_the_instruction_points() {
    let mut image = Buffer::new();
    // rva 0x1400 + 7 + 0x100 = 0x1507
    image.place(0x1400, LOAD_GLOBAL, &0x100u32.to_le_bytes());
    let sites = Resolved {
        sites: vec![ResolvedSite {
            name: "LOAD",
            rva: 0x1400,
            moved: 0,
        }],
        scanned: false,
    };
    let global = GlobalRef {
        name: "G",
        site: 0,
        insn_at: 0,
        disp_at: 3,
        insn_len: 7,
    };
    assert_eq!(resolve_globals(&image, &sites, &[global]), Ok(vec![0x1507]));
}

#[test]
fn the_generated_tables_are_consistent() {
    check_tables(&SITES, &GLOBALS, &FIELDS, &ANCHORS).unwrap();
}

#[test]
fn the_generated_site_table_passes_core_validation() {
    // An empty image: every site is missing, none is malformed.
    let image = Buffer {
        bytes: vec![0; 0x100],
        text: 0..0x100,
    };
    let Err(mhw_core::sig::GateFailure::Sites(problems)) = resolve_all(&image, &SITES) else {
        panic!("an empty image cannot resolve");
    };
    assert_eq!(problems.len(), SITES.len());
    assert!(problems
        .iter()
        .all(|p| p.problem == mhw_core::sig::Problem::Missing));
}

#[test]
fn the_generated_anchors_hold_the_known_offsets_in_their_own_patterns() {
    // Where the displacement was not wildcarded it is the known value.
    for anchor in ANCHORS.iter() {
        let pattern = Pattern::parse(anchor.pattern).unwrap();
        let disp = &pattern.bytes()[anchor.disp_at..anchor.disp_at + anchor.disp_size];
        if disp.iter().all(Option::is_some) {
            let bytes: Vec<u8> = disp.iter().map(|b| b.unwrap()).collect();
            let value = displacement(&bytes, 0, anchor.disp_size).unwrap();
            assert_eq!(
                value as usize, FIELDS[anchor.field].known,
                "{}",
                anchor.pattern
            );
        }
    }
}
