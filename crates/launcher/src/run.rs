//! One launch, start to finish.
//!
//! Order matters and is the point of the program: everything is resolved
//! and every message that concerns the chain file is shown *before* the game
//! exists, so nothing is held suspended behind a message box; failures of
//! individual libraries are shown only *after* the game was resumed, so a
//! missing overlay never becomes a game that will not start.

use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use log::LevelFilter;
use mhw_dlss_launcher::chain::{self, Entry, Plan, CHAIN_FILE, DEFAULT_CHAIN};
use mhw_dlss_launcher::cmdline::{self, USAGE};
use mhw_dlss_launcher::handoff::{
    renderer_list, AT_RENDERER_VARIABLE, BOOTED_EVENT, BOOT_TIMEOUT, RENDERER_READY_EVENT,
};
use mhw_dlss_launcher::layout::{self, LAUNCHER_EXE, PAYLOAD};
use mhw_dlss_launcher::logging::{self, LOG_FILE};
use mhw_dlss_launcher::{version, TARGET};

use crate::sys::inject::{self, InjectError, LOAD_TIMEOUT};
use crate::sys::process::{Game, Signal};
use crate::sys::ui::{self, Severity};

/// Exit codes.
const OK: i32 = 0;
const FAILED: i32 = 1;
const USAGE_ERROR: i32 = 2;

/// Run the launcher; returns the process exit code.
pub fn main() -> i32 {
    let directory = env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let invocation = cmdline::parse(
        env::args_os()
            .skip(1)
            .map(|a| a.to_string_lossy().into_owned()),
    );
    // Everything of the mod's is in one folder, this launcher included; the
    // log goes there too, wherever the launcher was started from.
    let (home, below) = layout::home_of(&directory, |path| path.exists());
    start_logging(&home, invocation.options.verbose);
    log::info!(target: TARGET, "{}", version::banner("mhw-dlss-launcher"));
    if below {
        log::warn!(
            target: TARGET,
            "this launcher is in {}, and the mod's files are in {}: they are used from there. \
             The launcher belongs in that folder; the Steam launch option is then \"{}\" %command%",
            directory.display(),
            home.display(),
            home.join(LAUNCHER_EXE).display()
        );
    }

    let Some(game) = invocation.game else {
        ui::tell(&format!("No game to start.\n\n{USAGE}"), Severity::Error);
        return USAGE_ERROR;
    };

    let (plan, chain_path) = read_plan(&home);
    if plan.present.is_empty() {
        ui::tell(
            &format!(
                "Nothing to inject, so the game was not started.\n\n\
                 None of the libraries listed in\n{}\nexist.",
                chain_path.display()
            ),
            Severity::Error,
        );
        return FAILED;
    }
    if plan.mod_missing {
        let renderer = if plan.renderer().next().is_some() {
            ", and the libraries marked `renderer` will not be loaded either, because it is \
             the mod that loads them"
        } else {
            ""
        };
        ui::tell(
            &format!(
                "{PAYLOAD} is listed in {CHAIN_FILE} but is not at\n{}\n\n\
                 Antivirus software sometimes removes it; check its quarantine, or extract \
                 the mod's files again.\n\nThe game will now start without the mod{renderer}.",
                home.join(PAYLOAD).display()
            ),
            Severity::Warning,
        );
    }

    log::info!(target: TARGET, "game    : {}", game.executable.display());
    for entry in &plan.present {
        log::info!(target: TARGET, "inject  : {} ({})", entry.path.display(), entry.load.describe());
    }
    hand_renderer_entries_to_the_mod(&plan);

    let created = |name: &str, what: &str| {
        Signal::create(name)
            .inspect_err(|error| {
                log::warn!(target: TARGET, "could not create the {what} event ({error})");
            })
            .ok()
    };
    let ready = created(RENDERER_READY_EVENT, "renderer-ready");
    let booted = created(BOOTED_EVENT, "booted");

    let mut process = match Game::start_suspended(&game.executable, &game.command_line) {
        Ok(process) => process,
        Err(error) => {
            ui::tell(
                &format!(
                    "The game could not be started.\n\n{}\n\n{error}",
                    game.executable.display()
                ),
                Severity::Error,
            );
            return FAILED;
        }
    };
    log::info!(target: TARGET, "created suspended, pid {}", process.pid);

    // One library failing does not cost the others: ReShade and the mod are
    // independent, and a missing overlay is no reason to play without DLSS.
    let mut failures: Vec<String> = plan
        .missing_others(&home)
        .map(|path| {
            format!(
                "{}\n    listed in {CHAIN_FILE} and not found",
                path.display()
            )
        })
        .collect();

    let mut mod_injected = false;
    for entry in plan.suspended() {
        let stuck = stuck_message(&entry.path);
        // SAFETY: the game's process handle, live while `process` is.
        let outcome =
            unsafe { inject::inject(process.handle(), &entry.path, || ui::retry(&stuck)) };
        if matches!(outcome, Err(InjectError::TimedOut)) {
            // The user chose Cancel. A library stuck in its own loading
            // holds the loader lock, so resuming would only trade a
            // suspended game for a hung one.
            log::warn!(target: TARGET, "closing the game, which never left its suspended start");
            process.terminate();
            return FAILED;
        }
        mod_injected |= outcome.is_ok() && layout::names_the_mod(&home, &entry.path);
        report(entry, outcome, &mut failures);
    }

    if mod_injected {
        wait_for_boot(booted.as_ref());
    }

    // Resume regardless: a failed injection costs the mod, not the game.
    if !process.resume() {
        log::error!(target: TARGET, "ResumeThread failed: {}", windows::core::Error::from_win32());
    }

    inject_late(&plan, &process, ready.as_ref(), &mut failures);

    if !failures.is_empty() {
        ui::tell(
            &format!(
                "The game is starting, but this could not be loaded into it:\n\n{}\n\n\
                 The game will run without it. Details are in {LOG_FILE}. A path with spaces \
                 in {CHAIN_FILE} can be written in double quotes.",
                failures.join("\n\n")
            ),
            Severity::Warning,
        );
    }

    if invocation.options.wait {
        log::info!(target: TARGET, "--wait: staying until the game exits");
        process.wait_exit();
        log::info!(target: TARGET, "the game has exited");
    } else {
        log::info!(target: TARGET, "done; the game carries on without the launcher");
    }
    OK
}

