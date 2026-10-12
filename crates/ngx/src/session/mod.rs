//! The NGX session: the game's NGX entry points, the re-issued `Init`, the
//! serving host, and what the game is told about DLSS.
//!
//! # Wiring (`mod`)
//!
//! `mod` owns the single `KERNEL32!GetProcAddress` hook. For every name the
//! game resolves it calls the real `GetProcAddress`, then
//! [`set_original`]`(name, module, address)`; when that returns `true` it
//! hands the game [`intercept`]`(name)` instead of the address. The game's
//! static NGX library resolves its seven table entries inside its own `Init`
//! wrapper, before it calls `Init` (`docs/re/ReDlss1Path.md` §2.1), so the
//! originals are always known by the time a replacement runs.
//!
//! Before the game initialises NGX (its render device init), `mod` registers
//! its [`InitPolicy`] with [`set_init_policy`]; once the engine's DLSS 1
//! path is taken over it calls [`set_spoof_availability`]`(true)`.
//!
//! # Threads
//!
//! Every replacement runs on the game's thread that made the call. The
//! policy is called from inside the game's `Init`, before anything is
//! forwarded, with no lock of this crate held.

mod core;
mod hooks;

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;

use crate::host::{self, HostKind};
use crate::init::{Availability, GameInit, InitAction, Route};
use crate::status::Status;
use crate::TARGET_NGX as TARGET;

pub(crate) use self::core::Api;

/// Decides what happens to the game's `Init` (see [`InitAction`]). Called
/// once, on the game's thread, inside its `Init`; must not block.
pub type InitPolicy = fn(&GameInit) -> InitAction;

/// The NGX entry points the game resolves (its D3D12 table at
/// `0x1451d8808`, in table order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Entry {
    /// `NVSDK_NGX_D3D12_Init`: re-issued with API 21 and the mod folder.
    Init = 0,
    /// `NVSDK_NGX_D3D12_Shutdown`: this crate's features are released first.
    Shutdown = 1,
    /// `NVSDK_NGX_D3D12_GetScratchBufferSize`: forwarded and counted.
    GetScratchBufferSize = 2,
    /// `NVSDK_NGX_D3D12_GetParameters`: the availability answer.
    GetParameters = 3,
    /// `NVSDK_NGX_D3D12_CreateFeature`: forwarded (refused for an unusable
    /// host) and counted.
    CreateFeature = 4,
    /// `NVSDK_NGX_D3D12_EvaluateFeature`: stand-in handles never reach NGX.
    EvaluateFeature = 5,
    /// `NVSDK_NGX_D3D12_ReleaseFeature`: stand-in handles never reach NGX.
    ReleaseFeature = 6,
}

impl Entry {
    /// Every entry, in table order.
    pub const ALL: [Entry; 7] = [
        Entry::Init,
        Entry::Shutdown,
        Entry::GetScratchBufferSize,
        Entry::GetParameters,
        Entry::CreateFeature,
        Entry::EvaluateFeature,
        Entry::ReleaseFeature,
    ];

    /// The exported name.
    pub const fn name(self) -> &'static str {
        match self {
            Entry::Init => "NVSDK_NGX_D3D12_Init",
            Entry::Shutdown => "NVSDK_NGX_D3D12_Shutdown",
            Entry::GetScratchBufferSize => "NVSDK_NGX_D3D12_GetScratchBufferSize",
            Entry::GetParameters => "NVSDK_NGX_D3D12_GetParameters",
            Entry::CreateFeature => "NVSDK_NGX_D3D12_CreateFeature",
            Entry::EvaluateFeature => "NVSDK_NGX_D3D12_EvaluateFeature",
            Entry::ReleaseFeature => "NVSDK_NGX_D3D12_ReleaseFeature",
        }
    }

    /// The entry called `name`.
    pub fn from_name(name: &str) -> Option<Entry> {
        Entry::ALL.into_iter().find(|entry| entry.name() == name)
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// The game's resolved originals, by [`Entry`].
static ORIGINALS: [AtomicUsize; 7] = [const { AtomicUsize::new(0) }; 7];
/// Calls the game made, by [`Entry`].
static CALLS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
/// Stand-in handles kept from NGX (evaluate + release).
static STAND_IN_CALLS: AtomicU64 = AtomicU64::new(0);
/// NGX initialised by this crate, entry points bound, SuperSampling
/// available.
static READY: AtomicBool = AtomicBool::new(false);
/// `mod` allows the availability answer.
static SPOOF: AtomicBool = AtomicBool::new(false);
/// Times the availability answer was written.
static SPOOFED: AtomicU64 = AtomicU64::new(0);
static POLICY: Mutex<Option<InitPolicy>> = Mutex::new(None);
static STATE: Mutex<State> = Mutex::new(State::new());

/// Whether the serving host may be called.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostUse {
    /// Yes.
    Usable,
    /// A foreign host without every export this crate needs; never called.
    MissingExports(Vec<String>),
    /// A foreign host that failed after initialisation (or was left
    /// uninitialised); nothing further is asked of it.
    GaveUp(String),
}

