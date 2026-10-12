//! HUD-less capture and conversion (Windows side of [`crate::hudless`]).
//!
//! `OMSetRenderTargets` and the draw hooks feed the [`Detector`] while
//! [`enable`]d (one relaxed load per call otherwise). At the capture moment
//! the scene target is copied into the converter's `hold` texture on the
//! engine's list (its tracked state moved to `COPY_SOURCE` and back), the
//! conversion is recorded on one of our own lists, and that list is executed
//! on the same queue right behind the engine's list. [`take`] hands out the
//! converted image of a frame only after that frame's encode confirmed the
//! capture.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use mhw_core::frame::Size;
use parking_lot::Mutex;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::ID3D12GraphicsCommandList;

pub use super::convert::Encoder;
use super::convert::{self, Converter, Key, OUTPUT_STATE};
use super::record::Transitions;
use super::registry::{self, RTV_HANDLES};
use super::{alive, gpu, liststate, pso, submit};
use crate::hudless::detect::{Bound, Capture, Detector, Learned};
use crate::hudless::encode::OutputTransform;
use crate::recognise::PassKind;
use crate::states;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static DETECTOR: Mutex<Detector> = Mutex::new(Detector::new());
static TRANSFORM: Mutex<Option<OutputTransform>> = Mutex::new(None);
static CONVERTER: Mutex<Option<Converter>> = Mutex::new(None);
/// Replaced converters with the present count at replacement.
static RETIRED: Mutex<Vec<(u64, Converter)>> = Mutex::new(Vec::new());
/// Presents a replaced converter is kept at least (its last image may still
/// be read by frame generation).
const RETIRE_PRESENTS: u64 = 8;
/// The key a build last failed for, so it is not retried every frame.
static FAILED_FOR: Mutex<Option<Key>> = Mutex::new(None);
/// The newest conversion: `(frame, image)`.
static PRODUCED: Mutex<Option<(u64, HudlessImage)>> = Mutex::new(None);
static CAPTURES: AtomicU64 = AtomicU64::new(0);
static FAILURES: AtomicU64 = AtomicU64::new(0);

/// The converted image of one frame, for `fg::tag_late`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HudlessImage {
    /// `ID3D12Resource*` (this crate's; alive at least until the frame is
    /// presented and [`RETIRE_PRESENTS`] presents after its converter is
    /// replaced).
    pub resource: *mut c_void,
    /// `D3D12_RESOURCE_STATES` it is in on the engine's queue after the
    /// conversion (`PIXEL_SHADER_RESOURCE | NON_PIXEL_SHADER_RESOURCE`).
    pub state: u32,
    /// Its size (the window's).
    pub size: Size,
    /// Its `DXGI_FORMAT` (the swapchain's).
    pub format: u32,
}

// SAFETY: a plain pointer to a free-threaded D3D12 resource of ours.
unsafe impl Send for HudlessImage {}

/// What the HUD-less path is doing, for logs and the dev server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    /// [`enable`]d.
    pub enabled: bool,
    /// The scene target and GUI binding learned so far.
    pub learned: Option<Learned>,
    /// Copies made.
    pub captures: u64,
    /// Captures confirmed by their frame's encode.
    pub confirmed: u64,
    /// Captures the encode showed were not the GUI's binding.
    pub refuted: u64,
    /// Captures that could not be made or converted.
    pub failures: u64,
    /// The encoder in use, once a converter exists.
    pub encoder: Option<Encoder>,
}

/// Turn the HUD-less path on or off (`[Experimental] HudLess`). Off, the
/// hooks cost one relaxed load; the objects made are kept for a later
/// enable.
pub fn enable(on: bool) {
    let was = ACTIVE.swap(on, Ordering::Relaxed);
    if was != on {
        log::info!(target: "hudless", "HUD-less capture {}", if on { "on" } else { "off" });
        if !on {
            DETECTOR.lock().end_frame();
            *PRODUCED.lock() = None;
        }
    }
}

