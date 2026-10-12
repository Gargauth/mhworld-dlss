//! Process-wide state shared by the detours and the API: the gate's
//! addresses, the parameters, the handler, counters and last values.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;

use mhw_core::frame::{CameraFrame, JitterSample};
use parking_lot::{Mutex, RwLock};

use crate::gate::{Addresses, StandDown};
use crate::params::Params;
use crate::status::Counters;
use crate::upscale::UpscaleHandler;

/// The addresses of the first successful gate; read by every detour.
pub(crate) static ADDRESSES: OnceLock<Addresses> = OnceLock::new();
/// The last gate's outcome.
pub(crate) static LAST_GATE: Mutex<Option<Result<Addresses, StandDown>>> = Mutex::new(None);
/// Whether the detours are in.
pub(crate) static INSTALLED: AtomicBool = AtomicBool::new(false);
/// What policy decided.
static PARAMS: RwLock<Params> = RwLock::new(Params::RELEASE);
/// Who handles the evaluations.
static HANDLER: RwLock<Option<&'static dyn UpscaleHandler>> = RwLock::new(None);
/// Whether the SS depth alias is on.
pub(crate) static DEPTH_ALIAS: AtomicBool = AtomicBool::new(false);
/// The game's reset, latched by the SS enqueue for the next EvalISP.
pub(crate) static RESET_PENDING: AtomicBool = AtomicBool::new(false);
/// The camera and jitter last published.
pub(crate) static LAST_FRAME: Mutex<Option<(CameraFrame, JitterSample)>> = Mutex::new(None);
/// The jitter of the last main viewport set up, whether or not its camera
/// was usable (published or not).
pub(crate) static LAST_JITTER: Mutex<Option<JitterSample>> = Mutex::new(None);

/// The gate's addresses, once it succeeded.
pub(crate) fn addresses() -> Option<&'static Addresses> {
    ADDRESSES.get()
}

/// The addresses, only while the detours are installed.
pub(crate) fn installed() -> Option<&'static Addresses> {
    INSTALLED.load(Ordering::Acquire).then(addresses).flatten()
}

pub(crate) fn params() -> Params {
    *PARAMS.read()
}

pub(crate) fn set_params(params: Params) {
    *PARAMS.write() = params.validated();
}

pub(crate) fn handler() -> Option<&'static dyn UpscaleHandler> {
    *HANDLER.read()
}

pub(crate) fn set_handler(handler: Option<&'static dyn UpscaleHandler>) {
    *HANDLER.write() = handler;
}

/// One counter.
#[derive(Debug)]
pub(crate) struct Counter(AtomicU64);

impl Counter {
    const fn new() -> Counter {
        Counter(AtomicU64::new(0))
    }

    /// Add one; the new value.
    pub(crate) fn bump(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub(crate) fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

macro_rules! counters {
    ($($name:ident),* $(,)?) => {
        /// The live counters behind [`Counters`].
        #[derive(Debug)]
        pub(crate) struct LiveCounters { $(pub(crate) $name: Counter,)* }

        pub(crate) static COUNTERS: LiveCounters = LiveCounters { $($name: Counter::new(),)* };

        impl LiveCounters {
            pub(crate) fn snapshot(&self) -> Counters {
                Counters { $($name: self.$name.get(),)* }
            }
        }
    };
}

counters!(
    camera_setups,
    frames,
    jitter_writes,
    cameras_published,
    cameras_unusable,
    optimal_answers,
    creates,
    eval_ss,
    eval_isp,
    unhandled,
    enqueues,
    enqueues_skipped,
    game_resets,
    depth_aliases,
    fatal_reports,
);
