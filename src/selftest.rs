//! One DLSS frame, on request, for the stand-in program.
//!
//! In the game the first DLSS frame happens deep inside a session: the
//! temporal pass has to be recognised, its inputs resolved out of the engine's
//! descriptor tables, a depth target chosen, the engine's detours installed.
//! None of that can exist in `tools/standin`, and twice a run of the game
//! failed at a step the stand-in never reached.
//!
//! So this is the *tail* of `gpu::run_dlss`, callable from outside: the motion
//! vector decode and `dlss::evaluate`, on resources the caller made, with the
//! command list **as the caller holds it**. That last part is the point. The
//! stand-in is handed its list by whatever sits above the mod -- ReShade's
//! proxy, when ReShade is loaded -- exactly as the engine is, and what
//! `run_dlss` receives in the game is the native list under that proxy,
//! because it is entered from a vtable hook on the native implementation.
//! Unwrapping the caller's list here gives the same pointer, so everything
//! from here down is the code the game runs, with the objects the game would
//! have, and a wrapped object reaching a native call fails here as it would
//! there.
//!
//! ## From under a dispatch, when there is a proxy
//!
//! In the game `run_dlss` runs from inside the engine's `Dispatch`, and with
//! ReShade loaded that call has come through ReShade's proxy, which is how
//! `dlss::list_for_ngx` learns which proxy the native list belongs to. A
//! frame asked for directly would have no `Dispatch` on the stack. So when
//! the caller's list is a proxy and the proxy's `Dispatch` is being watched,
//! the frame is only *armed* here, and it runs when the caller next
//! dispatches on that list: from `reshade::proxy_dispatch`, through the hook
//! the game's dispatches go through, with the engine's heaps set on the list
//! and restored afterwards. The caller then asks for the result.
//!
//! What still differs from the game: there the tail is reached from the
//! native `Dispatch` hook, one layer further down the same stack.
//!
//! Two exports, never called by the mod or by the game. A process that does
//! not ask for them by name pays one relaxed load per dispatch that comes
//! through ReShade's proxy, and nothing at all without ReShade.

use parking_lot::Mutex;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// What the stand-in hands over. Plain C layout.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SelftestFrame {
    /// Scene colour at render size, `R11G11B10_FLOAT`, readable.
    pub colour: *mut c_void,
    /// Depth at render size, readable.
    pub depth: *mut c_void,
    /// The engine's packed motion vectors at render size, `R32_UINT`, readable.
    pub motion: *mut c_void,
    /// Where DLSS writes, at display size, in the unordered-access state.
    pub output: *mut c_void,
    pub render: [u32; 2],
    pub display: [u32; 2],
    /// Bit 0: run the motion vector decode first, as `run_dlss` does.
    pub flags: u32,
}

/// The decode ran and produced a buffer.
pub const DECODED: u32 = 1;
/// `dlss::evaluate` returned true: the feature exists and NGX took the frame.
pub const EVALUATED: u32 = 2;
/// The frame is armed and runs at the caller's next `Dispatch` on this list;
/// ask `mhw_dlss_selftest_result` afterwards.
pub const PENDING: u32 = 4;
/// NGX is not initialised or its entry points are not bound yet.
pub const NOT_READY: u32 = 0x8000_0000;

struct Armed {
    proxy: usize,
    frame: SelftestFrame,
}
// Raw pointers to resources the caller keeps alive until it has the result.
unsafe impl Send for Armed {}

static ARMED: AtomicBool = AtomicBool::new(false);
static WAITING: Mutex<Option<Armed>> = Mutex::new(None);
static RESULT: AtomicU32 = AtomicU32::new(PENDING);

/// Run an armed frame, if this is the dispatch it was waiting for.
///
/// Called from `reshade::proxy_dispatch` with the proxy being dispatched on,
/// after that hook has noted it for this thread. One relaxed load when
/// nothing is armed, which outside the stand-in is always.
///
/// # Safety
/// `proxy` must be the live list whose `Dispatch` is on this thread's stack.
pub(crate) unsafe fn at_proxy_dispatch(proxy: *mut c_void) {
    if !ARMED.load(Ordering::Relaxed) {
        return;
    }
    let armed = {
        let mut waiting = WAITING.lock();
        match waiting.as_ref() {
            Some(armed) if armed.proxy == proxy as usize => waiting.take(),
            _ => None,
        }
    };
    let Some(armed) = armed else {
        return;
    };
    // Cleared before the frame runs: NGX dispatches on this same proxy from
    // inside the evaluation, and those calls come back through here.
    ARMED.store(false, Ordering::Relaxed);
    let (native, _) = crate::reshade::unwrap(proxy);
    RESULT.store(run(native, &armed.frame), Ordering::Relaxed);
}