/// Whether the hooks should feed the detector.
pub(crate) fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// The engine's output encode and the swapchain format (read by `mod`
/// each frame or when they change; cheap).
pub fn set_output_transform(transform: OutputTransform) {
    let mut held = TRANSFORM.lock();
    if *held != Some(transform) {
        mhw_core::log_changed!(
            Info,
            "hudless",
            "transform",
            (
                transform.gamma.to_bits(),
                transform.gamma_ex.to_bits(),
                transform.pq,
                transform.format
            ),
            "output transform: gamma {} ex {} {} format {}",
            transform.gamma,
            transform.gamma_ex,
            if transform.pq { "PQ" } else { "SDR" },
            transform.format
        );
        *held = Some(transform);
    }
}

/// The upscale slot of `frame` (the camera's frame number): bindings from
/// here to `Present` are candidates.
pub fn begin_frame(frame: u64) {
    if active() {
        DETECTOR.lock().begin_frame(frame);
    }
}

/// `Present`: close the frame, retire old converters.
pub(crate) fn end_frame() {
    if active() {
        DETECTOR.lock().end_frame();
    }
    let mut retired = RETIRED.lock();
    if retired.is_empty() {
        return;
    }
    let now = registry::presents();
    let completed = submit::current_fence().map_or(0, |fence| fence.completed());
    retired.retain(|(at, converter)| {
        now.saturating_sub(*at) <= RETIRE_PRESENTS || !converter.idle(completed)
    });
}

/// The display size (from `note_display`).
pub(crate) fn note_display(display: Size) {
    DETECTOR.lock().set_display(display);
}

/// `frame`'s HUD-less image, if its capture was confirmed and converted.
pub fn take(frame: u64) -> Option<HudlessImage> {
    if !active() || !DETECTOR.lock().confirmed(frame) {
        return None;
    }
    PRODUCED
        .lock()
        .filter(|(produced, _)| *produced == frame)
        .map(|(_, image)| image)
}

/// What the path is doing.
pub fn status() -> Status {
    let detector = DETECTOR.lock();
    Status {
        enabled: active(),
        learned: detector.learned(),
        captures: CAPTURES.load(Ordering::Relaxed),
        confirmed: detector.confirmations,
        refuted: detector.refuted,
        failures: FAILURES.load(Ordering::Relaxed),
        encoder: CONVERTER.lock().as_ref().map(|c| c.encoder),
    }
}

/// From `OMSetRenderTargets` on `list`: `rtv` is the first target's CPU
/// handle (`None`: no colour target).
pub(crate) fn on_bind(list: *mut c_void, rtv: Option<usize>) {
    let target = rtv.map(|handle| {
        let resource = RTV_HANDLES.lock().get(&handle).copied().unwrap_or(0);
        let info = registry::describe(resource);
        Bound {
            resource,
            serial: alive::serial_of(resource),
            size: info.map_or(Size::default(), |i| i.size()),
            format: info.map_or(0, |i| i.format),
        }
    });
    DETECTOR.lock().bind(list as usize, target);
}

/// From a draw on `list`, before it is recorded.
///
/// # Safety
/// `list` must be the live, open native list being recorded on this thread.
pub(crate) unsafe fn on_draw(list: *mut c_void) {
    let pipeline = liststate::with(list, |state| state.pipeline);
    let encode = matches!(
        pso::pass_of(pipeline),
        Some((PassKind::CopyToBackBuffer | PassKind::CopyToBackBufferPq, _))
    );
    let capture = {
        let mut detector = DETECTOR.lock();
        if encode {
            detector.encode(list as usize);
            None
        } else {
            detector.draw(list as usize)
        }
    };
    let Some(capture) = capture else {
        return;
    };
    // SAFETY: forwarded contract.
    match unsafe { capture_now(list, &capture) } {
        Ok(image) => {
            CAPTURES.fetch_add(1, Ordering::Relaxed);
            *PRODUCED.lock() = Some((capture.frame, image));
            mhw_core::log_once!(
                Info,
                "hudless",
                "first-capture",
                "first HUD-less capture: {:#x} {}x{} format {}, binding {} of the frame",
                capture.target.resource,
                capture.target.size.w,
                capture.target.size.h,
                capture.target.format,
                capture.occurrence
            );
        }
        Err(why) => {
            DETECTOR.lock().abandon();
            FAILURES.fetch_add(1, Ordering::Relaxed);
            mhw_core::log_changed!(
                Warn,
                "hudless",
                "capture-failed",
                why.clone(),
                "no HUD-less image: {why}"
            );
        }
    }
}

