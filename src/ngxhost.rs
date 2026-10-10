//! Which module serves NGX for this process.
//!
//! Normally the driver's: the game loads `...\DriverStore\...\nvngx.dll` by
//! full path, and this library takes its feature entry points from the
//! `_nvngx.dll` that brings in. A tool that presents itself as NGX changes
//! the first half of that without changing the second. OptiScaler, with its
//! DLSS inputs on, answers the game's `LoadLibrary` of the driver's path with
//! its own module, so the game's `NVSDK_NGX_D3D12_Init` -- which this library
//! stands in for and forwards -- lands in OptiScaler, while `ngx::core` goes
//! on finding the driver's module by name: initialisation in one library and
//! evaluation in the other.
//!
//! So it is decided here, once, at the moment the game resolves
//! `NVSDK_NGX_D3D12_Init`: the module that answered that resolve is the one
//! that serves NGX. If it is the driver's, nothing in this file does
//! anything and every caller takes the path it always took. If it is not,
//! every entry point `ngx.rs` and `dlss.rs` call is taken from that module
//! and from no other, or from nowhere at all.
//!
//! WHERE THE FIRST RUN DIED, since it decides how the host is initialised.
//! The game was built against the SDK 1.3 surface and calls
//! `Init(id, path, device, version)`: the API version is the fourth
//! argument, and the driver's export reads it there. OptiScaler 0.9.4
//! declares the same export name with the modern static library's order,
//! `Init(id, path, device, const FeatureCommonInfo*, version)`
//! (`inputs/NVNGX_DLSS_Dx12.cpp:230-233`), and the first thing it does with a
//! non-null fourth argument is `memcpy` forty bytes out of it (239-240). The
//! stand-in passed 21 there, the game itself would have passed 18, and either
//! is a read of an unmapped page. `NVSDK_NGX_D3D12_Init_Ext` has one argument
//! order everywhere -- `(id, path, device, version, const FeatureCommonInfo*)`
//! -- so that is the entry point a foreign host is initialised through, and
//! its legacy `Init` is never called.

use parking_lot::Mutex;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use windows::core::PCSTR;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetProcAddress};

/// The game has not resolved `NVSDK_NGX_D3D12_Init` yet. Read as `DRIVER` by
/// every caller, which is what the code did before this file existed.
const UNDECIDED: u8 = 0;
const DRIVER: u8 = 1;
/// Another module answered, it exports everything this library calls, and
/// all of it is taken from there.
const FOREIGN: u8 = 2;
/// Another module answered and an entry point is missing. Never called.
const UNUSABLE: u8 = 3;
/// Another module answered, was initialised, and then failed at something.
/// Nothing further is asked of it.
const GAVE_UP: u8 = 4;

static STATE: AtomicU8 = AtomicU8::new(UNDECIDED);
static MODULE: AtomicUsize = AtomicUsize::new(0);
static INIT_EXT: AtomicUsize = AtomicUsize::new(0);
/// The host's file name, for log lines.
static NAME: Mutex<String> = Mutex::new(String::new());
/// The last request for the NGX core by file name, and the module it got.
static LOADED: Mutex<Option<(String, usize)>> = Mutex::new(None);

const FEATURE_NOT_SUPPORTED: u32 = 0xBAD0_0001;

/// Everything `ngx.rs` and `dlss.rs` call, plus the initialisation used here.
const REQUIRED: [&[u8]; 7] = [
    b"NVSDK_NGX_D3D12_Init_Ext\0",
    b"NVSDK_NGX_D3D12_GetCapabilityParameters\0",
    b"NVSDK_NGX_D3D12_AllocateParameters\0",
    b"NVSDK_NGX_D3D12_DestroyParameters\0",
    b"NVSDK_NGX_D3D12_CreateFeature\0",
    b"NVSDK_NGX_D3D12_EvaluateFeature\0",
    b"NVSDK_NGX_D3D12_ReleaseFeature\0",
];

