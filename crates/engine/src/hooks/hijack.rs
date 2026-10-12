//! The game's DLSS 1 path, taken over (`docs/re/ReDlss1Path.md`).
//!
//! The engine already renders small and keeps the UI at full size around a
//! DLSS 1 integration the drivers no longer serve: GetOptimalSettings,
//! CreateFeatures, EvalSS (supersampling at render size) and EvalISP (the
//! upscale into `UpscaleOut`, after which the engine runs the rest of the
//! frame at display size). With [`Params::hijack`](crate::Params):
//!
//! * GetOptimalSettings answers the mod's ratio (snapped like the engine's
//!   setter) and flags 2 or 3, which unhides the path and makes the ratio
//!   the engine's render ratio;
//! * CreateFeatures stores stand-in handles
//!   ([`FAKE_SUPERSAMPLING`](crate::upscale::FAKE_SUPERSAMPLING),
//!   [`FAKE_IMAGE_SIGNAL`](crate::upscale::FAKE_IMAGE_SIGNAL)) without asking the driver; the game passes them
//!   to `NVSDK_NGX_D3D12_ReleaseFeature` later, so whoever owns that entry
//!   must drop them ([`is_stand_in_handle`](crate::is_stand_in_handle));
//! * EvalSS and EvalISP hand their resources to the mod's
//!   [`UpscaleHandler`](crate::UpscaleHandler);
//! * the SS enqueue (once per TAA draw, also with flags 2) latches the
//!   game's reset, clears the TAA sharpen, optionally aliases the depth
//!   copy, and is not passed on unless every target it binds exists;
//! * the Image Quality handler is always given High (the engine's own
//!   scale 1.0, dynamic resolution off): any other position would scale
//!   the picture a second time on top of the render ratio.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use mhw_core::{log_changed, log_every, log_once};

use super::{guard, original};
use crate::layout::{capture, descriptor, native, texture, view};
use crate::object::Object;
use crate::params::Params;
use crate::ratio;
use crate::runtime::{self, COUNTERS, DEPTH_ALIAS, RESET_PENDING};
use crate::sites::field;
use crate::upscale::{
    ImageSignalCall, Rect, SupersamplingCall, FAKE_IMAGE_SIGNAL, FAKE_SUPERSAMPLING,
    STATE_NON_PIXEL_SHADER_RESOURCE, STATE_UNORDERED_ACCESS,
};
use crate::views::{resource_of, Descriptor, DlssContext, RenderConfig, TaaFilter};

pub(crate) static T_OPTIMAL: AtomicUsize = AtomicUsize::new(0);
pub(crate) static T_CREATE: AtomicUsize = AtomicUsize::new(0);
pub(crate) static T_EVAL_SS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static T_EVAL_ISP: AtomicUsize = AtomicUsize::new(0);
pub(crate) static T_SS_ENQUEUE: AtomicUsize = AtomicUsize::new(0);
pub(crate) static T_SCALING_OPTION: AtomicUsize = AtomicUsize::new(0);

type OptimalFn = unsafe extern "system" fn(*mut u32, usize, u32, u32, i32) -> *mut u32;
type CreateFn = unsafe extern "system" fn(usize, u32, u32, i32, u32, usize) -> u32;
type EvaluateFn = unsafe extern "system" fn(usize, usize) -> u32;
type EnqueueFn = unsafe extern "system" fn(usize) -> u64;
type ScalingOptionFn = unsafe extern "system" fn(u8) -> u64;

/// The Image Quality position the game last applied, before substitution
/// (`u8::MAX` = none yet).
pub(crate) static MENU_POSITION: AtomicU8 = AtomicU8::new(u8::MAX);
/// Temporal passes on which `mSharpen` was cleared.
pub(crate) static SHARPEN_CLEARED: AtomicU64 = AtomicU64::new(0);
/// The TAA filter last seen (for introspection).
pub(crate) static LAST_TAA: AtomicUsize = AtomicUsize::new(0);

/// "High": scale 1.0, no dynamic resolution.
const HIGH: u8 = 2;

fn hijacking() -> Option<Params> {
    let params = runtime::params();
    (params.hijack && runtime::installed().is_some()).then_some(params)
}

/// `float* GetOptimalSettings(float out[3], ngx, w, h, mode)`.
pub(crate) unsafe extern "system" fn optimal(
    out: *mut u32,
    ngx: usize,
    width: u32,
    height: u32,
    mode: i32,
) -> *mut u32 {
    let answered = guard(
        "GetOptimalSettings",
        || answer_optimal(out as usize, width, height, mode),
        || false,
    );
    if answered {
        return out;
    }
    // SAFETY: this detour's trampoline, same signature.
    unsafe { original::<OptimalFn>(&T_OPTIMAL)(out, ngx, width, height, mode) }
}

