//! The game's DLSS evaluations (engine `UpscaleHandler`) and the temporal
//! pass (render `PassHandler`): where DLSS runs.
//!
//! Below a render ratio of 1 the resolve is made at the engine's upscale
//! point (EvalISP), from the colour the engine holds there: DOF, motion
//! blur and tonemapping have run on it (`ReDlss1Path.md` §5); resolving at
//! the temporal pass instead skipped them (0.2's paler `resolve_at_pass`).
//! The temporal pass then only identifies depth and motion vectors and
//! hands its colour through unfiltered (the temporal work is DLSS's, once).
//! At ratio 1 there is no upscale point (`engine::ratio::full_window`) and
//! DLSS resolves into the temporal pass's own output (DLAA).
//!
//! `InputPath = Supersampling` takes colour, dense velocity and the R32F
//! depth copy from the engine's supersampling call (EvalSS, flags 3, depth
//! alias on) instead of the temporal pass.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};

use mhw_core::config::InputPath;
use mhw_core::frame::{FrameInfo, ResetReason, Size};
use mhw_core::math::{dlss_jitter, Vec2};
use mhw_engine::{ImageSignalCall, SupersamplingCall, UpscaleHandler};
use mhw_render::{PassContext, PassVerdict};
use windows::core::Interface;

use super::evaluate::{evaluate, size_of, Evaluation};
use super::{fg_glue, live, runtime};
use crate::fallback::SlotOutcome;
use crate::frames::frame_after;
use crate::policy::{dlss_is_hdr, feature_desc, ColourStage, Policy, SS_MV_SCALE};

/// The engine's upscale handler.
#[derive(Debug)]
pub(crate) struct Handler;

/// The one handler registered with the engine.
pub(crate) static HANDLER: Handler = Handler;

/// What the slot did, for the dev state and the log.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    /// DLSS evaluations at the upscale point.
    pub upscaled: AtomicU64,
    /// DLAA evaluations (temporal pass or supersampling call at ratio 1).
    pub dlaa: AtomicU64,
    /// Colour handed through the temporal pass / SS call unfiltered.
    pub passed_through: AtomicU64,
    /// Evaluations that failed.
    pub failed: AtomicU64,
    /// Upscale points without inputs (no temporal pass this frame).
    pub without_inputs: AtomicU64,
    /// Evaluations whose frame (by the pass's camera buffer) was not the
    /// engine's number at the slot.
    pub renumbered: AtomicU64,
    /// Evaluations whose pass camera buffer matched no kept frame.
    pub unmatched: AtomicU64,
}

/// The counters.
pub(crate) static COUNTERS: Counters = Counters {
    upscaled: AtomicU64::new(0),
    dlaa: AtomicU64::new(0),
    passed_through: AtomicU64::new(0),
    failed: AtomicU64::new(0),
    without_inputs: AtomicU64::new(0),
    renumbered: AtomicU64::new(0),
    unmatched: AtomicU64::new(0),
};

/// `MV.Scale` of the dense velocity, as `f32` bits: [`SS_MV_SCALE`] until a
/// dev client writes another (`state.ss_mv_scale_address`, `mem.write
/// force`) to check the convention live (`probe mv`).
pub(crate) static SS_MV_SCALE_BITS: [AtomicU32; 2] = [
    AtomicU32::new(SS_MV_SCALE.x.to_bits()),
    AtomicU32::new(SS_MV_SCALE.y.to_bits()),
];

/// The dense velocity's `MV.Scale` in force.
pub(crate) fn ss_mv_scale() -> Vec2 {
    if cfg!(feature = "dev") {
        Vec2::new(
            f32::from_bits(SS_MV_SCALE_BITS[0].load(Ordering::Relaxed)),
            f32::from_bits(SS_MV_SCALE_BITS[1].load(Ordering::Relaxed)),
        )
    } else {
        SS_MV_SCALE
    }
}

