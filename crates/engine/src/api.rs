//! The mod-facing entry points on Windows: gate, install, parameters,
//! handler, latch and introspection.

use std::fmt;
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Mutex as StdMutex;

use mhw_core::frame::{CameraFrame, JitterSample};
use mhw_core::sig::Image;
use mhw_win::{install_detours, Detour, DetourError, Module, PatchError};

use crate::gate::{self, Addresses, StandDown};
use crate::params::Params;
use crate::renderer::{self, Renderer};
use crate::runtime::{
    self, ADDRESSES, COUNTERS, DEPTH_ALIAS, INSTALLED, LAST_FRAME, LAST_GATE, LAST_JITTER,
};
use crate::status::{Counters, MipBiasOutcome, Status, TuneState, WaitState};
use crate::upscale::UpscaleHandler;
use crate::views::RenderConfig;
use crate::{hooks, tune};

/// [`mhw_win::Module`] as a [`sig::Image`](mhw_core::sig::Image).
#[derive(Clone, Copy, Debug)]
pub struct ModuleImage<'a>(pub &'a Module);

impl Image for ModuleImage<'_> {
    fn section(&self, name: &str) -> Option<Range<usize>> {
        let section = self.0.section(name)?;
        let start = section.rva as usize;
        Some(start..start + section.mapped_len() as usize)
    }

    fn read(&self, rva: usize, len: usize) -> Option<&[u8]> {
        self.0.read(u32::try_from(rva).ok()?, len)
    }
}

/// The game's two renderer files in `game_dir` (the executable's
/// directory), decided as the game decides.
pub fn detect_renderer(game_dir: &Path) -> Renderer {
    let ini = std::fs::read(game_dir.join("graphics_option.ini")).ok();
    let marker = std::fs::read(game_dir.join("rendering_api_fallback.txt")).ok();
    renderer::renderer(ini.as_deref(), marker.as_deref())
}

/// Run the gate on the game's `module`, whose files are in `game_dir`.
///
/// Call before anything is installed (a detour overwrites the bytes its
/// pattern looks for). On success the addresses are kept for the process
/// (the first success wins) and returned. On failure nothing is kept;
/// [`StandDown::retryable`] says whether asking again later can help (the
/// code may still be packed). Every outcome is logged (target `gate`) when
/// it differs from the last.
pub fn check(module: &Module, game_dir: &Path) -> Result<&'static Addresses, StandDown> {
    let renderer = detect_renderer(game_dir);
    let outcome = gate::evaluate(&ModuleImage(module), module.base(), &renderer);
    *LAST_GATE.lock() = Some(outcome.clone());
    match outcome {
        Ok(addresses) => {
            let kept = ADDRESSES.get_or_init(|| addresses);
            let moved = crate::sites::SITES
                .iter()
                .enumerate()
                .filter(|(index, site)| kept.site_rva(*index) != site.known_rva)
                .count();
            mhw_core::log_once!(
                Info,
                "gate",
                "engaged",
                "game timestamp {:#010x}, image size {:#x}: every site, global and offset \
                 resolved ({} sites moved{}); DirectX 12",
                module.timestamp(),
                module.size(),
                moved,
                if kept.scanned { ", .text scanned" } else { "" }
            );
            Ok(kept)
        }
        Err(why) => {
            mhw_core::log_changed!(
                Warn,
                "gate",
                "stand down",
                why.to_string(),
                "game timestamp {:#010x}, image size {:#x}: standing down{}: {why}",
                module.timestamp(),
                module.size(),
                if why.retryable() { " for now" } else { "" }
            );
            Err(why)
        }
    }
}

/// The last gate's outcome.
pub fn gate_result() -> Option<Result<Addresses, StandDown>> {
    LAST_GATE.lock().clone()
}

/// The addresses of the successful gate.
pub fn addresses() -> Option<&'static Addresses> {
    runtime::addresses()
}

/// What [`install`] does besides the detours.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct InstallOptions {
    /// Texture mip bias in mip levels (negative = sharper), or `None` for
    /// the engine's bank 0. A start-up quantity: the engine builds its
    /// sampler banks once, so install before the game initialises its
    /// device.
    pub mip_bias: Option<f32>,
}

