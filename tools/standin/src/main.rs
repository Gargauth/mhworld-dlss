//! A stand-in for the game, for testing the mod and companion tools without
//! spending a launch of the game on it.
//!
//! It does what the engine's renderer start-up does, in the engine's order
//! and through the same calls, so the mod's hooks and anything layered above
//! them (ReShade, its add-ons, Streamline) meet what they would meet in the
//! game:
//!
//! 1. load `dxgi.dll` and `d3d12.dll` with `LOAD_LIBRARY_SEARCH_SYSTEM32`;
//! 2. resolve `CreateDXGIFactory2` and `D3D12CreateDevice` with
//!    `GetProcAddress`, through this executable's import table, which is
//!    what the mod patches;
//! 3. a factory, then a device, then a queue;
//! 4. a root signature and a **compute pipeline whose shader reflects like
//!    the engine's temporal pass** (`tInput`, `tHistory`, `rwOuput`), created
//!    with `ID3D12Device::CreateComputePipelineState` on whatever device it
//!    was handed, as the engine does; one dispatch of it;
//! 5. a swapchain with the game's flags (0x842), and 620 presents;
//! 6. optionally one DLSS frame (`src/dlss.rs`);
//! 7. a bare-name `LoadLibrary("DINPUT8.dll")`, as the game does later.
//!
//! Then it reads the mod's log and judges it (`src/verdict.rs`). Step 4 is
//! why this program exists: with an add-on that listens for pipeline events,
//! ReShade creates the engine's pipelines through a different native call,
//! and a stand-in without a pipeline passes while a mod that misses that
//! call fails in the game.
//!
//! What it is not: the game. No protection stub, no engine detours, one
//! thread. A pass means the plumbing meets; it does not mean a picture. The
//! mod must recognise this program by its file name (`standin.exe`) and give
//! it the render hooks and NGX without the engine gate.
//!
//! # Use
//!
//! A folder laid out as the game's:
//!
//! ```text
//!   standin.exe
//!   mhw-dlss\mhw-dlss-launcher.exe  mhw-dlss\mhw-dlss.dll
//!   mhw-dlss\nvngx_dlss.dll
//!   mhw-dlss\sl.*.dll, nvngx_dlssg.dll  (optional; [FrameGeneration] Enabled = true)
//!   mhw-dlss\ReShade64.dll              (optional; `ReShade64.dll  renderer` in the chain)
//!   mhw-dlss\renodx-mhworld.addon64     (optional; makes ReShade take the stream path)
//! ```
//!
//! then `mhw-dlss\mhw-dlss-launcher.exe --wait <folder>\standin.exe`. The
//! verdict is the last line of `standin.result` beside this program, `PASS`
//! or `FAIL`, and the exit code (0 pass, 3 fail, 4 the start-up itself
//! failed).
//!
//! NGX finds the DLSS library through Streamline with frame generation on,
//! and only beside `mhw-dlss.dll` with it off; that is why the mod's library
//! is installed in `mhw-dlss\` with the NVIDIA libraries.
//!
//! Environment: `STANDIN_NO_DLSS=1` skips the DLSS frame,
//! `STANDIN_PLAIN_SHADER=1` renames the pass's resources so that nothing is
//! recognised, `STANDIN_NGX=<path>` names the driver's `nvngx.dll`.
//!
//! Not a workspace member; built on its own:
//! `cargo build --release --manifest-path tools/standin/Cargo.toml`
//! (`cargo zigbuild ... --target x86_64-pc-windows-gnu` from Linux).

// Used by the Windows program; built everywhere so that its tests run on
// any host.
#[cfg_attr(not(windows), allow(dead_code))]
mod verdict;

#[cfg(windows)]
mod dlss;
#[cfg(windows)]
mod scene;

/// Errors are reported, not handled: a string says what failed.
#[cfg(windows)]
type Result<T, E = String> = std::result::Result<T, E>;

/// Where progress lines go (the console and `standin.result`).
#[cfg(windows)]
type Say<'a> = dyn FnMut(String) + 'a;

/// `result` with `what` in front of its error.
#[cfg(windows)]
fn context<T>(result: windows::core::Result<T>, what: &str) -> Result<T> {
    result.map_err(|error| format!("{what}: {error}"))
}

