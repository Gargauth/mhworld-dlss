//! The per-frame glue between this engine and Streamline.
//!
//! Three sites, and no more, because three is all this library owns once the
//! plugin has taken over presentation:
//!
//! ```text
//!   the engine's first ExecuteCommandLists   SIMULATION_END, RENDERSUBMIT_START
//!   the engine's upscale slot                constants, tags, mode, state
//!   Streamline's own Present                 RENDERSUBMIT_END, PRESENT_START,
//!                                            PRESENT_END, then the next
//!                                            frame's token, sleep and
//!                                            SIMULATION_START
//! ```
//!
//! The frame boundary is the present, and the frame index only advances there.
//! That is the same discipline the NVAPI version used and it is still the right
//! one here: `FUN_1422980b0` can return early when the render thread is behind,
//! so a frame can begin and never present, and an index that advanced anywhere
//! else would drift from what `slSetConstants` was told. Streamline cares --
//! `ePresentStart` and `ePresentEnd` must carry the index the constants did.

use parking_lot::Mutex;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::streamline as sl;

/// Which of the two worlds this session is in, said once, and only once the
/// world is established.
///
/// Called from the one place that knows, the swapchain creation, and it
/// states what was observed there and not what the configuration asked for:
/// until the swapchain exists there is no world to announce, and a line
/// written from intent reads as a report of something that may not have
/// happened.
static ANNOUNCED: AtomicBool = AtomicBool::new(false);

pub fn announce_swapchain(streamlines: bool, factory: usize) {
    if ANNOUNCED.swap(true, Ordering::Relaxed) {
        return;
    }
    // Whose factory it is, read off the object and not assumed: anything
    // loaded beside this mod may hand the engine a factory of its own, as
    // ReShade does.
    let implemented_by = unsafe { crate::reshade::owner(factory as *mut std::ffi::c_void) };
    if streamlines {
        crate::log!(
            "[fg ] WORLD: the factory at {factory:#x} is Streamline's, so the chain it \
             just made is Streamline's -- the engine renders off-screen and the plugin \
             presents"
        );
    } else if !implemented_by.eq_ignore_ascii_case("dxgi.dll") {
        crate::log!(
            "[fg ] WORLD: the factory at {factory:#x} is implemented by {implemented_by} \
             and was not upgraded, so the chain is not Streamline's -- the engine \
             presents it itself and the upscaling path is untouched"
        );
    } else {
        crate::log!(
            "[fg ] WORLD: the factory at {factory:#x} is DXGI's, so the chain is DXGI's \
             -- the engine presents it itself, nothing is upgraded, and the upscaling \
             path is untouched"
        );
    }
}

// ---------------------------------------------------------------------------
// The camera
// ---------------------------------------------------------------------------

/// The engine's own matrices, as of the last camera buffer it uploaded.
///
/// `fNoJitterProj`, `fViewProj` and `fViewProjPF`, at 0x320, 0x000 and 0x1e0 of
/// `CBViewProjection` -- the layout `camera.rs` records.
///
/// Taken from the engine's buffer rather than reconstructed, and that matters
/// twice over. Streamline wants matrices with **no jitter baked in**, and this
/// engine is the unusual case where that is the honest thing to hand over
/// rather than something to subtract back out: `native.rs` writes the two
/// floats the engine's own camera setup reads, so the engine derives the
/// jittered projection itself and leaves `fNoJitterProj` clean. And the
/// previous-frame matrix is the engine's own, which it derives with the
/// *current* frame's jitter -- the thing that makes its motion vectors
/// jitter-free, and the thing any reconstruction from outside would get subtly
/// wrong.
static CAMERA: Mutex<Option<Matrices>> = Mutex::new(None);

#[derive(Clone, Copy)]
struct Matrices {
    /// `fNoJitterProj`, +0x320.
    projection: crate::camera::Matrix,
    /// `fViewProj`, +0x000.
    view_projection: crate::camera::Matrix,
    /// `fViewProjPF`, +0x1e0.
    previous_view_projection: crate::camera::Matrix,
    /// `fViewI`, +0x0c0: view space to world. `None` if it could not be read.
    view_inverse: Option<crate::camera::Matrix>,
    /// The engine's own scalar fields, for comparison with what the matrices
    /// say: `fCameraPos`, `fCameraDir`, `fCameraNearClip`, `fCameraFarClip`.
    engine: Option<([f32; 3], [f32; 3], f32, f32)>,
}

