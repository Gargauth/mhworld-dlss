//! DLSS 4 for Monster Hunter: World.
//!
//! The game ships DLSS 1.1 through NGX SDK 1.3, an API that predates motion
//! vectors, so the DLL cannot simply be replaced. Instead this library is
//! injected by `mhw-dlss-launcher.exe` before the game's first instruction
//! and replaces the engine's own temporal pass with an NGX DLSS evaluation,
//! fed from the engine's own colour, depth and motion vectors, with the
//! engine's DLSS 1 path taken over to carry the upscale and, optionally,
//! frame generation through Streamline on top.
//!
//! What it does to the game, in the order it happens:
//!
//! * at load: patches five entries of the executable's import table
//!   (`hooks.rs`), creates a Direct3D 12 device of its own and patches the
//!   device, command list, queue and resource vtables through it (`gpu.rs`);
//! * once the executable is unpacked and recognised as the build this mod
//!   knows: detours eight of the game's functions and rewrites one operand
//!   (`gate.rs`). On any other build none of that happens;
//! * when the game initialises NGX: substitutes the API version and the
//!   application data path, and, if the driver then says DLSS is available,
//!   tells the game the half of DLSS 1 the driver no longer has is available
//!   too (`ngx.rs`);
//! * from then on, per frame: writes a sub-pixel offset into the engine's
//!   camera, replaces the temporal dispatch with DLSS, and writes a handful
//!   of engine settings (`native.rs`, `gpu.rs`, `tune.rs`, `hijack.rs`).
//!
//! # A map of the source
//!
//! **Load.** [`DllMain`] runs `startup`: the log ([`logger`], [`redact`]),
//! the settings ([`config`]), the import-table hooks ([`hooks`], [`iat`],
//! [`proxy`]), then three workers: the render hooks, the build gate and
//! Streamline. The launcher that injects this library is `src/bin/launcher.rs`.
//!
//! **Gate.** [`gate`] decides once whether this is the game build the mod
//! knows, by the bytes at every code site the mod depends on, and whether
//! DLSS can be used in this session at all. On "no" the mod stands down:
//! nothing of the game's is changed and every hook passes its call through.
//!
//! **Hooks.** [`vtable`] patches COM vtable slots; [`gpu`] holds the hooks
//! on the Direct3D 12 device, command lists, queues and resources, and
//! [`pipeline`] the pipeline creation ReShade can route around. What they
//! record goes into small registries: [`bindings`] (descriptor tables and
//! mapped constant buffers), [`states`] (resource states in submission
//! order), [`alive`] (whether a resource still exists), [`rootsig`] and
//! [`dxbc`] (root signature layout and shader reflection, by which the
//! engine's temporal pass is recognised), [`queue`] (which queue is the
//! engine's), with [`fasthash`] under the maps.
//!
//! **The engine's side.** [`native`] writes the engine's own jitter slot
//! from a detour on its camera setup and names its fatal errors; [`camera`]
//! reads its camera buffer; [`hijack`] takes over the four functions of its
//! DLSS 1 path, which is what makes it render small and present at full
//! size; [`tune`] writes the few engine settings that have to change under
//! DLSS.
//!
//! **The resolve path.** Each frame the temporal dispatch is identified in
//! [`gpu`], its colour, depth and motion vectors are resolved from the
//! registries, [`mv`] decodes the motion vectors, and [`dlss`] evaluates the
//! DLSS feature into the engine's full-size target at its upscale stage.
//! [`ngx`] is the NGX session those calls go through, and [`ngxhost`] says
//! which module serves it (the driver, or a tool such as OptiScaler).
//!
//! **Frame generation.** [`streamline`] is the binding to NVIDIA Streamline,
//! [`swap`] the hooks around swapchain creation and `Present`, [`fg`] the
//! per-frame glue (constants, tags, Reflex markers), and [`hudless`] and
//! [`fgmask`] two optional inputs. None of it is loaded with `fg = 0`.
//!
//! **Companions.** [`reshade`] handles sharing the process with ReShade's
//! proxy objects and reports the presentation stack; [`overlay`] is a
//! settings page in ReShade's overlay.
//!
//! **Diagnostics.** [`pathdump`], [`perf`], [`dred`], [`validate`], and
//! [`selftest`], which runs one DLSS frame for the test program in
//! `tools/standin`. [`version`] is the banner each log starts with.