/// A per-axis factor on the jitter handed to DLSS, as `f32` bits: `(1, 1)`
/// until a dev client writes another (`state.dlss_jitter_scale_address`,
/// `mem.write force`), to test the sign convention live. Release: none.
pub(crate) static DLSS_JITTER_SCALE_BITS: [AtomicU32; 2] = [
    AtomicU32::new(0x3f80_0000), // 1.0
    AtomicU32::new(0x3f80_0000),
];

/// The jitter DLSS is given for `px` (render pixels, engine axes).
fn evaluation_jitter(px: Vec2) -> Vec2 {
    let jitter = dlss_jitter(px);
    if cfg!(feature = "dev") {
        Vec2::new(
            jitter.x * f32::from_bits(DLSS_JITTER_SCALE_BITS[0].load(Ordering::Relaxed)),
            jitter.y * f32::from_bits(DLSS_JITTER_SCALE_BITS[1].load(Ordering::Relaxed)),
        )
    } else {
        jitter
    }
}

impl UpscaleHandler for Handler {
    fn supersampling(&self, call: &SupersamplingCall) -> bool {
        supersampling(call)
    }

    fn image_signal(&self, call: &ImageSignalCall) -> bool {
        image_signal(call)
    }
}

/// Motion vectors and depth for one evaluation.
#[derive(Clone, Copy, Debug)]
struct Sources {
    motion: *mut c_void,
    depth: *mut c_void,
    mv_scale: Vec2,
    /// `tGBuffer4`, for the dev captures.
    #[cfg_attr(not(feature = "dev"), allow(dead_code))]
    packed_motion: *mut c_void,
    /// The render frame the temporal pass drew ([`pass_frame`]).
    frame: Option<u64>,
}

/// The render frame whose camera filled a temporal pass's camera buffer,
/// found by its jitter. The engine numbers frames at the main thread's
/// kick while the render thread records, so the slot's own number can be
/// the next frame's: its camera and jitter would be the wrong ones, or not
/// published yet (`frames::CameraRing::frame_with_projection`).
fn pass_frame(camera: Option<&mhw_render::CameraCb>) -> Option<u64> {
    let proj = camera?.mat4("fProj")?;
    live::frame_of_projection([proj.rows[2][0], proj.rows[2][1]])
}

/// The frame an evaluation belongs to: the pass's, else the engine's
/// number at the slot. Counts the slots where the two differ.
fn evaluation_frame(sources: &Sources, numbered: u64) -> u64 {
    match sources.frame {
        Some(frame) => {
            if frame != numbered {
                COUNTERS.renumbered.fetch_add(1, Ordering::Relaxed);
            }
            frame
        }
        None => {
            COUNTERS.unmatched.fetch_add(1, Ordering::Relaxed);
            numbered
        }
    }
}

fn ptr(address: usize) -> *mut c_void {
    address as *mut c_void
}

