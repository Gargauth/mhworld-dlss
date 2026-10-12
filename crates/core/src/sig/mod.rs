//! Signature scanning over byte slices, and the all-or-nothing resolution
//! of the game's code sites (`docs/re/ReSignatures.md`).
//!
//! * [`Pattern::parse`], [`scan`], [`rip_target`], [`displacement`]: pure
//!   byte work.
//! * [`resolve_all`]: the gate's question "is every site the mod depends on
//!   where and what it should be?", over any [`Image`].

pub mod pattern;
pub mod resolve;

pub use self::pattern::{displacement, rip_target, scan, scan_up_to, Pattern, PatternError};
pub use self::resolve::{
    resolve_all, GateFailure, Image, Problem, Resolved, ResolvedSite, Site, SiteProblem,
    CODE_SECTION,
};

#[cfg(test)]
mod tests;
