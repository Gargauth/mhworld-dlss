//! NGX for mhw-dlss: the `NVSDK_NGX_Parameter` ABI, the session (the game's
//! NGX entry points, the re-issued `Init`, host detection, the availability
//! answer) and the DLSS-SR feature lifecycle. See `docs/ARCHITECTURE.md`,
//! section `mhw-ngx`, and `crates/ngx/README.md`.
//!
//! Pure, host-tested modules: [`param`] (slots, typed values, names),
//! [`status`], [`dlss`] (feature description, evaluation mapping, stand-in
//! handles), [`init`] (init planning, capability answer), [`host`],
//! [`retire`], [`budget`], [`version`]. Windows-only: `session` and
//! `feature`.
//!
//! Log targets: `ngx` (session) and `dlss` (feature).

pub mod budget;
pub mod dlss;
pub mod host;
pub mod init;
pub mod param;
pub mod retire;
pub mod status;
pub mod version;

#[cfg(windows)]
pub mod feature;
#[cfg(windows)]
pub mod session;

pub use dlss::{
    CreateFlags, EvalParams, FeatureDesc, Offset, ParamError, Quality, RawInputs, RenderPreset,
    Subrects,
};
pub use init::{Availability, GameInit, Identity, InitAction, InitOptions, InitTail};
pub use status::Status;
pub use version::{FileVersion, VersionInfo};

/// Log target of the session.
pub const TARGET_NGX: &str = "ngx";
/// Log target of the feature.
pub const TARGET_DLSS: &str = "dlss";

/// An address or id shown as `0x…` in a log field.
#[cfg(windows)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct Hex(pub usize);

#[cfg(windows)]
impl std::fmt::Display for Hex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}