/// `NVSDK_NGX_PathListInfo`, from `nvsdk_ngx_defs.h`.
#[repr(C)]
struct PathListInfo {
    path: *const *const u16,
    length: u32,
}

/// `NVSDK_NGX_LoggingInfo`. All zero is "no application logging".
#[repr(C)]
struct LoggingInfo {
    callback: usize,
    minimum_level: u32,
    disable_other_sinks: bool,
}

/// `NVSDK_NGX_FeatureCommonInfo`: where else to look for a feature's library.
#[repr(C)]
struct FeatureCommonInfo {
    paths: PathListInfo,
    internal: *mut c_void,
    logging: LoggingInfo,
}

type InitExtFn =
    unsafe extern "C" fn(u64, *const u16, *mut c_void, u32, *const FeatureCommonInfo) -> u32;

fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// Whether a path is the driver's NGX core: in the driver store, or under one
/// of the two names the driver ships it as. The second half keeps an install
/// whose core sits in System32 on the path it has always taken.
fn is_drivers(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = file_name(&lower);
    lower.contains("\\driverstore\\") || name == "nvngx.dll" || name == "_nvngx.dll"
}

unsafe fn path_of(module: HMODULE) -> String {
    let mut buffer = [0u16; 512];
    let length = GetModuleFileNameW(module, &mut buffer) as usize;
    if length == 0 {
        return format!("<module {:#x}>", module.0 as usize);
    }
    String::from_utf16_lossy(&buffer[..length])
}

/// Remember what a request for the NGX core was answered with.
///
/// Called for every library the game loads, so it decides on the last nine
/// bytes of the name and does nothing else for anything that is not
/// `nvngx.dll` or `_nvngx.dll`.
pub fn note_load(requested: &str, module: HMODULE) {
    let bytes = requested.as_bytes();
    if bytes.len() < 9 || !bytes[bytes.len() - 9..].eq_ignore_ascii_case(b"nvngx.dll") {
        return;
    }
    *LOADED.lock() = Some((requested.to_string(), module.0 as usize));
}

/// Decide which module serves NGX, from the one that answered the game's
/// resolve of `NVSDK_NGX_D3D12_Init`. The first call decides; later ones
/// return at once.
///
/// # Safety
/// `module` must be a loaded module.
pub unsafe fn decide(module: HMODULE) {
    if STATE.load(Ordering::Relaxed) != UNDECIDED {
        return;
    }
    let path = path_of(module);
    if is_drivers(&path) {
        STATE.store(DRIVER, Ordering::Relaxed);
        return;
    }

    let name = file_name(&path).to_string();
    let asked = match LOADED.lock().clone() {
        Some((requested, handle)) if handle == module.0 as usize => {
            format!("the game asked LoadLibrary for {requested} and was returned this module")
        }
        Some((requested, handle)) => format!(
            "the game's last request for the NGX core was {requested}, which returned module \
             {handle:#x}, and it resolved NVSDK_NGX_D3D12_Init from this one at {:#x}",
            module.0 as usize
        ),
        None => "no request for nvngx.dll was seen by this library's own hooks".to_string(),
    };
    let missing: Vec<String> = REQUIRED
        .iter()
        .filter(|export| GetProcAddress(module, PCSTR(export.as_ptr())).is_none())
        .map(|export| String::from_utf8_lossy(&export[..export.len() - 1]).into_owned())
        .collect();
    *NAME.lock() = name.clone();

    if missing.is_empty() {
        let init_ext = GetProcAddress(module, PCSTR(REQUIRED[0].as_ptr()))
            .map(|entry| entry as usize)
            .unwrap_or(0);
        MODULE.store(module.0 as usize, Ordering::Relaxed);
        INIT_EXT.store(init_ext, Ordering::Relaxed);
        STATE.store(FOREIGN, Ordering::Relaxed);
        crate::log!(
            "[ngx] NGX is served by {name} at {path}, not by the driver's nvngx: {asked}. Every \
             NGX entry point this library calls is taken from that module and from no other \
             (Init_Ext, GetCapabilityParameters, AllocateParameters, DestroyParameters, \
             CreateFeature, EvaluateFeature, ReleaseFeature; all seven are exported)"
        );
    } else {
        STATE.store(UNUSABLE, Ordering::Relaxed);
        crate::log!(
            "[ngx] NGX would be served by {name} at {path}, not by the driver's nvngx ({asked}), \
             but it does not export {}. It is not called at all: the game's NGX initialisation \
             is answered with FeatureNotSupported, DLSS will not resolve in this run, and \
             nothing is taken from the driver's module in its place",
            missing.join(", ")
        );
    }
}