/// Why a slot had no usable inputs, as the fallback counts it.
type NoInputs = (SlotOutcome, &'static str);

/// Temporal-pass dispatches the handler saw, and the count at the last
/// slot: whether the pass ran since tells "no pass" from "unusable pass".
static PASS_DISPATCHES: AtomicU64 = AtomicU64::new(0);
static PASS_DISPATCHES_AT_SLOT: AtomicU64 = AtomicU64::new(0);

/// Whether a temporal pass ran since the previous slot (and mark this one).
fn pass_ran_since_last_slot() -> bool {
    let now = PASS_DISPATCHES.load(Ordering::Relaxed);
    PASS_DISPATCHES_AT_SLOT.swap(now, Ordering::Relaxed) != now
}

/// No inputs: no temporal pass at all (`without`), or a pass whose inputs
/// could not be used (`unusable`).
fn missing(pass_ran: bool, without: &'static str, unusable: &'static str) -> NoInputs {
    if pass_ran {
        (SlotOutcome::Unresolved, unusable)
    } else {
        (SlotOutcome::WithoutPass, without)
    }
}

/// Depth and vectors from the temporal pass's record for `list`.
fn pass_sources(list: *mut c_void, pass_ran: bool) -> Result<Sources, NoInputs> {
    let unresolved = |why| (SlotOutcome::Unresolved, why);
    // SAFETY: the engine's live list.
    let (inputs, _) = unsafe { mhw_render::take_inputs(list) }.ok_or_else(|| {
        missing(
            pass_ran,
            "no temporal pass",
            "the temporal pass ran but its inputs were not identified",
        )
    })?;
    let motion = inputs
        .motion
        .ok_or(unresolved("motion vectors not decoded"))?;
    let depth = inputs.depth.ok_or(unresolved("no scene depth matched"))?;
    if !mhw_render::is_alive(motion.resource, motion.serial)
        || !mhw_render::is_alive(depth.resource, depth.serial)
    {
        return Err(unresolved("depth or motion vectors released since"));
    }
    Ok(Sources {
        motion: ptr(motion.resource),
        depth: ptr(depth.resource),
        mv_scale: crate::policy::DECODED_MV_SCALE,
        packed_motion: ptr(inputs.packed_motion.resource),
        frame: pass_frame(inputs.camera.as_ref()),
    })
}

/// Depth and vectors for the upscale point at engine count `numbered`, from
/// its frame's supersampling call ([`live::upscale_ss`]).
fn ss_sources(numbered: u64, pass_ran: bool) -> Result<Sources, NoInputs> {
    let (ss, frame) = live::upscale_ss(numbered).ok_or_else(|| {
        missing(
            pass_ran,
            "no temporal pass and no supersampling call",
            "the temporal pass ran but no supersampling call did",
        )
    })?;
    Ok(ss_call_sources(ss.motion, ss.depth, frame))
}

/// The supersampling call's dense velocity and depth copy, rendered for
/// `rendered`.
fn ss_call_sources(motion: usize, depth: usize, rendered: Option<u64>) -> Sources {
    Sources {
        motion: ptr(motion),
        depth: ptr(depth),
        mv_scale: ss_mv_scale(),
        packed_motion: std::ptr::null_mut(),
        frame: rendered,
    }
}

/// The last frame [`ss_rendered_frame`] answered, `u64::MAX` before the
/// first.
static LAST_SS_RENDERED: AtomicU64 = AtomicU64::new(u64::MAX);

/// The frame the supersampling call's image was rendered for. The TAA
/// draw records `CSTemporalAA2` and then the SS enqueue on the same list
/// (`ReDlss1Path.md` §2.5, §5), so the newest pass record of `list` is this
/// image's, and its camera buffer names the frame ([`pass_frame`]). The
/// engine's number at the call is not: the render thread can record frame
/// N while the main thread has already counted N + 1 without publishing its
/// camera. About one call in 200 finds its record not published yet (seen
/// in game: the board's `empty` takes); it gets the frame after the last
/// one answered while the run is unbroken ([`frame_after`]).
fn ss_rendered_frame(list: *mut c_void, numbered: u64) -> Option<u64> {
    // SAFETY: the engine's open list on this thread.
    let matched = unsafe { mhw_render::take_inputs(list) }
        .filter(|(_, association)| *association == mhw_render::Association::SameList)
        .and_then(|(inputs, _)| pass_frame(inputs.camera.as_ref()));
    let last = Some(LAST_SS_RENDERED.load(Ordering::Relaxed)).filter(|&f| f != u64::MAX);
    let frame = matched.or_else(|| frame_after(last, numbered, 1));
    LAST_SS_RENDERED.store(frame.unwrap_or(u64::MAX), Ordering::Relaxed);
    frame
}

fn output_is_hdr() -> bool {
    mhw_engine::addresses()
        .and_then(mhw_engine::views::RenderConfig::get)
        .and_then(|config| config.output_pq())
        .unwrap_or(false)
}

/// EvalISP: DLSS from the post-processed colour into the engine's
/// display-size target.
fn image_signal(call: &ImageSignalCall) -> bool {
    let policy = runtime::policy();
    if !policy.dlss {
        return false;
    }
    let frame = call.frame;
    live::note_display(call.display);
    let display = size_of(call.output as usize).unwrap_or(call.display);
    let render = Some(call.rect.size())
        .filter(|size| !size.is_empty())
        .or_else(|| size_of(call.colour as usize))
        .unwrap_or_default();
    let pass_ran = pass_ran_since_last_slot();
    if !runtime::may_evaluate(frame) {
        COUNTERS.without_inputs.fetch_add(1, Ordering::Relaxed);
        runtime::note_slot(
            SlotOutcome::Unresolved,
            frame,
            "paused after a failed evaluation",
        );
        return false;
    }
    let sources = match policy.input_path {
        InputPath::Pass => pass_sources(call.list, pass_ran),
        InputPath::Supersampling => ss_sources(frame, pass_ran),
    };
    let sources = match sources {
        Ok(sources) => sources,
        Err((outcome, why)) => {
            // The target stays unwritten: the picture stands still on this
            // frame, and the fallback counts it.
            COUNTERS.without_inputs.fetch_add(1, Ordering::Relaxed);
            mhw_core::log_changed!(Info, "dlss", "slot inputs", why, "not evaluating: {why}");
            runtime::note_slot(outcome, frame, why);
            return false;
        }
    };
    let frame = evaluation_frame(&sources, frame);
    fg_glue::at_slot(frame);
    mhw_core::log_changed!(
        Info,
        "dlss",
        "slot inputs",
        "",
        "evaluating at the upscale point"
    );
    let stage = ColourStage::AfterTonemap;
    let outcome = run(
        &policy,
        Run {
            list: call.list,
            frame,
            colour: call.colour,
            output: call.output,
            render,
            display,
            game_reset: call.game_reset,
            stage,
            sources,
        },
    );
    if outcome.is_some() {
        COUNTERS.upscaled.fetch_add(1, Ordering::Relaxed);
    }
    outcome.is_some()
}

/// One evaluation's request.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Run {
    pub list: *mut c_void,
    pub frame: u64,
    colour: *mut c_void,
    output: *mut c_void,
    render: Size,
    display: Size,
    game_reset: bool,
    stage: ColourStage,
    sources: Sources,
}

