//! Read-only introspection: plain data for the log, the dev server and
//! tests. Produced by [`status`](crate::status) and friends.

use mhw_core::frame::{CameraFrame, JitterSample};

use crate::gate::{Addresses, StandDown};
use crate::params::Params;

/// How often each detour ran and what it did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Camera setup calls (every viewport).
    pub camera_setups: u64,
    /// Render frames seen by the camera setup.
    pub frames: u64,
    /// Jitter written to `sCamera+0x38/+0x3c`.
    pub jitter_writes: u64,
    /// `CameraReady` published.
    pub cameras_published: u64,
    /// Main viewports whose matrices were not a usable camera.
    pub cameras_unusable: u64,
    /// `GetOptimalSettings` answered by the mod.
    pub optimal_answers: u64,
    /// `CreateFeatures` answered by the mod.
    pub creates: u64,
    /// EvalSS calls handed to the handler.
    pub eval_ss: u64,
    /// EvalISP calls handed to the handler.
    pub eval_isp: u64,
    /// Evaluations with no handler registered.
    pub unhandled: u64,
    /// SS enqueues (one per TAA draw).
    pub enqueues: u64,
    /// SS enqueues not passed on (flags 2, or a missing target).
    pub enqueues_skipped: u64,
    /// Temporal passes on which the game signalled a history reset.
    pub game_resets: u64,
    /// Depth copies aliased (`+0xf0 := +0x60`).
    pub depth_aliases: u64,
    /// Engine fatal reports.
    pub fatal_reports: u64,
}

/// The mip-bias rewrite made at install.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MipBias {
    /// The bias bank 1 now has, in mip levels.
    pub bias: f32,
    /// The constant it is built from.
    pub constant: f32,
    /// Where that constant is.
    pub literal: usize,
}

/// What became of the mip bias asked for at install.
#[derive(Clone, Debug, PartialEq)]
pub enum MipBiasOutcome {
    /// None was asked for.
    NotRequested,
    /// The bias asked for is no bias (or out of range): bank 0 stays.
    NoBias,
    /// The instruction does not load the stock 2.0 (data not in place, or
    /// already rewritten): left alone.
    NotStock {
        /// What it loads.
        found: f32,
    },
    /// No float within 1 % of the constant exists in its section.
    NoConstant {
        /// The constant wanted.
        wanted: f32,
    },
    /// The rewrite failed.
    Failed(String),
    /// Rewritten; bank 1 is selected from the next maintenance on.
    Built(MipBias),
}

/// The state of the tuning the engine maintains.
#[derive(Clone, Debug, PartialEq)]
pub struct TuneState {
    /// Ratio asked for (validated).
    pub ratio_wanted: f32,
    /// Last value handed to the engine's setter (or written directly).
    pub ratio_input: f32,
    /// What `cfg+0x200` read after it.
    pub ratio_stored: f32,
    /// What the engine has built (`cfg+0x204`), at the last maintenance.
    pub ratio_built: f32,
    /// Times the engine's own scale (`cfg+0x1f0`) was put back to 1.0.
    pub own_scale_restored: u64,
    /// Times dynamic resolution (`cfg+0x7b3b0`) was switched back off.
    pub dynamic_resolution_restored: u64,
    /// Times the anti-aliasing word was put back to TAA only.
    pub aa_word_restored: u64,
    /// The game's anti-aliasing word before the hold first changed it.
    pub aa_word_game: Option<u16>,
    /// Temporal passes on which `mSharpen` was cleared.
    pub sharpen_cleared: u64,
    /// The four dither bytes (SSLR dither, SSLR g-buffer jitter, contact
    /// noise, facial contact noise) before they were turned off.
    pub dither_game: Option<[u8; 4]>,
    /// The sampler bank selected, once selected.
    pub sampler_bank: Option<u32>,
    /// The mip-bias rewrite.
    pub mip_bias: MipBiasOutcome,
    /// Last position the game applied its Image Quality option with.
    pub menu_position: Option<u8>,
}

/// The engine's frame-latency wait (`ReFramePresent.md` §2.2).
///
/// The wait helper (`0x1423ade30`) is not a gate site, so its calls and
/// timeouts are not counted here; that needs a site and a detour (a dev
/// probe).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitState {
    /// `device+0x10d9`: something was presented (the helper waits only
    /// then).
    pub presented: bool,
    /// `device+0x10da`: the latch; when set the helper never waits again.
    pub latched: bool,
    /// `device+0x10f0`: the waitable handle.
    pub waitable: usize,
}

/// Everything at once.
#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    /// The last gate's outcome, if one ran.
    pub gate: Option<Result<Addresses, StandDown>>,
    /// Whether the detours are installed.
    pub installed: bool,
    /// The parameters in force.
    pub params: Params,
    /// Whether an upscale handler is registered.
    pub handler: bool,
    /// Whether the SS depth alias is enabled.
    pub depth_alias: bool,
    /// Detour counters.
    pub counters: Counters,
    /// Tuning.
    pub tune: TuneState,
    /// The jitter of the last main viewport set up (camera published or not).
    pub last_jitter: Option<JitterSample>,
    /// The camera of the last frame published.
    pub last_camera: Option<CameraFrame>,
    /// The frame-latency wait, once the device exists.
    pub wait: Option<WaitState>,
}
