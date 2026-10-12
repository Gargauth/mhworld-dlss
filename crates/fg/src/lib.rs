//! DLSS frame generation through Streamline 2.14.1; see
//! `docs/ARCHITECTURE.md`, section `mhw-fg`, and this crate's `README.md`.
//!
//! Pure, host-tested modules: [`abi`] (the C ABI and its layout),
//! [`constants`] (`sl::Constants` from `mhw_core` camera data), [`token`]
//! (frame-token and marker discipline), [`mod@options`] (DLSS-G/Reflex
//! requests and diffing), [`pacing`] (sleep point, frame limit, wait latch),
//! [`setup`] (plugin folder, identity, flags, signer), [`tags`] and
//! [`mod@status`].
//!
//! Windows-only, re-exported at the crate root: loading and initialising
//! Streamline (`start`), the factory/swapchain interception
//! (`upgrade_factory`) and the per-frame glue (`submit_frame`, `tag_late`,
//! `render_submit`, `frame_start`), options (`set_options`) and
//! introspection (`status()`).

#![warn(missing_docs)]

pub mod abi;
pub mod constants;
pub mod options;
pub mod pacing;
pub mod setup;
pub mod status;
pub mod tags;
pub mod token;

#[cfg(windows)]
mod frame;
#[cfg(windows)]
mod sl;
#[cfg(windows)]
mod swap;
#[cfg(windows)]
mod trust;

pub use constants::{ConstantsError, MotionVectors};
pub use options::{OffReason, Options};
pub use pacing::{wait_latch_wanted, FrameLimit, SleepPoint};
pub use setup::{Identity, Refusal, SlLogLevel};
pub use status::{ChainInfo, ConstantsSummary, DlssgSample, InitState, Status};
pub use tags::{TagKind, TagResource};

#[cfg(all(windows, feature = "dev"))]
pub use frame::last_constants;
#[cfg(windows)]
pub use frame::{
    frame_start, note_reset, options, owns_swapchain, render_submit, set_options, status,
    submit_frame, tag_late, FrameInputs, LateKind, Submitted,
};
#[cfg(windows)]
pub use sl::{faulted, init_state, ready, shutdown, stand_down, start, wait_settled, StartConfig};
#[cfg(windows)]
pub use swap::{chain, upgrade_factory};

/// Whether the mod should set the engine's wait-helper latch now, under
/// `policy` ([`wait_latch_wanted`] with [`owns_swapchain`]). The latch is
/// permanent once set, so the mod calls this when the swapchain exists
/// (after the first Present) and on policy changes.
#[cfg(windows)]
pub fn wait_latch(policy: mhw_core::config::WaitLatch) -> bool {
    wait_latch_wanted(policy, owns_swapchain())
}