/// What [`install`] did.
#[derive(Clone, Debug, PartialEq)]
pub struct Installed {
    /// Detours written.
    pub detours: usize,
    /// Other threads suspended while writing them.
    pub suspended: usize,
    /// The mip-bias rewrite.
    pub mip_bias: MipBiasOutcome,
}

/// Why [`install`] did nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallError {
    /// No successful [`check`] yet.
    NotGated,
    /// Already installed.
    AlreadyInstalled,
    /// A detour could not be prepared.
    Prepare {
        /// Which.
        what: &'static str,
        /// Why.
        error: DetourError,
    },
    /// The entry patches could not be written; [`PatchError::Busy`] means a
    /// thread was inside a prologue: retry.
    Patch(PatchError),
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstallError::NotGated => f.write_str("the gate has not passed"),
            InstallError::AlreadyInstalled => f.write_str("already installed"),
            InstallError::Prepare { what, error } => write!(f, "{what}: {error}"),
            InstallError::Patch(error) => write!(f, "writing the detours: {error}"),
        }
    }
}

impl std::error::Error for InstallError {}

/// The installed detours, kept for the life of the process.
static DETOURS: StdMutex<Vec<Detour>> = StdMutex::new(Vec::new());

/// Install every detour at once, other threads suspended, or none; then
/// the mip-bias rewrite (separately: its failure is reported, not fatal).
///
/// The parameters ([`set_params`]) and handler ([`set_upscale_handler`])
/// may be set before or after; until a handler is registered the
/// evaluations do nothing.
pub fn install(options: InstallOptions) -> Result<Installed, InstallError> {
    let at = runtime::addresses().ok_or(InstallError::NotGated)?;
    let mut kept = DETOURS.lock().unwrap_or_else(|poison| poison.into_inner());
    if INSTALLED.load(Ordering::Acquire) {
        return Err(InstallError::AlreadyInstalled);
    }
    let mut detours = Vec::new();
    for spec in hooks::specs(at) {
        // SAFETY: each target is a gate-verified function entry whose
        // prologue the gate compared byte for byte, and each hook has that
        // function's signature and calls it only through `original`.
        let detour = unsafe { Detour::prepare(spec.what, spec.target, spec.hook, spec.original) }
            .map_err(|error| InstallError::Prepare {
            what: spec.what,
            error,
        })?;
        detours.push(detour);
    }
    // Live before the first hook runs: the detours read it.
    INSTALLED.store(true, Ordering::Release);
    // SAFETY: as above; nothing else patches these functions.
    let suspended = match unsafe { install_detours(&mut detours) } {
        Ok(suspended) => suspended,
        Err(error) => {
            INSTALLED.store(false, Ordering::Release);
            return Err(InstallError::Patch(error));
        }
    };
    let count = detours.len();
    kept.extend(detours);
    drop(kept);
    log::info!(
        target: "engine",
        "{count} detours installed in one step ({suspended} threads suspended)"
    );

    let mip_bias = match Module::at(at.base) {
        Ok(module) => {
            let (patch, outcome) = tune::plan_mip_bias(at, &module, options.mip_bias);
            match patch {
                // SAFETY: a 4-byte displacement of a `movss xmm0,[rip+d]`
                // the gate verified, pointed at an existing float; no thread
                // can be inside an operand.
                Some(patch) => match unsafe { mhw_win::code::apply(&[patch]) } {
                    Ok(_) => outcome,
                    Err(error) => MipBiasOutcome::Failed(error.to_string()),
                },
                None => outcome,
            }
        }
        Err(error) => MipBiasOutcome::Failed(error.to_string()),
    };
    tune::set_mip_bias(mip_bias.clone());
    Ok(Installed {
        detours: count,
        suspended,
        mip_bias,
    })
}

/// Whether the detours are installed.
pub fn is_installed() -> bool {
    INSTALLED.load(Ordering::Acquire)
}

/// Replace the parameters (validated). Any thread, any time; the detours
/// see them from their next call.
pub fn set_params(params: Params) {
    runtime::set_params(params);
}