/// Offset of `fViewProjPF` within the camera buffer.
const F_VIEW_PROJ_PREVIOUS: usize = 0x1e0;

/// Record the camera as the engine uploaded it.
pub fn note_camera(bytes: &[u8]) {
    let projection = crate::camera::read(bytes, crate::camera::F_NO_JITTER_PROJ);
    let view_projection = crate::camera::read(bytes, crate::camera::F_VIEW_PROJ);
    let previous = crate::camera::read(bytes, F_VIEW_PROJ_PREVIOUS);
    if let (Some(projection), Some(view_projection), Some(previous_view_projection)) =
        (projection, view_projection, previous)
    {
        use crate::camera::{
            read_float, read_vector, F_CAMERA_DIR, F_CAMERA_FAR_CLIP, F_CAMERA_NEAR_CLIP,
            F_CAMERA_POS, F_INVERSE_VIEW,
        };
        let engine = (|| {
            Some((
                read_vector(bytes, F_CAMERA_POS)?,
                read_vector(bytes, F_CAMERA_DIR)?,
                read_float(bytes, F_CAMERA_NEAR_CLIP)?,
                read_float(bytes, F_CAMERA_FAR_CLIP)?,
            ))
        })();
        *CAMERA.lock() = Some(Matrices {
            projection,
            view_projection,
            previous_view_projection,
            view_inverse: crate::camera::read(bytes, F_INVERSE_VIEW),
            engine,
        });
    }
}