fn answer_optimal(out: usize, width: u32, height: u32, mode: i32) -> bool {
    let Some(params) = hijacking() else {
        return false;
    };
    let Some(out) = Object::writable(out, 12) else {
        return false;
    };
    let scale = ratio::snapped(params.ratio, width);
    let flags = params.flags.bits();
    let written = out.put(0, scale) && out.put(4, flags) && out.put(8, 0.0f32);
    COUNTERS.optimal_answers.bump();
    log_changed!(
        Info,
        "hijack",
        "optimal",
        (width, height, scale.to_bits(), flags),
        "the game asked what DLSS can do at {width}x{height} (mode {mode}): scale {scale:.5} \
         ({}x{}), flags {flags}",
        ratio::render_width(scale, width),
        ratio::render_width(scale, height)
    );
    written
}

/// `u32 CreateFeatures(ngx, outW, outH, mode, _, list)`.
pub(crate) unsafe extern "system" fn create(
    ngx: usize,
    width: u32,
    height: u32,
    mode: i32,
    unused: u32,
    list: usize,
) -> u32 {
    let made = guard("CreateFeatures", || stand_in_features(ngx), || None);
    match made {
        Some(true) => {
            COUNTERS.creates.bump();
            log_every!(
                16,
                Info,
                "hijack",
                "the game created DLSS features for {width}x{height} (mode {mode}); stand-ins \
                 stored"
            );
            1
        }
        Some(false) => {
            log_once!(
                Error,
                "hijack",
                "create",
                "the game's DLSS object at {ngx:#x} is not writable; feature creation fails"
            );
            0
        }
        // SAFETY: this detour's trampoline, same signature.
        None => unsafe { original::<CreateFn>(&T_CREATE)(ngx, width, height, mode, unused, list) },
    }
}

fn stand_in_features(ngx: usize) -> Option<bool> {
    hijacking()?;
    let at = runtime::installed()?;
    if ngx == 0 {
        return None;
    }
    Some(
        DlssContext::at(at, ngx)
            .is_some_and(|ctx| ctx.set_handles(FAKE_SUPERSAMPLING, FAKE_IMAGE_SIGNAL)),
    )
}

fn frame_now() -> u64 {
    runtime::installed()
        .and_then(RenderConfig::get)
        .and_then(|config| config.render_frame())
        .map_or(0, u64::from)
}

/// `u32 EvalSS(descriptor, list)`.
pub(crate) unsafe extern "system" fn eval_supersampling(desc: usize, list: usize) -> u32 {
    match guard("EvalSS", || supersampling(desc, list), || None) {
        Some(()) => 1,
        // SAFETY: this detour's trampoline, same signature.
        None => unsafe { original::<EvaluateFn>(&T_EVAL_SS)(desc, list) },
    }
}

fn supersampling(desc: usize, list: usize) -> Option<()> {
    hijacking()?;
    let at = runtime::installed()?;
    let Some(d) = Descriptor::at(desc, at) else {
        log_once!(
            Warn,
            "hijack",
            "ss descriptor",
            "EvalSS descriptor unreadable"
        );
        return Some(());
    };
    COUNTERS.eval_ss.bump();
    let call = SupersamplingCall {
        list: list as *mut _,
        colour: resource_of(d.pointer(descriptor::COLOUR)),
        motion: resource_of(d.pointer(descriptor::MOTION)),
        depth: resource_of(d.pointer(descriptor::DEPTH)),
        output: resource_of(d.pointer(descriptor::OUTPUT)),
        rect: Rect::from_floats(d.rect(at.field(field::SS_DESC_2C)).unwrap_or_default()),
        game_reset: d.float(descriptor::BLEND).is_some_and(|blend| blend <= 0.0),
        frame: frame_now(),
        input_state: STATE_NON_PIXEL_SHADER_RESOURCE,
        output_state: STATE_UNORDERED_ACCESS,
    };
    match runtime::handler() {
        Some(handler) => {
            handler.supersampling(&call);
        }
        None => unhandled("EvalSS"),
    }
    Some(())
}

fn unhandled(what: &'static str) {
    COUNTERS.unhandled.bump();
    log_once!(
        Warn,
        "hijack",
        what,
        "{what} ran with no upscale handler registered; nothing was evaluated"
    );
}

/// `u32 EvalISP(descriptor, list)`.
pub(crate) unsafe extern "system" fn eval_image_signal(desc: usize, list: usize) -> u32 {
    match guard("EvalISP", || image_signal(desc, list), || None) {
        Some(()) => 1,
        // SAFETY: this detour's trampoline, same signature.
        None => unsafe { original::<EvaluateFn>(&T_EVAL_ISP)(desc, list) },
    }
}