/// The module that answered the game's resolve of `Init`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostInfo {
    /// Driver or foreign.
    pub kind: HostKind,
    /// Module base.
    pub module: usize,
    /// Full path.
    pub path: String,
    /// Whether it may be called.
    pub usable: HostUse,
    /// The last library request for the NGX core the mod saw
    /// ([`note_core_load`]): `(requested name, module returned)`.
    pub requested: Option<(String, usize)>,
}

impl HostInfo {
    /// "the driver's NGX" or the foreign module's file name, for messages.
    pub fn who(&self) -> String {
        match self.kind {
            HostKind::Driver => "the driver's NGX".to_string(),
            HostKind::Foreign => host::file_name(&self.path).to_string(),
        }
    }
}

/// What happened to the game's `Init`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitRecord {
    /// The policy's decision.
    pub action: InitAction,
    /// The entry point used, for a redirect that got as far as a call.
    pub route: Option<Route>,
    /// What the game was answered.
    pub status: Status,
}

/// Session state behind the lock.
#[derive(Debug)]
struct State {
    host: Option<HostInfo>,
    requested: Option<(String, usize)>,
    game: Option<GameInit>,
    init: Option<InitRecord>,
    /// Owned by the session while NGX may hold pointers into it.
    storage: Option<core::InitStorage>,
    /// An `Init` timed out and may still be running: `storage` stays.
    storage_pinned: bool,
    api: Option<Api>,
    bound_from: Option<String>,
    availability: Option<Availability>,
    /// Why the session is not ready, once that is known.
    refusal: Option<String>,
}

impl State {
    const fn new() -> State {
        State {
            host: None,
            requested: None,
            game: None,
            init: None,
            storage: None,
            storage_pinned: false,
            api: None,
            bound_from: None,
            availability: None,
            refusal: None,
        }
    }
}

/// The replacement for `name`, if this crate intercepts it.
///
/// Only names whose original [`set_original`] accepted may be replaced; the
/// D3D11 entry points are never intercepted (the mod is DX12-only).
pub fn intercept(name: &str) -> Option<usize> {
    let entry = Entry::from_name(name)?;
    (ORIGINALS[entry.index()].load(Ordering::Acquire) != 0).then(|| hooks::replacement(entry))
}

/// Record what the game resolved for `name` from `module`. Returns whether
/// the name is intercepted (then the game must get [`intercept`]`(name)`).
///
/// The resolve of `Init` decides the host: the module that answered it
/// serves NGX for the process. The first decision stands.
pub fn set_original(name: &str, module: usize, original: usize) -> bool {
    let Some(entry) = Entry::from_name(name) else {
        return false;
    };
    if original == 0 {
        return false;
    }
    ORIGINALS[entry.index()].store(original, Ordering::Release);
    if entry == Entry::Init {
        decide_host(module);
    }
    true
}

/// Note a library request the game made and the module it got, so the host
/// decision can say how a foreign module came to answer. Anything that is
/// not a request for the NGX core is ignored.
pub fn note_core_load(requested: &str, module: usize) {
    if host::is_core_request(requested) {
        STATE.lock().requested = Some((requested.to_string(), module));
    }
}

fn decide_host(module: usize) {
    let mut state = STATE.lock();
    if state.host.is_some() {
        return;
    }
    let path = mhw_win::module_path(module).unwrap_or_else(|| format!("<module {module:#x}>"));
    let kind = host::classify(&path);
    let usable = match kind {
        HostKind::Driver => HostUse::Usable,
        HostKind::Foreign => {
            let missing = core::missing_exports(module, &host::FOREIGN_EXPORTS);
            if missing.is_empty() {
                HostUse::Usable
            } else {
                HostUse::MissingExports(missing)
            }
        }
    };
    match (&kind, &usable) {
        (HostKind::Driver, _) => {
            log::info!(target: TARGET, path = path.as_str(); "NGX is served by the driver")
        }
        (HostKind::Foreign, HostUse::Usable) => log::warn!(target: TARGET,
            path = path.as_str(), requested:? = state.requested;
            "NGX is served by {}, not the driver; everything this mod calls is taken from it",
            host::file_name(&path)),
        (HostKind::Foreign, _) => log::error!(target: TARGET,
            path = path.as_str(), usable:? = usable;
            "NGX would be served by {}, which lacks exports this mod needs; it is never called \
             and DLSS will not run", host::file_name(&path)),
    }
    state.host = Some(HostInfo {
        kind,
        module,
        path,
        usable,
        requested: state.requested.clone(),
    });
}

