//! Globals and struct offsets read out of the game's own instructions.
//!
//! Pure: works on any [`Image`], so the derivation is tested on byte buffers.
//! The tables are generated into [`crate::sites`].
//!
//! * A [`GlobalRef`] names a RIP-relative instruction inside a resolved gate
//!   site; the global is where it points.
//! * An [`OffsetAnchor`] is a pattern whose match holds a field's
//!   displacement. Most anchor patterns have that displacement wildcarded, so
//!   a field that moved in another build is still found and its new offset
//!   read. A field can have several anchors; they must agree.
//!
//! Nothing here depends on which build the image is. The one value compared
//! with the tables is a field's offset when its object also has unanchored
//! offsets ([`FieldInfo::layout`], [`crate::layout`]): those were found with
//! the field where the tables have it, so a field that moved means the
//! object's layout changed under them, and the field fails
//! ([`AnchorProblem::Moved`]). Fields of objects without unanchored offsets
//! follow the code.

use std::fmt;

use mhw_core::sig::{
    displacement, rip_target, scan_up_to, Image, Pattern, Resolved, Site, CODE_SECTION,
};

/// A global loaded by a RIP-relative instruction inside a gate site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlobalRef {
    /// Short name (e.g. `RENDER_CONFIG`).
    pub name: &'static str,
    /// Index of the site in the site table.
    pub site: usize,
    /// Offset of the instruction from the site.
    pub insn_at: usize,
    /// Offset of its 32-bit displacement from the site.
    pub disp_at: usize,
    /// Length of the instruction (the displacement is relative to its end).
    pub insn_len: usize,
}

/// One anchored struct offset, as the table names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldInfo {
    /// Name in `signatures.json` (e.g. `cfg+0x198`).
    pub name: &'static str,
    /// The offset in the reference build.
    pub known: usize,
    /// Whether [`crate::layout`] has unanchored offsets of the same object,
    /// found with this field at `known`: then it must read `known`.
    pub layout: bool,
    /// The engine's reflection name of the field, or empty.
    pub reflection: &'static str,
}

/// A pattern whose match holds a field's displacement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OffsetAnchor {
    /// Index of the field in the field table.
    pub field: usize,
    /// Unique pattern in `.text`.
    pub pattern: &'static str,
    /// Offset of the displacement from the match.
    pub disp_at: usize,
    /// Size of the displacement: 1 or 4 bytes, sign-extended.
    pub disp_size: usize,
    /// Where the pattern matches in the reference build (looked at first).
    pub known_rva: usize,
}

/// What went wrong with one global or field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnchorProblem {
    /// The anchor's pattern does not parse or its displacement lies outside
    /// it (a broken table).
    BadAnchor,
    /// The pattern does not occur in `.text`.
    Missing,
    /// The pattern occurs more than once.
    Ambiguous {
        /// Hits found (up to 4).
        count: usize,
    },
    /// The displacement could not be read.
    Unreadable {
        /// Where.
        rva: usize,
    },
    /// A field whose object has unanchored offsets ([`crate::layout`]) is
    /// not where they were found: the object's layout changed.
    Moved {
        /// What the image says.
        found: usize,
        /// What the tables and the unanchored offsets were made with.
        known: usize,
    },
    /// Anchors of one field read different values.
    Disagree {
        /// Every value read, in anchor order.
        found: Vec<usize>,
    },
    /// A displacement that is negative: not a field offset.
    Negative {
        /// The displacement.
        found: i32,
    },
}

/// One failing global or field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnchorFailure {
    /// The global's or field's name.
    pub name: &'static str,
    /// What is wrong.
    pub problem: AnchorProblem,
}

impl fmt::Display for AnchorFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: ", self.name)?;
        match &self.problem {
            AnchorProblem::BadAnchor => f.write_str("bad table entry"),
            AnchorProblem::Missing => f.write_str("anchor pattern not found"),
            AnchorProblem::Ambiguous { count } => write!(f, "anchor pattern found {count} times"),
            AnchorProblem::Unreadable { rva } => write!(f, "{rva:#x} not readable"),
            AnchorProblem::Moved { found, known } => write!(
                f,
                "reads {found:#x}, not {known:#x}: the object's layout changed under its \
                 unanchored offsets"
            ),
            AnchorProblem::Disagree { found } => {
                f.write_str("anchors disagree:")?;
                found.iter().try_for_each(|value| write!(f, " {value:#x}"))
            }
            AnchorProblem::Negative { found } => write!(f, "negative displacement {found}"),
        }
    }
}

/// Resolve every global from the resolved sites, in table order (RVAs).
pub fn resolve_globals<I: Image + ?Sized>(
    image: &I,
    sites: &Resolved,
    globals: &[GlobalRef],
) -> Result<Vec<usize>, Vec<AnchorFailure>> {
    let mut found = Vec::with_capacity(globals.len());
    let mut failures = Vec::new();
    for global in globals {
        let fail = |problem| AnchorFailure {
            name: global.name,
            problem,
        };
        let Some(site) = sites.sites.get(global.site) else {
            failures.push(fail(AnchorProblem::BadAnchor));
            continue;
        };
        let insn = site.rva + global.insn_at;
        let Some(bytes) = image.read(insn, global.insn_len) else {
            failures.push(fail(AnchorProblem::Unreadable { rva: insn }));
            continue;
        };
        let Some(target) = global
            .disp_at
            .checked_sub(global.insn_at)
            .and_then(|at| rip_target(insn, at, global.insn_len, bytes))
        else {
            failures.push(fail(AnchorProblem::BadAnchor));
            continue;
        };
        found.push(target);
    }
    if failures.is_empty() {
        Ok(found)
    } else {
        Err(failures)
    }
}