/// Copy the scene target and record its conversion.
///
/// # Safety
/// As [`on_draw`].
unsafe fn capture_now(list: *mut c_void, capture: &Capture) -> Result<HudlessImage, String> {
    let target = capture.target;
    let serial = target.serial.ok_or("the scene target is not watched")?;
    if !alive::is_alive(target.resource, serial) {
        return Err("the scene target is no longer alive".into());
    }
    let raw = target.resource as *mut c_void;
    // SAFETY: alive per its serial.
    let scene = unsafe { gpu::resource(&raw) }.ok_or("no scene target")?;
    // SAFETY: a live resource.
    let now = unsafe { scene.GetDesc() };
    if (now.Width, now.Height, now.Format.0 as u32)
        != (u64::from(target.size.w), target.size.h, target.format)
    {
        return Err("the scene target changed since it was bound".into());
    }
    let transform = (*TRANSFORM.lock()).ok_or("no output transform set")?;
    transform.check()?;
    // SAFETY: a live resource.
    let device = unsafe { gpu::device_of(scene) }?;
    let fence = submit::fence(&device)?;
    let key = Key {
        device: device.as_raw() as usize,
        size: target.size,
        scene_format: target.format,
        out_format: transform.format,
        pq: transform.pq,
    };

    let mut guard = CONVERTER.lock();
    if guard.as_ref().is_none_or(|c| c.key != key) {
        if *FAILED_FOR.lock() == Some(key) {
            return Err("conversion objects could not be made for this size".into());
        }
        let (converter, refused) = Converter::build(&device, key).inspect_err(|_| {
            *FAILED_FOR.lock() = Some(key);
        })?;
        if let Some(why) = refused {
            log::warn!(target: "hudless", "the engine's encode shader is not used ({why}); own gamma/PQ shader instead");
        }
        if let Some(old) = guard.replace(converter) {
            RETIRED.lock().push((registry::presents(), old));
        }
    }
    let converter = guard.as_mut().ok_or("no converter")?;
    let (index, slot) = converter
        .free_slot(fence.completed(), registry::presents())
        .ok_or("all conversion slots are still in use by the GPU")?;
    let shared = Arc::clone(&converter.shared);
    drop(guard);

    // The copy, on the engine's list, from the state tracked there (the
    // engine draws into it, so RENDER_TARGET, but that is observed, not
    // assumed); our `hold` rests in COPY_DEST.
    let hold = shared.hold.as_raw() as usize;
    // SAFETY: the live list.
    let plan = unsafe {
        Transitions::plan(
            list,
            &[
                (target.resource, states::COPY_SOURCE),
                (hold, states::COPY_DEST),
            ],
        )
    }
    .ok_or("the scene target's state is not known on this list")?;
    // SAFETY: caller's contract: the open native list.
    let engine =
        unsafe { ID3D12GraphicsCommandList::from_raw_borrowed(&list) }.ok_or("not a list")?;
    // SAFETY: both resources alive; the open list.
    unsafe {
        plan.forward(engine);
        engine.CopyResource(&shared.hold, scene);
        plan.back(engine);
    }
    let ticket = convert::record(
        &shared,
        &slot,
        index,
        target.size,
        &transform,
        registry::presents(),
    )?;
    let output = slot.output.as_raw();
    submit::after_submit(
        list as usize,
        Box::new(move |queue| {
            // Holding `shared` and `slot` keeps every object the recording
            // uses alive until it has been executed.
            let _shared = shared;
            convert::run(&slot, ticket, queue, &fence);
        }),
    );
    Ok(HudlessImage {
        resource: output,
        state: OUTPUT_STATE,
        size: target.size,
        format: transform.format,
    })
}
