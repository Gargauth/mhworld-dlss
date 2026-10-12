//! The temporal dispatch (`CSTemporalAA2`): identify its inputs, decode the
//! motion vectors, publish one [`DlssInputs`] record, and let `mod` act.
//!
//! Inputs are read from the bindings of this very dispatch: textures from
//! its descriptor table (they follow the engine's per-frame rotation), the
//! camera from the root CBV its own root signature assigns to
//! `CBViewProjection`. Depth is not bound (the pass binds a 1×1 dummy) and is
//! chosen among the depth targets bound this frame ([`crate::depth`]).
//!
//! Nothing is recorded on the engine's list unless `mod` asked for it
//! ([`configure_pass`] with a decode, or a [`PassHandler`]); then the list's
//! heaps, root signature, pipeline and roots are restored afterwards, always.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::{ID3D12GraphicsCommandList, ID3D12Resource};

use super::liststate::{self, ListState};
use super::record::Guard;
use super::registry::{self, BOARD, DEPTHS, DESCRIPTORS, ROOT_SIGNATURES, STATES, UPLOAD};
use super::{alive, mvdecode, proxylist};
use crate::descriptors::{Slot, ViewKind};
use crate::dxbc::Reflection;
use crate::inputs::{CameraCb, DlssInputs, TextureRef, CAMERA_CB_MAX};
use crate::mvpack::DecodeParams;
use crate::names;
use crate::rootsig::Layout;
use crate::states;
use crate::taa::{self, Input};

/// What to do at the temporal dispatch; `mod` policy.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PassConfig {
    /// Identify and publish the inputs (reads only).
    pub identify: bool,
    /// Also decode the motion vectors on the engine's list (requires
    /// `identify`). `None`: the published record has no decoded vectors.
    pub decode: Option<DecodeParams>,
}

/// What the handler is shown, on the recording thread, inside the dispatch.
pub struct PassContext<'a> {
    /// The engine's open command list (native).
    pub list: &'a ID3D12GraphicsCommandList,
    /// The list as the engine holds it: ReShade's proxy when its `Dispatch`
    /// is on this thread's stack and unwraps to `list`, otherwise
    /// `list.as_raw()`. What NGX must be given when the device it holds is
    /// a ReShade proxy (its heaps would be proxies, which a native list
    /// cannot take).
    pub engine_list: *mut c_void,
    /// The dispatch's thread groups.
    pub groups: [u32; 3],
    /// This dispatch's inputs, if they could be identified (already
    /// published; `seq` set).
    pub inputs: Option<&'a DlssInputs>,
}

impl std::fmt::Debug for PassContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PassContext")
            .field("list", &self.list.as_raw())
            .field("engine_list", &self.engine_list)
            .field("groups", &self.groups)
            .field("inputs", &self.inputs.map(|i| i.seq))
            .finish()
    }
}

/// The handler's decision about the engine's own dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassVerdict {
    /// Let the engine's `CSTemporalAA2` dispatch run.
    RunEngine,
    /// Do not issue it (the handler wrote the pass's output itself).
    Skip,
}

/// Called at every `CSTemporalAA2` dispatch, after the inputs were
/// published, on the engine's recording thread with its list open. It may
/// record on the list (render restores the engine's root state, heaps and
/// pipeline afterwards); it must not close or reset it, and should not
/// block.
pub type PassHandler = fn(&PassContext<'_>) -> PassVerdict;

static CONFIG: RwLock<PassConfig> = RwLock::new(PassConfig {
    identify: false,
    decode: None,
});
static HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Set what happens at the temporal dispatch. Takes effect at the next one.
pub fn configure_pass(config: PassConfig) {
    *CONFIG.write() = config;
}

/// Install (or with `None` remove) the temporal-pass handler.
pub fn set_pass_handler(handler: Option<PassHandler>) {
    HANDLER.store(handler.map_or(0, |f| f as usize), Ordering::Release);
}

fn handler() -> Option<PassHandler> {
    let address = HANDLER.load(Ordering::Acquire);
    // SAFETY: only `set_pass_handler` stores here, from a `PassHandler`.
    (address != 0).then(|| unsafe { std::mem::transmute::<usize, PassHandler>(address) })
}

/// Why a dispatch's inputs were not identified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentifyError {
    /// The bound root signature's layout does not place this input.
    NotInLayout(&'static str),
    /// No bound table held a usable descriptor for this input.
    Unresolved(&'static str, Slot),
    /// The resource was not created while hooked (no description).
    Undescribed(&'static str),
    /// `tGBuffer4` is not `R32_UINT`/`R32_TYPELESS`, so it cannot be
    /// viewed as packed motion vectors.
    MotionFormat(u32),
}

impl std::fmt::Display for IdentifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdentifyError::NotInLayout(name) => {
                write!(f, "{name} is not in the bound root signature")
            }
            IdentifyError::Unresolved(name, slot) => write!(f, "{name} unresolved ({slot:?})"),
            IdentifyError::Undescribed(name) => write!(f, "{name} was created before the hooks"),
            IdentifyError::MotionFormat(format) => {
                write!(f, "tGBuffer4 is {}, not R32_UINT", names::format(*format))
            }
        }
    }
}

