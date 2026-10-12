//! The DLSS-SR feature: created on demand, recreated when its description
//! changes, evaluated on the caller's command list, and released through a
//! 16-frame retire queue.
//!
//! # Threads and timing
//!
//! [`evaluate`], [`tick`], [`release`] and [`retire_resource`] are called by
//! `mod` on the render thread that records the engine's command list (the
//! upscale slot); the snapshot functions may be called from any thread. One
//! lock serialises them; NGX is called with it held, which is what keeps
//! creation, evaluation and release of one feature ordered.
//!
//! `frame` is the renderer's frame number (`mhw_core::frame::FrameInfo`);
//! only distinct values advance the retire queue, and `mod` should call
//! [`tick`] every frame so retired items age while DLSS is not evaluated.

use std::ffi::c_void;

use mhw_win::Module;
use parking_lot::Mutex;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::{ID3D12GraphicsCommandList, ID3D12Resource};

use crate::budget::{Attempt, CreateBudget};
use crate::dlss::{
    eval_writes, EvalParams, FeatureDesc, ParamError, RawInputs, FEATURE_SUPER_SAMPLING,
};
use crate::init::SNIPPET;
use crate::param::{names, Parameters};
use crate::retire::{RetireQueue, RETIRE_FRAMES};
use crate::session::{self, Api};
use crate::status::Status;
use crate::version::{image_version, VersionInfo};
use crate::{Hex, TARGET_DLSS as TARGET};

/// The resources of one evaluation, borrowed for the call. NGX records
/// reads/writes of them into the command list: they must stay alive until
/// that list has executed (retire them with [`retire_resource`] when
/// replacing one).
#[derive(Clone, Copy, Debug)]
pub struct DlssInputs<'a> {
    /// Render-resolution colour, `NON_PIXEL_SHADER_RESOURCE`.
    pub colour: &'a ID3D12Resource,
    /// Motion vectors, `NON_PIXEL_SHADER_RESOURCE`.
    pub motion: &'a ID3D12Resource,
    /// Depth, `NON_PIXEL_SHADER_RESOURCE`.
    pub depth: &'a ID3D12Resource,
    /// Output-resolution target, `UNORDERED_ACCESS`.
    pub output: &'a ID3D12Resource,
    /// 1×1 exposure texture; `None` with `AutoExposure` (or exposure 1).
    pub exposure: Option<&'a ID3D12Resource>,
}

impl DlssInputs<'_> {
    /// The raw addresses (no references taken).
    pub fn raw(&self) -> RawInputs {
        RawInputs {
            colour: self.colour.as_raw() as usize,
            motion: self.motion.as_raw() as usize,
            depth: self.depth.as_raw() as usize,
            output: self.output.as_raw() as usize,
            exposure: self.exposure.map_or(0, |e| e.as_raw() as usize),
        }
    }
}

/// Why nothing was evaluated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvalError {
    /// The NGX session is not ready ([`session::ready`]).
    NotReady,
    /// The description or the evaluation is invalid.
    Invalid(ParamError),
    /// Creation already failed this frame; tried again next frame.
    Waiting,
    /// Creation failed [`crate::budget::MAX_CREATE_ATTEMPTS`] times for this
    /// description; nothing is tried until it changes or [`retry_creation`].
    GaveUp {
        /// The last failure.
        last: Option<Status>,
    },
    /// `AllocateParameters` or `CreateFeature` failed.
    CreateFailed(Status),
    /// The parameter object does not read back what is written through the
    /// assumed slots; nothing is set through it (permanent).
    SlotCheckFailed,
    /// `EvaluateFeature` failed.
    Evaluate(Status),
}

/// A successful evaluation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Evaluated {
    /// A feature was (re)created for it: there was no history.
    pub created: bool,
}

/// The feature that exists now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveFeature {
    /// What it was created with.
    pub desc: FeatureDesc,
    /// `NVSDK_NGX_Handle*`.
    pub handle: usize,
    /// The frame it was created in.
    pub created_frame: u64,
}

/// A loaded DLSS library.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnippetInfo {
    /// Full path.
    pub path: String,
    /// Its version resource.
    pub version: Option<VersionInfo>,
}

/// Something waiting out [`RETIRE_FRAMES`].
#[derive(Debug)]
enum Retired {
    Feature {
        handle: usize,
        parameters: Parameters,
    },
    Resource(ID3D12Resource),
}

