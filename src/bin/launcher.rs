//! Launcher that injects the mod before the game's renderer exists.
//!
//! It starts the game, injects, and exits. It is a GUI-subsystem program, so
//! it never opens a console window of its own accord; everything it has to
//! say goes to `mhw-dlss-launcher.log`, and a failure the user has to know
//! about gets a message box.
//!
//! The launcher lives in `<game>\mhw-dlss\` with everything else of the
//! mod's: the mod's library, the chain file `mhw-dlss-loader.ini` that lists
//! what to inject, and its log. Started from the game folder instead, it
//! finds that folder one level down and says so in its log.
//!
//! Options, given before the game's path:
//!
//! ```text
//!   --verbose   (or --debug) attach to the parent console, or open one, and
//!               print there as well as to the log
//!   --wait      stay until the game exits, still without a window. Nothing
//!               in this program needs it, and Steam follows the processes
//!               started from the one it launched; it is there in case the
//!               game is ever shown as not running while it is, and for
//!               scripts that want to wait for the game.
//! ```
//!
//! MHW cannot be reached by a DLL proxy early enough. Its loader behaviour,
//! established by experiment:
//!
//! | library        | loaded from | why |
//! | -------------- | ----------- | --- |
//! | `dinput8.dll`  | game folder | the game asks for it with a plain LoadLibrary, but only at module ~101 |
//! | `winmm.dll`    | System32    | the Steam overlay pulls System32's copy in first |
//! | `version.dll`  | System32    | loaded by Steam/DRM with a restricted search path |
//! | `d3d12.dll`    | System32    | the game itself requests it with LOAD_LIBRARY_SEARCH_SYSTEM32 |
//!
//! So the only library we can replace is loaded 47 modules after the renderer,
//! by which point the main render targets' views exist and the constant buffer
//! upload heap is already mapped. Hooks installed then cannot see either.
//!
//! Creating the process suspended and injecting before its first instruction
//! sidesteps the loader entirely.
//!
//! Usage, as a Steam launch option:
//!
//! ```text
//! "<game>\mhw-dlss\mhw-dlss-launcher.exe" %command%
//! "<game>\mhw-dlss\mhw-dlss-launcher.exe" --verbose %command%
//! ```
//!
//! Steam substitutes the game's own command line for `%command%`, so the game
//! still starts through Steam and inherits its environment.

#![windows_subsystem = "windows"]

use std::env;
use std::ffi::c_void;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use windows::core::{Error, Result, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Console::{AllocConsole, AttachConsole, ATTACH_PARENT_PROCESS};
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, MODULEENTRY32W, TH32CS_SNAPMODULE,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateProcessW, CreateRemoteThread, GetExitCodeThread, GetProcessId,
    ResumeThread, TerminateProcess, WaitForSingleObject, CREATE_SUSPENDED, INFINITE,
    PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDRETRY, MB_ICONERROR, MB_ICONWARNING, MB_OK, MB_RETRYCANCEL, MB_SETFOREGROUND,
    MB_TOPMOST, MESSAGEBOX_RESULT, MESSAGEBOX_STYLE,
};

include!("../event_name.rs");

#[path = "../redact.rs"]
mod redact;
#[path = "../version.rs"]
mod version;

/// Where this program's own account of a launch goes, and where the account
/// of the launch before it is kept.
const LOG_FILE: &str = "mhw-dlss-launcher.log";
const PREVIOUS_LOG_FILE: &str = "mhw-dlss-launcher.previous.log";

/// The log file, and the console when one was asked for.
///
/// There is no standard output to speak of: this is a GUI-subsystem program,
/// started by Steam, so `println!` would write into nothing. Every line goes
/// to the file, which is what makes a launch that went wrong explainable
/// after the fact, and to the console only under `--verbose`.
struct Sinks {
    file: Option<File>,
    console: Option<File>,
}

static SINKS: Mutex<Sinks> = Mutex::new(Sinks {
    file: None,
    console: None,
});

fn write_line(text: &str) {
    // The Windows profile path is kept out, as it is out of the mod's log:
    // both files are what a bug report attaches.
    static PROFILE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let profile = PROFILE.get_or_init(|| env::var("USERPROFILE").unwrap_or_default());
    let mut text = text.to_string();
    redact::redact(&mut text, profile);
    let Ok(mut sinks) = SINKS.lock() else {
        return;
    };
    if let Some(file) = sinks.file.as_mut() {
        let _ = writeln!(file, "{text}");
    }
    if let Some(console) = sinks.console.as_mut() {
        let _ = write!(console, "{text}\r\n");
    }
}

macro_rules! say {
    ($($arg:tt)*) => { write_line(&format!($($arg)*)) };
}

/// Open the log in the mod's folder, or under %LOCALAPPDATA% when that folder
/// is not writable -- the same fallback the mod's own log uses.
fn open_log(directory: &Path) {
    let mut candidates = vec![directory.join(LOG_FILE)];
    if let Ok(local) = env::var("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local).join("mhw-dlss").join(LOG_FILE));
    }
    for path in candidates {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
            // The last launch's account is kept, once, rather than
            // overwritten: a launch that went wrong is usually followed by
            // another before anybody looks.
            if path.exists() {
                let previous = parent.join(PREVIOUS_LOG_FILE);
                let _ = std::fs::remove_file(&previous);
                let _ = std::fs::rename(&path, &previous);
            }
        }
        if let Ok(file) = File::create(&path) {
            if let Ok(mut sinks) = SINKS.lock() {
                sinks.file = Some(file);
            }
            return;
        }
    }
}

/// Give this program a console to print to: the one it was started from if
/// there is one, a new one otherwise. Only ever called for `--verbose`.
fn open_console() {
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() && AllocConsole().is_err() {
            return;
        }
    }
    // Opened by name rather than through the standard handles, which a
    // GUI-subsystem process does not have and which attaching does not set.
    if let Ok(console) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
        if let Ok(mut sinks) = SINKS.lock() {
            sinks.console = Some(console);
        }
    }
}

