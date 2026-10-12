//! One DLSS evaluation on an engine command list: states, NGX, RCAS, and
//! the references held until the GPU is done.

use std::ffi::c_void;

use mhw_core::frame::Size;
use mhw_core::math::Vec2;
use mhw_ngx::feature::{self, DlssInputs};
use mhw_ngx::{EvalParams, FeatureDesc, Subrects};
use mhw_render::states::{NON_PIXEL_SHADER_RESOURCE, UNORDERED_ACCESS};
use mhw_render::{sharpen, Transitions};
use parking_lot::Mutex;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::{ID3D12GraphicsCommandList, ID3D12Resource};

use crate::frames::KeepRing;

/// A reference the mod holds on a resource for the GPU.
struct Held(#[allow(dead_code)] ID3D12Resource);

// SAFETY: D3D12 resources are free-threaded; the reference is only kept
// and released.
unsafe impl Send for Held {}

static KEEP: Mutex<KeepRing<Held>> = Mutex::new(KeepRing::new());

/// What one evaluation reads and writes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Evaluation {
    /// The engine's open command list, as the engine holds it (a ReShade
    /// proxy is passed to NGX as is: NGX was initialised with the game's
    /// device as the game holds it).
    pub list: *mut c_void,
    /// Engine render frame.
    pub frame: u64,
    /// Render-size colour.
    pub colour: *mut c_void,
    /// Motion vectors.
    pub motion: *mut c_void,
    /// Depth.
    pub depth: *mut c_void,
    /// Display-size target, left in `UNORDERED_ACCESS`.
    pub output: *mut c_void,
    /// The feature to evaluate.
    pub desc: FeatureDesc,
    /// `MV.Scale`.
    pub mv_scale: Vec2,
    /// Jitter, NVIDIA convention.
    pub jitter: Vec2,
    /// Discard history.
    pub reset: bool,
    /// RCAS after DLSS (0 = off).
    pub sharpness: f32,
}

/// Why nothing was evaluated.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Failure {
    /// A resource state on the list is not known.
    UnknownState,
    /// The list is not a graphics command list, or a resource is null.
    NotUsable(&'static str),
    /// NGX did not evaluate.
    Ngx(feature::EvalError),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::UnknownState => f.write_str("a resource state on the list is unknown"),
            Failure::NotUsable(what) => write!(f, "{what} not usable"),
            Failure::Ngx(error) => write!(f, "NGX: {error:?}"),
        }
    }
}

/// What an evaluation did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Done {
    /// The feature was (re)created.
    pub created: bool,
    /// RCAS ran.
    pub sharpened: bool,
}

/// # Safety
/// `resource` is null or a live `ID3D12Resource`.
unsafe fn borrowed(resource: &*mut c_void) -> Option<&ID3D12Resource> {
    // SAFETY: caller's contract; null gives `None`.
    unsafe { ID3D12Resource::from_raw_borrowed(resource) }
}

/// Evaluate DLSS as `e` says, on `e.list`.
///
/// # Safety
/// `e.list` is the engine's open command list recorded on this thread;
/// every resource is alive.
pub(crate) unsafe fn evaluate(e: &Evaluation) -> Result<Done, Failure> {
    // SAFETY: caller's contract.
    let (Some(colour), Some(motion), Some(depth), Some(output)) = (unsafe {
        (
            borrowed(&e.colour),
            borrowed(&e.motion),
            borrowed(&e.depth),
            borrowed(&e.output),
        )
    }) else {
        return Err(Failure::NotUsable("an input"));
    };
    // SAFETY: caller's contract.
    let game_list = unsafe { ID3D12GraphicsCommandList::from_raw_borrowed(&e.list) }
        .ok_or(Failure::NotUsable("the command list"))?;
    let wanted = [
        (e.colour as usize, NON_PIXEL_SHADER_RESOURCE),
        (e.motion as usize, NON_PIXEL_SHADER_RESOURCE),
        (e.depth as usize, NON_PIXEL_SHADER_RESOURCE),
        (e.output as usize, UNORDERED_ACCESS),
    ];
    // SAFETY: caller's contract.
    let plan = unsafe { Transitions::plan(e.list, &wanted) }.ok_or(Failure::UnknownState)?;
    let intermediate = intermediate_for(e);
    let target = intermediate
        .as_ref()
        // SAFETY: render keeps the intermediate alive while handed out.
        .and_then(|i| unsafe { borrowed(&i.resource) }.cloned())
        .unwrap_or_else(|| output.clone());
    let inputs = DlssInputs {
        colour,
        motion,
        depth,
        output: &target,
        exposure: None,
    };
    let params = EvalParams {
        jitter: e.jitter,
        mv_scale: e.mv_scale,
        subrects: Subrects::full(e.desc.render),
        reset: e.reset,
    };
    // SAFETY: caller's contract; `record_on` restores the engine's state
    // and unbinds the depth target for the transitions.
    let outcome = unsafe {
        mhw_render::record_on(e.list, Some(e.depth as usize), |native| {
            plan.forward(native);
            let evaluated = feature::evaluate(game_list, &e.desc, &inputs, &params, e.frame);
            let mut sharpened = false;
            if let (Ok(_), Some(i)) = (&evaluated, intermediate.as_ref()) {
                match sharpen::record(
                    e.list,
                    i.resource,
                    i.state,
                    e.output,
                    UNORDERED_ACCESS,
                    e.sharpness,
                ) {
                    Ok(()) => sharpened = true,
                    Err(error) => mhw_core::log_changed!(
                        Warn,
                        "dlss",
                        "rcas",
                        error.to_string(),
                        "RCAS: {error}"
                    ),
                }
            }
            plan.back(native);
            (evaluated, sharpened)
        })
    }
    .ok_or(Failure::NotUsable("the command list"))?;
    hold(e.frame, [colour, motion, depth, output]);
    feature::tick(e.frame);
    match outcome {
        (Ok(evaluated), sharpened) => Ok(Done {
            created: evaluated.created,
            sharpened,
        }),
        (Err(error), _) => Err(Failure::Ngx(error)),
    }
}

/// The RCAS intermediate DLSS writes instead of the target, when
/// sharpening is on and possible.
fn intermediate_for(e: &Evaluation) -> Option<sharpen::Intermediate> {
    if e.sharpness.is_nan() || e.sharpness <= 0.0 {
        return None;
    }
    let format = mhw_render::describe(e.output as usize)?.format;
    match sharpen::intermediate(e.desc.display, format) {
        Ok(intermediate) => Some(intermediate),
        Err(error) => {
            mhw_core::log_changed!(
                Info,
                "dlss",
                "rcas intermediate",
                error.to_string(),
                "not sharpening: {error}"
            );
            None
        }
    }
}

/// Keep references to `resources` for the next few frames.
pub(crate) fn hold<'a>(frame: u64, resources: impl IntoIterator<Item = &'a ID3D12Resource>) {
    KEEP.lock()
        .hold(frame, resources.into_iter().map(|r| Held(r.clone())));
}

/// The size of a resource as render tracks it.
pub(crate) fn size_of(resource: usize) -> Option<Size> {
    let info = mhw_render::describe(resource)?;
    Some(Size::new(u32::try_from(info.width).ok()?, info.height))
}