// Lints that are left off on purpose, each for a stated reason.
#![allow(
    clippy::missing_transmute_annotations,
    reason = "these transmutes turn an address into a function pointer whose type is written \
              on the binding beside it"
)]
#![allow(
    clippy::type_complexity,
    reason = "the tuples in the statics are small records, each read in one place"
)]
#![allow(
    clippy::manual_c_str_literals,
    reason = "names handed to GetProcAddress and D3DCompile are byte strings with a visible \
              NUL, as the Windows bindings take them"
)]
#![allow(
    clippy::too_many_arguments,
    reason = "hook signatures are the COM methods' own, and the motion-vector decode takes \
              what the pass binds"
)]
#![allow(
    clippy::needless_range_loop,
    clippy::single_match,
    clippy::field_reassign_with_default,
    reason = "kept where the indexed or tabular form reads closer to the data it describes"
)]

pub mod alive;
pub mod bindings;
pub mod camera;
pub mod config;
pub mod dlss;
pub mod dred;
pub mod dxbc;
pub mod fasthash;
pub mod fg;
pub mod fgmask;
pub mod gate;
pub mod gpu;
pub mod hijack;
pub mod hooks;
pub mod hudless;
pub mod iat;
pub mod logger;
pub mod mv;
pub mod native;
pub mod ngx;
pub mod ngxhost;
pub mod overlay;
pub mod pathdump;
pub mod perf;
pub mod pipeline;
pub mod proxy;
pub mod queue;
pub mod redact;
pub mod reshade;
pub mod rootsig;
pub mod selftest;
pub mod sharpen;
pub mod states;
pub mod streamline;
pub mod swap;
pub mod tune;
pub mod validate;
pub mod version;
pub mod vtable;

use std::ffi::c_void;
use std::path::PathBuf;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, HMODULE, TRUE};
use windows::Win32::System::LibraryLoader::{
    DisableThreadLibraryCalls, GetModuleFileNameW, GetModuleHandleW,
};
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

unsafe fn module_dir(module: HMODULE) -> Option<PathBuf> {
    let mut buf = [0u16; 512];
    let len = GetModuleFileNameW(module, &mut buf) as usize;
    if len == 0 {
        return None;
    }
    PathBuf::from(String::from_utf16_lossy(&buf[..len]))
        .parent()
        .map(PathBuf::from)
}

/// The folder inside the game's folder that this library is installed in,
/// with the NVIDIA libraries. The same name as `ngx::snippet_folder` and
/// `streamline`'s plugin folder, which is the point: NGX looks for the DLSS
/// library in the folder of the module that initialises it.
const OWN_FOLDER: &str = "mhw-dlss";

/// Where this library is, relative to the host executable.
#[derive(Debug, PartialEq, Clone, Copy)]
enum Layout {
    /// `<host folder>\mhw-dlss\`: where it belongs.
    InItsFolder,
    /// Beside the host executable: where it was installed until it moved.
    BesideTheHost,
    /// Anywhere else.
    Elsewhere,
}

fn same_folder(a: &std::path::Path, b: &std::path::Path) -> bool {
    let normal = |path: &std::path::Path| {
        path.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    };
    normal(a) == normal(b)
}

fn layout(own: &std::path::Path, host: Option<&std::path::Path>) -> Layout {
    match host {
        Some(host) if same_folder(own, &host.join(OWN_FOLDER)) => Layout::InItsFolder,
        Some(host) if same_folder(own, host) => Layout::BesideTheHost,
        _ => Layout::Elsewhere,
    }
}

const SETTINGS_FILE: &str = "mhw-dlss.ini";