/// The four matrices Streamline asks for.
///
/// With row-vector matrices a clip position goes back through the inverse of
/// this frame's view-projection and forward through the previous frame's, so
/// `clipToPrevClip` is that product in that order.
/// `mvec` is the size of the buffer that is actually tagged and `decoded`
/// whether `mv::decode` produced it, because the scale depends on both and on
/// neither the display size nor the configuration alone.
fn camera_for_streamline(
    reset: bool,
    display: (u32, u32),
    mvec: (u32, u32),
    decoded: bool,
) -> Option<sl::Camera> {
    let m = (*CAMERA.lock())?;
    let inverse_projection = crate::camera::invert(&m.projection)?;
    let inverse_view_projection = crate::camera::invert(&m.view_projection)?;
    let clip_to_prev_clip =
        crate::camera::multiply(&inverse_view_projection, &m.previous_view_projection);
    let prev_clip_to_clip = crate::camera::invert(&clip_to_prev_clip)?;

    // In pixels, which is what Streamline wants, and the value this library
    // handed the engine rather than anything read back out of it.
    let jitter = crate::native::last_jitter();
    let settings = crate::config::get();
    // The scale for the buffer that is actually tagged. Three buffers are
    // easily confused here:
    //
    //   1. the engine's own `tGBuffer4`, R32_UINT (42) at render resolution,
    //      packed half-floats with horizontal in the high half;
    //   2. `mv::decode`'s output, R16G16_FLOAT (34) at render resolution,
    //      holding **pixels** -- `px = v * (-w/2, h/2)` -- with the jitter
    //      taken back out by `mv_dejitter`;
    //   3. what DLSS-G is given.
    //
    // It is (2) that is tagged: the tag line in the log reads
    // `mvec 2561x1440 ... format 34`, and 34 is R16G16_FLOAT where the
    // engine's own buffer is 42. Super-resolution is handed
    // `MV_Scale = (1, 1)` for the same texture, which is the same statement
    // from the other side: NGX's scale converts the buffer to pixels and
    // there is nothing left to convert.
    //
    // So the conversion is from pixels, not from the engine's raw units, and
    // it is a pure change of magnitude; the direction is already right, as
    // super-resolution on this texture shows. Streamline's scale takes the
    // buffer to a fraction of the screen -- its own samples pass
    // `{1/width, 1/height}` for pixel-space vectors -- so
    // `scale = (1/w, 1/h)`, positive on both axes. Applying the engine's
    // sign convention here would negate X a second time, the decode having
    // done it once.
    //
    // At 2561x1440 that is (0.00039, 0.00069). A scale of (-1, 1), the
    // arithmetic for the engine's raw buffer, is wrong by three orders of
    // magnitude, and the error shows as ghosting that grows with rotation
    // speed and sits at a distance from the object, where a jitter mismatch
    // would be a roughly constant sub-pixel error.
    //
    // Derived from the described size of the tagged resource, not from the
    // configuration and not from the display size, so the two scales have
    // one source and cannot drift apart.
    let (scale, basis) = match settings.fg_mvec_space {
        // The other reading of Streamline's documentation: that the target is
        // [-1,1] rather than [0,1], where a full-screen displacement is 2.
        // Worth one setting rather than an argument, because the two differ
        // by exactly the factor of two that distinguishes a fix from a
        // halving.
        1 => (
            (
                2.0 / (mvec.0 as f32).max(1.0),
                2.0 / (mvec.1 as f32).max(1.0),
            ),
            "derived x2, if Streamline means [-1,1] rather than a screen fraction",
        ),
        // The arithmetic for the engine's raw buffer applied to the decoded
        // one: wrong, and kept as a baseline to compare against by eye.
        2 => (
            (
                settings.mv_scale.0 / (display.0 as f32 / 2.0).max(1.0),
                settings.mv_scale.1 / (display.1 as f32 / 2.0).max(1.0),
            ),
            "the previous value, for comparison: the raw buffer's arithmetic",
        ),
        // Deliberately, unmistakably wrong: sixteen times the derived scale
        // cannot look subtle, so it shows in one look whether the scale is
        // read at all. A lever is tested at an extreme before it is tested
        // at plausible values.
        3 => (
            (
                16.0 / (mvec.0 as f32).max(1.0),
                16.0 / (mvec.1 as f32).max(1.0),
            ),
            "16x the derived value, to prove the scale is read at all",
        ),
        // Derived. When no decode ran the tagged buffer is the engine's own,
        // and then the raw arithmetic is the right one -- read from what
        // happened rather than from `mv_decode` being set, because the decode
        // can fail and fall back.
        _ => {
            if decoded {
                (
                    (
                        1.0 / (mvec.0 as f32).max(1.0),
                        1.0 / (mvec.1 as f32).max(1.0),
                    ),
                    "derived: the decode's pixels as a fraction of the screen",
                )
            } else {
                (
                    (
                        settings.mv_scale.0 / (mvec.0 as f32 / 2.0).max(1.0),
                        settings.mv_scale.1 / (mvec.1 as f32 / 2.0).max(1.0),
                    ),
                    "derived: the engine's own vectors, undecoded",
                )
            }
        }
    };
    if !SAID_SCALE.swap(true, Ordering::Relaxed) {
        crate::log!(
            "[fg ] mvec scale ({}, {}) for a {}x{} buffer -- {basis}",
            scale.0,
            scale.1,
            mvec.0,
            mvec.1
        );
    }
    // The lens and the pose, which Streamline's validation asks for by name
    // and which were left invalid for as long as this feature has existed.
    //
    // Both are **derived from the matrices that are handed over**, so the
    // scalars cannot describe a different camera from the one the matrices
    // do: near, far, field of view and aspect from `fNoJitterProj`, which is
    // the projection the depth buffer was rendered through, and position and
    // basis from `fViewI`. The engine's own `fCameraNearClip` and friends are
    // read as well, but only to be compared -- a disagreement is printed, and
    // it would mean one of the two offsets is not what it is taken to be.
    //
    // A value that cannot be derived stays invalid and the log says which.
    // Nothing is replaced by a plausible constant.
    let lens = crate::camera::lens(&m.projection);
    let pose = m
        .view_inverse
        .and_then(|inverse| crate::camera::pose(&inverse, m.projection[11] < 0.0));
    if !SAID_CAMERA.swap(true, Ordering::Relaxed) {
        if lens.is_none() {
            crate::log!(
                "[fg ] no near, far, field of view or aspect could be read out of \
                 fNoJitterProj (P22 {}, P23 {}, P32 {}), so those four constants stay invalid",
                m.projection[10],
                m.projection[11],
                m.projection[14]
            );
        }
        if pose.is_none() {
            crate::log!(
                "[fg ] fViewI at +0xc0 {}, so the camera position and basis stay invalid",
                if m.view_inverse.is_some() {
                    "is not an orthonormal view-to-world matrix"
                } else {
                    "could not be read"
                }
            );
        }
        match (m.engine, lens, pose) {
            (Some((position, direction, near, far)), Some(lens), Some(pose)) => {
                let close = |a: f32, b: f32| (a - b).abs() <= 1e-3 * a.abs().max(b.abs()).max(1.0);
                let agree = close(near, lens.near)
                    && (close(far, lens.far) || (lens.far.is_infinite() && far > 1e6))
                    && (0..3).all(|axis| close(position[axis], pose.position[axis]))
                    && (0..3).all(|axis| (direction[axis] - pose.forward[axis]).abs() < 1e-3);
                crate::log!(
                    "[fg ] camera cross-check: the engine's own fields read fCameraNearClip \
                     {near}, fCameraFarClip {far}, fCameraPos ({:.3}, {:.3}, {:.3}), fCameraDir \
                     ({:.4}, {:.4}, {:.4}) -- {}",
                    position[0],
                    position[1],
                    position[2],
                    direction[0],
                    direction[1],
                    direction[2],
                    if agree {
                        "these AGREE with what was derived from fNoJitterProj and fViewI"
                    } else {
                        "these DISAGREE with what was derived from fNoJitterProj and fViewI, \
                         which is what is sent; one of the offsets is not what it is taken for"
                    }
                );
            }
            (None, ..) => {
                crate::log!("[fg ] camera cross-check: the engine's own fields could not be read");
            }
            _ => {}
        }
    }
    Some(sl::Camera {
        lens,
        pose,
        view_to_clip: m.projection,
        clip_to_view: inverse_projection,
        clip_to_prev_clip,
        prev_clip_to_clip,
        jitter,
        mvec_scale: scale,
        jittered: settings.fg_mvec_jittered,
        dilated: settings.fg_mvec_dilated,
        reset,
    })
}

