//! Frame generation's per-frame inputs (`crates/fg/README.md`, *How the mod
//! wires it*): the frame at the upscale slot, the HUD-less image before
//! Present, the submit marker, and the engine's wait latch.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use mhw_core::frame::{CameraFrame, JitterSample};
use mhw_core::math::Vec2;
use mhw_fg::{FrameInputs, LateKind, TagResource};
use mhw_render::hudless::{self, OutputTransform};
use mhw_render::Submission;

use super::evaluate::size_of;
use super::runtime;
use crate::policy::latch_write;

/// What the own HDR encode maps scene 1.0 to (only used when the engine's
/// encode shader is not available; INFERENCE, `ReFramePresent` E7).
const HDR_WHITE_NITS: f32 = 100.0;

/// The render frame last handed to frame generation (the one Present
/// shows).
static SUBMITTED: AtomicU64 = AtomicU64::new(0);
/// This mod set the engine's wait latch.
static LATCHED: AtomicBool = AtomicBool::new(false);
/// The last HUD-less image tagged, with its frame (dev captures).
#[cfg(feature = "dev")]
static LAST_HUDLESS: parking_lot::Mutex<Option<(u64, hudless::HudlessImage)>> =
    parking_lot::Mutex::new(None);

/// The last HUD-less image tagged and its frame: at the next slot it is the
/// previous frame's, still unmodified (render rotates three).
#[cfg(feature = "dev")]
pub(crate) fn last_hudless() -> Option<(u64, hudless::HudlessImage)> {
    *LAST_HUDLESS.lock()
}

/// One rendered frame for frame generation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Submit {
    /// The engine's open list.
    pub list: *mut c_void,
    /// The frame's camera.
    pub camera: CameraFrame,
    /// Its jitter.
    pub jitter: JitterSample,
    /// History discarded.
    pub reset: bool,
    /// Depth DLSS read.
    pub depth: *mut c_void,
    /// Motion vectors DLSS read.
    pub motion: *mut c_void,
    /// Their state if fixed (the decoded vectors rest in
    /// `NON_PIXEL_SHADER_RESOURCE`); else read from the list.
    pub motion_state: Option<u32>,
    /// `MV.Scale` DLSS was given.
    pub mv_scale: Vec2,
}

/// A resource for a tag, in its state on `list` now.
fn tag(list: *mut c_void, resource: *mut c_void, state: Option<u32>) -> Option<TagResource> {
    if resource.is_null() {
        return None;
    }
    // SAFETY: the engine's open list on this thread.
    let state = state.or_else(|| unsafe { mhw_render::resource_state(list, resource as usize) })?;
    let info = mhw_render::describe(resource as usize)?;
    Some(TagResource {
        resource,
        state,
        size: size_of(resource as usize).unwrap_or_default(),
        format: info.format,
    })
}

/// The frame at the upscale slot, after DLSS.
pub(crate) fn submit(s: &Submit) {
    if !runtime::fg_started() {
        return;
    }
    let inputs = FrameInputs {
        camera: s.camera,
        jitter: s.jitter,
        reset: s.reset,
        depth: tag(s.list, s.depth, None),
        motion_vectors: tag(s.list, s.motion, s.motion_state),
        mvec_pixel_scale: s.mv_scale,
    };
    // SAFETY: the engine's open list; the tagged resources are the
    // engine's for this frame (and referenced by the evaluation's hold).
    let submitted = unsafe { mhw_fg::submit_frame(&inputs, s.list) };
    SUBMITTED.store(s.camera.frame, Ordering::Release);
    mhw_core::log_changed!(
        Debug,
        "fg",
        "submitted",
        std::mem::discriminant(&submitted),
        "frame inputs: {submitted:?}"
    );
    #[cfg(feature = "dev")]
    mhw_dev::trace_event("tags", s.camera.frame);
}

/// The upscale slot of `frame`: the HUD-less frame starts, with the
/// engine's output encode of this frame.
pub(crate) fn at_slot(frame: u64) {
    if !(runtime::fg_started() && runtime::policy().hudless) {
        return;
    }
    let config = mhw_engine::addresses().and_then(mhw_engine::views::RenderConfig::get);
    let format = mhw_fg::chain().map(|chain| chain.format);
    if let (Some(config), Some(format)) = (config, format) {
        if let (Some(gamma), Some(gamma_ex), Some(pq)) = (
            config.output_gamma(),
            config.output_gamma_ex(),
            config.output_pq(),
        ) {
            hudless::set_output_transform(OutputTransform {
                gamma,
                gamma_ex,
                pq,
                format,
                hdr_white_nits: HDR_WHITE_NITS,
            });
        }
    }
    hudless::begin_frame(frame);
}

/// Before the engine's Present: the frame's HUD-less image, and the wait
/// latch. Returns the last frame submitted to frame generation (0 without
/// it); the render frame being presented is the engine's counter.
pub(crate) fn before_present() -> u64 {
    let frame = SUBMITTED.load(Ordering::Acquire);
    let policy = runtime::policy();
    let wanted = mhw_fg::wait_latch(policy.wait_latch);
    if let Some(on) = latch_write(wanted, policy.wait_latch, LATCHED.load(Ordering::Acquire)) {
        if mhw_engine::set_wait_latch(on) {
            LATCHED.store(on, Ordering::Release);
            log::info!(
                target: "fg",
                "engine wait latch {} ({:?})",
                if on { "set" } else { "cleared" },
                policy.wait_latch
            );
        }
    }
    if runtime::fg_started() && policy.hudless {
        if let Some(image) = hudless::take(frame) {
            let resource = TagResource {
                resource: image.resource,
                state: image.state,
                size: image.size,
                format: image.format,
            };
            // SAFETY: render keeps the converted image unmodified until
            // its frame is presented.
            let _tagged = unsafe { mhw_fg::tag_late(LateKind::HudLess, Some(resource), frame) };
            #[cfg(feature = "dev")]
            if _tagged {
                *LAST_HUDLESS.lock() = Some((frame, image));
                mhw_dev::trace_event("hudless", frame);
            }
        }
    }
    frame
}

/// `ExecuteCommandLists` observer: the engine's graphics-queue submission
/// ends the frame's simulation for Reflex/PCL.
pub(crate) fn on_submit(submission: &Submission) {
    if submission.engine && runtime::fg_started() {
        mhw_fg::render_submit();
        #[cfg(feature = "dev")]
        mhw_dev::trace_event("submit", SUBMITTED.load(Ordering::Acquire));
    }
}