fn image_signal(desc: usize, list: usize) -> Option<()> {
    hijacking()?;
    let at = runtime::installed()?;
    let Some(d) = Descriptor::at(desc, at) else {
        log_once!(
            Warn,
            "hijack",
            "isp descriptor",
            "EvalISP descriptor unreadable"
        );
        return Some(());
    };
    COUNTERS.eval_isp.bump();
    let config = RenderConfig::get(at);
    let call = ImageSignalCall {
        list: list as *mut _,
        colour: resource_of(d.pointer(descriptor::COLOUR)),
        output: resource_of(d.pointer(at.field(field::ISP_DESC_18))),
        rect: Rect::from_floats(d.rect(at.field(field::ISP_DESC_2C)).unwrap_or_default()),
        sharpness: d.float(descriptor::SHARPNESS).unwrap_or(0.0),
        game_reset: RESET_PENDING.swap(false, Ordering::AcqRel),
        frame: config.and_then(|c| c.render_frame()).map_or(0, u64::from),
        display: config.and_then(|c| c.screen_size()).unwrap_or_default(),
        input_state: STATE_NON_PIXEL_SHADER_RESOURCE,
        output_state: STATE_UNORDERED_ACCESS,
    };
    match runtime::handler() {
        Some(handler) => {
            handler.image_signal(&call);
        }
        None => unhandled("EvalISP"),
    }
    Some(())
}

/// What the enqueue detour decided.
enum Enqueue {
    /// Not ours: pass through.
    Game,
    /// Do not record the pass.
    Skip,
    /// Record it.
    Run,
}

/// `u64 SupersamplingEnqueue(capture)`.
pub(crate) unsafe extern "system" fn ss_enqueue(capture: usize) -> u64 {
    COUNTERS.enqueues.bump();
    match guard("SS enqueue", || enqueue(capture), || Enqueue::Game) {
        Enqueue::Skip => {
            COUNTERS.enqueues_skipped.bump();
            0
        }
        // SAFETY: this detour's trampoline, same signature.
        Enqueue::Game | Enqueue::Run => unsafe { original::<EnqueueFn>(&T_SS_ENQUEUE)(capture) },
    }
}

fn enqueue(capture: usize) -> Enqueue {
    let Some(params) = hijacking() else {
        return Enqueue::Game;
    };
    let Some(at) = runtime::installed() else {
        return Enqueue::Game;
    };
    let Some(cap) = Object::readable(capture, capture::SIZE) else {
        return Enqueue::Skip;
    };
    let taa = cap
        .get::<usize>(capture::TAA)
        .and_then(|address| TaaFilter::at(at, address));
    if let Some(taa) = taa {
        LAST_TAA.store(taa.address(), Ordering::Relaxed);
        latch_reset(&cap, &taa);
        if params.taa_sharpen_off && taa.sharpen() == Some(true) && taa.set_sharpen(false) {
            SHARPEN_CLEARED.fetch_add(1, Ordering::Relaxed);
            log_once!(
                Info,
                "tune",
                "taa sharpen",
                "TAA sharpen (mSharpen, amount {:.2}) turned off on {:#x}: it would sharpen \
                 DLSS's input",
                taa.sharpen_amount().unwrap_or(f32::NAN),
                taa.address()
            );
        }
    }
    // With flags 2 there is no SS context: the original would stop at once
    // (`0x142391406`) and the dense velocity was never allocated.
    if !params.flags.supersampling() {
        return Enqueue::Skip;
    }
    if DEPTH_ALIAS.load(Ordering::Acquire) {
        alias_depth(&cap);
    } else {
        log_once!(
            Warn,
            "hijack",
            "no depth alias",
            "flags 3 without the depth alias: the engine's barrier planner reads a null depth \
             view (0x14229e892) unless set_depth_alias(true)"
        );
    }
    let Some(taa) = taa else {
        return Enqueue::Skip;
    };
    let targets = [
        ("colour", taa.colour()),
        ("dense motion", taa.dense_velocity()),
        ("history 0", taa.history(0)),
        ("history 1", taa.history(1)),
        ("history 2", taa.history(2)),
    ];
    if let Some((name, _)) = targets.iter().find(|(_, t)| t.unwrap_or(0) == 0) {
        log_every!(
            600,
            Warn,
            "hijack",
            "the supersampling pass is skipped: its {name} target does not exist"
        );
        return Enqueue::Skip;
    }
    Enqueue::Run
}