static SAID_SCALE: AtomicBool = AtomicBool::new(false);
static SAID_CAMERA: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// The three sites
// ---------------------------------------------------------------------------

static RESET: AtomicBool = AtomicBool::new(true);
static FRAMES: AtomicU64 = AtomicU64::new(0);
static SUBMITTED: AtomicBool = AtomicBool::new(false);
static SAID_NO_CAMERA: AtomicBool = AtomicBool::new(false);
static SAID_NO_INPUTS: AtomicBool = AtomicBool::new(false);

/// Tell Streamline the history is gone.
pub fn note_reset() {
    RESET.store(true, Ordering::Relaxed);
}

/// Whether anything should be done at all.
fn running() -> bool {
    crate::config::get().fg && sl::ready()
}

/// The engine has submitted the frame's first batch of work.
pub fn note_render_submit() {
    if !running() || !sl::device_ready() {
        return;
    }
    if SUBMITTED.swap(true, Ordering::Relaxed) {
        return;
    }
    sl::marker(sl::MARKER_SIMULATION_END);
    sl::marker(sl::MARKER_RENDERSUBMIT_START);
}

/// The engine's upscale slot has run: the frame's inputs exist and are
/// identified, and this is the last point before presentation that we own.
///
/// # Safety
/// `list` must be the open command list the upscale slot was given.
pub unsafe fn note_frame_inputs(list: *mut c_void, display: (u32, u32)) {
    if !running() {
        return;
    }
    // The device and the feature functions are settled at device creation now
    // -- see `streamline::set_device` for why they cannot be deferred to here.
    // This retries only the binding, which needs the plugins initialised and
    // is cheap once it has succeeded.
    if !sl::bind_feature_functions() {
        return;
    }
    // A token for this frame if the present path has not already opened one,
    // which is the case for the first frame of a session.
    if !sl::has_frame() {
        sl::begin_frame();
    }

    let Some((depth, motion, render, _jitter, _scale)) = crate::hijack::frame_inputs() else {
        // No temporal pass has run since the last frame described, or what
        // it used has been destroyed since -- the engine rebuilding its
        // targets. Either way there is nothing true to tag for this frame,
        // so generation is switched off for it rather than fed a stale
        // frame's depth and motion vectors: DLSS-G then presents the rendered
        // frame as it is. It comes back by itself, below, on the next frame
        // that has inputs.
        if !SAID_NO_INPUTS.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[fg ] no depth or motion vectors for this frame -- no temporal pass ran, or \
                 its targets have been destroyed since -- so nothing is tagged and generation \
                 is off until there are"
            );
        }
        let settings = crate::config::get();
        sl::set_mode(
            sl::MODE_OFF,
            settings.fg_multiplier.clamp(2, 4) - 1,
            display,
            display,
            settings.fg_uir,
        );
        return;
    };
    if SAID_NO_INPUTS.swap(false, Ordering::Relaxed) {
        crate::log!("[fg ] depth and motion vectors again; generation follows the settings");
    }

    let reset = RESET.swap(false, Ordering::Relaxed);
    // `(1.0, 1.0)` is what the upscaling path reports once `mv::decode` has
    // produced a texture of its own, because the decode leaves nothing for
    // NGX to scale. So it is the signal that the tagged buffer is in pixels,
    // read from what happened rather than from `mv_decode` being set -- the
    // decode can fail and fall back, and then the buffer is the engine's.
    let decoded = _scale == (1.0, 1.0);
    // The size of the resource that is tagged, from the same place the tag
    // takes it, so the scale and the tag can never describe different
    // buffers. `render` is the fallback only when the description is missing,
    // which is also what the tag falls back to.
    let mvec = crate::gpu::described(motion)
        .map(|info| (info.Width as u32, info.Height))
        .unwrap_or(render);
    let mut sent: Option<((f32, f32), (f32, f32))> = None;
    match camera_for_streamline(reset, display, mvec, decoded) {
        Some(camera) => {
            sent = Some((camera.mvec_scale, camera.jitter));
            sl::set_constants(&camera);
        }
        None => {
            if !SAID_NO_CAMERA.swap(true, Ordering::Relaxed) {
                crate::log!(
                    "[fg ] the engine's camera buffer has not been read, so the constants \
                     cannot be built; the launcher is what makes it readable"
                );
            }
        }
    }

    // Depth and motion vectors, in the states the tracking says they are in.
    // A state we never observed is not a state to assert: the header calls it
    // mandatory and getting it wrong is undefined behaviour rather than an
    // error return, which is the mistake `states.rs` exists to prevent.
    let mut tags: Vec<sl::Tagged> = Vec::with_capacity(3);
    for (resource, kind) in [(depth, sl::Kind::Depth), (motion, sl::Kind::MotionVectors)] {
        let Some(state) = crate::states::known(list as usize, resource) else {
            if !SAID_NO_STATE.swap(true, Ordering::Relaxed) {
                crate::log!(
                    "[fg ] the state of {resource:#x} has never been observed, so it is not \
                     tagged"
                );
            }
            continue;
        };
        let described = crate::gpu::described(resource);
        tags.push(sl::Tagged {
            resource: resource as *mut c_void,
            kind,
            state: state.0 as u32,
            size: described
                .map(|info| (info.Width as u32, info.Height))
                .unwrap_or(render),
            format: described.map(|info| info.Format.0 as u32).unwrap_or(0),
        });
    }

    // The interface-free frame, only when asked for. It is the right content
    // in the wrong colour space -- R11G11B10_FLOAT against the back buffer's
    // B8G8R8A8_UNORM, because this engine draws its interface before the
    // tonemap -- and the guide requires them to match, so this is off by
    // default and is a question for a run of its own.
    if !tags.is_empty() {
        sl::tag(&tags, list);
    }
    if crate::pathdump::armed() {
        let states: Vec<(u32, u32)> = tags
            .iter()
            .map(|tag| (tag.kind as u32, tag.state))
            .collect();
        crate::pathdump::frame_generation(
            depth,
            motion,
            &states,
            render,
            display,
            sent.map(|(scale, _)| scale),
            sent.map_or_else(crate::native::last_jitter, |(_, jitter)| jitter),
            decoded,
        );
    }

    let settings = crate::config::get();
    // Reflex on, and not as a preference. The DLSS-G guide states it as a
    // requirement -- *"It is required for sl.reflex to be integrated in the
    // host application"* -- and when it is missing DLSS-G does not fail: it
    // presents the real frame, generates nothing, and reports
    //
    //     slDLSSGGetState -> ok; status 0x2 (Reflex not active),
    //                        presented 1, max generated 5
    //
    // `slReflexSetOptions(mode 0)` is `eOff`, so with frame generation on
    // the mode is forced on: the only other outcome is a feature that runs
    // and silently does nothing. The markers and `slReflexSleep` around
    // `Present` are inert without it.
    sl::reflex_options(true, settings.reflex_boost);
    test_mask(list, display, &settings);
    // `fg_interpolate = 0` is the control for every judgement of the picture
    // made by eye: the same Streamline path, the same Reflex, the same tags,
    // and no generated frames.
    sl::set_mode(
        if settings.fg_interpolate {
            sl::MODE_ON
        } else {
            sl::MODE_OFF
        },
        settings.fg_multiplier.clamp(2, 4) - 1,
        display,
        render,
        settings.fg_uir,
    );
}