/// Which settings file this session reads and writes, and whether it is the
/// one in the game folder.
///
/// Everything the mod installs or writes lives in the folder this library is
/// in, so that removing the mod is removing that folder: the settings file,
/// the log, the launcher and its chain file, the NVIDIA libraries. The one
/// exception is an install from before that rule, whose settings file is in
/// the game folder: when this library's folder has none and the game folder
/// does, that file goes on being used where it is. It is the user's and is
/// not moved.
fn settings_file(
    own: &std::path::Path,
    host: Option<&std::path::Path>,
    exists: impl Fn(&std::path::Path) -> bool,
) -> (PathBuf, bool) {
    let here = own.join(SETTINGS_FILE);
    if let (Layout::InItsFolder, Some(host)) = (layout(own, host), host) {
        let in_game_folder = host.join(SETTINGS_FILE);
        if !exists(&here) && exists(&in_game_folder) {
            return (in_game_folder, true);
        }
    }
    (here, false)
}

unsafe fn startup(module: HMODULE) {
    let mut buf = [0u16; 512];
    let len = GetModuleFileNameW(HMODULE::default(), &mut buf) as usize;
    let exe_path = String::from_utf16_lossy(&buf[..len]);
    let host_dir = PathBuf::from(&exe_path).parent().map(PathBuf::from);
    // This library's own folder is the mod's home: the log and the settings
    // are here, beside it.
    let own_dir = module_dir(module);

    logger::init(own_dir.clone());

    log!("{}", version::banner("mhw-dlss"));
    log!("host : {exe_path}");
    match logger::path() {
        Some(path) => log!("log  : {}", path.display()),
        // Unreachable in practice: with no log there is nowhere to say so.
        None => log!("log  : none could be opened"),
    }
    log!("");

    // A null HMODULE means "the process image" to GetModuleFileNameW, but the
    // IAT walk needs a real base address, so ask for the image handle.
    let exe = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
    log!("exe  : base {:p}", exe.0);
    if exe.0.is_null() {
        log!("[iat] cannot locate the process image; hooks NOT installed");
        return;
    }

    hooks::install(exe);

    // Under which name and from where this library was loaded.
    {
        let mut own = [0u16; 512];
        let length = GetModuleFileNameW(module, &mut own) as usize;
        let own_path = PathBuf::from(String::from_utf16_lossy(&own[..length]));
        log!("self : {}", own_path.display());
        // Where that is, because it decides where NGX looks for the DLSS
        // library when this library is the one that initialises it.
        if let Some(own) = own_dir.as_deref() {
            match layout(own, host_dir.as_deref()) {
                Layout::InItsFolder => log!(
                    "home : this library is in the {OWN_FOLDER} folder of the host's folder, \
                     beside the NVIDIA libraries; this log is in that folder too"
                ),
                Layout::BesideTheHost => log!(
                    "home : this library is beside the host executable, where it used to be \
                     installed, and not in {OWN_FOLDER}\\. NGX looks for the DLSS library in the \
                     folder of the library that initialises it: from here, with fg = 0, it did \
                     not find the one in {OWN_FOLDER}\\ when that was measured \
                     (SuperSampling.Available = 0, FeatureNotFound). Install this library as \
                     {OWN_FOLDER}\\mhw-dlss.dll"
                ),
                Layout::Elsewhere => log!(
                    "home : this library is in {}, which is neither the host's folder nor its \
                     {OWN_FOLDER} folder; mhw-dlss.ini and this log are beside it, and the \
                     NVIDIA libraries are looked for in {}",
                    own.display(),
                    ngx::snippet_folder().display()
                ),
            }
        }
        // A stale copy to look for is one the game itself would load by
        // name, which is in the game's folder wherever this library is.
        if let (Some(directory), Some(name)) = (
            host_dir.clone().or_else(|| own_dir.clone()),
            own_path.file_name(),
        ) {
            hooks::look_for_stale_copy(directory, name.to_string_lossy().into_owned());
        }
    }

    if let Some(own) = own_dir.as_deref() {
        let (settings, in_game_folder) =
            settings_file(own, host_dir.as_deref(), |path| path.exists());
        if in_game_folder {
            log!(
                "[cfg] {SETTINGS_FILE} is not in {} and is in the game folder, where an \
                 earlier install kept it: that file is used where it is. Move it into \
                 {OWN_FOLDER}\\ to have everything of the mod's in one folder",
                own.display()
            );
        }
        config::write_default(&settings);
        config::set_path(settings);
    }

    // Before any of the workers below, and before the engine can reach its
    // own NGX initialisation: `observe_game_init` substitutes this folder
    // for the game's. In every configuration, and here, synchronously:
    // published from a worker it would race the game's initialisation.
    ngx::publish_snippet_folder();

    // The game's own settings say which renderer it starts with, and on
    // DirectX 11 this mod has nothing to do. Then nothing below is started:
    // no Direct3D 12 device of our own, no engine detours, no Streamline.
    // The import hooks above stay, passing everything through; they are what
    // loads a companion listed as `renderer`.
    if let Some(game) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
    {
        if gate::configured_for_directx11(&game) {
            gate::stand_down_for_directx11();
            log!("");
            return;
        }
    }

    // Install the render hooks as early as the loader allows: a worker, so a
    // device is never created under the loader lock, but with no real delay,
    // because every descriptor and mapping made before this point is invisible
    // to us. Under the launcher this runs while the game is still suspended.
    gpu::bootstrap_scratch(std::time::Duration::from_millis(10));
    // Starts asking now whether this is the game build the mod knows. It
    // installs the engine detours the moment every byte they depend on has
    // been unpacked and matches, which is neither at load nor at any hook we
    // own -- and installs none of them if any does not. See `gate.rs`.
    gate::start();
    // Streamline, before the engine has asked for a device or a factory, and
    // on a thread of its own. Initialising it from inside
    // `D3D12CreateDevice` loads six libraries and a plugin manager inside a
    // call the engine is waiting on, and the engine then creates its
    // swapchain before the interception is in place. See
    // `streamline::start`.
    streamline::start();
    log!("");
}