static TEMPORAL: AtomicU64 = AtomicU64::new(0);
static FAILED: AtomicU64 = AtomicU64::new(0);
static NO_DEPTH: AtomicU64 = AtomicU64::new(0);
static NO_CAMERA: AtomicU64 = AtomicU64::new(0);
static DECODED: AtomicU64 = AtomicU64::new(0);
static DECODE_FAILED: AtomicU64 = AtomicU64::new(0);
static SKIPPED: AtomicU64 = AtomicU64::new(0);
static LAST_ERROR: Mutex<Option<IdentifyError>> = Mutex::new(None);

/// Counters of the temporal-pass handling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PassStats {
    /// `CSTemporalAA2` dispatches seen.
    pub temporal_dispatches: u64,
    /// Of them, inputs not identified.
    pub identify_failures: u64,
    /// Published without a depth.
    pub without_depth: u64,
    /// Published without a camera buffer.
    pub without_camera: u64,
    /// Motion vector decodes recorded.
    pub decoded: u64,
    /// Decodes that failed.
    pub decode_failures: u64,
    /// Engine dispatches the handler skipped.
    pub skipped: u64,
}

pub(crate) fn stats() -> (PassStats, Option<IdentifyError>) {
    (
        PassStats {
            temporal_dispatches: TEMPORAL.load(Ordering::Relaxed),
            identify_failures: FAILED.load(Ordering::Relaxed),
            without_depth: NO_DEPTH.load(Ordering::Relaxed),
            without_camera: NO_CAMERA.load(Ordering::Relaxed),
            decoded: DECODED.load(Ordering::Relaxed),
            decode_failures: DECODE_FAILED.load(Ordering::Relaxed),
            skipped: SKIPPED.load(Ordering::Relaxed),
        },
        *LAST_ERROR.lock(),
    )
}

/// References held to recently published resources: they are handed on as
/// raw pointers and used by GPU work that runs frames later, and an engine
/// release in between (scene change) left the driver reading freed memory.
static HELD: Mutex<VecDeque<Vec<ID3D12Resource>>> = Mutex::new(VecDeque::new());
/// Publications held; the GPU runs at most two or three frames behind.
const HOLD_FRAMES: usize = 4;

/// Handle one `CSTemporalAA2` dispatch; returns whether to skip the engine's.
///
/// # Safety
/// `list` must be the live, open native list being recorded on this thread.
pub(crate) unsafe fn temporal_dispatch(
    list: *mut c_void,
    groups: [u32; 3],
    reflection: &Arc<Reflection>,
) -> bool {
    TEMPORAL.fetch_add(1, Ordering::Relaxed);
    let config = *CONFIG.read();
    let handler = handler();
    let mut skip = false;
    if config.identify || handler.is_some() {
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: forwarded contract.
            unsafe { handle(list, groups, reflection, &config, handler) }
        }));
        skip = outcome.unwrap_or_else(|_| {
            mhw_core::log_once!(
                Error,
                "taa",
                "pass-panic",
                "the temporal pass handling panicked"
            );
            false
        });
    }
    // Advanced after handling: depth is accepted when bound in the frame
    // being drawn, and advancing first compared against the next frame.
    registry::FRAME.fetch_add(1, Ordering::Relaxed);
    DESCRIPTORS.tick();
    if skip {
        SKIPPED.fetch_add(1, Ordering::Relaxed);
    }
    skip
}

