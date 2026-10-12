//! Start-up, on the `mhw-boot` thread started by `DllMain` (nothing here
//! may run under the loader lock).
//!
//! `DllMain` has already patched the executable's KERNEL32 imports; until
//! this thread has wired the mod ([`finish`]), those hooks hold the game's
//! renderer start-up ([`wait_active`]), and the launcher keeps the game
//! suspended until [`BOOTED_EVENT`] (both bounded by [`BOOT_TIMEOUT`]).
//! Order (README *Start-up*): settings and log; the stand-down checks;
//! Streamline's initialisation on its own thread, first, because it has
//! to finish before the renderer libraries load; the render hooks and
//! handlers; the delay slot; the gate; then the mod is active.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use mhw_core::bus::{self, CameraReady};
use mhw_core::config::{Build, Settings, FILE_NAME};
use mhw_core::log::LogConfig;
use mhw_dlss_launcher::handoff::{BOOTED_EVENT, BOOT_TIMEOUT};
use mhw_ngx::{GameInit, InitAction, InitOptions};
use mhw_win::Module;
use parking_lot::{Condvar, Mutex};
use windows::core::PCWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{OpenEventW, SetEvent, EVENT_MODIFY_STATE};

use super::{fg_glue, gate, live, procs, reload, runtime, slot, swap};
use crate::policy::Policy;

/// A driver `Init` after Streamline faulted was seen never to return.
const NGX_INIT_TIMEOUT: Duration = Duration::from_secs(10);

/// The boot's outcome: `None` while it runs, then whether the mod is
/// active.
static BOOTED: Mutex<Option<bool>> = Mutex::new(None);
static BOOTED_CHANGED: Condvar = Condvar::new();
/// A wait timed out once; later callers do not wait again.
static STOPPED_WAITING: AtomicBool = AtomicBool::new(false);

/// Whether the mod is active, waiting for the boot first (at most
/// [`BOOT_TIMEOUT`], once per process). Called by the import hooks on the
/// game's thread; never from the boot thread.
pub(crate) fn wait_active() -> bool {
    let mut state = BOOTED.lock();
    if state.is_none() && !STOPPED_WAITING.load(Ordering::Acquire) {
        let deadline = Instant::now() + BOOT_TIMEOUT;
        while state.is_none() {
            if BOOTED_CHANGED.wait_until(&mut state, deadline).timed_out() {
                STOPPED_WAITING.store(true, Ordering::Release);
                log::warn!(
                    target: "boot",
                    "the game reached its renderer and the mod's boot had not finished after \
                     {} s; it goes on without the mod's wiring",
                    BOOT_TIMEOUT.as_secs()
                );
                break;
            }
        }
    }
    state.unwrap_or(false)
}

/// The boot is over: wake the hooks waiting for it and tell the launcher.
/// Only the first call counts.
pub(crate) fn finish(active: bool) {
    {
        let mut state = BOOTED.lock();
        if state.is_some() {
            return;
        }
        *state = Some(active);
    }
    BOOTED_CHANGED.notify_all();
    if signal_event(BOOTED_EVENT) {
        log::info!(target: "boot", "launcher told the boot is done");
    }
}

/// Finishes the boot (inactive) on any early return.
struct Finish;

impl Drop for Finish {
    fn drop(&mut self) {
        finish(false);
    }
}

/// Set the named event `name` if someone (the launcher) created it.
/// Returns whether it was there.
pub(crate) fn signal_event(name: &str) -> bool {
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: a NUL-terminated name; the handle is closed below.
    match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(wide.as_ptr())) } {
        Ok(event) => {
            // SAFETY: an event handle we opened, closed once.
            unsafe {
                let _ = SetEvent(event);
                let _ = CloseHandle(event);
            }
            true
        }
        Err(_) => false,
    }
}

/// The settings file's bytes and settings; the default file is written
/// when there is none.
fn load_settings(path: &Path) -> (Vec<u8>, Settings, Vec<mhw_core::config::Diagnostic>, bool) {
    if let Some((bytes, settings, diagnostics)) = reload::read(path) {
        return (bytes, settings, diagnostics, false);
    }
    let text = Settings::write_default(Build::CURRENT);
    let wrote = std::fs::write(path, &text).is_ok();
    let (settings, diagnostics) = mhw_core::config::parse(&text, Build::CURRENT);
    (text.into_bytes(), settings, diagnostics, wrote)
}