/// Tell the user something they have to know, with no console to say it in.
fn message_box(text: &str, style: MESSAGEBOX_STYLE) {
    ask(text, MB_OK | style);
}

/// The same, with a choice, returning which button was pressed.
fn ask(text: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    say!("{text}");
    let body = wide(text);
    let title = wide("mhw-dlss launcher");
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(body.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_SETFOREGROUND | MB_TOPMOST | style,
        )
    }
}

/// The file listing what to inject, in the mod's folder.
const CHAIN_FILE: &str = "mhw-dlss-loader.ini";

/// What that file says when it does not exist yet.
const DEFAULT_CHAIN: &str = "\
# Libraries loaded into the game, one per line, in this order. A name or a
# relative path is looked for from the folder this file is in; an absolute
# path is taken as given, and a path may contain spaces or be written in
# double quotes. Lines starting with # are ignored, and a library
# that cannot be found is reported in mhw-dlss-launcher.log and does not stop
# the others.
#
# mhw-dlss.dll is the mod itself and stays first. It is in this folder,
# beside the NVIDIA libraries that fetch-nvidia.cmd puts here.
#
# A line may end with one of two words.
#
#   renderer   loaded by the mod itself, on the game's own thread, at the
#              moment the game loads d3d12.dll and before it creates its DXGI
#              factory or its device. This is the word for ReShade.
#   after      injected by the launcher once the game has loaded d3d12.dll,
#              optionally with a fallback timeout in milliseconds:
#              `after 1000`. For tools that hook nothing at creation, such as
#              RenderDoc's capture library.
#
# ReShade cannot load itself in this game: the game loads none of the usual
# proxy names from its folder in time. It has to be in before the game
# creates its DXGI factory, and `after` is too late for that, so use
# `renderer`: put ReShade64.dll in this folder and remove the # below.

mhw-dlss.dll
# ReShade64.dll  renderer
";

/// Fallback timeout for a late injection, in milliseconds, used only if the
/// mod never signals that the renderer's library has loaded.
const DEFAULT_DELAY: u64 = 15000;

/// One library and when to inject it.
struct Entry {
    path: PathBuf,
    /// Inject after the process is running rather than while suspended.
    late: bool,
    delay: u64,
    /// Not injected by this program at all: handed to the mod, which loads it
    /// itself on the game's own thread at the moment the game loads its
    /// renderer. See `AT_RENDERER_VARIABLE`.
    renderer: bool,
}

/// When a chain entry is loaded.
#[derive(Debug, PartialEq, Clone, Copy)]
enum When {
    /// By the launcher, before the game's first instruction.
    Suspended,
    /// By the mod, when the game loads its renderer.
    Renderer,
    /// By the launcher, once the renderer is loaded, with the fallback
    /// timeout in milliseconds if one was written.
    After(Option<u64>),
}

/// One line of the chain file, taken apart.
#[derive(Debug, PartialEq)]
struct ChainLine {
    name: String,
    when: When,
    /// Something on the line that was not understood, to be said in the log.
    complaint: Option<String>,
}

/// Take a chain line apart: a path, then optionally `renderer`, or `after`
/// with an optional number of milliseconds.
///
/// A path may contain spaces, as the game's default install path does. So
/// the keyword is taken off the *end* of the line and everything before it
/// is the path; a path may also be written in double quotes, and then
/// whatever follows the closing quote is the keyword. `None` for a blank
/// line or a comment.
fn parse_chain_line(line: &str) -> Option<ChainLine> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    if let Some(rest) = line.strip_prefix('"') {
        let (name, tail) = rest.split_once('"').unwrap_or((rest, ""));
        let mut words = tail.split_whitespace();
        let (when, complaint) = match (words.next(), words.next()) {
            (None, _) => (When::Suspended, None),
            (Some(word), None) if word.eq_ignore_ascii_case("renderer") => (When::Renderer, None),
            (Some(word), number) if word.eq_ignore_ascii_case("after") => match number {
                None => (When::After(None), None),
                Some(number) => match number.parse() {
                    Ok(milliseconds) => (When::After(Some(milliseconds)), None),
                    Err(_) => (
                        When::After(None),
                        Some(format!("`{number}` is not a number of milliseconds")),
                    ),
                },
            },
            (Some(word), _) => (
                When::Suspended,
                Some(format!("ignoring unrecognised `{word}`")),
            ),
        };
        return Some(ChainLine {
            name: name.trim().to_string(),
            when,
            complaint,
        });
    }

    // Unquoted: look at the last word, and the one before it.
    let words: Vec<&str> = line.split_whitespace().collect();
    let without_last = |count: usize| -> String {
        let mut rest = line;
        for _ in 0..count {
            let cut = rest.trim_end();
            let last_space = cut.rfind(char::is_whitespace).map_or(0, |index| index + 1);
            rest = &cut[..last_space];
        }
        rest.trim_end().to_string()
    };
    let last = words[words.len() - 1];
    let (name, when) = if words.len() >= 2 && last.eq_ignore_ascii_case("renderer") {
        (without_last(1), When::Renderer)
    } else if words.len() >= 2 && last.eq_ignore_ascii_case("after") {
        (without_last(1), When::After(None))
    } else if words.len() >= 3
        && words[words.len() - 2].eq_ignore_ascii_case("after")
        && last.parse::<u64>().is_ok()
    {
        (without_last(2), When::After(last.parse().ok()))
    } else {
        (line.to_string(), When::Suspended)
    };
    Some(ChainLine {
        name,
        when,
        complaint: None,
    })
}

