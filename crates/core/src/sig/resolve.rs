//! Resolving the site table against an image, all or nothing.

use std::fmt;
use std::ops::Range;

use super::pattern::{scan_up_to, Pattern, PatternError};

/// The section patterns are searched in.
pub const CODE_SECTION: &str = ".text";

/// How many hits an ambiguous site reports (the count is exact up to this).
const HITS_REPORTED: usize = 4;

/// A loaded or file-backed executable image, addressed by RVA.
///
/// Implemented by `mhw-win` for the live module and by tests for byte
/// buffers.
pub trait Image {
    /// The RVA range of the section `name` (e.g. `.text`).
    fn section(&self, name: &str) -> Option<Range<usize>>;
    /// `len` bytes at `rva`; `None` if any of them is outside the image or
    /// not readable.
    fn read(&self, rva: usize, len: usize) -> Option<&[u8]>;
}

/// One place in the game's code the mod depends on.
///
/// The table is generated from `docs/re/signatures.json` into the engine
/// crate. `pattern` finds the place when the build moved it; `verify` is
/// what must be there, which is deliberately strict: a prologue whose frame
/// size changed is a function whose semantics may have changed.
///
/// Comparison of `verify`:
/// * at `known_rva` (fast path): every byte except `wild`;
/// * at a scanned site: additionally not the bytes the pattern has as
///   wildcards (RIP displacements and branch offsets, which legitimately
///   change when code moves).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Site {
    /// Name in `signatures.json` (e.g. `CAMERA_SETUP`).
    pub name: &'static str,
    /// Unique pattern in `.text` (see [`Pattern::parse`]).
    pub pattern: &'static str,
    /// From the pattern match to the site.
    pub to_target: isize,
    /// The site's RVA in the known build (fast path).
    pub known_rva: usize,
    /// The bytes that must be at the site.
    pub verify: &'static [u8],
    /// Byte range of `verify` that is never compared (e.g. a displacement
    /// the mod itself rewrites). Empty for none.
    pub wild: Range<usize>,
}

impl Site {
    /// Whether `found` (at least `verify.len()` bytes) is what the site
    /// expects at its known RVA: every byte except `wild`.
    pub fn verifies(&self, found: &[u8]) -> bool {
        self.verifies_except(found, |_| false)
    }

    /// Whether `found` is what the site expects at a place found by
    /// scanning for `pattern`: bytes under the pattern's wildcards are not
    /// compared either.
    pub fn verifies_scanned(&self, found: &[u8], pattern: &Pattern) -> bool {
        self.verifies_except(found, |index| {
            index
                .checked_add_signed(self.to_target)
                .and_then(|at| pattern.bytes().get(at))
                .is_some_and(Option::is_none)
        })
    }

    fn verifies_except(&self, found: &[u8], skip: impl Fn(usize) -> bool) -> bool {
        found.len() >= self.verify.len()
            && self
                .verify
                .iter()
                .zip(found)
                .enumerate()
                .all(|(index, (want, have))| {
                    want == have || self.wild.contains(&index) || skip(index)
                })
    }

    fn check_definition(&self) -> Result<Pattern, Problem> {
        if self.verify.is_empty() {
            return Err(Problem::BadSite("empty verify bytes"));
        }
        if !self.wild.is_empty() && self.wild.end > self.verify.len() {
            return Err(Problem::BadSite("wild range outside verify bytes"));
        }
        Pattern::parse(self.pattern).map_err(Problem::BadPattern)
    }
}

/// Where one site was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedSite {
    /// [`Site::name`].
    pub name: &'static str,
    /// Where the site is in this image.
    pub rva: usize,
    /// `rva − known_rva`: zero in the known build.
    pub moved: isize,
}

/// Every site, in table order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// One per table entry, in table order.
    pub sites: Vec<ResolvedSite>,
    /// Whether `.text` had to be scanned (the fast path did not hold).
    pub scanned: bool,
}

impl Resolved {
    /// The RVA of the site called `name`.
    pub fn rva(&self, name: &str) -> Option<usize> {
        self.sites
            .iter()
            .find(|site| site.name == name)
            .map(|site| site.rva)
    }
}

/// What is wrong with one site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// The table entry itself is inconsistent.
    BadSite(&'static str),
    /// The pattern text does not parse.
    BadPattern(PatternError),
    /// The pattern does not occur in `.text`.
    Missing,
    /// The pattern occurs more than once.
    Ambiguous {
        /// Matches found (exact up to 4; the scan stops there).
        count: usize,
        /// The first matches' RVAs.
        rvas: Vec<usize>,
    },
    /// The site computed from the match is outside the image.
    Unreadable {
        /// The computed site.
        rva: usize,
    },
    /// The bytes at the site are not the expected ones.
    Differs {
        /// The site.
        rva: usize,
        /// What is there (`verify.len()` bytes).
        found: Vec<u8>,
    },
}

