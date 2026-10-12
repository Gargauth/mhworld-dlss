//! The mod's process-wide state: where it lives, the settings store, the
//! policy in force and the fallback, and applying a settings change to
//! every subsystem.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;

#[cfg(feature = "dev")]
use mhw_core::config::Diagnostic;
use mhw_core::config::{Group, Settings};
use mhw_core::log::{LevelFilter, TargetLevel};
use parking_lot::{Mutex, RwLock};

use crate::fallback::{
    Fallback, SlotOutcome, Transition, PATIENCE_UNRESOLVED, PATIENCE_WITHOUT_PASS,
};
use crate::policy::Policy;
use crate::store::{Replaced, Store};

/// Where the mod's files and the game are.
#[derive(Debug)]
pub(crate) struct Paths {
    /// The folder of this library (settings, log, NVIDIA's libraries).
    pub mod_folder: PathBuf,
    /// The game's folder (the executable's).
    pub game_dir: PathBuf,
    /// `mhw-dlss.ini`.
    pub settings: PathBuf,
}

static PATHS: OnceLock<Paths> = OnceLock::new();
static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
/// The policy of the settings in force (before the fallback).
static POLICY: RwLock<Option<Policy>> = RwLock::new(None);
static FALLBACK: Mutex<Fallback> = Mutex::new(Fallback::new());
/// The game's own path runs (DLSS held or given up, [`crate::fallback`]).
static FALLEN_BACK: AtomicBool = AtomicBool::new(false);
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// Frame generation was started at boot.
static FG_STARTED: AtomicBool = AtomicBool::new(false);

/// Set the paths and the settings read at start (once).
pub(crate) fn init(paths: Paths, settings: Settings) {
    *POLICY.write() = Some(Policy::from_settings(&settings));
    let _ = STORE.set(Mutex::new(Store::new(settings)));
    let _ = PATHS.set(paths);
}

/// The paths, once [`init`] ran.
pub(crate) fn paths() -> Option<&'static Paths> {
    PATHS.get()
}

/// The settings in force.
pub(crate) fn settings() -> Settings {
    STORE
        .get()
        .map_or_else(Settings::default, |store| store.lock().current().clone())
}

/// The policy in force: the settings', or the game's own path while DLSS
/// is held or given up.
pub(crate) fn policy() -> Policy {
    let base = POLICY
        .read()
        .unwrap_or_else(|| Policy::from_settings(&Settings::default()));
    if FALLEN_BACK.load(Ordering::Acquire) {
        base.fallen_back()
    } else {
        base
    }
}

/// Whether DLSS is off for now (held or given up).
pub(crate) fn given_up() -> bool {
    FALLEN_BACK.load(Ordering::Acquire)
}

/// The fallback state, for introspection.
#[cfg(feature = "dev")]
pub(crate) fn fallback() -> Fallback {
    *FALLBACK.lock()
}

/// The settings generation (increases with live changes).
#[cfg(feature = "dev")]
pub(crate) fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// Frame generation was started at boot.
pub(crate) fn note_fg_started() {
    FG_STARTED.store(true, Ordering::Release);
}

/// Whether frame generation runs in this session.
pub(crate) fn fg_started() -> bool {
    FG_STARTED.load(Ordering::Acquire)
}

/// The log filter `settings` ask for.
pub(crate) fn log_filter(settings: &Settings) -> (LevelFilter, Vec<TargetLevel>) {
    #[cfg(feature = "dev")]
    let targets = settings.dev.log_targets.clone();
    #[cfg(not(feature = "dev"))]
    let targets = Vec::new();
    (settings.general.log_level, targets)
}

/// Put `new` in force and apply what changed.
///
/// The store stays locked while the change is applied: the hot reload and
/// a dev client may change settings at once, and applying outside the
/// lock let the older change's policy land last (stale until the next
/// change, the generation going backwards).
pub(crate) fn replace(new: Settings) -> Option<Replaced> {
    let mut store = STORE.get()?.lock();
    let replaced = store.replace(new);
    apply(store.current(), &replaced, store.generation());
    Some(replaced)
}

/// Set one key like the file would and apply it (dev `settings.set`),
/// under the store lock like [`replace`].
#[cfg(feature = "dev")]
pub(crate) fn set_setting(
    section: &str,
    key: &str,
    value: &str,
) -> Result<Option<Diagnostic>, Diagnostic> {
    let Some(store) = STORE.get() else {
        return Ok(None);
    };
    let mut store = store.lock();
    let (note, replaced) = store.set(section, key, value)?;
    apply(store.current(), &replaced, store.generation());
    Ok(note)
}