/// Whether a foreign module serves NGX and is in use. One relaxed load.
pub fn foreign() -> bool {
    STATE.load(Ordering::Relaxed) == FOREIGN
}

/// Whether a foreign module answered and is never to be called.
pub fn unusable() -> bool {
    STATE.load(Ordering::Relaxed) == UNUSABLE
}

/// Whether nothing may be asked of NGX any more: a foreign module that is
/// unusable, or one that failed after it was initialised. Never true while
/// the driver serves NGX.
pub fn refused() -> bool {
    STATE.load(Ordering::Relaxed) >= UNUSABLE
}

/// Who answers the game's NGX questions, for a log line: "the driver", or the
/// foreign module's file name.
pub fn answerer() -> String {
    match STATE.load(Ordering::Relaxed) {
        UNDECIDED | DRIVER => "the driver".to_string(),
        _ => NAME.lock().clone(),
    }
}

/// Where the feature entry points are to be bound from.
pub enum Serving {
    /// The driver's module, found the way it always was.
    Driver,
    /// This module, and only this one.
    Foreign(HMODULE),
    /// Nowhere: a foreign module answered and cannot be used.
    Nothing,
}

pub fn serving() -> Serving {
    match STATE.load(Ordering::Relaxed) {
        FOREIGN => Serving::Foreign(HMODULE(MODULE.load(Ordering::Relaxed) as *mut c_void)),
        UNUSABLE | GAVE_UP => Serving::Nothing,
        _ => Serving::Driver,
    }
}

