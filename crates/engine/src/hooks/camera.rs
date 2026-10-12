//! The camera setup detour (`0x14228f3b0`, `ReJitterCamera.md`).
//!
//! Called for every active viewport of the pass being drawn. Per render
//! frame (`sRender+0x1f208`):
//!
//! * on the first call: the tuning maintenance ([`crate::tune`]);
//! * before the original runs for the **main** viewport (active, pass 0,
//!   no own target; the first such): the frame's jitter is computed and
//!   written to `sCamera+0x38/+0x3c`, where the original reads it
//!   (`0x14228faaf`) to build `J`; later viewports and the engine's second
//!   reader see the same value;
//! * after it: the viewport's unjittered matrices become a
//!   [`CameraFrame`](mhw_core::frame::CameraFrame), published with the
//!   frame's [`JitterSample`] as `bus::CameraReady`.

use std::sync::atomic::AtomicUsize;

use mhw_core::bus::{self, CameraReady};
use mhw_core::frame::{JitterSample, Size};
use mhw_core::math::engine_jitter_px;
use mhw_core::{log_changed, log_every, log_once};

use super::{guard, original};
use crate::runtime::{self, COUNTERS, LAST_FRAME, LAST_JITTER};
use crate::viewport::{jitter_for, render_size, OncePerFrame};
use crate::views::{self, Camera, RenderConfig};

/// The camera setup's trampoline.
pub(crate) static T_CAMERA_SETUP: AtomicUsize = AtomicUsize::new(0);

type CameraSetupFn = unsafe extern "system" fn(usize, usize) -> usize;

/// The frame whose maintenance ran, and the frame whose main viewport was
/// jittered.
static MAINTAINED: OncePerFrame = OncePerFrame::new();
static MAIN: OncePerFrame = OncePerFrame::new();

/// What `before` decided for the main viewport.
struct Main {
    frame: u64,
    /// The sample written, or `None` when nothing was written.
    written: Option<JitterSample>,
    render: Size,
    display: Size,
}

/// `bool sCamera::setupViewport(Viewport* viewport, pass)`.
pub(crate) unsafe extern "system" fn camera_setup(viewport: usize, pass: usize) -> usize {
    COUNTERS.camera_setups.bump();
    let main = guard("camera setup", || before(viewport), || None);
    // SAFETY: the trampoline of this very detour, same signature.
    let result = unsafe { original::<CameraSetupFn>(&T_CAMERA_SETUP)(viewport, pass) };
    if let Some(main) = main {
        guard("camera setup", || after(viewport, main), || ());
    }
    result
}

fn before(viewport: usize) -> Option<Main> {
    let at = runtime::installed()?;
    let config = RenderConfig::get(at)?;
    let frame = u64::from(config.render_frame()?);
    let params = runtime::params();
    if MAINTAINED.claim(frame) {
        COUNTERS.frames.bump();
        crate::tune::maintain_frame(at, &params, &config);
    }
    if !views::viewport_at(viewport).is_some_and(|v| v.is_main()) || !MAIN.claim(frame) {
        return None;
    }
    let display = config.screen_size().unwrap_or_default();
    let render = render_size(display, config.built_ratio().unwrap_or(0.0));
    let mut main = Main {
        frame,
        written: None,
        render,
        display,
    };
    if params.jittering() && !render.is_empty() {
        let sample = jitter_for(frame, render, display, params.jitter, params.jitter_phases);
        if Camera::get(at).is_some_and(|camera| camera.set_jitter(sample.units)) {
            COUNTERS.jitter_writes.bump();
            main.written = Some(sample);
        } else {
            log_once!(
                Warn,
                "jitter",
                "unwritable",
                "sCamera's jitter is not writable"
            );
        }
    }
    Some(main)
}

fn after(viewport: usize, main: Main) {
    let Some(snapshot) = views::viewport_at(viewport) else {
        return;
    };
    let at = runtime::installed();
    let camera = at.and_then(Camera::get);
    // What the engine built J from: ours, or (not jittering) whatever it had.
    let sample = main.written.unwrap_or_else(|| {
        let units = camera.and_then(|c| c.jitter()).unwrap_or_default();
        JitterSample {
            px: engine_jitter_px(units, main.render, main.display),
            units,
            phase: 0,
            frame: main.frame,
        }
    });
    // Known for this frame even when its camera turns out unusable below:
    // an evaluation must get exactly this frame's jitter, never an older
    // phase.
    *LAST_JITTER.lock() = Some(sample);
    // The jitter divisor is this rect; the RE says it is the display.
    log_changed!(
        Info,
        "jitter",
        "main viewport",
        (snapshot.size(), main.display),
        "main viewport rect {:?} (kind {:#x}), display {}x{}, render {}x{}",
        snapshot.rect,
        snapshot.kind,
        main.display.w,
        main.display.h,
        main.render.w,
        main.render.h
    );
    if snapshot.size() != main.display {
        log_once!(
            Warn,
            "jitter",
            "rect is not the display",
            "the main viewport's rect {:?} is not the display size {}x{}: the jitter divisor \
             differs from the one assumed",
            snapshot.rect,
            main.display.w,
            main.display.h
        );
    }
    if camera
        .and_then(|c| c.blend())
        .is_some_and(|t| t.is_finite() && t > 0.0)
    {
        log_once!(
            Warn,
            "jitter",
            "camera blend",
            "sCamera+0x44 (blend) is set: the engine lerps the camera with the previous \
             frame's, the published matrices are the unlerped ones"
        );
    }
    let Some(frame) = snapshot.camera_frame(main.frame) else {
        COUNTERS.cameras_unusable.bump();
        log_every!(
            600,
            Debug,
            "jitter",
            "the main viewport's matrices are not a usable camera (frame {})",
            main.frame
        );
        return;
    };
    *LAST_FRAME.lock() = Some((frame, sample));
    COUNTERS.cameras_published.bump();
    bus::publish(&CameraReady(frame, sample));
}