/// What the test mask was last handed over as: kind, value, box and the size,
/// or `None` while nothing is tagged. A change is what gets logged.
static MASK_SENT: Mutex<Option<(u32, u32, u32, (u32, u32))>> = Mutex::new(None);
static MASK_FRAMES: AtomicU64 = AtomicU64::new(0);
static SAID_MASK_FAILED: AtomicBool = AtomicBool::new(false);

/// Hand DLSS-G a constant mask as a no-warp mask or as UI alpha, when asked.
///
/// Called at the upscale slot, because the clear that fills the mask has to
/// be recorded on a list that executes before Present and this is the one
/// place a list of the engine's is in hand. Everything it logs is what
/// happened: the resource, its size and format, the rectangle that was
/// cleared, and what `slSetTagForFrame` answered -- on every change, in both
/// directions, so a run where nothing was handed over cannot read like a run
/// where it was handed over and ignored.
unsafe fn test_mask(list: *mut c_void, display: (u32, u32), settings: &crate::config::Config) {
    let kind = match settings.fg_test_mask {
        1 => Some(sl::Kind::NoWarpMask),
        2 => Some(sl::Kind::UiAlpha),
        _ => None,
    };
    let mut sent = MASK_SENT.lock();
    let Some(kind) = kind else {
        // Switched off: remove whichever tag was last set, once, and say what
        // the plugin answered. Tags are filed per frame, so this is belt and
        // braces -- but "off" must mean off for an A/B to mean anything.
        if let Some((was, ..)) = sent.take() {
            let was = if was == 1 {
                sl::Kind::NoWarpMask
            } else {
                sl::Kind::UiAlpha
            };
            let removed = sl::tag(
                &[sl::Tagged {
                    resource: std::ptr::null_mut(),
                    kind: was,
                    state: 0,
                    size: display,
                    format: 0,
                }],
                list,
            );
            crate::log!(
                "[fg ] test mask off: a removal of the {} tag (null resource) was sent, and \
                 slSetTagForFrame {}",
                was.label(),
                if removed {
                    "accepted that"
                } else {
                    "refused that"
                }
            );
        }
        return;
    };
    let value = settings.fg_test_mask_value;
    let boxed = settings.fg_test_mask_box;
    let wanted = (
        settings.fg_test_mask,
        value.to_bits(),
        boxed.to_bits(),
        display,
    );
    // A different kind than last frame: the old tag goes first, or both would
    // stand at once and the picture could not be attributed to either.
    if let Some((was, ..)) = *sent {
        if was != settings.fg_test_mask {
            let was = if was == 1 {
                sl::Kind::NoWarpMask
            } else {
                sl::Kind::UiAlpha
            };
            sl::tag(
                &[sl::Tagged {
                    resource: std::ptr::null_mut(),
                    kind: was,
                    state: 0,
                    size: display,
                    format: 0,
                }],
                list,
            );
        }
    }
    // Repainted on a change, and again every few seconds: the clear is
    // recorded on an engine list, and a list the engine abandons would
    // otherwise leave the mask holding something the log says it does not.
    let frames = MASK_FRAMES.fetch_add(1, Ordering::Relaxed);
    let painted = match crate::fgmask::paint(list, display, value, boxed, frames % 300 == 299) {
        Ok(painted) => painted,
        Err(error) => {
            if !SAID_MASK_FAILED.swap(true, Ordering::Relaxed) {
                crate::log!("[fg ] test mask: nothing was tagged -- {error}");
            }
            return;
        }
    };
    let accepted = sl::tag(
        &[sl::Tagged {
            resource: painted.resource,
            kind,
            state: crate::fgmask::READ_STATE.0 as u32,
            size: painted.size,
            format: crate::fgmask::FORMAT,
        }],
        list,
    );
    if *sent != Some(wanted) {
        crate::log!(
            "[fg ] test mask: {:#x}, {}x{} format {}, tagged as {} and slSetTagForFrame {}. \
             A clear to {value} over ({}, {})-({}, {}) and to 0 elsewhere {}. What the plugin \
             does with it is not known from here -- the overlay and the picture are the \
             measurement",
            painted.resource as usize,
            painted.size.0,
            painted.size.1,
            crate::fgmask::FORMAT,
            kind.label(),
            if accepted {
                "accepted it"
            } else {
                "REFUSED it"
            },
            painted.region.0,
            painted.region.1,
            painted.region.2,
            painted.region.3,
            if painted.recorded {
                "was recorded on this frame's list"
            } else {
                "was already recorded on an earlier list"
            }
        );
        *sent = Some(wanted);
    }
}

