//! NGX initialisation: what the game passes, what this crate passes instead,
//! and what the capability block says afterwards. Pure.
//!
//! The game calls `NVSDK_NGX_D3D12_Init(0x5f8a85f, L"DLSS", device, 0x12)`
//! from its static NGX library `rel_1_3` (`docs/re/ReDlss1Path.md` §2.2).
//! With API version 18 the driver reports SuperSampling unavailable for a
//! 310.x library, so the call is re-issued with version 21 and the mod folder
//! as application data path. Two more facts shape it:
//!
//! * At the call, the caller's stack holds the adapter LUID in the slot of a
//!   fifth argument and 0 in the sixth (`0x14273f2cd..e5`, `0x14273f275`).
//!   The 0.2 code forwarded both; a legacy export that reads a fifth
//!   argument for versions ≥ 0x13 would take the LUID for a
//!   `FeatureCommonInfo*`. [`InitTail`] chooses (RE §3.4 recommends zeros;
//!   experiment E7 compares).
//! * Whether an over-the-air DLSS update is applied to app id 0x5f8a85f is
//!   unknown (E4). [`Identity`] lets a dev build initialise by project id
//!   instead, to A/B it.

use std::ffi::{c_void, CStr};
use std::path::PathBuf;
use std::time::Duration;

use crate::host::HostKind;
use crate::param::{names, Value, Write};
use crate::status::Status;

/// `NVSDK_NGX_Version_API` of SDK 310.x, which a 310.x DLSS library needs.
pub const API_VERSION: u32 = 0x15;
/// The API version the game asks for (SDK 1.3).
pub const GAME_API_VERSION: u32 = 0x12;
/// Monster Hunter: World's NGX application id, as the game passes it.
pub const MHW_APP_ID: u64 = 0x05F8_A85F;
/// The DLSS library a 310.x client needs, by the name NGX loads it by.
pub const SNIPPET: &str = "nvngx_dlss.dll";
/// `NVSDK_NGX_ENGINE_TYPE_CUSTOM`.
pub const ENGINE_TYPE_CUSTOM: u32 = 0;

/// The game's call, as six machine words.
///
/// The game is a working NGX client, so what it passes is correct by
/// construction; the Microsoft x64 convention lets a caller pass more than
/// the callee reads, so six words are taken and forwarded whatever the real
/// arity. The last two are not arguments the game meant: they are whatever
/// its stack holds there (the adapter LUID and 0, see the module notes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GameInit {
    /// `InApplicationId` (0x5f8a85f).
    pub app_id: u64,
    /// `InApplicationDataPath` (`const wchar_t*`, `L"DLSS"`).
    pub data_path: usize,
    /// `InDevice` (`ID3D12Device*`, as the game holds it: a ReShade proxy
    /// when ReShade is loaded, which is passed on unwrapped).
    pub device: usize,
    /// `InSDKVersion` (0x12).
    pub version: usize,
    /// The stack slots of a fifth and sixth argument.
    pub stack: [usize; 2],
}

/// What NGX is initialised as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Identity {
    /// The application id the game passed (0x5f8a85f): the 0.2 behaviour.
    GameAppId,
    /// Another application id.
    AppId(u64),
    /// `Init_ProjectID` with `NVSDK_NGX_ENGINE_TYPE_CUSTOM`. Both strings must
    /// be non-empty.
    ProjectId {
        /// `InProjectId` (a GUID string).
        project_id: String,
        /// `InEngineVersion`.
        engine_version: String,
    },
}

/// What goes in the fifth and sixth argument slots of the legacy `Init`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InitTail {
    /// Zeros (RE recommendation).
    #[default]
    Zero,
    /// What the game's stack held (LUID, 0): the 0.2 behaviour.
    Game,
}

/// How the game's `Init` is re-issued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitOptions {
    /// Application data path: the mod folder. NGX writes its log there; the
    /// DLSS library is found beside the module making the call (the mod
    /// DLL lives in this folder) and, for `Init_Ext`/`Init_ProjectID`, in
    /// this folder as the one entry of the feature path list.
    pub data_path: PathBuf,
    /// `InSDKVersion`, [`API_VERSION`].
    pub api_version: u32,
    /// Application id or project id.
    pub identity: Identity,
    /// Legacy `Init` tail.
    pub tail: InitTail,
    /// Make the call on a thread of its own and give up after this long
    /// (answering `FeatureNotSupported`). For sessions in which Streamline
    /// already faulted: a driver `Init` after that was seen never to return.
    pub timeout: Option<Duration>,
}

impl InitOptions {
    /// API 21, the game's app id, zero tail, no timeout.
    pub fn new(data_path: PathBuf) -> InitOptions {
        InitOptions {
            data_path,
            api_version: API_VERSION,
            identity: Identity::GameAppId,
            tail: InitTail::Zero,
            timeout: None,
        }
    }
}

/// What to do with the game's `Init`, decided by `mod`'s init policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InitAction {
    /// Forward the call exactly as made (the mod stands down). Not done into
    /// a foreign host, whose `Init` reads the game's arguments wrongly: that
    /// is answered with `FeatureNotSupported` instead.
    PassThrough,
    /// Re-issue it with these options.
    Redirect(InitOptions),
    /// Answer `FeatureNotSupported` without calling anything.
    Refuse,
}

/// The entry point a redirected `Init` goes through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// The `NVSDK_NGX_D3D12_Init` the game resolved, `(id, path, device,
    /// version, tail[0], tail[1])`.
    Legacy {
        /// The fifth and sixth words.
        tail: [usize; 2],
    },
    /// `NVSDK_NGX_D3D12_Init_Ext(id, path, device, version, info)`.
    InitExt,
    /// `NVSDK_NGX_D3D12_Init_ProjectID(project, engine type, engine
    /// version, path, device, version, info)`.
    ProjectId,
}

