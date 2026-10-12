//! Dev-build control server for mhw-dlss (`docs/DEV-PROTOCOL.md`,
//! `docs/ARCHITECTURE.md` § mhw-dev): a TCP JSON-lines server that lets a
//! client inspect and drive the running game — state, live settings,
//! memory, watches, the log, captures and probes.
//!
//! The crate exists only in dev builds (`mhw-dlss/dev` makes it a
//! dependency); a release build contains none of it. The mod calls
//! [`start`] once and the four execution points every frame; each costs one
//! atomic load when nothing is queued for it.
//!
//! Pure parts (framing, requests, expressions, the write whitelist, settings
//! staging, probe arithmetic) are `cfg`-free and host-tested; the server
//! itself runs on any host against a [`Host`] and the game abstraction.

#![warn(missing_docs)]

pub mod analysis;
pub mod exec;
pub mod expr;
pub mod frame;
pub mod game;
pub mod logline;
pub mod modules;
pub mod proto;
pub mod settings;
pub mod symbols;
pub mod token;
pub mod trace;

mod server;

#[cfg(windows)]
mod capture_glue;
#[cfg(windows)]
mod live;

use std::ffi::c_void;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use mhw_core::config::{Diagnostic, Settings};
use mhw_core::frame::{CameraFrame, FrameInfo, JitterSample};

/// In every dev build and in no release build; CI checks the DLLs for it.
#[used]
pub static DEV_BUILD_MARKER: &[u8] = b"MHW_DLSS_DEV_BUILD";

/// Where the server listens and keeps its files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevConfig {
    /// The mod folder: relative paths below resolve against it.
    pub mod_folder: PathBuf,
    /// `[Dev] Listen`.
    pub listen: SocketAddr,
    /// `[Dev] TokenFile`.
    pub token_file: PathBuf,
    /// `[Dev] CaptureDir` at start; the live setting is read again for
    /// each capture.
    pub capture_dir: PathBuf,
}

/// What the dev server needs from the mod (implemented by mhw-dlss).
pub trait Host: Send + Sync {
    /// The settings in force.
    fn settings(&self) -> Settings;
    /// Apply one setting like the file would (validation, live/restart).
    /// `Ok(None)` applied; `Ok(Some(d))` applied with a note; `Err(d)`
    /// rejected.
    fn set_setting(
        &self,
        section: &str,
        key: &str,
        value: &str,
    ) -> Result<Option<Diagnostic>, Diagnostic>;
    /// Free-form JSON object describing policy and per-frame state the mod
    /// owns (render/display size, scale, path, FG on, last reset reason, …).
    fn state(&self) -> serde_json::Value;
}

/// One resource the upscale slot works on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotResource {
    /// `colour`, `depth`, `mv`, `mv_raw`, `pass_depth`, `dlss_out`,
    /// `hudless`, `game_mv`, `game_depth`.
    pub name: &'static str,
    /// `ID3D12Resource*`.
    pub resource: *mut c_void,
    /// Its `D3D12_RESOURCE_STATES` on the slot's list now.
    pub state: u32,
}

/// The upscale slot, as [`on_slot`] sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct SlotContext {
    /// The frame being upscaled.
    pub frame: FrameInfo,
    /// That frame's camera, when it had a usable one.
    pub camera: Option<CameraFrame>,
    /// That frame's jitter. Not the newest camera's: the camera setup runs
    /// on the main thread, ahead of the slot.
    pub jitter: Option<JitterSample>,
    /// The resources available now.
    pub resources: Vec<SlotResource>,
    /// Resources not offered at this slot, each with why (e.g. `mv_raw`:
    /// "the temporal pass did not run this frame"). A capture or probe
    /// asking for one answers `not_ready` with the reason.
    pub absent: Vec<(&'static str, &'static str)>,
}

static STARTED: AtomicBool = AtomicBool::new(false);
static SERVER: OnceLock<Arc<server::Shared>> = OnceLock::new();

/// Start the server: token file, listener thread, module watcher. Once per
/// process; a second call fails with `AlreadyExists`.
pub fn start(config: DevConfig, host: &'static dyn Host) -> io::Result<()> {
    std::hint::black_box(DEV_BUILD_MARKER);
    if STARTED.swap(true, Ordering::AcqRel) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "dev server already started",
        ));
    }
    let host: Arc<dyn Host> = Arc::new(StaticHost(host));
    #[cfg(windows)]
    let live = Arc::new(live::Live::new());
    #[cfg(windows)]
    let game: Arc<dyn game::Game> = live.clone();
    #[cfg(not(windows))]
    let game: Arc<dyn game::Game> = Arc::new(game::Absent);
    match server::Shared::bind(config, host, game) {
        Ok(shared) => {
            #[cfg(windows)]
            live.start_watcher();
            let _ = SERVER.set(shared);
            Ok(())
        }
        Err(error) => {
            STARTED.store(false, Ordering::Release);
            Err(error)
        }
    }
}

/// The mod's `&'static` host where the server keeps an `Arc<dyn Host>`.
struct StaticHost(&'static dyn Host);

impl Host for StaticHost {
    fn settings(&self) -> Settings {
        self.0.settings()
    }
    fn set_setting(
        &self,
        section: &str,
        key: &str,
        value: &str,
    ) -> Result<Option<Diagnostic>, Diagnostic> {
        self.0.set_setting(section, key, value)
    }
    fn state(&self) -> serde_json::Value {
        self.0.state()
    }
}

/// Execution point `frame`: start of a main-loop iteration.
pub fn on_frame(frame: u64) {
    if let Some(s) = SERVER.get() {
        s.on_frame(frame);
    }
}

/// Execution point `camera`: the camera detour, after the original.
pub fn on_camera(camera: &CameraFrame, jitter: &JitterSample) {
    if let Some(s) = SERVER.get() {
        s.on_camera(camera, jitter);
    }
}

/// Execution point `slot`: the upscale slot with the engine's command list
/// open.
///
/// # Safety
/// `list` must be the engine's open `ID3D12GraphicsCommandList` and every
/// resource in `slot` live and in the stated state on it, for the duration
/// of the call.
pub unsafe fn on_slot(list: *mut c_void, slot: &SlotContext) {
    if let Some(s) = SERVER.get() {
        s.on_slot(list, slot);
    }
}

/// Execution point `present`: before Present.
pub fn on_present(frame: u64) {
    if let Some(s) = SERVER.get() {
        s.on_present(frame);
    }
}

/// Timeline event for `trace` (camera, evaluate, tags, markers, …); the
/// performance counter is read inside, and only while a `trace` runs.
pub fn trace_event(name: &'static str, frame: u64) {
    if let Some(s) = SERVER.get() {
        s.trace.record(name, frame, trace::clock::now);
    }
}