/// Tag the interface-free frame, at Present rather than at the upscale slot.
///
/// The capture happens at the first interface draw, which is *after* the
/// upscale slot. Tagging it alongside the depth and motion vectors would
/// hand DLSS-G the image captured during the **previous** frame, every
/// frame.
///
/// DLSS-G works out which pixels are interface by comparing the HUD-less
/// image against the back buffer, so an image from the wrong frame marks
/// everything that moved in between as interface. By Present this frame's
/// capture has happened, the frame token is still this frame's, and every
/// tag is `eValidUntilPresent` -- so the list may be null, which is what the
/// header permits and what there is to give here anyway.
unsafe fn tag_hudless(display: (u32, u32)) {
    if !crate::config::get().fg_hudless {
        return;
    }
    // Once per **rendered** frame, not once per Present.
    //
    // This runs from the Present hook, and with 2x generation about half of
    // the Presents are frames DLSS-G made. A generated frame has no capture
    // of its own to tag and must not be counted as a frame that was missing
    // one. `FRAME` advances at the upscale slot, so when Present runs with
    // no rendered frame in between, the stamp still matches and the *same*
    // capture would be handed over again.
    //
    // So: skip entirely unless the engine has rendered since the last
    // attempt. Those Presents are counted separately, so the log says how
    // many Presents carried no rendered frame.
    let frame = crate::hudless::frame_index();
    if LAST_TAGGED.swap(frame, Ordering::Relaxed) == frame {
        REPEATS.fetch_add(1, Ordering::Relaxed);
        return;
    }
    match crate::hudless::frame(display) {
        Some(held) => {
            use windows::core::Interface;
            // The real format, not `UNKNOWN`. Passing 0 left Streamline to
            // infer the format of the one tag whose format is the whole
            // question -- the guide requires the HUD-less image to match the
            // back buffer's colour space, and this engine's does not.
            let format = crate::hudless::format()
                .map(|value| value.0 as u32)
                .unwrap_or(0);
            if !SAID_HUDLESS.swap(true, Ordering::Relaxed) {
                let back = crate::swap::chain().map(|chain| chain.format);
                crate::log!(
                    "[fg ] handing over a HUD-less frame: {:#x}, {}x{}, format {format} \
                     against the back buffer's {}. {}",
                    held.as_raw() as usize,
                    display.0,
                    display.1,
                    back.map(|value| value.0.to_string())
                        .unwrap_or_else(|| "?".into()),
                    if back.is_some_and(|value| value.0 as u32 != format) {
                        "These differ, which the guide forbids -- measured accepted, the \
                         overlay reads Hudless: Yes"
                    } else {
                        "These match"
                    }
                );
            }
            let tags = [sl::Tagged {
                resource: held.as_raw(),
                kind: sl::Kind::HudlessColour,
                state: crate::hudless::RESTING_STATE.0 as u32,
                size: display,
                format,
            }];
            sl::tag(&tags, std::ptr::null_mut());
            FRESH.fetch_add(1, Ordering::Relaxed);
        }
        None => {
            // A rendered frame with no capture of its own. DLSS-G's documented
            // fallback is to interpolate the finished image, so the interface
            // is handled one way on this frame and another on its neighbours
            // -- and **inconsistent is its own kind of wrong**, distinct from
            // stale and from missing. A world-space label, the one interface
            // element that moves, is exactly where an unstable mask shows.
            MISSING.fetch_add(1, Ordering::Relaxed);
        }
    }
}

