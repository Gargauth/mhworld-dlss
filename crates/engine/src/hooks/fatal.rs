//! The engine's fatal reporter (`0x1421fd060`): its assert, which formats a
//! message, hands it to a registered handler and then writes `0xdeadbeef`
//! to address 0 (`0x1421fd0c9`). The second argument is a printf format
//! naming the failure (`ERR12 : …`, `ERR22 : Failed to initialize
//! DirectX12.`); logging it turns an access violation at a fixed address
//! into a sentence. Then the call goes through: by now the engine has
//! decided its renderer cannot go on.

use std::sync::atomic::AtomicUsize;

use mhw_win::mem;

use super::{guard, original};
use crate::runtime::COUNTERS;

/// The fatal reporter's trampoline.
pub(crate) static T_FATAL: AtomicUsize = AtomicUsize::new(0);

/// Only the register arguments are forwarded: the messages take at most
/// two, which arrive in `r8`/`r9`.
type FatalFn = unsafe extern "system" fn(usize, usize, usize, usize) -> usize;

/// Longest message read.
const MESSAGE_MAX: usize = 512;

/// `void Fatal(context, format, ...)`.
pub(crate) unsafe extern "system" fn fatal(
    context: usize,
    format: usize,
    a3: usize,
    a4: usize,
) -> usize {
    guard(
        "fatal reporter",
        || {
            let count = COUNTERS.fatal_reports.bump();
            log::error!(
                target: "engine",
                "the engine is aborting: {} (arguments {a3:#x} {a4:#x}; report {count})",
                c_string(format)
            );
            log::logger().flush();
        },
        || (),
    );
    // SAFETY: this detour's trampoline, same signature.
    unsafe { original::<FatalFn>(&T_FATAL)(context, format, a3, a4) }
}

/// A bounded C string from memory that may not hold one.
fn c_string(address: usize) -> String {
    let len = mem::readable_len(address, MESSAGE_MAX);
    if address == 0 || len == 0 {
        return format!("<{address:#x} is not readable>");
    }
    let mut bytes = vec![0u8; len];
    // SAFETY: `len` bytes at `address` were just found readable.
    if !unsafe { mem::read_bytes(address, &mut bytes) } {
        return format!("<{address:#x} is not readable>");
    }
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}
