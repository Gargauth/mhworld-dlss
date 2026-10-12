//! The per-frame glue: four sites, each with its thread.
//!
//! ```text
//!   main thread, iteration start   frame_start      Reflex sleep, SIMULATION_START
//!                                                   (when SleepPoint::IterationStart)
//!   render thread, upscale slot    submit_frame     constants, depth + mvec tags
//!   main thread, first graphics    render_submit    SIMULATION_END, RENDERSUBMIT_START
//!     queue submit
//!   after the GUI is drawn         tag_late         HUD-less / UI alpha (filed at Present)
//!   main thread, Present (hook)    before/after     options, late tags, RENDERSUBMIT_END,
//!                                                   PRESENT_START … PRESENT_END, next
//!                                                   token, state sample, sleep
//! ```
//!
//! The render thread signals `done` before the main thread submits and
//! presents (docs/re/ReFramePresent.md §2.3), so a frame's constants are
//! always filed before its `PRESENT_START`. DLSS-G options are sent from
//! the present thread, which the guide asks for, and only on change.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use mhw_core::frame::{CameraFrame, JitterSample, Size};
use mhw_core::math::Vec2;
use parking_lot::Mutex;

use crate::abi;
use crate::constants::{self, MotionVectors};
use crate::options::{
    dlssg_request, reflex_request, Conditions, DlssgRequest, Hold, Options, ReflexRequest, Sent,
    RESIZE_HOLD_PRESENTS,
};
use crate::pacing::SleepPoint;
use crate::sl;
use crate::status::{ChainCounters, ConstantsSummary, DlssgSample, Status};
use crate::tags::{resource_tag, TagKind, TagRecord, TagResource};
use crate::token::FrameLoop;

mod present;

pub(crate) use present::{after_present, before_present, before_resize};

const FG: &str = "fg";
const REFLEX: &str = "reflex";

/// How often `slDLSSGGetState` is sampled.
const SAMPLE_EVERY: Duration = Duration::from_secs(1);

/// What the upscale slot hands over for one rendered frame.
#[derive(Clone, Copy, Debug)]
pub struct FrameInputs {
    /// The engine's main camera, unjittered.
    pub camera: CameraFrame,
    /// The jitter of the same frame.
    pub jitter: JitterSample,
    /// History is discarded (game reset, camera cut, settings, resize).
    pub reset: bool,
    /// Depth at render size, in the state it will be in at Present.
    pub depth: Option<TagResource>,
    /// Decoded motion vectors at render size, likewise.
    pub motion_vectors: Option<TagResource>,
    /// Multiplying a motion-vector texel by this gives render pixels,
    /// NVIDIA axes (the scale DLSS-SR is given for the same texture).
    pub mvec_pixel_scale: Vec2,
}

/// What [`submit_frame`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Submitted {
    /// Frame generation does not own the swapchain, or its functions are
    /// not bound: nothing to do.
    Inactive,
    /// Depth or motion vectors missing or unusable: their tags were removed
    /// and generation is off for this frame.
    NoInputs,
    /// Constants and tags filed under this token index.
    Filed {
        /// The token index.
        index: u32,
    },
    /// This index already has constants (a second slot call before
    /// Present); nothing was filed.
    Duplicate {
        /// The token index.
        index: u32,
    },
    /// The constants could not be built or Streamline refused them
    /// (logged once per reason).
    Rejected,
}

/// Inputs captured after the upscale slot, filed at Present.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LateKind {
    /// `kBufferTypeHUDLessColor`: the frame before the interface, already
    /// in the back buffer's format and colour space.
    HudLess,
    /// `kBufferTypeUIAlpha`.
    UiAlpha,
}

impl LateKind {
    const fn tag(self) -> TagKind {
        match self {
            LateKind::HudLess => TagKind::HudLess,
            LateKind::UiAlpha => TagKind::UiAlpha,
        }
    }

    const fn slot(self) -> usize {
        match self {
            LateKind::HudLess => 0,
            LateKind::UiAlpha => 1,
        }
    }

    const ALL: [LateKind; 2] = [LateKind::HudLess, LateKind::UiAlpha];
}

#[derive(Clone, Copy)]
struct Late {
    resource: usize,
    state: u32,
    size: Size,
    format: u32,
    frame: u64,
}

impl Late {
    fn tag(&self) -> TagResource {
        TagResource {
            resource: self.resource as *mut c_void,
            state: self.state,
            size: self.size,
            format: self.format,
        }
    }
}

