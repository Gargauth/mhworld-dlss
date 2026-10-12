//! Read-only introspection: plain data for the dev server's `state` and
//! for periodic log lines. Nothing here changes behaviour.

use mhw_core::frame::Size;

use crate::options::{DlssgRequest, Options, ReflexRequest};
use crate::tags::TagRecord;
use crate::token::Timeline;

/// How far Streamline's initialisation got.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum InitState {
    /// `start` was not called (frame generation disabled).
    #[default]
    NotStarted,
    /// `slInit` is running on its thread.
    Running,
    /// `slInit` returned `eOk`.
    Ready,
    /// It will not be used this session; the text says why.
    Failed(String),
    /// Shut down (stand-down before a device was given, or process exit).
    ShutDown,
}

/// The engine's swapchain as created and last resized.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChainInfo {
    /// Address of the swapchain the engine holds.
    pub swapchain: usize,
    /// Its window.
    pub window: isize,
    /// Back-buffer size.
    pub size: Size,
    /// Back-buffer `DXGI_FORMAT` (B8G8R8A8 SDR, R10G10B10A2 HDR).
    pub format: u32,
    /// Buffer count.
    pub buffers: u32,
    /// `DXGI_SWAP_CHAIN_FLAG` bits.
    pub flags: u32,
    /// Whether it is Streamline's proxy (frame generation owns it).
    pub proxied: bool,
}

/// The last successful `slDLSSGGetState` (sampled on the present thread).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DlssgSample {
    /// `DLSSGStatus` bits ([`crate::abi::dlssg_status_names`]).
    pub status: u32,
    /// Frames presented since the previous sample.
    pub presented: u32,
    /// Most frames the system can generate per rendered frame.
    pub max_generated: u32,
    /// Smallest supported back-buffer side.
    pub min_dimension: u32,
    /// VSync support hint.
    pub vsync_available: bool,
    /// Present count when sampled.
    pub at_present: u64,
}

/// What the last accepted constants said, in short.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ConstantsSummary {
    /// Engine render frame they came from.
    pub frame: u64,
    /// Token index they were filed under.
    pub index: u32,
    /// `jitterOffset`.
    pub jitter: [f32; 2],
    /// `mvecScale`.
    pub mvec_scale: [f32; 2],
    /// `cameraNear`, `cameraFar`, `cameraFOV`, `cameraAspectRatio`.
    pub lens: [f32; 4],
    /// `reset`.
    pub reset: bool,
}

/// Counters of the swapchain path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChainCounters {
    /// Swapchains created through the hook.
    pub created: u64,
    /// `Present` calls on the engine's chain.
    pub presents: u64,
    /// `ResizeBuffers` calls on it.
    pub resizes: u64,
    /// Frames submitted at the upscale slot.
    pub frames: u64,
    /// Frames without depth or motion vectors.
    pub frames_without_inputs: u64,
    /// HUD-less images tagged at Present.
    pub hudless_tagged: u64,
    /// Rendered frames presented without a HUD-less image of their own.
    pub hudless_missing: u64,
}

/// Everything at once.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    /// Initialisation.
    pub init: InitState,
    /// `slSetD3DDevice` accepted a device.
    pub device_set: bool,
    /// DLSS-G, Reflex and PCL functions are bound.
    pub features_bound: bool,
    /// The engine's swapchain.
    pub chain: Option<ChainInfo>,
    /// The options in force.
    pub options: Options,
    /// Last `slDLSSGSetOptions` accepted.
    pub dlssg_sent: Option<DlssgRequest>,
    /// Last `slReflexSetOptions` accepted.
    pub reflex_sent: Option<ReflexRequest>,
    /// Last `slDLSSGGetState`.
    pub dlssg_state: Option<DlssgSample>,
    /// The window was in the foreground at the last Present.
    pub focused: bool,
    /// Last constants.
    pub constants: Option<ConstantsSummary>,
    /// Last tag per [`TagKind`](crate::TagKind) slot.
    pub tags: [Option<TagRecord>; 4],
    /// Token and marker counters.
    pub timeline: Timeline,
    /// Swapchain and frame counters.
    pub counters: ChainCounters,
}