/// Evaluate, count, hand the frame to frame generation and the dev server.
fn run(policy: &Policy, r: Run) -> Option<FrameInfo> {
    let camera = live::camera(r.frame);
    let jitter = live::jitter(r.frame);
    let is_hdr = dlss_is_hdr(policy.dlss_hdr, r.stage, output_is_hdr());
    let desc = feature_desc(policy, r.render, r.display, is_hdr);
    let info = live::begin_evaluation(policy.reset, r.frame, r.game_reset, camera.as_ref(), &desc);
    let evaluation = Evaluation {
        list: r.list,
        frame: r.frame,
        colour: r.colour,
        motion: r.sources.motion,
        depth: r.sources.depth,
        output: r.output,
        desc,
        mv_scale: r.sources.mv_scale,
        jitter: jitter.map_or(Vec2::ZERO, |j| evaluation_jitter(j.px)),
        reset: info.reset != ResetReason::None,
        sharpness: policy.sharpness,
    };
    // SAFETY: inside the engine's recording of its open list, on this
    // thread; the resources are the engine's current ones (liveness checked
    // for the temporal pass's record).
    let result = unsafe { evaluate(&evaluation) };
    #[cfg(feature = "dev")]
    mhw_dev::trace_event("evaluate", r.frame);
    match &result {
        Ok(done) => {
            runtime::note_slot(SlotOutcome::Evaluated, r.frame, "evaluated");
            mhw_core::log_changed!(Info, "dlss", "evaluate", "ok", "DLSS evaluating");
            if done.created {
                log::info!(
                    target: "dlss",
                    "feature for {}x{} -> {}x{} ({:?}, HDR {is_hdr}, reset {:?})",
                    desc.render.w,
                    desc.render.h,
                    desc.display.w,
                    desc.display.h,
                    desc.quality,
                    info.reset
                );
            }
        }
        Err(failure) => {
            COUNTERS.failed.fetch_add(1, Ordering::Relaxed);
            let why = failure.to_string();
            mhw_core::log_changed!(
                Warn,
                "dlss",
                "evaluate",
                why.clone(),
                "not evaluated: {why}"
            );
            runtime::note_slot(SlotOutcome::Failed, r.frame, &why);
        }
    }
    // Only this frame's own camera: another frame's constants would be
    // filed under this image (and its HUD-less image not matched).
    if let (Some(camera), Some(jitter)) = (camera, jitter) {
        fg_glue::submit(&fg_glue::Submit {
            list: r.list,
            camera,
            jitter,
            reset: info.reset != ResetReason::None,
            depth: r.sources.depth,
            motion: r.sources.motion,
            motion_state: match policy.input_path {
                InputPath::Pass => Some(mhw_render::states::NON_PIXEL_SHADER_RESOURCE),
                InputPath::Supersampling => None,
            },
            mv_scale: r.sources.mv_scale,
        });
    }
    #[cfg(feature = "dev")]
    super::devwire::slot(r.list, info, &r, camera, jitter);
    result.ok().map(|_| info)
}