/// The parameters in force.
pub fn params() -> Params {
    runtime::params()
}

/// Register the handler of the game's DLSS evaluations (replacing any
/// earlier one); `None` unregisters.
pub fn set_upscale_handler(handler: Option<&'static dyn UpscaleHandler>) {
    runtime::set_handler(handler);
}

/// Enable or disable the depth alias the supersampling path needs
/// (`ReDlss1Path.md` §2.5): in the SS enqueue, before the pass is recorded,
/// the R32F depth copy's depth view (`+0xf0`, null) is set to its colour
/// view (`+0x60`) with a reference added (the engine releases both slots
/// separately; a lost race gives the reference back). Disabling stops
/// aliasing new copies; existing aliases stay until the engine releases the
/// texture. Required with [`DlssFlags::Supersampling`](crate::DlssFlags).
pub fn set_depth_alias(enabled: bool) {
    DEPTH_ALIAS.store(enabled, Ordering::Release);
}

/// Write the device's frame-latency wait latch (`device+0x10DA`): `true`
/// makes the engine's wait helper (`0x1423ade30`) skip its wait on the
/// swapchain's waitable for the rest of the process (unless written back
/// to `false`). A data write, no code patch. `false` until the device
/// exists.
pub fn set_wait_latch(on: bool) -> bool {
    let written = runtime::addresses()
        .and_then(RenderConfig::get)
        .and_then(|config| config.device())
        .is_some_and(|device| device.set_wait_latch(on));
    if written {
        mhw_core::log_changed!(
            Info,
            "engine",
            "wait latch",
            on,
            "frame-latency wait latch (device+0x10da) set to {on}"
        );
    }
    written
}

/// The engine's frame-latency wait state, once the device exists.
pub fn wait_state() -> Option<WaitState> {
    let device = runtime::addresses()
        .and_then(RenderConfig::get)
        .and_then(|config| config.device())?;
    Some(WaitState {
        presented: device.presented()?,
        latched: device.wait_latch()?,
        waitable: device.waitable()?,
    })
}

/// Apply the frame's tuning now (it also runs once per render frame from
/// the camera setup).
pub fn maintain() {
    tune::maintain();
}

/// Hold the tonemap filter's `mDownsampleVolume` off (`true`): the
/// volumetric fog composited at full resolution instead of from a blurred
/// half-resolution copy (`docs/re/ReVolumetrics.md`). Applied from the next
/// frame; `false` (the default) gives the game its value back.
pub fn hold_downsample_volume(off: bool) {
    tune::hold_downsample_volume(off);
}

/// The camera last published.
pub fn last_camera() -> Option<CameraFrame> {
    LAST_FRAME.lock().map(|(camera, _)| camera)
}

/// The jitter of the last main viewport set up (its frame in
/// [`JitterSample::frame`]), whether or not its camera was published.
pub fn last_jitter() -> Option<JitterSample> {
    *LAST_JITTER.lock()
}

/// Take (and clear) the game's history reset the supersampling enqueue
/// latched (`taa+0x185` set, or a zero blend rate), for an evaluation
/// EvalISP does not make: DLAA at the temporal pass (ratio 1, no upscale
/// point). EvalISP takes it itself as [`ImageSignalCall::game_reset`].
///
/// [`ImageSignalCall::game_reset`]: crate::ImageSignalCall::game_reset
pub fn take_game_reset() -> bool {
    runtime::RESET_PENDING.swap(false, Ordering::AcqRel)
}

/// The detour counters.
pub fn counters() -> Counters {
    COUNTERS.snapshot()
}

/// The tuning state.
pub fn tune_state() -> TuneState {
    tune::state()
}

/// Everything at once.
pub fn status() -> Status {
    let last = *LAST_FRAME.lock();
    Status {
        gate: gate_result(),
        installed: is_installed(),
        params: params(),
        handler: runtime::handler().is_some(),
        depth_alias: DEPTH_ALIAS.load(Ordering::Acquire),
        counters: counters(),
        tune: tune_state(),
        last_jitter: last_jitter(),
        last_camera: last.map(|(camera, _)| camera),
        wait: wait_state(),
    }
}