/// The rendered frame whose constants are under the current token.
#[derive(Clone, Copy)]
struct Filed {
    frame: u64,
    inputs: bool,
}

#[cfg(feature = "dev")]
#[derive(Clone, Copy)]
struct Dump(abi::Constants);

// SAFETY: `base.next` is always null; the structure is plain data and no
// pointer in it is ever dereferenced.
#[cfg(feature = "dev")]
unsafe impl Send for Dump {}

struct Shared {
    machine: FrameLoop,
    token: usize,
    options: Options,
    dlssg: Sent<DlssgRequest>,
    reflex: Sent<ReflexRequest>,
    hold: Hold,
    filed: Option<Filed>,
    render: Size,
    late: [Option<Late>; 2],
    late_seen: [bool; 2],
    reset_pending: bool,
    sample: Option<DlssgSample>,
    sampled_at: Option<Instant>,
    focused: bool,
    constants: Option<ConstantsSummary>,
    tags: [Option<TagRecord>; 4],
    counters: ChainCounters,
    #[cfg(feature = "dev")]
    dump: Option<Dump>,
}

static SHARED: LazyLock<Mutex<Shared>> = LazyLock::new(|| {
    Mutex::new(Shared {
        machine: FrameLoop::new(),
        token: 0,
        options: Options::default(),
        dlssg: Sent::new(),
        reflex: Sent::new(),
        hold: Hold::default(),
        filed: None,
        render: Size::default(),
        late: [None; 2],
        late_seen: [false; 2],
        reset_pending: true,
        sample: None,
        sampled_at: None,
        focused: true,
        constants: None,
        tags: [None; 4],
        counters: ChainCounters::default(),
        #[cfg(feature = "dev")]
        dump: None,
    })
});

/// Frame generation owns the engine's swapchain (Streamline's proxy).
static OWNS: AtomicBool = AtomicBool::new(false);
/// It owns it and the feature functions are bound: per-frame work runs.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether frame generation owns the engine's swapchain: the engine
/// renders off-screen and Streamline presents. The mod's wait-latch policy
/// keys on this ([`wait_latch`](crate::wait_latch)).
pub fn owns_swapchain() -> bool {
    OWNS.load(Ordering::Acquire)
}

fn active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

/// Set the options; they take effect at the next Present. Any thread.
pub fn set_options(options: Options) {
    SHARED.lock().options = options;
}

/// The options in force.
pub fn options() -> Options {
    SHARED.lock().options
}

/// Tell Streamline the next frame has no history (sent with its constants).
pub fn note_reset() {
    SHARED.lock().reset_pending = true;
}

/// The swapchain was created; `proxied`: through Streamline.
pub(crate) fn activate(proxied: bool) {
    let mut s = SHARED.lock();
    s.reset_pending = true;
    s.dlssg.forget();
    s.reflex.forget();
    s.counters.created += 1;
    OWNS.store(proxied, Ordering::Release);
    ACTIVE.store(proxied && sl::features_bound(), Ordering::Release);
}

fn acquire(s: &mut Shared, index: u32) -> bool {
    match sl::new_token(index) {
        Ok(token) => {
            s.token = token;
            s.machine.acquired(index);
            s.filed = None;
            true
        }
        Err(error) => {
            mhw_core::log_once!(
                Warn,
                REFLEX,
                "token",
                "slGetNewFrameToken({index}) -> {}; the frame keeps its token",
                abi::result_name(error)
            );
            false
        }
    }
}

fn ensure_token(s: &mut Shared) {
    if let Some(index) = s.machine.first_token() {
        acquire(s, index);
    }
}

fn marker(marker: abi::Marker, token: usize) {
    let result = sl::marker(marker, token);
    if result != abi::OK {
        mhw_core::log_once!(
            Warn,
            REFLEX,
            marker.name(),
            "slPCLSetMarker({}) -> {}",
            marker.name(),
            abi::result_name(result)
        );
    }
}

/// The start of a main-loop iteration: Reflex sleep and
/// `SIMULATION_START`. Called right after Present with
/// [`SleepPoint::AfterPresent`]; the mod calls it from the engine's
/// iteration start (where the wait helper is called) with
/// [`SleepPoint::IterationStart`]. Once per present cycle either way.
pub fn frame_start() {
    if !active() {
        return;
    }
    let (plan, token) = {
        let mut s = SHARED.lock();
        ensure_token(&mut s);
        (s.machine.frame_start(), s.token)
    };
    if plan.sleep {
        let result = sl::reflex_sleep(token);
        if result != abi::OK {
            mhw_core::log_once!(
                Warn,
                REFLEX,
                "sleep",
                "slReflexSleep -> {}",
                abi::result_name(result)
            );
        }
    }
    if plan.simulation_start {
        marker(abi::Marker::SimulationStart, token);
    }
}