/// # Safety
/// As [`temporal_dispatch`].
unsafe fn handle(
    list: *mut c_void,
    groups: [u32; 3],
    reflection: &Arc<Reflection>,
    config: &PassConfig,
    handler: Option<PassHandler>,
) -> bool {
    // SAFETY: caller guarantees a live list.
    let Some(graphics) = (unsafe { ID3D12GraphicsCommandList::from_raw_borrowed(&list) }) else {
        return false;
    };
    let saved = liststate::with(list, |state| state.clone());
    let guard = Guard::enter();
    let mut recorded = false;
    let inputs = if config.identify {
        match identify(list, &saved, groups, reflection) {
            Ok(mut inputs) => {
                if let Some(params) = &config.decode {
                    recorded = true;
                    // SAFETY: the open list on this thread; the source was
                    // resolved alive just now.
                    inputs.motion = unsafe { decoded(graphics, &inputs, params) };
                }
                hold(&inputs);
                let seq = BOARD.lock().publish(inputs.clone());
                inputs.seq = seq;
                Some(inputs)
            }
            Err(error) => {
                FAILED.fetch_add(1, Ordering::Relaxed);
                mhw_core::log_changed!(
                    Warn,
                    "inputs",
                    "identify",
                    error,
                    "temporal pass inputs not identified: {error}"
                );
                *LAST_ERROR.lock() = Some(error);
                None
            }
        }
    } else {
        None
    };
    let mut skip = false;
    if let Some(handler) = handler {
        recorded = true;
        // SAFETY: the live native list (caller's contract).
        let engine_list = unsafe { proxylist::engine_list(list) };
        // Before the handler: NGX's own heaps, set through the proxy,
        // would overwrite the record (see `record_on`).
        let proxy_heaps = if engine_list != list {
            proxylist::held_heaps(engine_list)
        } else {
            None
        };
        let verdict = handler(&PassContext {
            list: graphics,
            engine_list,
            groups,
            inputs: inputs.as_ref(),
        });
        if let Some(heaps) = proxy_heaps {
            // SAFETY: the proxy whose `Dispatch` is on this thread's stack
            // over this open list.
            unsafe { proxylist::set_heaps_through(engine_list, heaps) };
        }
        skip = verdict == PassVerdict::Skip;
    }
    if recorded {
        // Unconditionally once anything may have been recorded: the decode
        // binds our heap, signature and pipeline, and a bail-out after it
        // left the engine's dispatch running as our shader.
        // SAFETY: the snapshot of this very list, on it.
        unsafe { liststate::restore(list, &saved) };
    }
    drop(guard);
    skip
}

/// The decode, as a texture reference; failures counted and logged once
/// per kind.
///
/// # Safety
/// As [`mvdecode::decode`].
unsafe fn decoded(
    list: &ID3D12GraphicsCommandList,
    inputs: &DlssInputs,
    params: &DecodeParams,
) -> Option<TextureRef> {
    // SAFETY: forwarded.
    match unsafe { mvdecode::decode(list, inputs.packed_motion.resource, inputs.render, params) } {
        Ok(decoded) => {
            DECODED.fetch_add(1, Ordering::Relaxed);
            Some(TextureRef {
                resource: decoded.resource,
                serial: decoded.serial,
                format: names::R16G16_FLOAT,
                size: inputs.render,
                state: Some(states::NON_PIXEL_SHADER_RESOURCE),
            })
        }
        Err(error) => {
            DECODE_FAILED.fetch_add(1, Ordering::Relaxed);
            mhw_core::log_once!(Warn, "inputs", error.to_string(), "{error}");
            None
        }
    }
}

/// Keep references to a record's resources for a few publications.
fn hold(inputs: &DlssInputs) {
    let held: Vec<ID3D12Resource> = inputs
        .textures()
        .filter_map(|texture| {
            let raw = texture.resource as *mut c_void;
            // SAFETY: each was resolved alive (witness and serial) during
            // this dispatch; AddRef keeps it so.
            unsafe { ID3D12Resource::from_raw_borrowed(&raw) }.cloned()
        })
        .collect();
    let mut frames = HELD.lock();
    frames.push_back(held);
    while frames.len() > HOLD_FRAMES {
        frames.pop_front();
    }
}

fn identify(
    list: *mut c_void,
    saved: &ListState,
    groups: [u32; 3],
    reflection: &Reflection,
) -> Result<DlssInputs, IdentifyError> {
    let layout: Option<Arc<Layout>> = ROOT_SIGNATURES.lock().get(&saved.signature).cloned();
    let layout = layout.as_deref();
    let colour = texture(list, saved, layout, reflection, taa::COLOUR, ViewKind::Srv)?;
    let packed_motion = texture(list, saved, layout, reflection, taa::MOTION, ViewKind::Srv)?;
    let output = texture(list, saved, layout, reflection, taa::OUTPUT, ViewKind::Uav)?;
    if packed_motion.format != names::R32_UINT && packed_motion.format != names::R32_TYPELESS {
        return Err(IdentifyError::MotionFormat(packed_motion.format));
    }
    let render = taa::render_size(groups, output.size);
    let frame = registry::frame();
    let pick = DEPTHS
        .lock()
        .pick(frame, render, output.size, alive::is_alive);
    let depth = pick.map(|pick| TextureRef {
        resource: pick.resource,
        serial: pick.serial,
        format: registry::describe(pick.resource).map_or(0, |info| info.format),
        size: pick.size,
        state: STATES.lock().known(list as usize, pick.resource),
    });
    if depth.is_none() {
        NO_DEPTH.fetch_add(1, Ordering::Relaxed);
    }
    if pick.is_some_and(|pick| pick.candidates > 1) {
        mhw_core::log_once!(
            Info,
            "inputs",
            "many-depths",
            "{} depth targets match {}x{}; the most recently bound is used",
            pick.map_or(0, |p| p.candidates),
            render.w,
            render.h
        );
    }
    let camera = camera(saved, layout, reflection);
    if camera.is_none() {
        NO_CAMERA.fetch_add(1, Ordering::Relaxed);
    }
    Ok(DlssInputs {
        seq: 0,
        list: list as usize,
        groups,
        render,
        colour,
        packed_motion,
        motion: None,
        output,
        depth,
        depth_match: pick.map(|pick| pick.matched),
        camera,
    })
}