/// The mod's log: in its folder, or beside this program when the mod's
/// library was put there.
#[cfg(windows)]
fn mod_log(folder: &std::path::Path) -> std::path::PathBuf {
    let in_its_folder = folder.join("mhw-dlss").join("mhw-dlss.log");
    if in_its_folder.exists() {
        in_its_folder
    } else {
        folder.join("mhw-dlss.log")
    }
}

#[cfg(windows)]
fn main() {
    use std::io::Write as _;

    let folder = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| ".".into());
    let mut result = std::fs::File::create(folder.join("standin.result")).ok();
    let mut say = |line: String| {
        println!("{line}");
        if let Some(file) = result.as_mut() {
            let _ = writeln!(file, "{line}");
        }
    };
    let code = match run(&folder, &mut say) {
        Ok(failures) if failures.is_empty() => {
            say("PASS".to_owned());
            0
        }
        Ok(failures) => {
            for failure in failures {
                say(format!("  {failure}"));
            }
            say("FAIL".to_owned());
            3
        }
        Err(error) => {
            say(format!("  the start-up failed: {error}"));
            say("FAIL".to_owned());
            4
        }
    };
    std::process::exit(code);
}

#[cfg(windows)]
fn run(folder: &std::path::Path, say: &mut Say<'_>) -> Result<Vec<String>> {
    use windows::core::w;
    use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, LoadLibraryW};

    // The mod's bootstrap runs on its own thread right after injection; the
    // game takes far longer than this to get here.
    std::thread::sleep(std::time::Duration::from_millis(1500));

    let scene = scene::start(say)?;
    let mut done = verdict::Done::default();
    let mut failures = Vec::new();
    if let Some(pipeline) = &scene.pipeline {
        scene::dispatch_once(&scene, pipeline)?;
        say("dispatched the pipeline once".to_owned());
        done.dispatched = true;
    } else {
        failures.push("the pipeline could not be created".to_owned());
    }
    if scene.swapchain.is_none() {
        failures.push("the swapchain could not be created".to_owned());
    }

    // After the swapchain, which is the game's order: Streamline has by then
    // set up its own NGX context on the device it was given.
    if std::env::var_os("STANDIN_NO_DLSS").is_none() {
        match &scene.pipeline {
            None => say("no pipeline to bind for the DLSS frame".to_owned()),
            Some(pipeline) => match dlss::frame(&scene, pipeline, folder, say) {
                Ok(None) => {}
                Ok(Some(outcome)) => {
                    done.dlss_frame = true;
                    if outcome & dlss::DECODED == 0 {
                        failures.push("the mod's motion vector decode did not run".to_owned());
                    }
                    if outcome & dlss::EVALUATED == 0 {
                        failures.push("the mod's DLSS evaluation did not succeed".to_owned());
                    }
                }
                Err(error) => failures.push(format!("the DLSS frame could not be set up: {error}")),
            },
        }
        // SAFETY: a live device.
        if unsafe { scene.device.GetDeviceRemovedReason() }.is_err() {
            failures.push("the device was removed".to_owned());
        }
    }

    scene::present(&scene, 620);

    // SAFETY: a library by bare name, as the game asks for it; the buffer
    // is sized for the call.
    unsafe {
        let input = LoadLibraryW(w!("DINPUT8.dll")).unwrap_or_default();
        let mut buffer = [0u16; 300];
        let length = GetModuleFileNameW(input, &mut buffer) as usize;
        let name = String::from_utf16_lossy(buffer.get(..length).unwrap_or_default());
        say(format!("DINPUT8.dll -> {name}"));
    }

    // Let the mod's periodic account reach the file.
    std::thread::sleep(std::time::Duration::from_millis(1000));
    let log = std::fs::read_to_string(mod_log(folder)).unwrap_or_default();
    for line in log.lines().filter(|line| {
        verdict::Line::parse(line).is_some_and(|record| {
            record.level == 'E'
                || record.level == 'W'
                || matches!(record.target, "render" | "swap" | "dlss" | "ngx" | "perf")
        })
    }) {
        say(format!(
            "  mod: {}",
            line.chars().take(260).collect::<String>()
        ));
    }
    failures.extend(verdict::judge(&log, done));
    Ok(failures)
}

#[cfg(not(windows))]
fn main() {
    eprintln!("standin stands in for the game's renderer and runs on Windows only.");
    std::process::exit(2);
}