/// The engine submitted to its graphics queue (mod calls this from render's
/// `ExecuteCommandLists` observation, engine's queue only). The first call
/// of a frame sets `SIMULATION_END` and `RENDERSUBMIT_START`; see
/// [`FrameLoop::render_submit`] for the mode-change exception.
pub fn render_submit() {
    if !active() {
        return;
    }
    let (plan, token) = {
        let mut s = SHARED.lock();
        ensure_token(&mut s);
        (s.machine.render_submit(), s.token)
    };
    if plan.simulation_end {
        marker(abi::Marker::SimulationEnd, token);
    }
    if plan.render_submit_start {
        marker(abi::Marker::RenderSubmitStart, token);
    }
}

/// File `items` under the current token. At most four; `None` removes.
///
/// # Safety
/// Every resource must be live and in its stated state until Present;
/// `list` null or an open command list.
unsafe fn file_tags(
    s: &mut Shared,
    items: &[(TagKind, Option<TagResource>)],
    list: *mut c_void,
) -> bool {
    let Some(index) = s.machine.tags() else {
        return false;
    };
    let count = items.len().min(4);
    let mut resources: [Option<abi::Resource>; 4] = [None; 4];
    for (slot, (_, resource)) in resources.iter_mut().zip(items.iter().take(count)) {
        *slot = resource.map(|r| r.to_abi());
    }
    let removal = resource_tag(TagKind::Depth, std::ptr::null_mut(), Size::default());
    let mut tags = [removal; 4];
    for ((tag, resource), (kind, item)) in tags
        .iter_mut()
        .zip(resources.iter_mut())
        .zip(items.iter().take(count))
    {
        let pointer = resource
            .as_mut()
            .map_or(std::ptr::null_mut(), |r| r as *mut abi::Resource);
        *tag = resource_tag(*kind, pointer, item.map_or(Size::default(), |r| r.size));
    }
    let tags = tags.get(..count).unwrap_or(&[]);
    // SAFETY: `resources` outlives the call and the caller vouches for the
    // resources and the list.
    let result = unsafe { sl::set_tags(s.token, tags, list) };
    let accepted = result == abi::OK;
    for (kind, item) in items.iter().take(count) {
        s.tags[kind.slot()] = Some(TagRecord {
            resource: item.map_or(0, |r| r.resource as usize),
            state: item.map_or(0, |r| r.state),
            size: item.map_or(Size::default(), |r| r.size),
            format: item.map_or(0, |r| r.format),
            index,
            accepted,
        });
    }
    if !accepted {
        mhw_core::log_once!(
            Warn,
            FG,
            "tags",
            "slSetTagForFrame({} tags) -> {}",
            count,
            abi::result_name(result)
        );
    }
    accepted
}