/// Read the chain, writing the default list the first time.
///
/// A name or a relative path in the file is looked for from the folder the
/// file itself is in, so an earlier install's file in the game folder goes on
/// meaning what it meant.
fn chain(home: &Path) -> Vec<Entry> {
    let (path, above) = chain_file(home, |path| path.exists());
    if above {
        say!(
            "{CHAIN_FILE} is not in {} and is in the folder above it, where an earlier install \
             kept it: {} is used where it is, and names in it are looked for from its own \
             folder. Move it into {HOME_FOLDER}\\ to have everything of the mod's in one folder",
            home.display(),
            path.display()
        );
    }
    let base = path.parent().unwrap_or(home).to_path_buf();
    if !path.exists() {
        if let Err(error) = std::fs::write(&path, DEFAULT_CHAIN) {
            say!("could not write {}: {error}", path.display());
        } else {
            say!("wrote {}", path.display());
        }
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            say!(
                "could not read {} ({error}); using the mod alone",
                path.display()
            );
            DEFAULT_CHAIN.to_string()
        }
    };

    let mut entries = Vec::new();
    for line in text.lines() {
        let Some(parsed) = parse_chain_line(line) else {
            continue;
        };
        if let Some(complaint) = &parsed.complaint {
            say!("{}: {complaint}", parsed.name);
        }
        let (late, renderer, delay) = match parsed.when {
            When::Suspended => (false, false, DEFAULT_DELAY),
            When::Renderer => (false, true, DEFAULT_DELAY),
            When::After(delay) => (true, false, delay.unwrap_or(DEFAULT_DELAY)),
        };
        let candidate = Path::new(&parsed.name);
        let resolved = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            base.join(candidate)
        };
        let resolved = where_the_mod_is(home, resolved);
        entries.push(Entry {
            path: resolved,
            late,
            delay,
            renderer,
        });
    }
    entries
}

/// The mod's library, beside this launcher.
const PAYLOAD: &str = "mhw-dlss.dll";

/// The folder, inside the game folder, that holds everything the mod installs
/// or writes: this launcher, the mod's library, the NVIDIA libraries, the
/// settings, the chain file and the logs. Removing the mod is removing it.
///
/// The mod's library has to be in the same folder as the NVIDIA libraries:
/// NGX looks for the DLSS library in the folder of the module that
/// initialises it, not in the application data path it is given.
const HOME_FOLDER: &str = "mhw-dlss";

/// The folder the mod's files are in, given the folder this launcher is in,
/// and whether that is a different folder.
///
/// They are beside the launcher. A launcher started from the game folder, as
/// an earlier install had it, finds them one folder down.
fn home_of(directory: &Path, exists: impl Fn(&Path) -> bool) -> (PathBuf, bool) {
    let below = directory.join(HOME_FOLDER);
    if !exists(&directory.join(PAYLOAD)) && exists(&below.join(PAYLOAD)) {
        (below, true)
    } else {
        (directory.to_path_buf(), false)
    }
}