/// The temporal pass (`CSTemporalAA2`), `InputPath = Pass`.
pub(crate) fn on_pass(ctx: &PassContext<'_>) -> PassVerdict {
    PASS_DISPATCHES.fetch_add(1, Ordering::Relaxed);
    // Seen whatever the policy: a hold for want of a pass ends here.
    runtime::note_pass_seen();
    let policy = runtime::policy();
    if !policy.dlss || policy.input_path != InputPath::Pass {
        return PassVerdict::RunEngine;
    }
    let Some(inputs) = ctx.inputs else {
        if policy.engine.ratio >= 1.0 {
            // DLAA was due here and no upscale point follows to count it.
            runtime::note_slot(
                SlotOutcome::Unresolved,
                live::current_frame().unwrap_or(0),
                "temporal pass inputs not identified",
            );
        }
        return PassVerdict::RunEngine;
    };
    let native = ctx.list.as_raw();
    let display = live::display();
    if !display.is_empty() && inputs.output.size == display {
        // Ratio 1: no upscale point follows; DLAA into the pass's output.
        // Whatever stops it, the engine's own pass runs: the picture moves.
        let frame = live::current_frame().unwrap_or(0);
        let (Some(motion), Some(depth)) = (inputs.motion, inputs.depth) else {
            mhw_core::log_changed!(
                Info,
                "dlss",
                "dlaa inputs",
                (inputs.motion.is_some(), inputs.depth.is_some()),
                "DLAA: motion vectors {} / depth {}; the engine's pass runs",
                inputs.motion.is_some(),
                inputs.depth.is_some()
            );
            let why = if inputs.motion.is_none() {
                "motion vectors not decoded"
            } else {
                "no scene depth matched"
            };
            runtime::note_slot(SlotOutcome::Unresolved, frame, why);
            return PassVerdict::RunEngine;
        };
        if !runtime::may_evaluate(frame) {
            runtime::note_slot(
                SlotOutcome::Unresolved,
                frame,
                "paused after a failed evaluation",
            );
            return PassVerdict::RunEngine;
        }
        let Some(list) = ngx_list(ctx) else {
            runtime::note_slot(
                SlotOutcome::Unresolved,
                frame,
                "ReShade's proxy for the engine's list is not known",
            );
            return PassVerdict::RunEngine;
        };
        let sources = Sources {
            motion: ptr(motion.resource),
            depth: ptr(depth.resource),
            mv_scale: crate::policy::DECODED_MV_SCALE,
            packed_motion: ptr(inputs.packed_motion.resource),
            frame: pass_frame(inputs.camera.as_ref()),
        };
        let frame = evaluation_frame(&sources, frame);
        fg_glue::at_slot(frame);
        let outcome = run(
            &policy,
            Run {
                list,
                frame,
                colour: ptr(inputs.colour.resource),
                output: ptr(inputs.output.resource),
                render: inputs.render,
                display,
                game_reset: mhw_engine::take_game_reset(),
                stage: ColourStage::BeforeTonemap,
                sources,
            },
        );
        return match outcome {
            Some(_) => {
                COUNTERS.dlaa.fetch_add(1, Ordering::Relaxed);
                PassVerdict::Skip
            }
            None => PassVerdict::RunEngine,
        };
    }
    // An upscale point follows: hand the colour through unfiltered.
    // SAFETY: the engine's open list on this thread; the record's
    // resources were bound by this very dispatch.
    match unsafe {
        mhw_render::copy_resource(native, inputs.colour.resource, inputs.output.resource)
    } {
        Ok(()) => {
            COUNTERS.passed_through.fetch_add(1, Ordering::Relaxed);
            PassVerdict::Skip
        }
        Err(error) => {
            // The engine's own pass then runs and the picture is filtered
            // twice: soft, not wrong.
            mhw_core::log_changed!(
                Warn,
                "taa",
                "pass through",
                format!("{error:?}"),
                "temporal pass colour not handed through ({error:?}); the engine's filter runs"
            );
            PassVerdict::RunEngine
        }
    }
}