/// Resolve one input to a described, live texture.
fn texture(
    list: *mut c_void,
    saved: &ListState,
    layout: Option<&Layout>,
    reflection: &Reflection,
    input: Input,
    view: ViewKind,
) -> Result<TextureRef, IdentifyError> {
    let slot =
        taa::locate(layout, reflection, input).ok_or(IdentifyError::NotInLayout(input.name))?;
    let resolve = |base: u64| DESCRIPTORS.resolve(base, slot.offset, alive::is_alive);
    let mut last = Slot::Missing;
    let named = match slot.root {
        Some(root) => match saved.table(root).map(resolve) {
            Some(Slot::Named(named)) => Some(named),
            Some(other) => {
                last = other;
                None
            }
            None => None,
        },
        // No layout: the measured offset, in whichever bound table holds a
        // view of the right kind there.
        None => saved
            .tables
            .iter()
            .find_map(|(_, base)| match resolve(*base) {
                Slot::Named(named) if named.view == view => Some(named),
                other => {
                    last = other;
                    None
                }
            }),
    };
    let named = named
        .filter(|named| named.view == view)
        .ok_or(IdentifyError::Unresolved(input.name, last))?;
    let info = registry::describe(named.resource).ok_or(IdentifyError::Undescribed(input.name))?;
    Ok(TextureRef {
        resource: named.resource,
        serial: named.serial,
        format: info.format,
        size: info.size(),
        state: STATES.lock().known(list as usize, named.resource),
    })
}

/// The camera buffer this dispatch bound, copied out of its mapping.
fn camera(saved: &ListState, layout: Option<&Layout>, reflection: &Reflection) -> Option<CameraCb> {
    let register = taa::camera_register(reflection);
    let root = layout?.root_cbv(register, 0)?;
    let address = saved.cbv(root)?;
    let declared = reflection.buffer_at(register);
    let len = declared
        .map_or(CAMERA_CB_MAX, |cb| cb.size as usize)
        .min(CAMERA_CB_MAX);
    let mut bytes = [0u8; CAMERA_CB_MAX];
    let region = {
        let upload = UPLOAD.lock();
        let cpu = upload.resolve(address, len)?;
        (cpu, upload.region_of(address))
    };
    // A buffer released while still mapped leaves its entry pointing at
    // memory the runtime took back: asked before reading, and dropped.
    // SAFETY: `read_bytes` checks the range is committed and readable.
    if !unsafe { mhw_win::mem::read_bytes(region.0, &mut bytes[..len]) } {
        if let Some(stale) = region.1 {
            UPLOAD.lock().drop_region(stale.gpu, stale.cpu);
        }
        mhw_core::log_once!(
            Warn,
            "inputs",
            "camera-unmapped",
            "the camera buffer's mapping is gone (released while mapped); dropped"
        );
        return None;
    }
    Some(CameraCb::new(
        root,
        address,
        declared.cloned().map(Arc::new),
        &bytes[..len],
    ))
}

/// The newest published inputs, taken or not.
pub fn last_inputs() -> Option<DlssInputs> {
    BOARD.lock().latest().cloned()
}

/// Take this frame's inputs for work recorded on `list` (a ReShade proxy is
/// unwrapped): the newest record not yet taken, preferring one identified
/// on the same list. Each record is handed out once and never one older
/// than the last taken. Check each texture's liveness ([`is_alive`]) before
/// use if the record may be old.
///
/// [`is_alive`]: super::alive::is_alive
///
/// # Safety
/// `list` must be null or a live COM object.
pub unsafe fn take_inputs(list: *mut c_void) -> Option<(DlssInputs, crate::Association)> {
    // SAFETY: caller's contract.
    let native = unsafe { super::reshade::unwrap_reshade(list) };
    BOARD.lock().take(native as usize)
}