#[derive(Debug)]
struct Manager {
    live: Option<(LiveFeature, Parameters)>,
    retire: RetireQueue<Retired>,
    budget: CreateBudget,
    slot_check_failed: bool,
    creations: u64,
    evaluations: u64,
    eval_failures: u64,
    snippets: Vec<SnippetInfo>,
    #[cfg(feature = "dev")]
    last: Option<LastEvaluate>,
}

static MANAGER: Mutex<Manager> = Mutex::new(Manager {
    live: None,
    retire: RetireQueue::new(RETIRE_FRAMES),
    budget: CreateBudget::new(),
    slot_check_failed: false,
    creations: 0,
    evaluations: 0,
    eval_failures: 0,
    snippets: Vec::new(),
    #[cfg(feature = "dev")]
    last: None,
});

/// The bound entry points while NGX may be called: none after `Shutdown`,
/// and none once a foreign host has been given up on (its features are then
/// abandoned rather than released through it).
fn usable_api() -> Option<Api> {
    session::api().filter(|_| session::ready())
}

/// Release one retired item now.
fn free(api: Option<&Api>, item: Retired) {
    match item {
        Retired::Feature { handle, parameters } => {
            // Without an API NGX has been shut down (taking the feature with
            // it) or must not be called; nothing is left to do.
            if let Some(api) = api {
                // SAFETY: a handle and parameter object this crate created
                // through `api`, retired past every command list using them.
                let status = unsafe {
                    let status = Status((api.release)(handle as *mut c_void));
                    (api.destroy)(parameters.as_raw());
                    status
                };
                log::debug!(target: TARGET, handle:% = Hex(handle), status:%; "feature released");
            }
        }
        Retired::Resource(resource) => drop(resource),
    }
}

impl Manager {
    fn advance(&mut self, api: Option<&Api>, frame: u64) {
        for item in self.retire.advance(frame) {
            free(api, item);
        }
    }

    fn retire_live(&mut self, frame: u64) {
        if let Some((live, parameters)) = self.live.take() {
            log::info!(target: TARGET, handle:% = Hex(live.handle), frame; "feature retired");
            self.retire.retire(
                Retired::Feature {
                    handle: live.handle,
                    parameters,
                },
                frame,
            );
        }
    }