/// The command list NGX gets at the temporal pass.
///
/// NGX creates descriptor heaps on the device it was initialised with and
/// sets them on the list it is given, so both must be on the same side of
/// ReShade. With ReShade wrapping the game's device NGX holds the proxy
/// device, and a native list then faulted inside D3D12Core at the first
/// DLSS frame (0.2, measured in all four combinations): it gets the engine's
/// proxy list, which render finds from ReShade's `Dispatch` on this thread's
/// stack. When that proxy is not known nothing is evaluated (the engine's
/// pass runs) rather than risk the fault. Without ReShade: the native list.
fn ngx_list(ctx: &PassContext<'_>) -> Option<*mut c_void> {
    let native = ctx.list.as_raw();
    if ctx.engine_list != native {
        return Some(ctx.engine_list);
    }
    if !mhw_render::reshade_present() || !ngx_device_is_proxy() {
        return Some(native);
    }
    mhw_core::log_once!(
        Warn,
        "dlss",
        "no proxy list",
        "NGX holds ReShade's proxy device and the proxy of the engine's list is not on the \
         stack ({} proxy dispatches seen, watching {}): DLAA is not evaluated at the temporal \
         pass, the engine's own pass runs",
        mhw_render::proxy_dispatches(),
        mhw_render::proxy_lists_watched()
    );
    None
}

/// Whether the device the game's NGX `Init` got is a ReShade proxy (asked
/// once NGX is initialised; the device lives as long as the game).
fn ngx_device_is_proxy() -> bool {
    const UNKNOWN: u8 = 0;
    const NO: u8 = 1;
    const YES: u8 = 2;
    static KNOWN: AtomicU8 = AtomicU8::new(UNKNOWN);
    match KNOWN.load(Ordering::Relaxed) {
        NO => false,
        YES => true,
        _ => {
            let Some(device) = mhw_ngx::session::init_device() else {
                return false;
            };
            // SAFETY: the game's live device, as it passed it to NGX.
            let proxy = unsafe { mhw_render::is_reshade_proxy(device as *mut c_void) };
            KNOWN.store(if proxy { YES } else { NO }, Ordering::Relaxed);
            proxy
        }
    }
}

