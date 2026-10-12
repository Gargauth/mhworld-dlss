//! Direct3D 12 interception, tracking and temporal-pass inputs for mhw-dlss.
//!
//! See `docs/ARCHITECTURE.md`, section `mhw-render`, and this crate's
//! `README.md`.
//!
//! Pure logic lives in `cfg`-free modules tested on any host:
//! [`dxbc`] reflection, [`recognise`] (which pass a shader is), [`stream`]
//! (pipeline state streams), [`rootsig`] layout arithmetic, [`states`]
//! (resource states in submission order), [`descriptors`] (tables back to
//! resources), [`upload`] (GPU addresses back to CPU bytes), [`depth`]
//! (scene depth selection), [`mvpack`] (the packed motion vector format and
//! its decode), [`taa`] (where the temporal pass binds what), [`inputs`]
//! (the per-frame hand-over) and [`signature`] (input signatures and the
//! vertex shader that feeds a pixel shader).
//!
//! The crate's own passes are public modules whose pure part (constants,
//! shaders, reference math, decision logic) is host-tested and whose
//! Direct3D part is re-exported from the Windows-only code: [`sharpen`]
//! (RCAS after DLSS), [`hudless`] (the HUD-less image for frame generation)
//! and, with feature `dev`, `capture` (readback and colour statistics).
//!
//! Everything else that touches Direct3D lives in a Windows-only module
//! whose items are re-exported at the crate root.

#![warn(missing_docs)]

pub mod addrmap;
#[cfg(feature = "dev")]
pub mod capture;
pub mod depth;
pub mod descriptors;
pub mod dxbc;
pub mod hudless;
pub mod inputs;
pub mod mvpack;
pub mod names;
mod proxystate;
pub mod recognise;
pub mod rootsig;
pub mod sharpen;
pub mod signature;
pub mod states;
pub mod stream;
pub mod taa;
pub mod tracked;
pub mod upload;

#[cfg(windows)]
mod sys;

pub use inputs::{Association, CameraCb, DlssInputs, TextureRef};
pub use mvpack::{DecodeParams, YMask};
pub use recognise::PassKind;

#[cfg(windows)]
pub use sys::*;