/// The whole start-up. `own` is this library's base address.
pub(crate) fn run(own: usize) {
    // Every return before the end leaves the mod inactive and lets the
    // game (and the launcher) go on.
    let _finish = Finish;
    let Some(mod_folder) = mhw_win::module_path(own)
        .map(PathBuf::from)
        .and_then(|path| path.parent().map(Path::to_path_buf))
    else {
        return;
    };
    let game_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| mod_folder.clone());
    let settings_path = mod_folder.join(FILE_NAME);
    let (bytes, settings, diagnostics, wrote_default) = load_settings(&settings_path);

    let (level, targets) = runtime::log_filter(&settings);
    let log_file = mhw_core::log::init(LogConfig {
        dirs: vec![mod_folder.clone(), std::env::temp_dir().join("mhw-dlss")],
        level,
        targets,
        profile: std::env::var("USERPROFILE").unwrap_or_default(),
        ..LogConfig::default()
    });
    log::info!(
        target: "boot",
        "{} ({} build)",
        mhw_dlss_launcher::version::banner("mhw-dlss"),
        if Build::CURRENT.has_dev() { "dev" } else { "release" }
    );
    log::info!(target: "boot", "game {}; mod folder {}", game_dir.display(), mod_folder.display());
    if let Ok(Some(path)) = log_file {
        log::info!(target: "boot", "log {}", path.display());
    }
    if wrote_default {
        log::info!(target: "config", "wrote the default {}", settings_path.display());
    }
    reload::report(&diagnostics);
    runtime::init(
        runtime::Paths {
            mod_folder: mod_folder.clone(),
            game_dir: game_dir.clone(),
            settings: settings_path,
        },
        settings.clone(),
    );
    if !mhw_win::install_crash_logger() {
        log::warn!(target: "crash", "crash logger not installed");
    }
    procs::report_imports();
    if !settings.general.enabled {
        log::info!(target: "boot", "[General] Enabled = false: the mod does nothing this session");
        return;
    }
    if let Some(why) = gate::dx11(&game_dir) {
        log::info!(target: "boot", "the game starts on DirectX 11 ({why}); the mod stands down");
        return;
    }
    let policy = Policy::from_settings(&settings);

    // Streamline first, on its own thread: started any later it loses the
    // race with the engine's device and swapchain creation, and the
    // `renderer` libraries wait for it to settle (0.2 started it from
    // `DllMain`; settings and log come first here).
    if settings.frame_generation.enabled {
        let started = mhw_fg::start(mhw_fg::StartConfig {
            mod_folder: mod_folder.clone(),
            identity: mhw_fg::Identity::default(),
            allow_ota: false,
            verify_signature: true,
            log_level: mhw_fg::SlLogLevel::Default,
        });
        if started {
            runtime::note_fg_started();
            mhw_fg::set_options(policy.fg);
        }
    }

    mhw_ngx::session::set_init_policy(ngx_init_policy);
    #[cfg(feature = "dev")]
    mhw_render::configure_device(mhw_render::DeviceOptions {
        debug_layer: settings.dev.debug_layer,
        dred: settings.dev.dred,
    });
    #[cfg(feature = "dev")]
    mhw_render::configure_shader_dump(
        settings
            .dev
            .dump_shaders
            .then(|| mod_folder.join(&settings.dev.capture_dir).join("shaders")),
    );
    // The full-resolution fog: the upsample shader is chosen when the game
    // builds it (load); the composite switch is held by the engine only once
    // one was built, since the game's own upsample unblurred is noisy.
    let full_resolution_fog = !settings.upscaling.downsample_volume;
    mhw_render::configure_volume_upsample(full_resolution_fog, || {
        mhw_engine::hold_downsample_volume(true);
    });
    if full_resolution_fog {
        log::info!(target: "boot", "DownsampleVolume = false: volumetric fog at full resolution");
    }
    if let Err(error) = mhw_render::install() {
        log::error!(target: "boot", "Direct3D 12 hooks not installed ({error}); the mod stands down");
        if runtime::fg_started() {
            mhw_fg::stand_down();
        }
        return;
    }
    mhw_render::set_factory_hook(Some(swap::on_factory));
    mhw_render::set_submit_observer(Some(fg_glue::on_submit));
    mhw_render::set_pass_handler(Some(slot::on_pass));
    gate::prepare_engine();
    bus::subscribe::<CameraReady>(live::on_camera);
    runtime::apply_hudless(&policy);

    match Module::main() {
        Ok(exe) => procs::patch_create_device(&exe),
        Err(error) => {
            log::error!(target: "boot", "the game module is unreadable ({error}); no delay slot")
        }
    }
    gate::start();
    finish(true);
    #[cfg(feature = "dev")]
    super::devwire::start(&settings);
    reload::start(bytes);
    log::info!(target: "boot", "started");
}

/// The game's NGX `Init`: re-issued with API 21 and the mod folder (where
/// `nvngx_dlss.dll` is).
fn ngx_init_policy(_game: &GameInit) -> InitAction {
    let Some(paths) = runtime::paths() else {
        return InitAction::PassThrough;
    };
    let snippet = paths.mod_folder.join(mhw_ngx::init::SNIPPET);
    if !snippet.exists() {
        log::warn!(
            target: "ngx",
            "{} is missing: DLSS needs it beside the mod (run fetch-nvidia.cmd)",
            snippet.display()
        );
    }
    let mut options = InitOptions::new(paths.mod_folder.clone());
    if runtime::fg_started() && mhw_fg::faulted() {
        options.timeout = Some(NGX_INIT_TIMEOUT);
    }
    InitAction::Redirect(options)
}