/// EvalSS: keep the inputs for the upscale point; at ratio 1 resolve here
/// (DLAA into the history slot the engine copies back). Held or given up:
/// the colour copy only.
fn supersampling(call: &SupersamplingCall) -> bool {
    let policy = runtime::policy();
    let render = Some(call.rect.size())
        .filter(|size| !size.is_empty())
        .or_else(|| size_of(call.colour as usize))
        .unwrap_or_default();
    if policy.input_path != InputPath::Supersampling {
        live::set_ss(live::SsInputs {
            frame: call.frame,
            rendered: None,
            motion: call.motion as usize,
            depth: call.depth as usize,
        });
        return false;
    }
    let rendered = ss_rendered_frame(call.list, call.frame);
    live::set_ss(live::SsInputs {
        frame: call.frame,
        rendered,
        motion: call.motion as usize,
        depth: call.depth as usize,
    });
    let output = size_of(call.output as usize).unwrap_or_default();
    let display = live::display();
    let due = policy.dlss && !display.is_empty() && output == display;
    if due && !runtime::may_evaluate(call.frame) {
        runtime::note_slot(
            SlotOutcome::Unresolved,
            call.frame,
            "paused after a failed evaluation",
        );
    } else if due {
        let sources = ss_call_sources(call.motion as usize, call.depth as usize, rendered);
        let frame = evaluation_frame(&sources, call.frame);
        fg_glue::at_slot(frame);
        let outcome = run(
            &policy,
            Run {
                list: call.list,
                frame,
                colour: call.colour,
                output: call.output,
                render,
                display,
                game_reset: call.game_reset,
                stage: ColourStage::BeforeTonemap,
                sources,
            },
        );
        if outcome.is_some() {
            COUNTERS.dlaa.fetch_add(1, Ordering::Relaxed);
            return true;
        }
    }
    // The history slot is copied into the main colour afterwards: it must
    // hold this frame's colour at least.
    // SAFETY: the engine's open list on this thread, its live resources.
    match unsafe {
        mhw_render::copy_resource(call.list, call.colour as usize, call.output as usize)
    } {
        Ok(()) => {
            COUNTERS.passed_through.fetch_add(1, Ordering::Relaxed);
            true
        }
        Err(error) => {
            mhw_core::log_changed!(
                Warn,
                "hijack",
                "ss copy",
                format!("{error:?}"),
                "EvalSS: colour not copied into the history slot ({error:?})"
            );
            false
        }
    }
}

impl Run {
    /// The resources by their dev names (`docs/DEV-PROTOCOL.md`, capture),
    /// and the reasons the others are absent ([`crate::slotnames`]).
    #[cfg(feature = "dev")]
    pub(crate) fn named(&self) -> crate::slotnames::Named {
        let path = runtime::policy().input_path;
        let pass = match path {
            InputPath::Pass => (!self.sources.packed_motion.is_null()).then_some((
                self.sources.packed_motion as usize,
                self.sources.depth as usize,
            )),
            // The newest published record, only while its textures live.
            InputPath::Supersampling => mhw_render::last_inputs().and_then(|inputs| {
                let depth = inputs.depth?;
                let raw = inputs.packed_motion;
                (mhw_render::is_alive(raw.resource, raw.serial)
                    && mhw_render::is_alive(depth.resource, depth.serial))
                .then_some((raw.resource, depth.resource))
            }),
        };
        crate::slotnames::slot_names(&crate::slotnames::SlotSources {
            path: Some(path),
            colour: self.colour as usize,
            depth: self.sources.depth as usize,
            mv: self.sources.motion as usize,
            dlss_out: self.output as usize,
            pass,
            ss: live::latest_ss().map(|ss| (ss.motion, ss.depth)),
        })
    }
}