/// Register `mod`'s decision for the game's `Init`. Without one the call is
/// passed through unchanged.
pub fn set_init_policy(policy: InitPolicy) {
    *POLICY.lock() = Some(policy);
}

/// Allow (or stop) telling the game SuperSampling and ImageSignalProcessing
/// are available. Effective only while the session is [`ready`] and NGX's
/// own block says SuperSampling is available: said to a machine where this
/// mod's DLSS cannot run, the game would render small for an upscale that
/// never comes.
pub fn set_spoof_availability(on: bool) {
    SPOOF.store(on, Ordering::Relaxed);
}

/// NGX is initialised by this crate, its entry points are bound and it
/// reports SuperSampling available. One relaxed load.
pub fn ready() -> bool {
    READY.load(Ordering::Relaxed)
}

/// Why the session is not ready, once known (for `mod`'s user message).
pub fn refusal() -> Option<String> {
    STATE.lock().refusal.clone()
}

/// What NGX's capability block said after initialisation.
pub fn availability() -> Option<Availability> {
    STATE.lock().availability
}

/// The serving host, once the game resolved `Init`.
pub fn host() -> Option<HostInfo> {
    STATE.lock().host.clone()
}

/// The device the game's NGX `Init` was given (as the game holds it: a
/// ReShade proxy when ReShade wraps the game's device), once it was called.
/// Feature evaluations need a command list from the same side of any proxy.
pub fn init_device() -> Option<usize> {
    STATE.lock().game.map(|game| game.device)
}

/// The bound feature entry points (for [`crate::feature`]).
pub(crate) fn api() -> Option<Api> {
    STATE.lock().api
}

/// Whether the host serving NGX is foreign (feature code notes its handles).
pub(crate) fn foreign_host() -> bool {
    STATE
        .lock()
        .host
        .as_ref()
        .is_some_and(|host| host.kind == HostKind::Foreign)
}

/// Stop calling a foreign host after a failure in feature code; the session
/// stops being ready. Does nothing for the driver.
pub(crate) fn give_up_on_foreign_host(reason: &str) {
    let mut state = STATE.lock();
    let Some(host) = state.host.as_mut() else {
        return;
    };
    if host.kind != HostKind::Foreign || host.usable != HostUse::Usable {
        return;
    }
    host.usable = HostUse::GaveUp(reason.to_string());
    let who = host.who();
    state.refusal = Some(format!("{who}: {reason}"));
    READY.store(false, Ordering::Relaxed);
    log::error!(target: TARGET, "{who}: {reason}; nothing further is asked of it");
}

/// Counters and state for the dev server.
#[cfg(feature = "dev")]
#[derive(Clone, Debug)]
pub struct SessionSnapshot {
    /// The serving host.
    pub host: Option<HostInfo>,
    /// The game's `Init` call.
    pub game: Option<GameInit>,
    /// What happened to it.
    pub init: Option<InitRecord>,
    /// Path of the module the feature entry points are bound from.
    pub bound_from: Option<String>,
    /// The capability answer.
    pub availability: Option<Availability>,
    /// [`ready`].
    pub ready: bool,
    /// [`refusal`].
    pub refusal: Option<String>,
    /// [`set_spoof_availability`] state.
    pub spoof_enabled: bool,
    /// Times the availability answer was written.
    pub spoofed: u64,
    /// Calls the game made per entry, with whether it is intercepted.
    pub calls: Vec<(Entry, u64, bool)>,
    /// Stand-in handles kept away from NGX.
    pub stand_in_calls: u64,
}

/// A snapshot of the session.
#[cfg(feature = "dev")]
pub fn snapshot() -> SessionSnapshot {
    let state = STATE.lock();
    SessionSnapshot {
        host: state.host.clone(),
        game: state.game,
        init: state.init.clone(),
        bound_from: state.bound_from.clone(),
        availability: state.availability,
        ready: ready(),
        refusal: state.refusal.clone(),
        spoof_enabled: SPOOF.load(Ordering::Relaxed),
        spoofed: SPOOFED.load(Ordering::Relaxed),
        calls: Entry::ALL
            .into_iter()
            .map(|entry| {
                (
                    entry,
                    CALLS[entry.index()].load(Ordering::Relaxed),
                    ORIGINALS[entry.index()].load(Ordering::Relaxed) != 0,
                )
            })
            .collect(),
        stand_in_calls: STAND_IN_CALLS.load(Ordering::Relaxed),
    }
}
