//! Per-frame state shared by the hooks: the engine's cameras, the reset
//! decision, the last evaluation and the supersampling call's inputs.

use mhw_core::bus::CameraReady;
use mhw_core::frame::{CameraFrame, FrameInfo, JitterSample, ResetReason, Size};
use mhw_ngx::FeatureDesc;
use parking_lot::Mutex;

use crate::frames::{CameraRing, SsPairing};
use crate::reset::{reset_reason, CutDetector, ResetCause};

/// What the supersampling call (EvalSS) handed over this frame.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SsInputs {
    /// Engine render frame at the call (`sRender+0x1f208`).
    pub frame: u64,
    /// The frame whose camera the call's image was rendered with: from the
    /// camera buffer of the `CSTemporalAA2` pass recorded before it on the
    /// same list, else the frame after the last one (`slot::ss_rendered_frame`).
    pub rendered: Option<u64>,
    /// Dense velocity (`taa+0x1a8`, `R32G32_FLOAT`).
    pub motion: usize,
    /// R32F depth copy.
    pub depth: usize,
}

#[derive(Debug, Default)]
struct Live {
    cameras: CameraRing,
    cuts: CutDetector,
    display: Size,
    last_desc: Option<FeatureDesc>,
    last_frame: Option<FrameInfo>,
    /// Supersampling calls for the upscale points.
    pairing: SsPairing<SsInputs>,
}

static LIVE: Mutex<Live> = Mutex::new(Live {
    cameras: CameraRing::new(),
    cuts: CutDetector::new(),
    display: Size::new(0, 0),
    last_desc: None,
    last_frame: None,
    pairing: SsPairing::new(),
});

/// `CameraReady` subscriber (engine render thread, once per frame).
pub(crate) fn on_camera(event: &CameraReady) {
    let CameraReady(camera, jitter) = *event;
    LIVE.lock().cameras.push(camera, jitter);
    note_display_from_engine();
    #[cfg(feature = "dev")]
    mhw_dev::on_camera(&camera, &jitter);
}

/// The engine's display size, handed to render when it changes (the
/// HUD-less detector and the temporal-pass size floor need it).
fn note_display_from_engine() {
    let size = mhw_engine::addresses()
        .and_then(mhw_engine::views::RenderConfig::get)
        .and_then(|config| config.screen_size());
    if let Some(size) = size {
        note_display(size);
    }
}

/// Record the display size (and tell render when it changed).
pub(crate) fn note_display(size: Size) {
    if size.is_empty() {
        return;
    }
    let changed = {
        let mut live = LIVE.lock();
        std::mem::replace(&mut live.display, size) != size
    };
    if changed {
        mhw_render::note_display(size);
        log::info!(target: "render", "display {}x{}", size.w, size.h);
    }
}

/// The display size last seen.
pub(crate) fn display() -> Size {
    LIVE.lock().display
}

/// The camera of exactly `frame` (none when that frame's main viewport had
/// no usable camera: another frame's would mislead cut detection and frame
/// generation).
pub(crate) fn camera(frame: u64) -> Option<CameraFrame> {
    let found = LIVE.lock().cameras.get(frame).map(|(camera, _)| camera);
    if found.is_none() {
        mhw_core::log_every!(600, Debug, "jitter", "no camera for render frame {frame}");
    }
    found
}

/// The jitter of exactly `frame`: its camera's, or the engine's sample of
/// that frame when the camera was not usable.
pub(crate) fn jitter(frame: u64) -> Option<JitterSample> {
    let engine = mhw_engine::last_jitter();
    let found = LIVE.lock().cameras.jitter_of(frame, engine);
    if found.is_none() {
        mhw_core::log_every!(
            600,
            Debug,
            "jitter",
            "no jitter known for render frame {frame}; DLSS is told none"
        );
    }
    found
}

/// The frame whose camera put `proj` (`fProj[8]`, `fProj[9]`) into a
/// temporal pass's camera buffer ([`CameraRing::frame_with_projection`]).
pub(crate) fn frame_of_projection(proj: [f32; 2]) -> Option<u64> {
    let live = LIVE.lock();
    live.cameras.frame_with_projection(proj, live.display)
}

/// The newest camera's frame (the frame being recorded).
pub(crate) fn current_frame() -> Option<u64> {
    LIVE.lock().cameras.latest().map(|(c, _)| c.frame)
}

/// Decide the frame's reset and remember the evaluation.
pub(crate) fn begin_evaluation(
    policy: mhw_core::config::ResetPolicy,
    frame: u64,
    game_reset: bool,
    camera: Option<&CameraFrame>,
    desc: &FeatureDesc,
) -> FrameInfo {
    let mut live = LIVE.lock();
    let cut = camera.is_some_and(|c| live.cuts.observe(c));
    let (resized, settings) = match live.last_desc {
        Some(last) => {
            let sizes = (last.render, last.display) != (desc.render, desc.display);
            let other = FeatureDesc {
                render: desc.render,
                display: desc.display,
                quality: desc.quality,
                ..last
            } != *desc;
            (sizes, other)
        }
        None => (false, false),
    };
    let reset = reset_reason(
        policy,
        ResetCause {
            game: game_reset,
            cut,
            resized,
            settings,
        },
    );
    if reset == ResetReason::CameraCut {
        log::debug!(target: "dlss", "camera cut at frame {frame}: history reset");
    }
    live.last_desc = Some(*desc);
    let info = FrameInfo {
        frame,
        render: desc.render,
        display: desc.display,
        scale: if desc.display.w == 0 {
            1.0
        } else {
            desc.render.w as f32 / desc.display.w as f32
        },
        reset,
    };
    live.last_frame = Some(info);
    info
}

/// The last evaluated frame.
pub(crate) fn last_frame() -> Option<FrameInfo> {
    LIVE.lock().last_frame
}

/// Keep the supersampling call's inputs for the upscale slot.
pub(crate) fn set_ss(inputs: SsInputs) {
    let mut live = LIVE.lock();
    live.pairing.call(inputs.frame, inputs.rendered, inputs);
}

/// The supersampling call for the upscale point at engine count
/// `numbered`, and the frame it evaluates ([`SsPairing`]).
pub(crate) fn upscale_ss(numbered: u64) -> Option<(SsInputs, Option<u64>)> {
    LIVE.lock().pairing.upscale(numbered)
}

/// The latest supersampling call (dev captures: its textures are the
/// engine's from frame to frame).
#[cfg(feature = "dev")]
pub(crate) fn latest_ss() -> Option<SsInputs> {
    LIVE.lock().pairing.latest()
}