/// Apply one change to every subsystem. Called with the store locked;
/// must not ask the store.
fn apply(settings: &Settings, replaced: &Replaced, generation: u64) {
    if replaced.changes.is_empty() {
        return;
    }
    let policy = Policy::from_settings(settings);
    let installed_bias = POLICY.read().and_then(|p| p.mip_bias);
    *POLICY.write() = Some(policy);
    GENERATION.store(generation, Ordering::Release);
    let mut engine = false;
    for group in replaced.live() {
        log::info!(target: "config", "applying {group:?}");
        match group {
            Group::LogLevel => {
                let (level, targets) = log_filter(settings);
                mhw_core::log::set_filter(level, targets);
            }
            #[cfg(feature = "dev")]
            Group::LogTargets => {
                let (level, targets) = log_filter(settings);
                mhw_core::log::set_filter(level, targets);
            }
            Group::Scale => {
                engine = true;
                if policy.mip_bias != installed_bias {
                    mhw_core::log_once!(
                        Info,
                        "config",
                        "mip bias restart",
                        "the texture mip bias follows the render scale at start; the new scale's \
                         bias ({:?}) applies after a restart",
                        policy.mip_bias
                    );
                }
            }
            #[cfg(feature = "dev")]
            Group::Jitter | Group::Tune | Group::InputPath => engine = true,
            #[cfg(feature = "dev")]
            Group::HudLess => apply_hudless(&policy),
            Group::FrameGenerationOptions if fg_started() => mhw_fg::set_options(policy.fg),
            _ => {}
        }
    }
    for group in replaced.newly_pending.iter() {
        log::warn!(
            target: "config",
            "{group:?} changed in the settings; it takes effect when the game is started again"
        );
    }
    if replaced.live().any(may_fix)
        && FALLBACK.lock().note_generation(generation) == Transition::Retry
    {
        FALLEN_BACK.store(false, Ordering::Release);
        log::info!(target: "dlss", "settings changed: trying DLSS again");
        engine = true;
    }
    if engine {
        push_engine();
    }
}

/// Whether a change to `group` could make a given-up DLSS work: a retry.
fn may_fix(group: Group) -> bool {
    match group {
        Group::LogLevel | Group::FrameGenerationOptions | Group::Sharpness => false,
        #[cfg(feature = "dev")]
        Group::LogTargets | Group::CaptureDir | Group::HudLess | Group::WaitLatch => false,
        _ => true,
    }
}

/// HUD-less capture is wanted only when frame generation runs.
pub(crate) fn apply_hudless(policy: &Policy) {
    mhw_render::hudless::enable(policy.hudless && fg_started());
}

/// Hand the policy in force to the engine and the temporal pass.
pub(crate) fn push_engine() {
    let policy = policy();
    mhw_engine::set_params(policy.engine);
    mhw_engine::set_depth_alias(policy.engine.flags.supersampling());
    mhw_render::configure_pass(mhw_render::PassConfig {
        identify: policy.pass_inputs(),
        decode: (policy.pass_inputs() && policy.input_path == mhw_core::config::InputPath::Pass)
            .then_some(policy.decode),
    });
}

/// Whether an evaluation may be attempted on render `frame` (not in the
/// pause after a failed one).
pub(crate) fn may_evaluate(frame: u64) -> bool {
    FALLBACK.lock().may_evaluate(frame)
}

/// What happened at a slot that should have been evaluated on render
/// `frame`; `why` says what stopped it. Enough bad slots in a row hand the
/// frame to the game's own path ([`crate::fallback`]).
pub(crate) fn note_slot(outcome: SlotOutcome, frame: u64, why: &str) {
    let transition = FALLBACK.lock().slot(outcome, frame);
    match transition {
        Transition::Hold => log::warn!(
            target: "dlss",
            "no temporal pass at {PATIENCE_WITHOUT_PASS} upscale slots in a row (last: {why}); \
             the game renders the whole window with its own anti-aliasing until the pass is \
             seen again, instead of the picture standing still"
        ),
        Transition::GaveUp => log::warn!(
            target: "dlss",
            "nothing was evaluated at {PATIENCE_UNRESOLVED} slots in a row, or the temporal pass \
             keeps going away (last: {why}); the game's own anti-aliasing at full resolution \
             takes over until a setting changes"
        ),
        _ => return,
    }
    FALLEN_BACK.store(true, Ordering::Release);
    push_engine();
}

/// A temporal pass ran: ends a hold. One atomic load unless held.
pub(crate) fn note_pass_seen() {
    if !FALLEN_BACK.load(Ordering::Acquire) {
        return;
    }
    if FALLBACK.lock().pass_seen() == Transition::Resume {
        FALLEN_BACK.store(false, Ordering::Release);
        log::info!(target: "dlss", "the temporal pass is back: DLSS again");
        push_engine();
    }
}