/// Derive every field's offset, in field-table order.
///
/// Each anchor is tried at its known RVA first; otherwise `.text` is scanned
/// for exactly one match. All or nothing: every failing field is listed.
pub fn derive_offsets<I: Image + ?Sized>(
    image: &I,
    fields: &[FieldInfo],
    anchors: &[OffsetAnchor],
) -> Result<Vec<usize>, Vec<AnchorFailure>> {
    let mut code: Option<Option<(usize, &[u8])>> = None;
    let mut read: Vec<Vec<Result<usize, AnchorProblem>>> = vec![Vec::new(); fields.len()];
    for anchor in anchors {
        let value = derive_one(image, anchor, &mut code);
        match read.get_mut(anchor.field) {
            Some(values) => values.push(value),
            None => {
                return Err(vec![AnchorFailure {
                    name: "anchor table",
                    problem: AnchorProblem::BadAnchor,
                }])
            }
        }
    }
    let mut offsets = Vec::with_capacity(fields.len());
    let mut failures = Vec::new();
    for (field, values) in fields.iter().zip(read) {
        match settle(field, values) {
            Ok(offset) => offsets.push(offset),
            Err(problem) => failures.push(AnchorFailure {
                name: field.name,
                problem,
            }),
        }
    }
    if failures.is_empty() {
        Ok(offsets)
    } else {
        Err(failures)
    }
}

fn settle(
    field: &FieldInfo,
    values: Vec<Result<usize, AnchorProblem>>,
) -> Result<usize, AnchorProblem> {
    let mut found = Vec::with_capacity(values.len());
    for value in values {
        found.push(value?);
    }
    let first = *found.first().ok_or(AnchorProblem::BadAnchor)?;
    if found.iter().any(|value| *value != first) {
        return Err(AnchorProblem::Disagree { found });
    }
    if field.layout && first != field.known {
        return Err(AnchorProblem::Moved {
            found: first,
            known: field.known,
        });
    }
    Ok(first)
}

fn derive_one<'a, I: Image + ?Sized>(
    image: &'a I,
    anchor: &OffsetAnchor,
    code: &mut Option<Option<(usize, &'a [u8])>>,
) -> Result<usize, AnchorProblem> {
    let pattern = Pattern::parse(anchor.pattern).map_err(|_| AnchorProblem::BadAnchor)?;
    let end = anchor.disp_at.checked_add(anchor.disp_size);
    if !matches!(anchor.disp_size, 1 | 4) || end.is_none_or(|end| end > pattern.len()) {
        return Err(AnchorProblem::BadAnchor);
    }
    let at = if image
        .read(anchor.known_rva, pattern.len())
        .is_some_and(|bytes| pattern.matches(bytes))
    {
        anchor.known_rva
    } else {
        let (base, text) = code
            .get_or_insert_with(|| {
                let range = image.section(CODE_SECTION)?;
                Some((range.start, image.read(range.start, range.len())?))
            })
            .ok_or(AnchorProblem::Missing)?;
        match scan_up_to(text, &pattern, 4).as_slice() {
            [] => return Err(AnchorProblem::Missing),
            [only] => base + only,
            more => return Err(AnchorProblem::Ambiguous { count: more.len() }),
        }
    };
    let rva = at + anchor.disp_at;
    let bytes = image
        .read(rva, anchor.disp_size)
        .ok_or(AnchorProblem::Unreadable { rva })?;
    let value =
        displacement(bytes, 0, anchor.disp_size).ok_or(AnchorProblem::Unreadable { rva })?;
    usize::try_from(value).map_err(|_| AnchorProblem::Negative { found: value })
}

/// Whether every row of the tables is internally consistent: patterns parse,
/// displacements lie inside them and globals point into existing sites.
/// (Checked by the tests; the generator checks the rest against the
/// executable.)
pub fn check_tables(
    sites: &[Site],
    globals: &[GlobalRef],
    fields: &[FieldInfo],
    anchors: &[OffsetAnchor],
) -> Result<(), String> {
    for global in globals {
        let site = sites
            .get(global.site)
            .ok_or_else(|| format!("{}: no site {}", global.name, global.site))?;
        if global.disp_at < global.insn_at || global.disp_at + 4 > global.insn_at + global.insn_len
        {
            return Err(format!(
                "{}: displacement outside the instruction",
                global.name
            ));
        }
        if global.insn_at + global.insn_len > Pattern::parse(site.pattern).map_or(0, |p| p.len()) {
            return Err(format!(
                "{}: instruction outside {}",
                global.name, site.name
            ));
        }
    }
    for (index, anchor) in anchors.iter().enumerate() {
        let pattern =
            Pattern::parse(anchor.pattern).map_err(|error| format!("anchor {index}: {error}"))?;
        if !matches!(anchor.disp_size, 1 | 4) || anchor.disp_at + anchor.disp_size > pattern.len() {
            return Err(format!("anchor {index}: displacement outside the pattern"));
        }
        if anchor.field >= fields.len() {
            return Err(format!("anchor {index}: no field {}", anchor.field));
        }
    }
    if let Some(field) = (0..fields.len()).find(|f| anchors.iter().all(|a| a.field != *f)) {
        return Err(format!("{} has no anchor", fields[field].name));
    }
    Ok(())
}

#[cfg(test)]
#[path = "anchors/tests.rs"]
mod tests;