    /// Create a feature for `desc` on `list`.
    ///
    /// # Safety
    /// `list` is open for recording on the device NGX was initialised with.
    unsafe fn create(
        &mut self,
        api: &Api,
        list: *mut c_void,
        desc: &FeatureDesc,
        frame: u64,
    ) -> Result<(), EvalError> {
        let tick = self.retire.clock().ticks();
        match self.budget.check(desc, tick) {
            Attempt::Go => {}
            Attempt::Wait => return Err(EvalError::Waiting),
            Attempt::GaveUp => {
                return Err(EvalError::GaveUp {
                    last: self.budget.last_failure(),
                })
            }
        }
        let mut raw = std::ptr::null_mut();
        // SAFETY: `allocate` writes a new parameter object into `raw`.
        let status = Status(unsafe { (api.allocate)(&mut raw) });
        // SAFETY: on success `raw` is a live parameter object until destroyed.
        let parameters = match unsafe { Parameters::from_raw(raw) } {
            Some(parameters) if status.is_success() => parameters,
            _ => {
                self.budget.failed(tick, status);
                log::warn!(target: TARGET, status:%; "AllocateParameters failed");
                return Err(EvalError::CreateFailed(status));
            }
        };
        // SAFETY: `parameters` is live.
        let check = unsafe { parameters.verify_slots() };
        if !check.held() {
            // SAFETY: allocated above, not used by anything.
            unsafe { (api.destroy)(parameters.as_raw()) };
            self.slot_check_failed = true;
            log::error!(target: TARGET, int:? = check.int, float:? = check.float;
                "NGX's parameter object does not read back through the assumed vtable slots; \
                 DLSS is off for this session");
            if session::foreign_host() {
                session::give_up_on_foreign_host(
                    "its parameter object does not follow the NVSDK_NGX_Parameter layout",
                );
            }
            return Err(EvalError::SlotCheckFailed);
        }
        let mut handle = std::ptr::null_mut();
        // SAFETY: live parameter object; the writes are typed by the SDK
        // header; `list` per the contract.
        let status = Status(unsafe {
            parameters.apply(&desc.create_writes());
            (api.create)(
                list,
                FEATURE_SUPER_SAMPLING,
                parameters.as_raw(),
                &mut handle,
            )
        });
        log::info!(target: TARGET,
            render:? = desc.render, display:? = desc.display, quality:? = desc.quality,
            preset:? = desc.preset, flags:% = Hex(desc.flags.bits() as usize),
            output_subrects = desc.output_subrects, status:%, frame;
            "CreateFeature");
        if !status.is_success() || handle.is_null() {
            // SAFETY: allocated above; the failed creation did not keep it.
            unsafe { (api.destroy)(parameters.as_raw()) };
            self.budget.failed(tick, status);
            if session::foreign_host() {
                // Another module is asked once.
                session::give_up_on_foreign_host(&format!("its CreateFeature returned {status}"));
            }
            return Err(EvalError::CreateFailed(status));
        }
        self.budget.succeeded();
        self.creations += 1;
        if session::foreign_host() {
            // SAFETY: live handle (`NVSDK_NGX_Handle { unsigned Id }`) and
            // parameter object; both read only.
            let (id, marker) = unsafe {
                (
                    mhw_win::mem::read::<u32>(handle as usize),
                    parameters.get_i32(names::OPTISCALER_MARKER),
                )
            };
            mhw_core::log_once!(Info, TARGET, "foreign-feature", id:?, optiscaler_marker:? = marker;
                "the foreign NGX host created the feature");
        }
        self.live = Some((
            LiveFeature {
                desc: *desc,
                handle: handle as usize,
                created_frame: frame,
            },
            parameters,
        ));
        self.snippets = loaded_snippets();
        for snippet in &self.snippets {
            log::info!(target: TARGET, path = snippet.path.as_str(),
                version:? = snippet.version.map(|v| v.file.to_string());
                "DLSS library loaded");
        }
        Ok(())
    }
}

/// Every loaded `nvngx_dlss.dll` with its version.
fn loaded_snippets() -> Vec<SnippetInfo> {
    mhw_win::loaded_modules()
        .into_iter()
        .filter(|module| module.name.eq_ignore_ascii_case(SNIPPET))
        .map(|module| SnippetInfo {
            version: Module::at(module.base)
                .ok()
                .and_then(|image| image_version(&image, image.headers())),
            path: module.path,
        })
        .collect()
}

/// Create the feature for `desc` if it does not exist (or exists for another
/// description, which is retired), then evaluate it on `list`.
///
/// # Safety
/// `list` is open for recording, belongs to the same side of any proxy
/// layer as the device NGX was initialised with (the game's device as the
/// game holds it: with ReShade, the proxy list), and is executed after this
/// returns. Every resource is in the state [`DlssInputs`] documents and
/// stays alive until the list has executed.
pub unsafe fn evaluate(
    list: &ID3D12GraphicsCommandList,
    desc: &FeatureDesc,
    inputs: &DlssInputs<'_>,
    params: &EvalParams,
    frame: u64,
) -> Result<Evaluated, EvalError> {
    let api = usable_api();
    let mut manager = MANAGER.lock();
    manager.advance(api.as_ref(), frame);
    let Some(api) = api else {
        return Err(EvalError::NotReady);
    };
    if manager.slot_check_failed {
        return Err(EvalError::SlotCheckFailed);
    }
    let raw = inputs.raw();
    desc.validate().map_err(EvalError::Invalid)?;
    params.validate(desc, &raw).map_err(EvalError::Invalid)?;

    let list = list.as_raw();
    if manager.live.is_some_and(|(live, _)| live.desc != *desc) {
        manager.retire_live(frame);
    }
    let created = manager.live.is_none();
    if created {
        // SAFETY: forwarded.
        unsafe { manager.create(&api, list, desc, frame) }?;
    }
    let Some((live, parameters)) = manager.live else {
        return Err(EvalError::NotReady);
    };
    // SAFETY: live parameter object and feature; the resources and list per
    // the contract.
    let status = Status(unsafe {
        parameters.apply(&eval_writes(&raw, params));
        (api.evaluate)(
            list,
            live.handle as *const c_void,
            parameters.as_raw(),
            std::ptr::null(),
        )
    });
    manager.evaluations += 1;
    #[cfg(feature = "dev")]
    {
        manager.last = Some(LastEvaluate {
            frame,
            desc: *desc,
            inputs: raw,
            params: *params,
            status,
        });
    }
    if !status.is_success() {
        manager.eval_failures += 1;
        mhw_core::log_every!(120, Warn, TARGET, status:%, frame; "EvaluateFeature failed");
        return Err(EvalError::Evaluate(status));
    }
    mhw_core::log_once!(Info, TARGET, "first-evaluate", frame; "first DLSS evaluation");
    Ok(Evaluated { created })
}