/// The log in the mod's folder (or `%LOCALAPPDATA%\mhw-dlss` when that is
/// not writable), and a console under `--verbose`.
fn start_logging(home: &Path, verbose: bool) {
    let mut dirs = vec![home.to_path_buf()];
    if let Some(local) = env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(local).join(layout::HOME_FOLDER));
    }
    let profile = env::var("USERPROFILE").unwrap_or_default();
    // Only fails if a logger exists already, which cannot happen here.
    let _ = logging::init(&dirs, profile, LevelFilter::Debug);
    if verbose {
        if let Some(console) = ui::open_console() {
            logging::attach_console(console);
        }
    }
}

/// Read the chain file (writing the default the first time) and decide what
/// to load. Returns the plan and the chain file's path.
fn read_plan(home: &Path) -> (Plan, PathBuf) {
    let exists = |path: &Path| path.exists();
    let (path, above) = chain::chain_file(home, exists);
    if above {
        log::warn!(
            target: TARGET,
            "{CHAIN_FILE} is not in {} but in the folder above it, where an earlier install kept \
             it: {} is used where it is, and names in it are looked for from its own folder. \
             Move it into {} to have everything of the mod's in one folder",
            home.display(),
            path.display(),
            home.display()
        );
    }
    if !path.exists() {
        match std::fs::write(&path, DEFAULT_CHAIN) {
            Ok(()) => log::info!(target: TARGET, "wrote {}", path.display()),
            Err(error) => log::warn!(target: TARGET, "could not write {}: {error}", path.display()),
        }
    }
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        log::warn!(
            target: TARGET,
            "could not read {} ({error}); using the mod alone",
            path.display()
        );
        DEFAULT_CHAIN.to_owned()
    });
    let base = path.parent().unwrap_or(home);
    let read = chain::read(&text, base, home, exists);
    for note in &read.notes {
        log::warn!(target: TARGET, "{note}");
    }
    let plan = Plan::new(read.entries, home, exists);
    for missing in &plan.missing {
        log::warn!(target: TARGET, "not found, skipping : {}", missing.display());
    }
    (plan, path)
}