/// The loader's entry point. Everything is started from the attach call.
///
/// # Safety
/// Called by the Windows loader, under the loader lock: nothing here may
/// load a library or wait on a thread, which is why `startup` only patches
/// tables and starts workers.
#[no_mangle]
pub unsafe extern "system" fn DllMain(
    module: HMODULE,
    reason: u32,
    _reserved: *mut c_void,
) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        let _ = DisableThreadLibraryCalls(module);
        startup(module);
    }
    TRUE
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn the_layout_is_told_from_where_this_library_is() {
        let game = Path::new("C:/Games/Monster Hunter World");
        let folder = game.join("mhw-dlss");
        assert_eq!(layout(&folder, Some(game)), Layout::InItsFolder);
        // Case and slash direction are the file system's business.
        let shouted = Path::new("c:/games/monster hunter world/mhw-dlss/");
        assert_eq!(layout(shouted, Some(game)), Layout::InItsFolder);
        assert_eq!(layout(game, Some(game)), Layout::BesideTheHost);
        assert_eq!(
            layout(Path::new("D:/tools/mhw-dlss"), Some(game)),
            Layout::Elsewhere
        );
        assert_eq!(layout(&folder, None), Layout::Elsewhere);
        // A folder of that name somewhere deeper is not the one meant.
        let deeper = game.join("mods").join("mhw-dlss");
        assert_eq!(layout(&deeper, Some(game)), Layout::Elsewhere);
    }

    /// The settings file is the one beside this library, except that an
    /// earlier install's file in the game folder is used when there is none
    /// beside this library.
    #[test]
    fn the_settings_are_beside_this_library_unless_only_the_game_folder_has_them() {
        let game = Path::new("C:/Games/Monster Hunter World");
        let folder = game.join("mhw-dlss");
        let here = folder.join(SETTINGS_FILE);
        let in_game_folder = game.join(SETTINGS_FILE);

        // A fresh install: neither exists, and the new file goes in the folder.
        assert_eq!(
            settings_file(&folder, Some(game), |_| false),
            (here.clone(), false)
        );
        // Both exist: the folder's.
        assert_eq!(
            settings_file(&folder, Some(game), |_| true),
            (here.clone(), false)
        );
        // Only the game folder's: that one, where it is.
        assert_eq!(
            settings_file(&folder, Some(game), |path| path == in_game_folder),
            (in_game_folder.clone(), true)
        );
        // In any other arrangement, beside this library and nowhere else.
        let other = Path::new("D:/tools/mhw-dlss");
        assert_eq!(
            settings_file(other, Some(game), |path| path == in_game_folder),
            (other.join(SETTINGS_FILE), false)
        );
        assert_eq!(
            settings_file(&folder, None, |_| true),
            (here.clone(), false)
        );
    }
}