impl Route {
    /// The export to resolve from the host; `None` for the game's own entry.
    pub const fn export(self) -> Option<&'static CStr> {
        match self {
            Route::Legacy { .. } => None,
            Route::InitExt => Some(c"NVSDK_NGX_D3D12_Init_Ext"),
            Route::ProjectId => Some(c"NVSDK_NGX_D3D12_Init_ProjectID"),
        }
    }
}

/// A redirected `Init`, ready to be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Which entry point.
    pub route: Route,
    /// The application id (unused by [`Route::ProjectId`]).
    pub app_id: u64,
}

/// Choose the entry point for `identity` on `host`.
pub fn plan(host: HostKind, identity: &Identity, tail: InitTail, game: &GameInit) -> Plan {
    let app_id = match identity {
        Identity::GameAppId | Identity::ProjectId { .. } => game.app_id,
        Identity::AppId(id) => *id,
    };
    let route = match (identity, host) {
        (Identity::ProjectId { .. }, _) => Route::ProjectId,
        (_, HostKind::Foreign) => Route::InitExt,
        (_, HostKind::Driver) => Route::Legacy {
            tail: match tail {
                InitTail::Zero => [0, 0],
                InitTail::Game => game.stack,
            },
        },
    };
    Plan { route, app_id }
}

/// `NVSDK_NGX_PathListInfo`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PathListInfo {
    /// `wchar_t const* const* Path`.
    pub path: *const *const u16,
    /// `unsigned int Length`.
    pub length: u32,
}

/// `NVSDK_NGX_LoggingInfo`; all zero is "no application logging".
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LoggingInfo {
    /// `NVSDK_NGX_AppLogCallback LoggingCallback`.
    pub callback: usize,
    /// `NVSDK_NGX_Logging_Level MinimumLoggingLevel`.
    pub minimum_level: u32,
    /// `bool DisableOtherLoggingSinks`.
    pub disable_other_sinks: bool,
}

/// `NVSDK_NGX_FeatureCommonInfo`: where else to look for feature libraries.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FeatureCommonInfo {
    /// `PathListInfo`.
    pub paths: PathListInfo,
    /// `InternalData`, always null.
    pub internal: *mut c_void,
    /// `LoggingInfo`.
    pub logging: LoggingInfo,
}

impl FeatureCommonInfo {
    /// A feature path list of `count` entries at `list`, no logging callback.
    pub const fn with_paths(list: *const *const u16, count: u32) -> FeatureCommonInfo {
        FeatureCommonInfo {
            paths: PathListInfo {
                path: list,
                length: count,
            },
            internal: std::ptr::null_mut(),
            logging: LoggingInfo {
                callback: 0,
                minimum_level: 0,
                disable_other_sinks: false,
            },
        }
    }
}

/// What the capability block says about DLSS-SR after initialisation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Availability {
    /// `SuperSampling.Available`.
    pub available: Option<i32>,
    /// `SuperSampling.FeatureInitResult`.
    pub init_result: Option<Status>,
    /// `SuperSampling.NeedsUpdatedDriver`.
    pub needs_updated_driver: Option<i32>,
    /// `SuperSampling.MinDriverVersionMajor/Minor`.
    pub min_driver: (Option<u32>, Option<u32>),
}

impl Availability {
    /// Whether the block says yes (exactly `Available = 1`; unset is no).
    pub fn usable(&self) -> bool {
        self.available == Some(1)
    }

    /// Why DLSS cannot be used, in words, or `None` when it can. `who` names
    /// the module whose answer it is ("the driver's NGX", "OptiScaler.dll").
    pub fn refusal(&self, who: &str) -> Option<String> {
        if self.usable() {
            return None;
        }
        let shown = |value: Option<String>| value.unwrap_or_else(|| "unset".to_string());
        let mut text = format!(
            "{who} reports SuperSampling.Available = {} with FeatureInitResult {}",
            shown(self.available.map(|v| v.to_string())),
            shown(self.init_result.map(|s| s.to_string())),
        );
        if self.needs_updated_driver == Some(1) {
            text.push_str(&format!(
                ", and says the driver has to be updated to at least {}.{}",
                self.min_driver.0.unwrap_or(0),
                self.min_driver.1.unwrap_or(0)
            ));
        }
        Some(text)
    }
}

/// The writes that tell the game its DLSS (both halves) is available.
///
/// The game wants SuperSampling (feature 1) **and** ImageSignalProcessing
/// (feature 9, DLSS 1's upscale/sharpen half, which current drivers do not
/// carry); without the second it hides its DLSS option and with it the
/// render-small-then-upscale path this mod takes over. The game's check
/// reads `*.Available` (failure codes 2/3 at `0x1451c6c28`); the other keys
/// are cleared so nothing it logs contradicts them.
pub const fn availability_writes() -> [Write; 8] {
    let success = Status::SUCCESS.0 as i32;
    [
        Write::new(names::SS_AVAILABLE, Value::I32(1)),
        Write::new(names::SS_NEEDS_UPDATED_DRIVER, Value::I32(0)),
        Write::new(names::SS_FEATURE_INIT_RESULT, Value::I32(success)),
        Write::new(names::SS_MIN_DRIVER_MAJOR, Value::U32(0)),
        Write::new(names::SS_MIN_DRIVER_MINOR, Value::U32(0)),
        Write::new(names::ISP_AVAILABLE, Value::I32(1)),
        Write::new(names::ISP_NEEDS_UPDATED_DRIVER, Value::I32(0)),
        Write::new(names::ISP_FEATURE_INIT_RESULT, Value::I32(success)),
    ]
}

#[cfg(test)]
mod tests;