/// Ask for one DLSS frame on `list`.
///
/// Returns `DECODED | EVALUATED` bits when it ran at once, or `PENDING` when
/// it will run at the next `Dispatch` on this list.
///
/// # Safety
/// `list` must be an open direct command list, as the caller was handed it,
/// and every resource in `frame` live, in the state named above, until the
/// result has been read.
#[no_mangle]
pub unsafe extern "system" fn mhw_dlss_selftest_frame(
    list: *mut c_void,
    frame: *const SelftestFrame,
) -> u32 {
    if list.is_null() || frame.is_null() {
        return 0;
    }
    if !crate::dlss::bound() {
        crate::log!("[tst] a DLSS frame was asked for before NGX's entry points were bound");
        return NOT_READY;
    }
    let frame = *frame;
    let (native, was_proxy) = crate::reshade::unwrap(list);
    crate::log!(
        "[tst] one DLSS frame asked for on the list {:#x}{}: {}x{} -> {}x{}",
        list as usize,
        if was_proxy {
            format!(
                ", a ReShade proxy over {:#x}, which is what the hooks are entered with",
                native as usize
            )
        } else {
            " (native)".to_string()
        },
        frame.render[0],
        frame.render[1],
        frame.display[0],
        frame.display[1]
    );
    if was_proxy && crate::reshade::proxy_lists_watched() {
        *WAITING.lock() = Some(Armed {
            proxy: list as usize,
            frame,
        });
        RESULT.store(PENDING, Ordering::Relaxed);
        ARMED.store(true, Ordering::Relaxed);
        crate::log!("[tst] armed: it runs at the next Dispatch on that list, as the game's does");
        return PENDING;
    }
    let outcome = run(native, &frame);
    RESULT.store(outcome, Ordering::Relaxed);
    outcome
}

/// The outcome of the last frame asked for: `PENDING` until it has run.
#[no_mangle]
pub extern "system" fn mhw_dlss_selftest_result() -> u32 {
    RESULT.load(Ordering::Relaxed)
}

/// The tail of `gpu::run_dlss`, with the list a vtable hook would be given.
unsafe fn run(this: *mut c_void, frame: &SelftestFrame) -> u32 {
    let render = (frame.render[0], frame.render[1]);
    let display = (frame.display[0], frame.display[1]);
    let mut outcome = 0;
    // The same arguments `run_dlss` passes: gpu::MV_MODE is 2 (half floats)
    // and gpu::MV_DEJITTER is 0.
    let mv_scale = (-(render.0 as f32) / 2.0, render.1 as f32 / 2.0);
    let (motion, motion_scale) = if frame.flags & 1 != 0 {
        match crate::mv::decode(
            this,
            frame.motion as usize,
            frame.colour as usize,
            render,
            mv_scale,
            2,
            (0.0, 0.0),
            0.0,
        ) {
            Some(decoded) => {
                outcome |= DECODED;
                (decoded, (1.0, 1.0))
            }
            None => {
                crate::log!("[tst] the motion vector decode produced nothing");
                (frame.motion, mv_scale)
            }
        }
    } else {
        (frame.motion, mv_scale)
    };

    let evaluated = crate::dlss::evaluate(
        this,
        &crate::dlss::Frame {
            colour: frame.colour,
            output: frame.output,
            depth: frame.depth,
            motion,
            render,
            display,
            jitter: (0.25, -0.25),
            motion_scale,
            reset: true,
        },
    );
    if evaluated {
        outcome |= EVALUATED;
    }
    crate::log!(
        "[tst] ran on the native list {:#x}: decode {}, evaluate {}",
        this as usize,
        if outcome & DECODED != 0 {
            "ok"
        } else {
            "not run or failed"
        },
        if evaluated { "ok" } else { "FAILED" }
    );
    outcome
}