/// Initialise a foreign host in the game's place.
///
/// `None` when the driver serves NGX: the caller then makes exactly the call
/// it always made. `Some(result)` otherwise, and the caller must not call the
/// entry point the game resolved -- see the note at the top of this file.
///
/// What the host is given: the application id and the device unchanged, the
/// application data path the caller chose, the API version in the argument
/// that is the version in every implementation, and -- when this library's
/// snippet folder is known -- that folder as the one entry of the feature
/// path list, which is how an NGX client says where its feature libraries
/// are. The logging fields are zero: no callback.
///
/// # Safety
/// `data_path` must be a NUL-terminated wide string or zero, and `device` a
/// live `ID3D12Device`.
pub unsafe fn init(app_id: u64, data_path: usize, device: usize, version: usize) -> Option<u32> {
    match STATE.load(Ordering::Relaxed) {
        FOREIGN => {}
        UNUSABLE | GAVE_UP => {
            crate::log!(
                "[ngx] {} is not initialised; the game's Init is answered with \
                 FeatureNotSupported",
                NAME.lock()
            );
            return Some(FEATURE_NOT_SUPPORTED);
        }
        _ => return None,
    }
    let entry = INIT_EXT.load(Ordering::Relaxed);
    let name = NAME.lock().clone();
    if entry == 0 {
        give_up("its NVSDK_NGX_D3D12_Init_Ext was not recorded");
        return Some(FEATURE_NOT_SUPPORTED);
    }

    // Leaked, once per initialisation: a few hundred bytes that the host is
    // free to keep pointers into for as long as it likes.
    static CURRENT_DIRECTORY: [u16; 2] = [b'.' as u16, 0];
    let data_path = if data_path == 0 {
        CURRENT_DIRECTORY.as_ptr()
    } else {
        data_path as *const u16
    };
    let folder = crate::ngx::data_path();
    let info: *const FeatureCommonInfo = match &folder {
        Some(folder) => {
            let wide: &'static [u16] = Box::leak(
                folder
                    .to_string_lossy()
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect::<Vec<u16>>()
                    .into_boxed_slice(),
            );
            let list: &'static [*const u16; 1] = Box::leak(Box::new([wide.as_ptr()]));
            Box::leak(Box::new(FeatureCommonInfo {
                paths: PathListInfo {
                    path: list.as_ptr(),
                    length: 1,
                },
                internal: std::ptr::null_mut(),
                logging: LoggingInfo {
                    callback: 0,
                    minimum_level: 0,
                    disable_other_sinks: false,
                },
            }))
        }
        None => std::ptr::null(),
    };
    crate::log!(
        "[ngx] calling {name}!NVSDK_NGX_D3D12_Init_Ext(app_id={app_id}, data path, device \
         {device:#x}, version={version}, {}) and not the NVSDK_NGX_D3D12_Init the game resolved: \
         the game's argument order puts the API version fourth, where that export takes a pointer",
        match &folder {
            Some(folder) => format!("a feature path list of one entry, {}", folder.display()),
            None => "no feature path list, because this library's snippet folder was not \
                     published in time"
                .to_string(),
        }
    );
    let call: InitExtFn = std::mem::transmute(entry);
    Some(call(
        app_id,
        data_path,
        device as *mut c_void,
        version as u32,
        info,
    ))
}