static FRESH: AtomicU64 = AtomicU64::new(0);
static MISSING: AtomicU64 = AtomicU64::new(0);
static REPEATS: AtomicU64 = AtomicU64::new(0);
static LAST_TAGGED: AtomicU64 = AtomicU64::new(u64::MAX);

/// Rendered frames tagged, rendered frames with no capture, and Presents that
/// carried no rendered frame at all.
///
/// The third is the denominator the first two have to be read against, and it
/// is also the measurement of how many Presents are DLSS-G's own: with 2x it
/// should come to roughly the sum of the other two.
pub fn hudless_counts() -> (u64, u64, u64) {
    (
        FRESH.load(Ordering::Relaxed),
        MISSING.load(Ordering::Relaxed),
        REPEATS.load(Ordering::Relaxed),
    )
}

static SAID_NO_STATE: AtomicBool = AtomicBool::new(false);
static SAID_HUDLESS: AtomicBool = AtomicBool::new(false);

/// Immediately before Streamline's `Present`.
pub fn before_present() {
    if !running() || !sl::device_ready() {
        return;
    }
    if SUBMITTED.swap(false, Ordering::Relaxed) {
        sl::marker(sl::MARKER_RENDERSUBMIT_END);
    }
    // Before the present marker, because the tag belongs to this frame's work
    // and the marker closes it.
    if let Some(chain) = crate::swap::chain() {
        // Safe at this point for the reason the function documents: the tag is
        // `eValidUntilPresent`, so a null list is permitted, and the resource
        // is a texture this library owns and holds a reference to.
        unsafe { tag_hudless(chain.size) };
    }
    sl::marker(sl::MARKER_PRESENT_START);
}