/// The game folder, when `home` is the `mhw-dlss` folder inside one.
fn folder_above(home: &Path) -> Option<PathBuf> {
    let named = home
        .file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(HOME_FOLDER));
    if named {
        home.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

/// Which chain file is read, and whether it is the one in the folder above.
///
/// The one in `home`. When there is none and the folder above has one, an
/// earlier install's, that file is used where it is: it is the user's and is
/// not moved. When neither exists the default is written in `home`.
fn chain_file(home: &Path, exists: impl Fn(&Path) -> bool) -> (PathBuf, bool) {
    let here = home.join(CHAIN_FILE);
    if !exists(&here) {
        if let Some(above) = folder_above(home).map(|folder| folder.join(CHAIN_FILE)) {
            if exists(&above) {
                return (above, true);
            }
        }
    }
    (here, false)
}

/// Whether a resolved chain entry is one of the other ways an earlier chain
/// file spelled the mod: `mhw-dlss\mhw-dlss.dll` read from inside the
/// folder, or `mhw-dlss.dll` read from the game folder.
fn is_another_spelling_of_the_mod(home: &Path, resolved: &Path) -> bool {
    let resolved = resolved.to_string_lossy();
    let nested = home.join(HOME_FOLDER).join(PAYLOAD);
    same_file_name(&resolved, &nested.to_string_lossy())
        || folder_above(home)
            .is_some_and(|above| same_file_name(&resolved, &above.join(PAYLOAD).to_string_lossy()))
}

/// Whether a resolved chain entry names the mod's library, however spelled.
fn names_the_mod(home: &Path, resolved: &Path) -> bool {
    same_file_name(
        &resolved.to_string_lossy(),
        &home.join(PAYLOAD).to_string_lossy(),
    ) || is_another_spelling_of_the_mod(home, resolved)
}

/// A chain entry for the mod, pointed at the mod's library in `home`.
///
/// The chain file is the user's and is not rewritten, so a line written for
/// an earlier layout keeps working, with one line here saying what was done.
/// Any other entry is returned as it was given.
fn where_the_mod_is(home: &Path, resolved: PathBuf) -> PathBuf {
    let payload = home.join(PAYLOAD);
    if !is_another_spelling_of_the_mod(home, &resolved) || !payload.exists() {
        return resolved;
    }
    say!(
        "{CHAIN_FILE} names the mod as {}, as an earlier layout had it; injecting {} instead. \
         The line can be changed to say {PAYLOAD}",
        resolved.display(),
        payload.display()
    );
    payload
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Write a NUL-terminated wide string into the target and return its address.
unsafe fn write_string(process: HANDLE, text: &Path) -> Result<*mut c_void> {
    let bytes: Vec<u16> = wide(&text.to_string_lossy());
    let size = bytes.len() * std::mem::size_of::<u16>();

    let remote = VirtualAllocEx(
        process,
        None,
        size,
        MEM_COMMIT | MEM_RESERVE,
        PAGE_READWRITE,
    );
    if remote.is_null() {
        return Err(Error::from_win32());
    }
    WriteProcessMemory(process, remote, bytes.as_ptr() as *const c_void, size, None)?;
    Ok(remote)
}

/// How long a library is given to load inside the game before the user is
/// asked. A library's `DllMain` that never returns would otherwise leave this
/// program waiting for ever on a game that is suspended and has no window.
const LOAD_TIMEOUT: u32 = 30_000;

/// Why an injection did not happen.
enum InjectError {
    /// A system call failed, or the library was not loaded.
    Failed(Error),
    /// The library was still loading after `LOAD_TIMEOUT` and the wait was
    /// given up.
    TimedOut,
}

impl From<Error> for InjectError {
    fn from(error: Error) -> Self {
        InjectError::Failed(error)
    }
}

/// Whether the process has a module loaded from `path`.
///
/// `None` when the module list could not be read at all.
unsafe fn has_module(process: HANDLE, path: &Path) -> Option<bool> {
    let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, GetProcessId(process)).ok()?;
    let mut entry = MODULEENTRY32W {
        dwSize: std::mem::size_of::<MODULEENTRY32W>() as u32,
        ..Default::default()
    };
    let wanted = path.to_string_lossy();
    let mut found = false;
    let mut more = Module32FirstW(snapshot, &mut entry).is_ok();
    let listed = more;
    while more {
        let length = entry
            .szExePath
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(entry.szExePath.len());
        let loaded = String::from_utf16_lossy(&entry.szExePath[..length]);
        if same_file_name(&loaded, &wanted) {
            found = true;
            break;
        }
        more = Module32NextW(snapshot, &mut entry).is_ok();
    }
    let _ = CloseHandle(snapshot);
    listed.then_some(found)
}

/// Whether two paths name the same file, as far as text can tell: the same
/// characters without regard to case or to which slash was used.
fn same_file_name(a: &str, b: &str) -> bool {
    let fold = |text: &str| text.replace('/', "\\").to_lowercase();
    fold(a) == fold(b)
}

/// What a remote `LoadLibraryW` thread's exit code says.
///
/// The exit code is the module handle cut to 32 bits, and a library can be
/// loaded at a base whose low half is zero -- 0x7ff9_0000_0000 is a valid
/// base. Zero was read as "not loaded" on its own, which reported a working
/// injection as failed. Zero now only raises the question, and the module
/// list answers it.
fn was_loaded(exit_code: Option<u32>, in_module_list: Option<bool>) -> bool {
    match (exit_code, in_module_list) {
        (Some(code), _) if code != 0 => true,
        (_, Some(listed)) => listed,
        // Neither the exit code nor the module list could be read. Nothing
        // says it failed.
        (None, None) => true,
        // A zero exit code and no module list to check it against.
        (Some(_), None) => false,
    }
}

/// Run LoadLibraryW in the target process and wait for it to finish.
///
/// This program's own address for `LoadLibraryW` is passed as the thread's
/// start routine, which is valid in the game because kernel32.dll is mapped
/// at the same address in every process of a boot session. In a process that
/// was created suspended the remote thread is the first one to run, so the
/// process is initialised on it, before `LoadLibraryW` is entered, and it
/// runs to completion before the game's main thread is resumed.
///
/// `while_suspended` says the game is still waiting on this. Then a library
/// that does not finish loading is the user's decision: wait longer, or close
/// the game, which has no window to close it by.
unsafe fn inject(
    process: HANDLE,
    payload: &Path,
    while_suspended: bool,
) -> std::result::Result<(), InjectError> {
    let module_name = wide("kernel32.dll");
    let kernel32 = GetModuleHandleW(PCWSTR(module_name.as_ptr()))?;
    let load_library = GetProcAddress(kernel32, windows::core::s!("LoadLibraryW"))
        .ok_or_else(Error::from_win32)?;

    let remote_path = write_string(process, payload)?;
    let thread = CreateRemoteThread(
        process,
        None,
        0,
        // SAFETY: `LoadLibraryW` takes one pointer and returns a handle, which
        // is the shape of a thread start routine, and kernel32.dll is mapped
        // at the same address in every process of a session.
        #[allow(clippy::missing_transmute_annotations)]
        Some(std::mem::transmute(load_library)),
        Some(remote_path),
        0,
        None,
    )?;

    loop {
        let outcome = WaitForSingleObject(thread, LOAD_TIMEOUT);
        if outcome == WAIT_OBJECT_0 {
            break;
        }
        if outcome != WAIT_TIMEOUT {
            // The wait itself failed. Nothing more can be learned by waiting.
            let error = Error::from_win32();
            let _ = CloseHandle(thread);
            return Err(InjectError::Failed(error));
        }
        say!(
            "{} has not finished loading after {} s",
            payload.display(),
            LOAD_TIMEOUT / 1000
        );
        let keep_waiting = while_suspended
            && ask(
                &format!(
                    "{} has not finished loading inside the game after {} seconds.\n\n\
                     The game is held suspended until it does, so it has no window yet.\n\n\
                     Retry waits another {} seconds. Cancel closes the game; to play \
                     without the mod, clear the Steam launch option.",
                    payload.display(),
                    LOAD_TIMEOUT / 1000,
                    LOAD_TIMEOUT / 1000
                ),
                MB_RETRYCANCEL | MB_ICONWARNING,
            ) == IDRETRY;
        if !keep_waiting {
            // The remote string is left where it is: the thread may still
            // read it.
            let _ = CloseHandle(thread);
            return Err(InjectError::TimedOut);
        }
    }
    // The thread's exit code is what LoadLibraryW returned, cut to 32 bits.
    let mut code = 0u32;
    let exit_code = GetExitCodeThread(thread, &mut code).is_ok().then_some(code);
    let _ = CloseHandle(thread);
    let _ = VirtualFreeEx(process, remote_path, 0, MEM_RELEASE);
    let listed = match exit_code {
        Some(0) | None => has_module(process, payload),
        Some(_) => None,
    };
    if exit_code == Some(0) && listed == Some(true) {
        say!(
            "{}: LoadLibraryW's result has a zero low half; the game's module list has the \
             library, so it is loaded",
            payload.display()
        );
    }
    if !was_loaded(exit_code, listed) {
        return Err(InjectError::Failed(Error::new(
            windows::core::HRESULT(-2147467259), // E_FAIL
            match listed {
                Some(_) => {
                    "LoadLibraryW returned null inside the game, and the library is not in \
                     the game's module list"
                }
                None => {
                    "LoadLibraryW returned zero inside the game and the game's module list \
                     could not be read to check"
                }
            },
        )));
    }
    Ok(())
}

/// What was asked of the launcher itself, ahead of the game's command line.
#[derive(Default)]
struct Options {
    /// Print to a console as well as to the log.
    verbose: bool,
    /// Stay until the game exits.
    wait: bool,
}

/// Everything after our own executable path, which is the game command line
/// Steam substituted for `%command%`.
///
/// Our own options come first and all begin with `-`; the first argument
/// that is not one of them is the game. Anything after that is the game's and
/// is never looked at, so an option of the game's cannot be taken for ours.
fn game_command_line() -> (Options, Option<(PathBuf, String)>) {
    let mut options = Options::default();
    let mut args = env::args().skip(1).peekable();
    while let Some(argument) = args.peek() {
        match argument.as_str() {
            "--verbose" | "--debug" | "-v" => options.verbose = true,
            "--wait" => options.wait = true,
            _ => break,
        }
        args.next();
    }
    let Some(executable) = args.next().map(PathBuf::from) else {
        return (options, None);
    };
    let rest: Vec<String> = args.collect();
    let line = command_line(&executable.to_string_lossy(), &rest);
    (options, Some((executable, line)))
}

/// The command line for the game: its path, then its arguments, each quoted
/// so that the game reads back exactly the arguments this program was given.
///
/// They reach this program already split and unquoted, so appended as they
/// are, one that holds a space would arrive at the game as two, and one that
/// holds a quote as something else.
fn command_line(executable: &str, arguments: &[String]) -> String {
    // The program name is not parsed like the rest: it ends at the closing
    // quote and backslashes in it mean nothing, so it is only wrapped.
    let mut line = format!("\"{executable}\"");
    for argument in arguments {
        line.push(' ');
        line.push_str(&quoted(argument));
    }
    line
}

/// One argument, as `CommandLineToArgvW` and the C runtime will read it back.
///
/// Their rule: backslashes are literal unless they run up to a double quote,
/// where 2n of them followed by a quote mean n backslashes and a delimiter,
/// and 2n+1 mean n backslashes and a literal quote. So a quote inside the
/// argument is written as a backslash and a quote with every backslash before
/// it doubled, and the backslashes at the very end are doubled because the
/// closing quote follows them.
fn quoted(argument: &str) -> String {
    let plain = !argument.is_empty()
        && !argument
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '\n' | '\u{b}' | '"'));
    if plain {
        return argument.to_string();
    }
    let mut out = String::with_capacity(argument.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            other => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(other);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

fn main() {
    let directory = env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let (options, game) = game_command_line();
    // Everything of the mod's is in one folder, this launcher included. The
    // log goes there too, wherever the launcher was started from.
    let (home, below) = home_of(&directory, |path| path.exists());
    open_log(&home);
    if options.verbose {
        open_console();
    }
    say!("{}", version::banner("mhw-dlss-launcher"));
    if below {
        say!(
            "this launcher is in {}, and the mod's files are in {}: they are used from \
             there. The launcher belongs in that folder with them; the Steam launch option \
             is then \"{}\" %command%",
            directory.display(),
            home.display(),
            home.join("mhw-dlss-launcher.exe").display()
        );
    }

    let Some((executable, command_line)) = game else {
        message_box(
            "No game to start.\n\n\
             Usage: mhw-dlss-launcher.exe [--verbose] [--wait] <game executable> [arguments]\n\n\
             As a Steam launch option:\n\
             \"<path to mhw-dlss-launcher.exe>\" %command%",
            MB_ICONERROR,
        );
        std::process::exit(2);
    };

    // Resolve everything up front, so a typo is reported before the game is
    // started rather than after it is already running without the mod.
    let chain = chain(&home);
    let listed: Vec<PathBuf> = chain.iter().map(|entry| entry.path.clone()).collect();
    let mut missing: Vec<PathBuf> = Vec::new();
    let entries: Vec<Entry> = chain
        .into_iter()
        .filter(|entry| {
            entry.path.exists() || {
                say!("not found, skipping : {}", entry.path.display());
                missing.push(entry.path.clone());
                false
            }
        })
        .collect();

    if entries.is_empty() {
        message_box(
            &format!(
                "Nothing to inject, so the game was not started.\n\n\
                 None of the libraries listed in\n{}\nexist.",
                chain_file(&home, |path| path.exists()).0.display()
            ),
            MB_ICONERROR,
        );
        std::process::exit(1);
    }

    // The mod itself is listed and is not there, while something else is.
    // The game would start and look as if the mod did nothing, with one line
    // in a log to say why. Said before the game exists, so nothing is held
    // suspended behind the box.
    // A listed entry for the mod, however an earlier chain file spelled it,
    // has already been pointed at the library in the mod's folder if it is
    // there, so one that does not exist means it is not.
    let payload = home.join(PAYLOAD);
    if listed
        .iter()
        .any(|path| names_the_mod(&home, path) && !path.exists())
    {
        message_box(
            &format!(
                "{PAYLOAD} is listed in {CHAIN_FILE} but is not at\n{}\n\n\
                 Antivirus software sometimes removes it; check its quarantine, or \
                 extract the mod's files again.\n\n\
                 The game will now start without the mod{}.",
                payload.display(),
                if entries.iter().any(|entry| entry.renderer) {
                    ", and the libraries marked `renderer` will not be loaded either, \
                     because it is the mod that loads them"
                } else {
                    ""
                }
            ),
            MB_ICONWARNING,
        );
    }

    say!("game    : {}", executable.display());
    for entry in &entries {
        let when = if entry.renderer {
            "loaded by the mod when the game loads its renderer".to_string()
        } else if entry.late {
            format!("when the renderer loads, or {} ms", entry.delay)
        } else {
            "while suspended".to_string()
        };
        say!("inject  : {} ({when})", entry.path.display());
    }

    // The `renderer` entries are not injected from here. An injection from
    // outside is asynchronous, and the game creates the DXGI factory its
    // swapchain comes from on the line after it loads d3d12.dll -- so ReShade
    // injected on that signal always arrives too late to see the swapchain.
    // The mod is already inside and on the game's own thread at that moment,
    // so it does the loading; this only tells it what. The variable is
    // inherited by the game because the process is created with this
    // program's environment.
    let at_renderer: Vec<String> = entries
        .iter()
        .filter(|entry| entry.renderer)
        .map(|entry| entry.path.to_string_lossy().into_owned())
        .collect();
    if at_renderer.is_empty() {
        env::remove_var(AT_RENDERER_VARIABLE);
    } else {
        env::set_var(AT_RENDERER_VARIABLE, at_renderer.join("|"));
        say!(
            "handed to the mod through {AT_RENDERER_VARIABLE}: {}",
            at_renderer.join(" | ")
        );
        if !entries.iter().any(|entry| !entry.renderer && !entry.late) {
            say!(
                "warning: nothing is injected while suspended, so the mod is not there \
                 to load the `renderer` entries"
            );
        }
    }

    unsafe {
        // Created before the game starts, so the mod can open it by name from
        // inside the process the moment it has something to say.
        let name: Vec<u16> = RENDERER_READY_EVENT
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let ready = CreateEventW(None, true, false, PCWSTR(name.as_ptr())).ok();
        if ready.is_none() {
            say!("could not create the ready event; late entries will use the timeout");
        }

        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut process = PROCESS_INFORMATION::default();

        // These buffers must outlive the call: a PCWSTR built from a temporary
        // inside the argument list -- or worse, inside a closure -- would
        // dangle before CreateProcessW reads it.
        let mut line = wide(&command_line);
        let target = wide(&executable.to_string_lossy());
        // Start in the game directory; MHW resolves its data relative to it.
        let working_dir = executable.parent().map(|dir| wide(&dir.to_string_lossy()));

        if let Err(error) = CreateProcessW(
            PCWSTR(target.as_ptr()),
            PWSTR(line.as_mut_ptr()),
            None,
            None,
            false,
            CREATE_SUSPENDED,
            None,
            match &working_dir {
                Some(buffer) => PCWSTR(buffer.as_ptr()),
                None => PCWSTR::null(),
            },
            &startup,
            &mut process,
        ) {
            message_box(
                &format!(
                    "The game could not be started.\n\n{}\n\n{error}",
                    executable.display()
                ),
                MB_ICONERROR,
            );
            std::process::exit(1);
        }

        say!("created suspended, pid {}", process.dwProcessId);

        // One library failing should not cost the others: ReShade and the mod
        // are independent, and a missing overlay is no reason to play without
        // DLSS.
        // A listed library that is not there is said to the user as well as
        // to the log, with the failures below and after the game is running.
        // The mod's own library has had its own message already.
        let mut failures: Vec<String> = missing
            .iter()
            .filter(|path| !names_the_mod(&home, path))
            .map(|path| {
                format!(
                    "{}\n    listed in {CHAIN_FILE} and not found",
                    path.display()
                )
            })
            .collect();
        let mut report =
            |entry: &Entry, outcome: std::result::Result<(), InjectError>| match outcome {
                Ok(()) => say!("injected: {}", entry.path.display()),
                Err(InjectError::Failed(error)) => {
                    say!("failed  : {} ({error})", entry.path.display());
                    failures.push(format!("{}\n    {error}", entry.path.display()));
                }
                Err(InjectError::TimedOut) => {
                    say!("gave up : {} (still loading)", entry.path.display());
                    failures.push(format!(
                        "{}\n    still loading after {} seconds",
                        entry.path.display(),
                        LOAD_TIMEOUT / 1000
                    ));
                }
            };

        for entry in entries
            .iter()
            .filter(|entry| !entry.late && !entry.renderer)
        {
            let outcome = inject(process.hProcess, &entry.path, true);
            if matches!(outcome, Err(InjectError::TimedOut)) {
                // The user chose Cancel. A library stuck inside its own
                // loading holds the loader, so resuming the game would only
                // trade a suspended process for a hung one.
                say!("closing the game, which never left its suspended start");
                let _ = TerminateProcess(process.hProcess, 1);
                let _ = CloseHandle(process.hThread);
                let _ = CloseHandle(process.hProcess);
                std::process::exit(1);
            }
            report(entry, outcome);
        }

        // Resume regardless: a failed injection should cost the mod, not the
        // ability to play the game.
        ResumeThread(process.hThread);
        let _ = CloseHandle(process.hThread);

        // The rest go in once the renderer's library is loaded. A fixed delay
        // was a race: ReShade injected slightly too early faults the game
        // inside D3D12Core, and one second was sometimes early and sometimes
        // not. The mod signals this event from its LoadLibrary hook the moment
        // d3d12.dll arrives, which is the point being waited for; the
        // configured number is only a fallback timeout.
        //
        // This is the one wait the launcher genuinely needs: it has to be
        // alive to do the injecting. It is bounded, and with no late entry it
        // does not happen at all.
        if entries.iter().any(|entry| entry.late) {
            let timeout = entries
                .iter()
                .filter(|entry| entry.late)
                .map(|entry| entry.delay)
                .max()
                .unwrap_or(DEFAULT_DELAY);
            match ready {
                Some(event) => {
                    let outcome = WaitForSingleObject(event, timeout as u32);
                    if outcome.0 == 0 {
                        say!("renderer ready; injecting the rest");
                    } else {
                        say!(
                            "no signal within {timeout} ms; injecting anyway, which may be \
                             too early"
                        );
                    }
                }
                None => {
                    say!("no signal to wait on; falling back to {timeout} ms");
                    std::thread::sleep(std::time::Duration::from_millis(timeout));
                }
            }
            for entry in entries.iter().filter(|entry| entry.late) {
                report(entry, inject(process.hProcess, &entry.path, false));
            }
        }

        // After the game is running, never before: a message box that held up
        // the resume would turn a missing overlay into a game that will not
        // start.
        if !failures.is_empty() {
            message_box(
                &format!(
                    "The game is starting, but this could not be loaded into it:\n\n{}\n\n\
                     The game will run without it. Details are in {LOG_FILE}. A path with \
                     spaces in {CHAIN_FILE} can be written in double quotes.",
                    failures.join("\n\n")
                ),
                MB_ICONWARNING,
            );
        }

        // Only when asked. Nothing here depends on the game's exit, so by
        // default the launcher is simply done once the game is running.
        if options.wait {
            say!("--wait: staying until the game exits");
            WaitForSingleObject(process.hProcess, INFINITE);
            say!("the game has exited");
        } else {
            say!("done; the game carries on without the launcher");
        }
        let _ = CloseHandle(process.hProcess);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Shell::CommandLineToArgvW;

    /// What Windows itself makes of a command line.
    fn parsed(line: &str) -> Vec<String> {
        let wide = wide(line);
        let mut count = 0i32;
        unsafe {
            let arguments = CommandLineToArgvW(PCWSTR(wide.as_ptr()), &mut count);
            assert!(!arguments.is_null());
            (0..count as usize)
                .map(|index| (*arguments.add(index)).to_string().unwrap())
                .collect()
        }
    }

    /// Every argument comes back from Windows' own parser as it went in.
    #[test]
    fn arguments_survive_the_round_trip() {
        let executable =
            "C:\\Program Files (x86)\\Steam\\common\\Monster Hunter World\\MonsterHunterWorld.exe";
        let arguments: Vec<String> = [
            "-windowed",
            "two words",
            "",
            "a \"quoted\" word",
            "ends with a backslash\\",
            "C:\\path with space\\",
            "backslashes before a quote \\\\\" here",
            "plain\\path\\no\\space",
            "tab\there",
            "\"",
            "\\\\",
        ]
        .iter()
        .map(|argument| argument.to_string())
        .collect();
        let line = command_line(executable, &arguments);
        let back = parsed(&line);
        assert_eq!(back[0], executable);
        assert_eq!(&back[1..], &arguments[..]);
    }

    /// An argument with nothing special in it is passed as it is.
    #[test]
    fn plain_arguments_are_not_quoted() {
        assert_eq!(quoted("-dx12"), "-dx12");
        assert_eq!(quoted("C:\\a\\b"), "C:\\a\\b");
        assert_eq!(quoted(""), "\"\"");
    }

    /// A zero exit code is a failure only when the module list agrees.
    #[test]
    fn a_zero_exit_code_alone_is_not_a_failed_injection() {
        assert!(was_loaded(Some(0x1234_0000), None));
        assert!(was_loaded(Some(0), Some(true)));
        assert!(!was_loaded(Some(0), Some(false)));
        assert!(!was_loaded(Some(0), None));
        assert!(was_loaded(None, Some(true)));
        assert!(!was_loaded(None, Some(false)));
    }

    /// A chain line is a path, which may hold spaces, and an optional word
    /// at its end.
    #[test]
    fn a_chain_line_is_a_path_and_an_optional_keyword_at_its_end() {
        let line = |text: &str| parse_chain_line(text).expect("a line");
        let plain = |name: &str, when: When| ChainLine {
            name: name.to_string(),
            when,
            complaint: None,
        };
        assert_eq!(parse_chain_line(""), None);
        assert_eq!(parse_chain_line("   # ReShade64.dll  renderer"), None);
        assert_eq!(line("mhw-dlss.dll"), plain("mhw-dlss.dll", When::Suspended));
        assert_eq!(
            line("ReShade64.dll  renderer"),
            plain("ReShade64.dll", When::Renderer)
        );
        assert_eq!(
            line("renderdoc.dll after 1000"),
            plain("renderdoc.dll", When::After(Some(1000)))
        );
        assert_eq!(
            line("renderdoc.dll AFTER"),
            plain("renderdoc.dll", When::After(None))
        );
        assert_eq!(
            line("..\\OptiScaler.dll renderer"),
            plain("..\\OptiScaler.dll", When::Renderer)
        );
        assert_eq!(line("..\\x.dll"), plain("..\\x.dll", When::Suspended));
        // The game's default install path has spaces in it.
        let spaced =
            "C:\\Program Files (x86)\\Steam\\steamapps\\common\\Monster Hunter World\\OptiScaler.dll";
        assert_eq!(line(spaced), plain(spaced, When::Suspended));
        assert_eq!(
            line(&format!("{spaced}  renderer")),
            plain(spaced, When::Renderer)
        );
        assert_eq!(
            line(&format!("  {spaced}\tafter   250  ")),
            plain(spaced, When::After(Some(250)))
        );
        // Quoted, with and without a keyword.
        assert_eq!(
            line(&format!("\"{spaced}\"")),
            plain(spaced, When::Suspended)
        );
        assert_eq!(
            line(&format!("\"{spaced}\" renderer")),
            plain(spaced, When::Renderer)
        );
        assert_eq!(
            line("\"a b.dll\" after 30"),
            plain("a b.dll", When::After(Some(30)))
        );
        // Quoted, what follows the quote is the keyword and is checked.
        let odd = line("\"a b.dll\" sometimes");
        assert_eq!((odd.name.as_str(), odd.when), ("a b.dll", When::Suspended));
        assert!(odd.complaint.is_some_and(|text| text.contains("sometimes")));
        let odd = line("\"a b.dll\" after soon");
        assert_eq!(odd.when, When::After(None));
        assert!(odd.complaint.is_some());
        // A name that is only a keyword is a name.
        assert_eq!(line("renderer"), plain("renderer", When::Suspended));
        // `after` with something that is not a number is part of the path.
        assert_eq!(
            line("my after party.dll"),
            plain("my after party.dll", When::Suspended)
        );
    }

    /// The mod's files are beside the launcher, or one folder down when the
    /// launcher was started from the game folder.
    #[test]
    fn the_mods_folder_is_found_from_either_place_the_launcher_can_be() {
        let game = Path::new("C:\\Game");
        let folder = game.join(HOME_FOLDER);
        let payload = folder.join(PAYLOAD);
        // In the folder with the mod.
        assert_eq!(
            home_of(&folder, |path| path == payload),
            (folder.clone(), false)
        );
        // In the game folder, the mod one folder down.
        assert_eq!(
            home_of(game, |path| path == payload),
            (folder.clone(), true)
        );
        // Nothing found: beside the launcher, where it is then reported missing.
        assert_eq!(home_of(game, |_| false), (game.to_path_buf(), false));
        assert_eq!(folder_above(&folder), Some(game.to_path_buf()));
        assert_eq!(folder_above(game), None);
    }

    /// The chain file is the one in the mod's folder, except that an earlier
    /// install's file in the game folder is used when the folder has none.
    #[test]
    fn the_chain_file_is_in_the_folder_unless_only_the_game_folder_has_one() {
        let game = Path::new("C:\\Game");
        let folder = game.join(HOME_FOLDER);
        let here = folder.join(CHAIN_FILE);
        let above = game.join(CHAIN_FILE);
        assert_eq!(chain_file(&folder, |_| false), (here.clone(), false));
        assert_eq!(chain_file(&folder, |_| true), (here.clone(), false));
        assert_eq!(
            chain_file(&folder, |path| path == above),
            (above.clone(), true)
        );
        // A folder that is not the mod's own has no folder above to look in.
        assert_eq!(
            chain_file(game, |path| path != game.join(CHAIN_FILE)),
            (game.join(CHAIN_FILE), false)
        );
    }

    /// Every spelling of the chain entry is the mod, and nothing else is.
    #[test]
    fn a_chain_entry_for_the_mod_is_known_however_it_is_spelled() {
        let folder = Path::new("C:\\Game\\mhw-dlss");
        // As the default chain says it, read from the folder.
        assert!(names_the_mod(
            folder,
            Path::new("c:/game/mhw-dlss/mhw-dlss.dll")
        ));
        // An earlier line read from inside the folder, and a bare name read
        // from the game folder.
        assert!(names_the_mod(
            folder,
            Path::new("C:\\Game\\mhw-dlss\\mhw-dlss\\mhw-dlss.dll")
        ));
        assert!(names_the_mod(folder, Path::new("C:\\Game\\mhw-dlss.dll")));
        assert!(!names_the_mod(folder, Path::new("C:\\Game\\ReShade64.dll")));
        assert!(!names_the_mod(folder, Path::new("C:\\Other\\mhw-dlss.dll")));
        assert!(!names_the_mod(
            folder,
            Path::new("C:\\Game\\mhw-dlss\\nvngx_dlss.dll")
        ));
    }

    /// With files on disk: an earlier spelling is pointed at the library in
    /// the folder, and a line for something else is not touched.
    #[test]
    fn an_earlier_chain_line_still_finds_the_mod() {
        let scratch =
            std::env::temp_dir().join(format!("mhw-dlss-launcher-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        let folder = scratch.join(HOME_FOLDER);
        std::fs::create_dir_all(&folder).unwrap();
        let payload = folder.join(PAYLOAD);
        let nested = folder.join(HOME_FOLDER).join(PAYLOAD);
        let above = scratch.join(PAYLOAD);

        // The library is not there: the entry is left to be reported missing.
        assert_eq!(where_the_mod_is(&folder, nested.clone()), nested);
        std::fs::write(&payload, b"x").unwrap();
        assert_eq!(where_the_mod_is(&folder, payload.clone()), payload);
        assert_eq!(where_the_mod_is(&folder, nested.clone()), payload);
        // A stray copy in the game folder is not what is injected.
        std::fs::write(&above, b"x").unwrap();
        assert_eq!(where_the_mod_is(&folder, above.clone()), payload);
        // Another library is left as it was given.
        let other = scratch.join("ReShade64.dll");
        assert_eq!(where_the_mod_is(&folder, other.clone()), other);
        // The default chain names the library beside the launcher.
        assert!(DEFAULT_CHAIN.lines().any(|line| line == PAYLOAD));
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn paths_are_compared_without_case_or_slash_direction() {
        assert!(same_file_name(
            "C:\\Game\\mhw-dlss.dll",
            "c:/game/mhw-dlss.dll"
        ));
        assert!(!same_file_name(
            "C:\\Game\\mhw-dlss.dll",
            "C:\\Other\\mhw-dlss.dll"
        ));
    }
}