/// The game's reset: the TAA draw's copy of `taa+0x185` (`*capture+0x38`),
/// or a blend rate of zero.
fn latch_reset(cap: &Object, taa: &TaaFilter) {
    let invalid = cap
        .follow(capture::RESET, 1, false)
        .and_then(|flag| flag.get::<u8>(0))
        .is_some_and(|flag| flag != 0);
    let blend_zero = taa.blend_rate().is_some_and(|blend| blend <= 0.0);
    if invalid || blend_zero {
        RESET_PENDING.store(true, Ordering::Release);
        let count = COUNTERS.game_resets.bump();
        log_every!(
            64,
            Debug,
            "hijack",
            "the game reset its temporal history (history invalid {invalid}, blend zero \
             {blend_zero}; {count} so far)"
        );
    }
}

/// Alias the depth copy's depth view to its colour view, as the engine's
/// own clone path does (`0x14239c1f4`): the SS barrier planner reads
/// `+0xf0`, which only type-6 textures have. One reference is added for the
/// new slot; the engine releases both slots separately.
fn alias_depth(cap: &Object) {
    let texture = cap
        .follow(capture::VIEW, 8, false)
        .and_then(|view_slot| view_slot.follow(0, view::TARGETS + 8, false))
        .and_then(|scene| scene.follow(view::TARGETS, view::DEPTH_COPY + 8, false))
        .and_then(|targets| targets.follow(view::DEPTH_COPY, texture::SIZE, true));
    let Some(texture) = texture else {
        log_every!(
            600,
            Debug,
            "hijack",
            "depth copy not reachable from the capture"
        );
        return;
    };
    if texture.get::<usize>(texture::DEPTH_NATIVE) != Some(0) {
        return;
    }
    let Some(native) = texture
        .get::<usize>(texture::NATIVE)
        .and_then(|address| Object::writable(address, native::SIZE))
    else {
        return;
    };
    let (Some(count), Some(slot)) = (
        native
            .address_of(native::REFCOUNT, 4)
            .filter(|a| a % 4 == 0),
        texture
            .address_of(texture::DEPTH_NATIVE, 8)
            .filter(|a| a % 8 == 0),
    ) else {
        return;
    };
    // SAFETY: both are aligned, writable fields of live engine objects; the
    // engine itself updates them with locked instructions.
    let (count, slot) = unsafe {
        (
            AtomicU32::from_ptr(count as *mut u32),
            AtomicUsize::from_ptr(slot as *mut usize),
        )
    };
    // A view with no references is being destroyed: leave it.
    if count
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n > 0).then(|| n + 1)
        })
        .is_err()
    {
        return;
    }
    match slot.compare_exchange(0, native.base(), Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {
            let total = COUNTERS.depth_aliases.bump();
            log_changed!(
                Info,
                "hijack",
                "depth alias",
                texture.base(),
                "depth copy {:#x}: depth view aliased to its colour view {:#x} ({total} so far)",
                texture.base(),
                native.base()
            );
        }
        // Someone else filled it: give our reference back.
        Err(_) => release(count, native.base()),
    }
}

/// The engine's release (`0x142554f90`): drop a reference; the last one
/// destroys the object through its vtable slot 0 with argument 1.
fn release(count: &AtomicU32, object: usize) {
    if count.fetch_sub(1, Ordering::AcqRel) != 1 {
        return;
    }
    type Destroy = unsafe extern "system" fn(usize, u32) -> usize;
    // SAFETY: the object's first word is its vtable, whose slot 0 is the
    // scalar deleting destructor the engine's own release calls.
    unsafe {
        let Some(vtable) = mhw_win::mem::read::<usize>(object) else {
            return;
        };
        let Some(destroy) = mhw_win::mem::read::<usize>(vtable).filter(|f| *f != 0) else {
            return;
        };
        std::mem::transmute::<usize, Destroy>(destroy)(object, 1);
    }
}

/// `u64 ImageQualityApply(position)`.
pub(crate) unsafe extern "system" fn scaling_option(position: u8) -> u64 {
    let given = guard("Image Quality", || image_quality(position), || position);
    // SAFETY: this detour's trampoline, same signature.
    unsafe { original::<ScalingOptionFn>(&T_SCALING_OPTION)(given) }
}

fn image_quality(position: u8) -> u8 {
    MENU_POSITION.store(position, Ordering::Relaxed);
    let given = if hijacking().is_some() {
        HIGH
    } else {
        position
    };
    log_changed!(
        Info,
        "hijack",
        "image quality",
        (position, given),
        "the game applied Image Quality position {position} ({}); the engine is given {}",
        match position {
            0 => "Low",
            1 => "Mid",
            2 => "High",
            3 => "Variable, resolution",
            4 => "Variable, performance",
            _ => "unknown",
        },
        if given == HIGH && position != HIGH {
            "High: the render ratio is the only scaling"
        } else {
            "it unchanged"
        }
    );
    given
}
