//! mhw-dlss: DLSS 4 upscaling and DLSS frame generation for Monster Hunter:
//! World (DirectX 12). The cdylib: start-up, wiring between the crates, and
//! policy. See `docs/ARCHITECTURE.md` (section *mhw-dlss*) and this crate's
//! `README.md`.
//!
//! Pure, host-tested modules: [`policy`] (settings → every subsystem),
//! [`reset`] (when history is discarded), [`fallback`] (giving DLSS up and
//! retrying), [`frames`] (cameras by frame, references held for the GPU),
//! [`store`] (settings in force, live vs restart), [`ini_edit`] and
//! [`page`] (the overlay's file edits). Everything else is Windows-only.

#![warn(missing_docs)]

pub mod fallback;
pub mod frames;
pub mod ini_edit;
pub mod page;
pub mod policy;
pub mod reset;
pub mod slotnames;
pub mod store;

#[cfg(windows)]
mod sys;
