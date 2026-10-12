//! The gate: is this the game, on DirectX 12, with every site, global and
//! struct offset the mod depends on where it should be? All or nothing.
//!
//! Whichever build the executable is: what decides is the code. Every site
//! must match its signature exactly once and verify, every global must be
//! read out of a site, and every anchored offset out of the code, unmoved
//! where unanchored offsets of the same object depend on it
//! ([`crate::anchors`]).
//!
//! [`evaluate`] is pure (any [`Image`]); the Windows entry point
//! [`check`](crate::check) adapts the live module and reads the game's
//! renderer files. Nothing may be installed unless it returns
//! [`Addresses`].

use std::fmt;

use mhw_core::sig::{resolve_all, GateFailure, Image};

use crate::anchors::{derive_offsets, resolve_globals, AnchorFailure};
use crate::renderer::{Dx11Reason, Renderer};
use crate::sites::{field, global, site, ANCHORS, FIELDS, GLOBALS, SITES};

/// Everything the gate resolved: where each site, global and anchored
/// field is in this process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Addresses {
    /// Image base.
    pub base: usize,
    /// Whether `.text` had to be scanned for the sites.
    pub scanned: bool,
    sites: [usize; site::COUNT],
    globals: [usize; global::COUNT],
    fields: [usize; field::COUNT],
}

impl Addresses {
    /// Absolute address of site `index` ([`crate::sites::site`]).
    pub fn site(&self, index: usize) -> usize {
        self.base + self.sites[index]
    }

    /// RVA of site `index`.
    pub fn site_rva(&self, index: usize) -> usize {
        self.sites[index]
    }

    /// Absolute address of global `index` ([`crate::sites::global`]): the
    /// slot holding the object pointer, or the constant itself.
    pub fn global(&self, index: usize) -> usize {
        self.base + self.globals[index]
    }

    /// Offset of field `index` ([`crate::sites::field`]).
    pub fn field(&self, index: usize) -> usize {
        self.fields[index]
    }

    /// `(name, absolute address)` of every site.
    pub fn sites(&self) -> impl Iterator<Item = (&'static str, usize)> + '_ {
        SITES
            .iter()
            .zip(self.sites)
            .map(|(s, rva)| (s.name, self.base + rva))
    }

    /// `(name, absolute address)` of every global.
    pub fn globals(&self) -> impl Iterator<Item = (&'static str, usize)> + '_ {
        GLOBALS
            .iter()
            .zip(self.globals)
            .map(|(g, rva)| (g.name, self.base + rva))
    }

    /// `(name, offset)` of every anchored field.
    pub fn fields(&self) -> impl Iterator<Item = (&'static str, usize)> + '_ {
        FIELDS
            .iter()
            .zip(self.fields)
            .map(|(f, offset)| (f.name, offset))
    }
}

/// Why the mod stands down. Nothing is installed in any case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StandDown {
    /// The game will start on DirectX 11.
    NotDx12(Dx11Reason),
    /// Sites did not resolve.
    Sites(GateFailure),
    /// Globals did not resolve.
    Globals(Vec<AnchorFailure>),
    /// Struct offsets did not derive, disagree, or moved under unanchored
    /// ones.
    Offsets(Vec<AnchorFailure>),
}

impl StandDown {
    /// Whether asking again later may succeed: the code may still be
    /// packed (a retail executable is unpacked by its stub before its entry
    /// point). `mod` decides how long to keep asking.
    pub fn retryable(&self) -> bool {
        !matches!(self, StandDown::NotDx12(_))
    }
}

fn list(f: &mut fmt::Formatter<'_>, failures: &[AnchorFailure]) -> fmt::Result {
    for (index, failure) in failures.iter().enumerate() {
        if index > 0 {
            f.write_str("; ")?;
        }
        write!(f, "{failure}")?;
    }
    Ok(())
}

impl fmt::Display for StandDown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StandDown::NotDx12(why) => write!(f, "the game starts on DirectX 11: {why}"),
            StandDown::Sites(failure) => write!(f, "code sites: {failure}"),
            StandDown::Globals(failures) => {
                f.write_str("globals: ")?;
                list(f, failures)
            }
            StandDown::Offsets(failures) => {
                f.write_str("struct offsets: ")?;
                list(f, failures)
            }
        }
    }
}

impl std::error::Error for StandDown {}

/// The whole gate over `image`, loaded at `base`.
///
/// The renderer first (cheap, and final); then every site
/// ([`resolve_all`]), every global and every anchored field.
pub fn evaluate<I: Image + ?Sized>(
    image: &I,
    base: usize,
    renderer: &Renderer,
) -> Result<Addresses, StandDown> {
    if let Renderer::Dx11(why) = renderer {
        return Err(StandDown::NotDx12(why.clone()));
    }
    let resolved = resolve_all(image, &SITES).map_err(StandDown::Sites)?;
    let globals = resolve_globals(image, &resolved, &GLOBALS).map_err(StandDown::Globals)?;
    let fields = derive_offsets(image, &FIELDS, &ANCHORS).map_err(StandDown::Offsets)?;
    let mut addresses = Addresses {
        base,
        scanned: resolved.scanned,
        sites: [0; site::COUNT],
        globals: [0; global::COUNT],
        fields: [0; field::COUNT],
    };
    // The resolvers return one value per table row, in order.
    for (slot, found) in addresses.sites.iter_mut().zip(&resolved.sites) {
        *slot = found.rva;
    }
    for (slot, found) in addresses.globals.iter_mut().zip(globals) {
        *slot = found;
    }
    for (slot, found) in addresses.fields.iter_mut().zip(fields) {
        *slot = found;
    }
    Ok(addresses)
}

#[cfg(test)]
#[path = "gate/tests.rs"]
mod tests;