/// The `renderer` entries are not injected from here. An injection from
/// outside is asynchronous, and the game creates the DXGI factory its
/// swapchain comes from on the line after it loads `d3d12.dll`, so ReShade
/// injected on that signal always arrives too late to see the swapchain.
/// The mod is already inside and on the game's own thread at that moment,
/// so it does the loading; this only tells it what.
fn hand_renderer_entries_to_the_mod(plan: &Plan) {
    match renderer_list(plan.renderer().map(|entry| entry.path.as_path())) {
        None => env::remove_var(AT_RENDERER_VARIABLE),
        Some(value) => {
            log::info!(target: TARGET, "handed to the mod through {AT_RENDERER_VARIABLE}: {value}");
            if plan.suspended().next().is_none() {
                log::warn!(
                    target: TARGET,
                    "nothing is injected while suspended, so the mod is not there to load the \
                     `renderer` entries"
                );
            }
            env::set_var(AT_RENDERER_VARIABLE, value);
        }
    }
}

/// Hold the game suspended until the mod has booted: its boot thread wires
/// what the game's start-up reaches first (frame generation's
/// initialisation, the factory and device interception, the temporal-pass
/// handler), and the import hooks it patched from `DllMain` would
/// otherwise make the game wait for it there. Bounded: a slow boot costs
/// the mod's wiring at worst, never the game.
fn wait_for_boot(booted: Option<&Signal>) {
    let Some(event) = booted else {
        log::warn!(target: TARGET, "no boot signal to wait on; resuming the game at once");
        return;
    };
    let started = std::time::Instant::now();
    if event.wait(BOOT_TIMEOUT) {
        log::info!(
            target: TARGET,
            "the mod booted in {} ms",
            started.elapsed().as_millis()
        );
    } else {
        log::warn!(
            target: TARGET,
            "the mod had not booted after {} s; resuming the game anyway",
            BOOT_TIMEOUT.as_secs()
        );
    }
}

/// Inject the `after` entries once the renderer is loaded.
///
/// A fixed delay was a race: ReShade injected slightly too early faults the
/// game inside D3D12Core. The mod signals [`RENDERER_READY_EVENT`] from its
/// `LoadLibrary` hook the moment `d3d12.dll` arrives; the configured number
/// is only a fallback timeout. The launcher has to be alive to inject; with
/// no late entry this wait does not happen.
fn inject_late(plan: &Plan, process: &Game, ready: Option<&Signal>, failures: &mut Vec<String>) {
    let Some(timeout_ms) = plan.late_timeout_ms() else {
        return;
    };
    let timeout = Duration::from_millis(timeout_ms);
    match ready {
        Some(event) if event.wait(timeout) => {
            log::info!(target: TARGET, "renderer ready; injecting the rest");
        }
        Some(_) => log::warn!(
            target: TARGET,
            "no signal within {timeout_ms} ms; injecting anyway, which may be too early"
        ),
        None => {
            log::warn!(target: TARGET, "no signal to wait on; falling back to {timeout_ms} ms");
            std::thread::sleep(timeout);
        }
    }
    for entry in plan.late() {
        // SAFETY: the game's process handle, live while `process` is.
        let outcome = unsafe { inject::inject(process.handle(), &entry.path, || false) };
        report(entry, outcome, failures);
    }
}

fn report(entry: &Entry, outcome: Result<(), InjectError>, failures: &mut Vec<String>) {
    let path = entry.path.display();
    match outcome {
        Ok(()) => log::info!(target: TARGET, "injected: {path}"),
        Err(InjectError::Failed(error)) => {
            log::error!(target: TARGET, "failed  : {path} ({error})");
            failures.push(format!("{path}\n    {error}"));
        }
        Err(InjectError::TimedOut) => {
            log::error!(target: TARGET, "gave up : {path} (still loading)");
            failures.push(format!(
                "{path}\n    still loading after {} seconds",
                LOAD_TIMEOUT.as_secs()
            ));
        }
    }
}

fn stuck_message(path: &Path) -> String {
    let seconds = LOAD_TIMEOUT.as_secs();
    format!(
        "{} has not finished loading inside the game after {seconds} seconds.\n\n\
         The game is held suspended until it does, so it has no window yet.\n\n\
         Retry waits another {seconds} seconds. Cancel closes the game; to play without the \
         mod, clear the Steam launch option.",
        path.display()
    )
}
