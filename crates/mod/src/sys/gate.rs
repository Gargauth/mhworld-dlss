//! The engine gate and the detours' installation.
//!
//! The executable is unpacked by its stub before its entry point, while the
//! boot thread already runs; until then the code sites do not match and the
//! gate answers "retryable". It is asked on a timer and, without delay,
//! whenever the game resolves a renderer or NGX symbol (its own code is
//! running then, and its device does not exist yet: the optimal-settings
//! answer and the mip bias must be in place before the engine builds its
//! TAA units and sampler banks). A thread inside a detoured prologue makes
//! the installation answer `Busy`: tried again at once, a few times.

use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use mhw_engine::{InstallError, InstallOptions, StandDown};
use mhw_win::{Module, PatchError};
use parking_lot::Mutex;

use super::{procs, runtime, slot};

const PENDING: u8 = 0;
const INSTALLED: u8 = 1;
const STOOD_DOWN: u8 = 2;
static STATE: AtomicU8 = AtomicU8::new(PENDING);
static ATTEMPT: Mutex<()> = Mutex::new(());

/// Fast polling for this long after start, then slow.
const FAST_FOR: Duration = Duration::from_secs(10);
const FAST: Duration = Duration::from_millis(20);
const SLOW: Duration = Duration::from_millis(250);
/// Give up asking after this long.
const GIVE_UP_AFTER: Duration = Duration::from_secs(180);
/// `Busy` retries per attempt.
const BUSY_RETRIES: u32 = 50;

/// Whether the detours are installed.
#[cfg(feature = "dev")]
pub(crate) fn installed() -> bool {
    STATE.load(Ordering::Acquire) == INSTALLED
}

/// Poll the gate on a thread of its own until it decides.
pub(crate) fn start() {
    if let Err(error) = mhw_win::spawn("gate", poll) {
        log::error!(target: "gate", "gate thread not started: {error}");
    }
}

fn poll() {
    let started = Instant::now();
    while STATE.load(Ordering::Acquire) == PENDING {
        attempt();
        if started.elapsed() > GIVE_UP_AFTER {
            log::error!(
                target: "gate",
                "the game's code did not match within {} s; the mod stands down",
                GIVE_UP_AFTER.as_secs()
            );
            stand_down();
            return;
        }
        std::thread::sleep(if started.elapsed() < FAST_FOR {
            FAST
        } else {
            SLOW
        });
    }
}

/// The game resolved a renderer symbol: ask now (never blocks; a running
/// attempt elsewhere is enough).
pub(crate) fn nudge() {
    if STATE.load(Ordering::Acquire) == PENDING {
        attempt();
    }
}

fn attempt() {
    let Some(_one) = ATTEMPT.try_lock() else {
        return;
    };
    if STATE.load(Ordering::Acquire) != PENDING {
        return;
    }
    let Some(paths) = runtime::paths() else {
        return;
    };
    let module = match Module::main() {
        Ok(module) => module,
        Err(error) => {
            mhw_core::log_once!(
                Error,
                "gate",
                "module",
                "the game module is unreadable: {error}"
            );
            return;
        }
    };
    match mhw_engine::check(&module, &paths.game_dir) {
        Ok(_) => install(&module),
        Err(why) if why.retryable() => {}
        Err(why) => {
            log::info!(target: "gate", "standing down: {why}");
            stand_down_for(&why);
        }
    }
}

fn install(module: &Module) {
    let policy = runtime::policy();
    let options = InstallOptions {
        mip_bias: policy.mip_bias,
    };
    for _ in 0..BUSY_RETRIES {
        match mhw_engine::install(options) {
            Ok(installed) => {
                STATE.store(INSTALLED, Ordering::Release);
                log::info!(
                    target: "gate",
                    "engine detours in: {} ({} threads held), mip bias {:?}",
                    installed.detours,
                    installed.suspended,
                    installed.mip_bias
                );
                // The DLSS 1 path is ours now: the game may be told DLSS is
                // available (only while NGX really says so).
                mhw_ngx::session::set_spoof_availability(true);
                procs::patch_create_device(module);
                return;
            }
            Err(InstallError::AlreadyInstalled) => {
                STATE.store(INSTALLED, Ordering::Release);
                return;
            }
            Err(InstallError::Patch(PatchError::Busy)) => std::thread::yield_now(),
            // A code page in transition (an unpacker or another tool's hook
            // still has it writable, or rewrote it since the check): the
            // state stays pending and the next poll checks and tries again,
            // up to `GIVE_UP_AFTER`.
            Err(InstallError::Patch(
                error @ (PatchError::NotCode { .. } | PatchError::Changed { .. }),
            )) => {
                mhw_core::log_changed!(
                    Info,
                    "gate",
                    "transient patch",
                    error.to_string(),
                    "engine detours not written yet ({error}); trying again"
                );
                return;
            }
            Err(error) => {
                log::error!(target: "gate", "engine detours not installed: {error}");
                stand_down();
                return;
            }
        }
    }
    log::warn!(
        target: "gate",
        "a thread stayed inside a detoured prologue {BUSY_RETRIES} times; trying again later"
    );
}

fn stand_down_for(why: &StandDown) {
    if matches!(why, StandDown::NotDx12(_)) {
        log::info!(target: "gate", "DirectX 11: nothing of the mod runs in this session");
    }
    stand_down();
}

fn stand_down() {
    STATE.store(STOOD_DOWN, Ordering::Release);
    mhw_engine::set_upscale_handler(None);
    mhw_render::set_pass_handler(None);
    if runtime::fg_started() {
        mhw_fg::stand_down();
    }
}

/// Whether the game's renderer files say DirectX 11 (checked at boot,
/// before anything is started).
pub(crate) fn dx11(game_dir: &Path) -> Option<String> {
    match mhw_engine::detect_renderer(game_dir) {
        mhw_engine::Renderer::Dx11(why) => Some(format!("{why:?}")),
        _ => None,
    }
}

/// Register the engine side (before the gate passes; the detours read
/// these from their first call).
pub(crate) fn prepare_engine() {
    runtime::push_engine();
    mhw_engine::set_upscale_handler(Some(&slot::HANDLER));
}