/// Advance the retire queue to `frame` and release whatever is due. Call
/// every frame.
pub fn tick(frame: u64) {
    let api = usable_api();
    MANAGER.lock().advance(api.as_ref(), frame);
}

/// Retire the feature (DLSS turned off, or the session about to change);
/// it is released [`RETIRE_FRAMES`] distinct frames after `frame`.
pub fn release(frame: u64) {
    MANAGER.lock().retire_live(frame);
}

/// Keep `resource` alive for [`RETIRE_FRAMES`] distinct frames after
/// `frame` (an input replaced while a list using it may be in flight).
pub fn retire_resource(resource: ID3D12Resource, frame: u64) {
    MANAGER
        .lock()
        .retire
        .retire(Retired::Resource(resource), frame);
}

/// Allow creation again after giving up (e.g. a new DLSS library is in place).
pub fn retry_creation() {
    MANAGER.lock().budget.retry();
}

/// Release the feature and everything retired at once: NGX is shutting down
/// and the GPU work using them has been waited for by the game.
pub(crate) fn release_all_now() {
    let api = usable_api();
    let mut manager = MANAGER.lock();
    let live = manager.live.take();
    let mut items = manager.retire.drain_all();
    if let Some((live, parameters)) = live {
        items.push(Retired::Feature {
            handle: live.handle,
            parameters,
        });
    }
    for item in items {
        free(api.as_ref(), item);
    }
}

/// The feature that exists now.
pub fn live() -> Option<LiveFeature> {
    MANAGER.lock().live.map(|(live, _)| live)
}

/// The DLSS libraries loaded when the feature was last created.
pub fn snippets() -> Vec<SnippetInfo> {
    MANAGER.lock().snippets.clone()
}

/// The last evaluation, for the dev server.
#[cfg(feature = "dev")]
#[derive(Clone, Copy, Debug)]
pub struct LastEvaluate {
    /// Frame.
    pub frame: u64,
    /// The feature description it ran on.
    pub desc: FeatureDesc,
    /// Its resources.
    pub inputs: RawInputs,
    /// Its parameters.
    pub params: EvalParams,
    /// What `EvaluateFeature` returned.
    pub status: Status,
}

/// Feature state for the dev server.
#[cfg(feature = "dev")]
#[derive(Clone, Debug)]
pub struct FeatureSnapshot {
    /// The feature that exists now.
    pub live: Option<LiveFeature>,
    /// Items in the retire queue.
    pub retiring: usize,
    /// Consecutive failed creations of the current description.
    pub create_failures: u32,
    /// The last creation failure.
    pub last_create_failure: Option<Status>,
    /// The slot check failed (permanent).
    pub slot_check_failed: bool,
    /// Features created.
    pub creations: u64,
    /// Evaluations made.
    pub evaluations: u64,
    /// Evaluations that failed.
    pub eval_failures: u64,
    /// The last evaluation.
    pub last: Option<LastEvaluate>,
    /// Loaded DLSS libraries and their versions.
    pub snippets: Vec<SnippetInfo>,
}

/// A snapshot of the feature state.
#[cfg(feature = "dev")]
pub fn snapshot() -> FeatureSnapshot {
    let manager = MANAGER.lock();
    FeatureSnapshot {
        live: manager.live.map(|(live, _)| live),
        retiring: manager.retire.len(),
        create_failures: manager.budget.failures(),
        last_create_failure: manager.budget.last_failure(),
        slot_check_failed: manager.slot_check_failed,
        creations: manager.creations,
        evaluations: manager.evaluations,
        eval_failures: manager.eval_failures,
        last: manager.last,
        snippets: manager.snippets.clone(),
    }
}
