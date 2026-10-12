//! Which module serves NGX in this process.
//!
//! Normally the driver's: the game's static NGX library loads `nvngx.dll`
//! (exe folder, then the driver store via `D3DKMTQueryAdapterInfo`, then the
//! registry; `docs/re/ReDlss1Path.md` §2.2) and resolves its entry points
//! from it. A tool that presents itself as NGX changes that: OptiScaler, with
//! its DLSS inputs on, answers the game's load of the driver's path with its
//! own module. It is decided once, when the game resolves
//! `NVSDK_NGX_D3D12_Init`: the module that answered serves NGX, and every
//! entry point this crate calls is taken from it — never the driver's module
//! in its place.
//!
//! Why a foreign host is initialised through `Init_Ext`: the game was built
//! against SDK 1.3 and calls `Init(id, path, device, version)`, which is how
//! the driver's export reads it. OptiScaler 0.9 declares the same export
//! with the modern order `Init(id, path, device, const FeatureCommonInfo*,
//! version)` and `memcpy`s forty bytes from a non-null fourth argument — the
//! version number, an unmapped address. `Init_Ext(id, path, device, version,
//! info)` has one order everywhere.
//!
//! The classification is pure; the decision itself lives in the session.

use std::ffi::CStr;

/// What kind of module answered the game's resolve of `Init`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HostKind {
    /// The driver's NGX core.
    Driver,
    /// Another module (OptiScaler or similar).
    Foreign,
}

/// Exports a foreign host must have to be used at all: the initialisation
/// this crate calls on it and everything the feature code binds.
pub const FOREIGN_EXPORTS: [&CStr; 7] = [
    c"NVSDK_NGX_D3D12_Init_Ext",
    c"NVSDK_NGX_D3D12_GetCapabilityParameters",
    c"NVSDK_NGX_D3D12_AllocateParameters",
    c"NVSDK_NGX_D3D12_DestroyParameters",
    c"NVSDK_NGX_D3D12_CreateFeature",
    c"NVSDK_NGX_D3D12_EvaluateFeature",
    c"NVSDK_NGX_D3D12_ReleaseFeature",
];

/// The file name of a Windows or POSIX path.
pub fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// Whether `path` is the driver's NGX core: inside the driver store, or under
/// one of the two names the driver ships it as (an install whose core sits
/// in System32 keeps the path it always had).
pub fn classify(path: &str) -> HostKind {
    let lower = path.to_ascii_lowercase();
    let name = file_name(&lower);
    if lower.contains("\\driverstore\\") || name == "nvngx.dll" || name == "_nvngx.dll" {
        HostKind::Driver
    } else {
        HostKind::Foreign
    }
}

/// Whether a library request names the NGX core (`nvngx.dll` or
/// `_nvngx.dll`, any folder, any case) — the loads worth remembering for the
/// host decision's log line.
pub fn is_core_request(requested: &str) -> bool {
    let name = file_name(requested);
    name.eq_ignore_ascii_case("nvngx.dll") || name.eq_ignore_ascii_case("_nvngx.dll")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_drivers_core_is_recognised_by_folder_or_name() {
        let store =
            r"C:\Windows\System32\DriverStore\FileRepository\nv_dispi.inf_amd64_x\nvngx.dll";
        assert_eq!(classify(store), HostKind::Driver);
        assert_eq!(
            classify(r"C:\WINDOWS\system32\DRIVERSTORE\x\Something.dll"),
            HostKind::Driver
        );
        assert_eq!(
            classify(r"C:\Windows\System32\_nvngx.dll"),
            HostKind::Driver
        );
        assert_eq!(classify(r"C:\Windows\System32\NVNGX.DLL"), HostKind::Driver);
    }

    #[test]
    fn anything_else_is_foreign() {
        assert_eq!(classify(r"C:\Game\OptiScaler.dll"), HostKind::Foreign);
        assert_eq!(classify(r"C:\Game\dxgi.dll"), HostKind::Foreign);
        // A snippet is NVIDIA's but not the core.
        assert_eq!(classify(r"C:\Game\nvngx_dlss.dll"), HostKind::Foreign);
        assert_eq!(classify(""), HostKind::Foreign);
    }

    #[test]
    fn core_requests_match_on_the_file_name() {
        assert!(is_core_request("nvngx.dll"));
        assert!(is_core_request(r"C:\x\DriverStore\y\NVNGX.dll"));
        assert!(is_core_request("_nvngx.dll"));
        assert!(!is_core_request("nvngx_dlss.dll"));
        assert!(!is_core_request("xnvngx.dll"));
        assert!(!is_core_request("kernel32.dll"));
    }

    #[test]
    fn file_names_split_on_either_separator() {
        assert_eq!(file_name(r"C:\a\b.dll"), "b.dll");
        assert_eq!(file_name("/a/b.dll"), "b.dll");
        assert_eq!(file_name("b.dll"), "b.dll");
    }
}
