//! The game: address book, struct views and detours on game functions.
//! See `docs/ARCHITECTURE.md`, section `mhw-engine`, and `README.md`.
//!
//! Pure, host-tested modules: [`sites`] (generated tables), [`anchors`],
//! [`gate`], [`renderer`], [`ratio`], [`mipbias`], [`viewport`],
//! [`params`], [`upscale`], [`status`], [`layout`]. Everything that
//! touches the process is `#[cfg(windows)]`:
//!
//! 1. [`check`] the game module (renderer files, build, sites, globals,
//!    offsets) — all or nothing;
//! 2. [`set_params`], [`set_upscale_handler`], optionally
//!    [`set_depth_alias`];
//! 3. [`install`] the eight detours at once.
//!
//! The camera setup then publishes `mhw_core::bus::CameraReady` once per
//! render frame, the DLSS evaluations call the handler, and the tuning is
//! held every frame. [`status`] and friends read it all back.

#![warn(missing_docs)]

pub mod anchors;
pub mod gate;
pub mod layout;
pub mod mipbias;
pub mod params;
pub mod ratio;
pub mod renderer;
#[rustfmt::skip]
pub mod sites;
pub mod status;
pub mod upscale;
pub mod viewport;

#[cfg(windows)]
mod api;
#[cfg(all(windows, feature = "dev"))]
pub mod dev;
#[cfg(windows)]
mod hooks;
#[cfg(windows)]
mod object;
#[cfg(windows)]
mod runtime;
#[cfg(windows)]
mod tune;
#[cfg(windows)]
pub mod views;

pub use gate::{Addresses, StandDown};
pub use params::{DlssFlags, Params};
pub use renderer::{Dx11Reason, Renderer};
pub use status::{Counters, MipBias, MipBiasOutcome, Status, TuneState, WaitState};
pub use upscale::{
    is_stand_in_handle, ImageSignalCall, Rect, SupersamplingCall, UpscaleHandler,
    FAKE_IMAGE_SIGNAL, FAKE_SUPERSAMPLING,
};

#[cfg(windows)]
pub use api::{
    addresses, check, counters, detect_renderer, gate_result, hold_downsample_volume, install,
    is_installed, last_camera, last_jitter, maintain, params, set_depth_alias, set_params,
    set_upscale_handler, set_wait_latch, status, take_game_reset, tune_state, wait_state,
    InstallError, InstallOptions, Installed, ModuleImage,
};