/// Stop asking anything of a foreign host, and say why in one line.
///
/// Does nothing while the driver serves NGX, and nothing the second time.
pub fn give_up(reason: &str) {
    if STATE
        .compare_exchange(FOREIGN, GAVE_UP, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    crate::log!(
        "[ngx] {}: {reason}. Nothing further is asked of it, DLSS will not resolve for the rest \
         of this run, and nothing is taken from the driver's module in its place",
        NAME.lock()
    );
}

/// Decide that a foreign host is not going to be initialised at all, and say
/// why in one line. From then on it is treated like one with an export
/// missing: never called, and the game's own NGX calls into it are answered
/// here.
///
/// For a session in which this library initialises nothing (`gate` stood
/// down). Passing the game's own `Init` through instead is not available:
/// see the note at the top of this file.
pub fn leave_uncalled(reason: &str) {
    if STATE
        .compare_exchange(FOREIGN, UNUSABLE, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    crate::log!(
        "[ngx] {} serves NGX in the driver's place and is not initialised: {reason}. It is not \
         called at all, and nothing is taken from the driver's module in its place",
        NAME.lock()
    );
}

static SAID_FEATURE: AtomicBool = AtomicBool::new(false);

/// What a foreign host answered the first feature creation with.
///
/// `NVSDK_NGX_Handle` is one `unsigned int`, the feature's id. OptiScaler
/// numbers the features it owns itself from a high offset and marks a
/// parameter object of its own class with the key `OptiScaler`; the driver's
/// object, which it hands through while DLSS is its back end, has no such
/// key. Both are read, neither is relied on.
///
/// # Safety
/// `handle` must be the handle `CreateFeature` just returned.
pub unsafe fn note_feature(handle: *mut c_void, own_marker: Option<i32>) {
    if SAID_FEATURE.swap(true, Ordering::Relaxed) || handle.is_null() {
        return;
    }
    let id = std::ptr::read_unaligned(handle as *const u32);
    crate::log!(
        "[ngx] {} answered CreateFeature with a handle whose id is {id}; the parameter object \
         it allocated {}",
        NAME.lock(),
        match own_marker {
            Some(value) => format!(
                "carries the key \"OptiScaler\" = {value}: it is that module's own object, not \
                 the driver's"
            ),
            None => "does not carry the key \"OptiScaler\", which is how the driver's own \
                     object reads when it is handed through"
                .to_string(),
        }
    );
}

/// The value of `key` in `[section]` of an ini file's text.
///
/// Enough of the format for a report: sections, `key=value`, `;` comments, a
/// byte order mark, names compared without case. A later duplicate wins, as
/// it does in the readers this is used to describe.
fn ini_value(text: &str, section: &str, key: &str) -> Option<String> {
    let mut inside = false;
    let mut found = None;
    for line in text.lines() {
        let line = line.trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            inside = name.trim().eq_ignore_ascii_case(section);
            continue;
        }
        if !inside {
            continue;
        }
        if let Some((name, value)) = line.split_once('=') {
            if name.trim().eq_ignore_ascii_case(key) {
                found = Some(value.trim().to_string());
            }
        }
    }
    found
}

/// Say what else is true in the process now that a foreign host is bound.
///
/// From `ngx::after_init`, inside the game's own initialisation of NGX and
/// once per session: this reads a file. The settings are the host's, read
/// from `OptiScaler.ini` beside it when there is one, and reported as the
/// file has them -- OptiScaler's menu can change a value without writing it.
pub fn report_bound() {
    if !foreign() {
        return;
    }
    let name = NAME.lock().clone();
    let module = HMODULE(MODULE.load(Ordering::Relaxed) as *mut c_void);
    let ini = std::path::PathBuf::from(unsafe { path_of(module) })
        .parent()
        .map(|folder| folder.join("OptiScaler.ini"));
    let text = ini
        .as_ref()
        .and_then(|path| std::fs::read_to_string(path).ok());
    let configured = crate::config::get().mip_bias;

    let Some(text) = text else {
        crate::log!(
            "[ngx] the capability readings below are {name}'s answers, not the driver's. No \
             OptiScaler.ini was read beside it, so its settings are not known here. mip_bias = \
             {configured} in mhw-dlss.ini stays as configured; if {name} overrides the texture \
             LOD bias as well, both apply"
        );
        return;
    };
    let value = |section: &str, key: &str| {
        ini_value(&text, section, key).unwrap_or_else(|| "absent".to_string())
    };
    crate::log!(
        "[ngx] the capability readings below are {name}'s answers, not the driver's. {} has, on \
         disk now: [Inputs] EnableDlssInputs={}, [DLSS] Enabled={}, UseGenericAppIdWithDlss={}, \
         [Upscalers] Dx12Upscaler={}, [Libraries] NvngxFeaturePath={}, NvngxDlssPath={}, \
         [Spoofing] StreamlineSpoofing={}, Dxgi={} (auto is that program's default)",
        ini.as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        value("Inputs", "EnableDlssInputs"),
        value("DLSS", "Enabled"),
        value("DLSS", "UseGenericAppIdWithDlss"),
        value("Upscalers", "Dx12Upscaler"),
        value("Libraries", "NvngxFeaturePath"),
        value("Libraries", "NvngxDlssPath"),
        value("Spoofing", "StreamlineSpoofing"),
        value("Spoofing", "Dxgi"),
    );
    let theirs = value("Mipmap", "MipmapBiasOverride");
    crate::log!(
        "[ngx] two texture LOD bias settings exist in this process: mip_bias = {configured} in \
         mhw-dlss.ini, which stays as configured, and [Mipmap] MipmapBiasOverride={theirs} in \
         OptiScaler.ini{}",
        if theirs.eq_ignore_ascii_case("auto") || theirs == "absent" {
            ", which is off at that value"
        } else {
            ". Both apply while both are set"
        }
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_drivers_core_is_recognised_by_folder_or_by_name() {
        assert!(is_drivers(
            r"C:\Windows\System32\DriverStore\FileRepository\nv_dispi.inf_amd64_x\nvngx.dll"
        ));
        assert!(is_drivers(
            r"C:\WINDOWS\system32\driverstore\filerepository\nv_dispi.inf_amd64_x\_nvngx.dll"
        ));
        assert!(is_drivers(r"C:\Windows\System32\nvngx.dll"));
        assert!(!is_drivers(r"C:\game\OptiScaler.dll"));
        assert!(!is_drivers(r"C:\game\winmm.dll"));
        // A snippet is NVIDIA's and still is not the core.
        assert!(!is_drivers(r"C:\game\nvngx_dlss.dll"));
    }

    #[test]
    fn only_the_core_is_remembered_as_a_load() {
        *LOADED.lock() = None;
        note_load("kernel32.dll", HMODULE(std::ptr::dangling_mut::<c_void>()));
        note_load("nvngx_dlss.dll", HMODULE(2 as *mut c_void));
        assert!(LOADED.lock().is_none());
        note_load(r"C:\x\DriverStore\y\NVNGX.dll", HMODULE(3 as *mut c_void));
        assert_eq!(LOADED.lock().as_ref().map(|(_, handle)| *handle), Some(3));
        note_load(r"C:\x\DriverStore\y\_nvngx.dll", HMODULE(4 as *mut c_void));
        assert_eq!(LOADED.lock().as_ref().map(|(_, handle)| *handle), Some(4));
        *LOADED.lock() = None;
    }

    /// The layout `nvsdk_ngx_defs.h` gives on x64: a pointer and a length,
    /// a pointer, then a pointer, an enum and a bool.
    #[test]
    fn the_feature_info_has_the_headers_layout() {
        assert_eq!(std::mem::size_of::<PathListInfo>(), 16);
        assert_eq!(std::mem::size_of::<LoggingInfo>(), 16);
        assert_eq!(std::mem::size_of::<FeatureCommonInfo>(), 40);
        assert_eq!(std::mem::offset_of!(FeatureCommonInfo, internal), 16);
        assert_eq!(std::mem::offset_of!(FeatureCommonInfo, logging), 24);
        assert_eq!(std::mem::offset_of!(LoggingInfo, minimum_level), 8);
        assert_eq!(std::mem::offset_of!(LoggingInfo, disable_other_sinks), 12);
    }

    #[test]
    fn ini_values_are_read_by_section() {
        let text = "\u{feff}; comment\n[Inputs]\nEnableDlssInputs=false\n\n[DLSS]\n; Enabled=no\n\
                    Enabled = auto \n[mipmap]\nMipmapBiasOverride=-1.5\n";
        assert_eq!(
            ini_value(text, "Inputs", "EnableDlssInputs").as_deref(),
            Some("false")
        );
        assert_eq!(ini_value(text, "DLSS", "Enabled").as_deref(), Some("auto"));
        assert_eq!(
            ini_value(text, "Mipmap", "mipmapbiasoverride").as_deref(),
            Some("-1.5")
        );
        assert_eq!(ini_value(text, "DLSS", "EnableDlssInputs"), None);
        assert_eq!(ini_value(text, "Log", "LogLevel"), None);
    }

    /// With nothing decided every caller is told what it was told before this
    /// module existed: the driver serves NGX, nothing is refused, and the
    /// stand-in makes its own call.
    #[test]
    fn undecided_reads_as_the_driver() {
        assert_eq!(STATE.load(Ordering::Relaxed), UNDECIDED);
        assert!(!foreign() && !unusable() && !refused());
        assert_eq!(answerer(), "the driver");
        assert!(matches!(serving(), Serving::Driver));
        assert_eq!(unsafe { init(1, 0, 0, 21) }, None);
        give_up("a test");
        assert_eq!(STATE.load(Ordering::Relaxed), UNDECIDED);
    }
}
