//! The detours on the game's functions: what they replace, and the guard
//! every entry point runs in.
//!
//! All eight go in together ([`crate::install`]) or none does: the DLSS
//! detours are one mechanism (the feature handles `CreateFeatures` makes up
//! are only safe because both evaluations are ours), and the camera and
//! tuning work assumes the DLSS path is the mod's.

use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::gate::Addresses;
use crate::sites::site;

pub(crate) mod camera;
pub(crate) mod fatal;
pub(crate) mod hijack;

/// One detour to prepare: name, target, hook and where its original goes.
pub(crate) struct Spec {
    pub(crate) what: &'static str,
    pub(crate) target: usize,
    pub(crate) hook: *const c_void,
    pub(crate) original: &'static AtomicUsize,
}

/// Every detour, at the addresses the gate resolved.
pub(crate) fn specs(at: &Addresses) -> [Spec; 8] {
    let spec = |what, index, hook: *const c_void, original| Spec {
        what,
        target: at.site(index),
        hook,
        original,
    };
    [
        spec(
            "GetOptimalSettings",
            site::OPTIMAL,
            hijack::optimal as *const c_void,
            &hijack::T_OPTIMAL,
        ),
        spec(
            "CreateFeatures",
            site::CREATE,
            hijack::create as *const c_void,
            &hijack::T_CREATE,
        ),
        spec(
            "EvalSS",
            site::EVAL_SS,
            hijack::eval_supersampling as *const c_void,
            &hijack::T_EVAL_SS,
        ),
        spec(
            "EvalISP",
            site::EVAL_ISP,
            hijack::eval_image_signal as *const c_void,
            &hijack::T_EVAL_ISP,
        ),
        spec(
            "SS enqueue",
            site::SS_ENQUEUE,
            hijack::ss_enqueue as *const c_void,
            &hijack::T_SS_ENQUEUE,
        ),
        spec(
            "Image Quality handler",
            site::SCALING_OPTION,
            hijack::scaling_option as *const c_void,
            &hijack::T_SCALING_OPTION,
        ),
        spec(
            "camera setup",
            site::CAMERA_SETUP,
            camera::camera_setup as *const c_void,
            &camera::T_CAMERA_SETUP,
        ),
        spec(
            "fatal reporter",
            site::FATAL,
            fatal::fatal as *const c_void,
            &fatal::T_FATAL,
        ),
    ]
}

/// The trampoline stored in `original` as a function pointer of type `F`.
///
/// # Safety
/// `original` must hold the trampoline of a detour whose target has
/// exactly the signature `F` (set by `Detour::prepare` before the detour
/// could be entered).
pub(crate) unsafe fn original<F: Copy>(original: &AtomicUsize) -> F {
    const { assert!(std::mem::size_of::<F>() == std::mem::size_of::<usize>()) };
    let address = original.load(Ordering::Acquire);
    // SAFETY: a non-null code address of the right signature (caller).
    unsafe { std::mem::transmute_copy::<usize, F>(&address) }
}

/// Run `body`; if it panics (debug builds unwind; release aborts), log once
/// and run `fallback` instead, which passes the call through to the game.
pub(crate) fn guard<R>(
    what: &'static str,
    body: impl FnOnce() -> R,
    fallback: impl FnOnce() -> R,
) -> R {
    match std::panic::catch_unwind(AssertUnwindSafe(body)) {
        Ok(result) => result,
        Err(_) => {
            mhw_core::log_once!(
                Error,
                "engine",
                what,
                "panic in the {what} detour; passing the call to the game"
            );
            fallback()
        }
    }
}