/// One site that did not resolve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiteProblem {
    /// [`Site::name`].
    pub site: &'static str,
    /// What is wrong.
    pub problem: Problem,
}

/// Why the site table did not resolve. Nothing may be installed then.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateFailure {
    /// The image has no readable code section.
    NoCode,
    /// These sites failed (every other one resolved).
    Sites(Vec<SiteProblem>),
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl fmt::Display for SiteProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: ", self.site)?;
        match &self.problem {
            Problem::BadSite(why) => write!(f, "bad table entry ({why})"),
            Problem::BadPattern(error) => write!(f, "bad pattern ({error})"),
            Problem::Missing => f.write_str("pattern not found"),
            Problem::Ambiguous { count, rvas } => {
                let more = if *count >= HITS_REPORTED { "+" } else { "" };
                write!(f, "pattern found {count}{more} times at")?;
                rvas.iter().try_for_each(|rva| write!(f, " {rva:#x}"))
            }
            Problem::Unreadable { rva } => write!(f, "site {rva:#x} outside the image"),
            Problem::Differs { rva, found } => {
                write!(f, "bytes at {rva:#x} differ: {}", hex(found))
            }
        }
    }
}

impl fmt::Display for GateFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GateFailure::NoCode => f.write_str("no readable .text section"),
            GateFailure::Sites(problems) => {
                for (index, problem) in problems.iter().enumerate() {
                    if index > 0 {
                        f.write_str("; ")?;
                    }
                    write!(f, "{problem}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for GateFailure {}

/// Resolve every site, or none.
///
/// The table entries are checked first (patterns parse, `verify` is
/// non-empty, `wild` lies inside it), whichever path is taken, so a broken
/// entry is found in the known build and not only after a game update.
///
/// Fast path: if every site's `verify` bytes are at its `known_rva`, done
/// without scanning. Otherwise `.text` is scanned for every pattern: each
/// must match exactly once, and the site (`match + to_target`) must verify.
/// Any failure fails the whole table, listing every failing site.
///
/// Scan before installing anything: a detour overwrites the bytes its own
/// pattern looks for.
pub fn resolve_all<I: Image + ?Sized>(image: &I, sites: &[Site]) -> Result<Resolved, GateFailure> {
    let mut patterns = Vec::with_capacity(sites.len());
    let mut problems = Vec::new();
    for site in sites {
        match site.check_definition() {
            Ok(pattern) => patterns.push(pattern),
            Err(problem) => problems.push(SiteProblem {
                site: site.name,
                problem,
            }),
        }
    }
    if !problems.is_empty() {
        return Err(GateFailure::Sites(problems));
    }

    let fast = sites.iter().all(|site| {
        image
            .read(site.known_rva, site.verify.len())
            .is_some_and(|found| site.verifies(found))
    });
    if fast {
        return Ok(Resolved {
            sites: sites
                .iter()
                .map(|site| ResolvedSite {
                    name: site.name,
                    rva: site.known_rva,
                    moved: 0,
                })
                .collect(),
            scanned: false,
        });
    }

    let code_range = image.section(CODE_SECTION).ok_or(GateFailure::NoCode)?;
    let code = image
        .read(code_range.start, code_range.len())
        .ok_or(GateFailure::NoCode)?;
    let mut resolved = Vec::with_capacity(sites.len());
    for (site, pattern) in sites.iter().zip(&patterns) {
        match locate(image, code_range.start, code, site, pattern) {
            Ok(rva) => resolved.push(ResolvedSite {
                name: site.name,
                rva,
                moved: (rva as isize).wrapping_sub(site.known_rva as isize),
            }),
            Err(problem) => problems.push(SiteProblem {
                site: site.name,
                problem,
            }),
        }
    }
    if problems.is_empty() {
        Ok(Resolved {
            sites: resolved,
            scanned: true,
        })
    } else {
        Err(GateFailure::Sites(problems))
    }
}

fn locate<I: Image + ?Sized>(
    image: &I,
    code_rva: usize,
    code: &[u8],
    site: &Site,
    pattern: &Pattern,
) -> Result<usize, Problem> {
    let hits = scan_up_to(code, pattern, HITS_REPORTED);
    let at = match hits.as_slice() {
        [] => return Err(Problem::Missing),
        [only] => code_rva + only,
        _ => {
            return Err(Problem::Ambiguous {
                count: hits.len(),
                rvas: hits.iter().map(|hit| code_rva + hit).collect(),
            })
        }
    };
    let rva = at
        .checked_add_signed(site.to_target)
        .ok_or(Problem::Unreadable { rva: at })?;
    let found = image
        .read(rva, site.verify.len())
        .ok_or(Problem::Unreadable { rva })?;
    if site.verifies_scanned(found, pattern) {
        Ok(rva)
    } else {
        Err(Problem::Differs {
            rva,
            found: found.to_vec(),
        })
    }
}