/// Hand over one rendered frame at the engine's upscale slot (render
/// thread, after DLSS-SR evaluated): its constants and its depth and motion
/// vectors, under the current token. `list` is the engine's open command
/// list (Streamline may record into it).
///
/// # Safety
/// The tagged resources must stay alive and unmodified, in their stated
/// states, until the frame is presented; `list` must be null or an open
/// `ID3D12GraphicsCommandList`.
pub unsafe fn submit_frame(inputs: &FrameInputs, list: *mut c_void) -> Submitted {
    if !active() {
        return Submitted::Inactive;
    }
    let mut s = SHARED.lock();
    ensure_token(&mut s);
    s.counters.frames += 1;
    let usable = |tag: Option<TagResource>| tag.filter(|t| t.validate().is_ok());
    let (Some(depth), Some(motion)) = (usable(inputs.depth), usable(inputs.motion_vectors)) else {
        s.counters.frames_without_inputs += 1;
        mhw_core::log_changed!(
            Info,
            FG,
            "inputs",
            false,
            "no depth or motion vectors: their tags are removed and generation is off until there are"
        );
        // SAFETY: removals only (null resources).
        unsafe {
            file_tags(
                &mut s,
                &[(TagKind::Depth, None), (TagKind::MotionVectors, None)],
                list,
            );
        }
        s.filed = Some(Filed {
            frame: inputs.camera.frame,
            inputs: false,
        });
        return Submitted::NoInputs;
    };
    mhw_core::log_changed!(
        Info,
        FG,
        "inputs",
        true,
        "depth and motion vectors are tagged"
    );
    let index = match s.machine.constants() {
        Ok(index) => index,
        Err(crate::token::Refused::Duplicate(index)) => return Submitted::Duplicate { index },
        Err(crate::token::Refused::NoToken) => return Submitted::Rejected,
    };
    let reset = inputs.reset || s.reset_pending;
    let motion_space = MotionVectors {
        pixel_scale: inputs.mvec_pixel_scale,
        size: motion.size,
    };
    let built = match constants::build(&inputs.camera, &inputs.jitter, motion_space, reset) {
        Ok(built) => built,
        Err(error) => {
            mhw_core::log_once!(Warn, FG, error, "no constants for this frame: {error}");
            return Submitted::Rejected;
        }
    };
    let result = sl::set_constants(&built, s.token);
    if result != abi::OK {
        mhw_core::log_once!(
            Warn,
            FG,
            result,
            "slSetConstants -> {}",
            abi::result_name(result)
        );
        return Submitted::Rejected;
    }
    s.machine.constants_filed(index);
    s.reset_pending = false;
    s.render = depth.size;
    s.filed = Some(Filed {
        frame: inputs.camera.frame,
        inputs: true,
    });
    s.constants = Some(ConstantsSummary {
        frame: inputs.camera.frame,
        index,
        jitter: built.jitter_offset,
        mvec_scale: built.mvec_scale,
        lens: [
            built.camera_near,
            built.camera_far,
            built.camera_fov,
            built.camera_aspect_ratio,
        ],
        reset,
    });
    #[cfg(feature = "dev")]
    {
        s.dump = Some(Dump(built));
    }
    mhw_core::log_once!(
        Info,
        FG,
        "constants",
        "first constants: jitter ({:.4}, {:.4}) px, mvec scale ({:.6}, {:.6}), near {}, far {}, \
         fov {:.4}, aspect {:.4}",
        built.jitter_offset[0],
        built.jitter_offset[1],
        built.mvec_scale[0],
        built.mvec_scale[1],
        built.camera_near,
        built.camera_far,
        built.camera_fov,
        built.camera_aspect_ratio
    );
    // SAFETY: the caller vouches for the resources and the list.
    let tagged = unsafe {
        file_tags(
            &mut s,
            &[
                (TagKind::Depth, Some(depth)),
                (TagKind::MotionVectors, Some(motion)),
            ],
            list,
        )
    };
    if tagged {
        mhw_core::log_once!(
            Info,
            FG,
            "tagged",
            "first tags: depth {}x{} format {} state {:#x}, mvec {}x{} format {} state {:#x}",
            depth.size.w,
            depth.size.h,
            depth.format,
            depth.state,
            motion.size.w,
            motion.size.h,
            motion.format,
            motion.state
        );
    }
    Submitted::Filed { index }
}

/// Hand over an input captured after the upscale slot (`resource: None`
/// withdraws it), for rendered frame `frame` (the camera's frame). It is
/// filed at this frame's Present, and only if this frame's constants were
/// filed: an image from another frame is never tagged. `false` when frame
/// generation is not active.
///
/// # Safety
/// The resource must stay alive and unmodified, in its stated state, until
/// the frame is presented.
pub unsafe fn tag_late(kind: LateKind, resource: Option<TagResource>, frame: u64) -> bool {
    if !active() {
        return false;
    }
    let mut s = SHARED.lock();
    let slot = kind.slot();
    s.late[slot] = resource.filter(|r| r.validate().is_ok()).map(|r| Late {
        resource: r.resource as usize,
        state: r.state,
        size: r.size,
        format: r.format,
        frame,
    });
    s.late_seen[slot] |= s.late[slot].is_some();
    true
}

/// Everything at once, for the dev server and periodic log lines.
pub fn status() -> Status {
    let chain = crate::swap::chain();
    let s = SHARED.lock();
    Status {
        init: sl::init_state(),
        device_set: sl::device_set(),
        features_bound: sl::features_bound(),
        chain,
        options: s.options,
        dlssg_sent: s.dlssg.last(),
        reflex_sent: s.reflex.last(),
        dlssg_state: s.sample,
        focused: s.focused,
        constants: s.constants,
        tags: s.tags,
        timeline: s.machine.timeline(),
        counters: s.counters,
    }
}

/// The complete last constants Streamline accepted (dev builds).
#[cfg(feature = "dev")]
pub fn last_constants() -> Option<abi::Constants> {
    SHARED.lock().dump.map(|dump| dump.0)
}