/// Immediately after it returns, which is where the next frame begins.
pub fn after_present() {
    if !running() || !sl::device_ready() {
        return;
    }
    sl::marker(sl::MARKER_PRESENT_END);
    // Once a second, because the status word is the whole diagnosis and it
    // changes as the engine moves between menus, loading and play.
    //
    // Read **here**, on the present thread. Streamline requires it:
    //
    //     dlss_gEntry.cpp:1144[slGetData] slDLSSGGetState must be
    //     synchronized with the present thread
    //
    // The upscale slot is the engine's render thread, and reading from
    // there gives a torn or stale sample rather than a fault. Directly after
    // `Present` has returned is the one point that is unambiguously on the
    // present thread and unambiguously after the frame it describes.
    let frames = FRAMES.fetch_add(1, Ordering::Relaxed);
    if frames == 0 || frames.is_multiple_of(600) {
        sl::report_state();
    }
    // The next frame's token, then the sleep, then the marker that opens it.
    // `slReflexSleep` must always be called, even with low latency off, and
    // this is the only place it means anything: it returns when the driver
    // judges the frame should start.
    // A new token only if the outgoing one was actually used.
    //
    // This runs on every Present, and the token index was advancing with it.
    // A Present that carried no rendered frame -- 3706 of them in one session,
    // all during loading -- therefore moved the index on without anything
    // being filed under the new one, and the plugin then asks under an index
    // that has neither constants nor tags:
    //
    //     commonInterface.h:622 Unable to find 'common' constants for frame N
    //     commonInterface.h:256 Failed to find global tag 'kBufferTypeDepth'
    //     interpolation state changed from enabled to disabled
    //
    // Reusing the previous token is not merely the lesser evil here, it is
    // what the plugin does anyway when it cannot find the current one --
    // *"using last set for frame"*. The difference is that it then has a
    // complete set rather than a partial one, and **consistent beats fresh**
    // when the alternative is interpolation switching itself off.
    if sl::token_used() || !sl::has_frame() {
        sl::begin_frame();
    }
    sl::reflex_sleep();
    sl::marker(sl::MARKER_SIMULATION_START);
}
