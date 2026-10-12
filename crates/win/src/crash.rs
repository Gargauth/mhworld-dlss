//! A vectored exception handler that logs faults: what, where (module +
//! offset), and for access violations what was accessed. Once per faulting
//! address, so a fault handled and retried in a loop is one line.
//!
//! Vectored handlers see first-chance exceptions, before any `__try` in the
//! faulting code: a logged fault may still be handled (drivers and anti-
//! tamper code probe memory this way). The line says so. The handler never
//! handles anything itself; it always continues the search.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use windows::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, RemoveVectoredExceptionHandler, EXCEPTION_POINTERS,
};
use windows::Win32::System::Threading::GetCurrentThreadId;

use crate::fault::{self, Insert, OnceSet};
use crate::module::{module_base_of, module_path};

/// Log target of the crash logger.
pub const TARGET: &str = "crash";

const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
const ACCESS_VIOLATION: u32 = 0xC000_0005;
const IN_PAGE_ERROR: u32 = 0xC000_0006;
const STACK_OVERFLOW: u32 = 0xC000_00FD;

static HANDLER: AtomicUsize = AtomicUsize::new(0);
static SEEN: OnceSet<256> = OnceSet::new();
static FULL_REPORTED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Set while this thread is inside the handler: a fault in the logging
    /// itself must not recurse.
    static INSIDE: Cell<bool> = const { Cell::new(false) };
}

/// Install the crash logger (first in the handler chain, so a handler that
/// resumes execution cannot hide a fault from it). Idempotent; `false` if
/// the system refused.
///
/// The handler stays installed until [`remove_crash_logger`]; the module
/// containing it must not be unloaded before that.
pub fn install_crash_logger() -> bool {
    if HANDLER.load(Ordering::SeqCst) != 0 {
        return true;
    }
    // SAFETY: registers a handler with the right signature; it only reads
    // the exception record it is given.
    let handle = unsafe { AddVectoredExceptionHandler(1, Some(on_exception)) };
    if handle.is_null() {
        return false;
    }
    if HANDLER
        .compare_exchange(0, handle as usize, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        // Another thread installed one meanwhile.
        // SAFETY: `handle` is the registration made just above.
        unsafe { RemoveVectoredExceptionHandler(handle) };
    }
    true
}

/// Remove the crash logger, if installed.
pub fn remove_crash_logger() {
    let handle = HANDLER.swap(0, Ordering::SeqCst);
    if handle != 0 {
        // SAFETY: `handle` came from AddVectoredExceptionHandler.
        unsafe { RemoveVectoredExceptionHandler(handle as *const c_void) };
    }
}

unsafe extern "system" fn on_exception(info: *mut EXCEPTION_POINTERS) -> i32 {
    // SAFETY: the system passes valid exception pointers; null is checked.
    let Some(record) =
        (unsafe { info.as_ref() }).and_then(|i| unsafe { i.ExceptionRecord.as_ref() })
    else {
        return EXCEPTION_CONTINUE_SEARCH;
    };
    let code = record.ExceptionCode.0 as u32;
    let Some(name) = fault::fault_name(code) else {
        return EXCEPTION_CONTINUE_SEARCH;
    };
    // The handler of a stack overflow runs on what is left of the stack;
    // formatting and logging there would turn it into a silent kill.
    if code == STACK_OVERFLOW {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let address = record.ExceptionAddress as usize;
    match SEEN.insert(address) {
        Insert::New => {}
        Insert::Seen => return EXCEPTION_CONTINUE_SEARCH,
        Insert::Full => {
            if !FULL_REPORTED.swap(true, Ordering::Relaxed) {
                log::error!(target: TARGET, "fault table full; further fault addresses are not logged");
            }
            return EXCEPTION_CONTINUE_SEARCH;
        }
    }
    let entered = INSIDE
        .try_with(|inside| !inside.replace(true))
        .unwrap_or(false);
    if !entered {
        return EXCEPTION_CONTINUE_SEARCH;
    }

    let module = module_base_of(address).map(|base| {
        let path = module_path(base).unwrap_or_default();
        let name = path.rsplit(['\\', '/']).next().unwrap_or("").to_owned();
        (name, base)
    });
    let location = fault::location(address, module.as_ref().map(|(n, b)| (n.as_str(), *b)));
    // SAFETY: GetCurrentThreadId has no preconditions.
    let thread = unsafe { GetCurrentThreadId() };
    let parameters = &record.ExceptionInformation[..(record.NumberParameters as usize).min(15)];
    match (code, parameters) {
        (ACCESS_VIOLATION | IN_PAGE_ERROR, [kind, target, ..]) => log::error!(
            target: TARGET,
            "{name} at {location} (first chance, may yet be handled) access={} target={target:#x} thread={thread}",
            fault::access_kind(*kind)
        ),
        _ => log::error!(
            target: TARGET,
            "{name} ({code:#010x}) at {location} (first chance, may yet be handled) thread={thread}"
        ),
    }
    let _ = INSIDE.try_with(|inside| inside.set(false));
    EXCEPTION_CONTINUE_SEARCH
}
