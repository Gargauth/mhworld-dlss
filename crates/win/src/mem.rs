//! Reading and writing this process's memory with page checks.
//!
//! Reads check that every page of the range is committed and readable;
//! writes make each page writable for the duration of the write, put the
//! previous protection back and flush the instruction cache, so the same
//! call serves data, read-only tables (vtables, IATs) and code.
//!
//! The checks narrow, but cannot close, the window in which another thread
//! frees or reprotects the memory between the check and the access. Callers
//! use these on memory that stays mapped: game images, engine globals and
//! objects they otherwise know to be alive.

use std::ffi::c_void;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::sys;

/// Why a write failed. In every case nothing was written to the failing
/// page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemError {
    /// The page at `address` is not committed.
    Unmapped {
        /// First address of the range that is not committed.
        address: usize,
    },
    /// `VirtualProtect` refused to make the page at `address` writable.
    Protect {
        /// The page's address.
        address: usize,
        /// Its protection at the time.
        protection: u32,
    },
    /// The address is not aligned for an atomic pointer write.
    Misaligned {
        /// The address.
        address: usize,
    },
}

impl fmt::Display for MemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemError::Unmapped { address } => write!(f, "{address:#x} is not committed"),
            MemError::Protect {
                address,
                protection,
            } => write!(
                f,
                "cannot make {address:#x} writable (protection {protection:#x})"
            ),
            MemError::Misaligned { address } => write!(f, "{address:#x} is not pointer-aligned"),
        }
    }
}

impl std::error::Error for MemError {}

/// How many bytes from `address` (at most `len`) are committed and readable,
/// walking region by region.
pub fn readable_len(address: usize, len: usize) -> usize {
    let Some(end) = address.checked_add(len) else {
        return 0;
    };
    let mut at = address;
    while at < end {
        let Some(info) = sys::query(at) else { break };
        if info.State.0 != sys::MEM_COMMIT || !sys::readable_protection(info.Protect.0) {
            break;
        }
        at = sys::region_end(&info).min(end);
    }
    at - address
}

/// Whether all of `[address, address + len)` is committed and readable.
pub fn is_readable(address: usize, len: usize) -> bool {
    readable_len(address, len) == len
}

/// Whether all of `[address, address + len)` is committed and writable
/// without a protection change.
pub fn is_writable(address: usize, len: usize) -> bool {
    let Some(end) = address.checked_add(len) else {
        return false;
    };
    let mut at = address;
    while at < end {
        let Some(info) = sys::query(at) else {
            return false;
        };
        let protect = info.Protect.0;
        if info.State.0 != sys::MEM_COMMIT
            || protect & sys::WRITABLE == 0
            || protect & sys::PAGE_GUARD != 0
        {
            return false;
        }
        at = sys::region_end(&info);
    }
    true
}

/// Copy `buffer.len()` bytes from `address` into `buffer`; `false` (and
/// nothing copied) unless all of them are readable.
///
/// # Safety
/// The memory must not be freed or reprotected by another thread during the
/// call (see the module documentation).
pub unsafe fn read_bytes(address: usize, buffer: &mut [u8]) -> bool {
    if !is_readable(address, buffer.len()) {
        return false;
    }
    // SAFETY: the range was just found committed and readable, and the
    // caller guarantees it stays so; `buffer` is a distinct Rust allocation.
    unsafe {
        std::ptr::copy_nonoverlapping(address as *const u8, buffer.as_mut_ptr(), buffer.len())
    };
    true
}

/// Read a `T` from `address` (unaligned), if readable.
///
/// # Safety
/// As for [`read_bytes`]; additionally any bit pattern must be a valid `T`.
pub unsafe fn read<T: Copy>(address: usize) -> Option<T> {
    if !is_readable(address, std::mem::size_of::<T>()) {
        return None;
    }
    // SAFETY: readable for size_of::<T>() bytes (checked above); the caller
    // guarantees validity of the bit pattern.
    Some(unsafe { (address as *const T).read_unaligned() })
}

/// Run `write` with `[address, address + len)` writable, region by region,
/// restoring each region's protection afterwards and flushing the
/// instruction cache. `write` receives each sub-range.
///
/// # Safety
/// `write` must only touch the sub-range it is given.
unsafe fn with_writable(
    address: usize,
    len: usize,
    mut write: impl FnMut(usize, usize),
) -> Result<(), MemError> {
    let end = address
        .checked_add(len)
        .ok_or(MemError::Unmapped { address })?;
    let mut at = address;
    while at < end {
        let info = sys::query(at).ok_or(MemError::Unmapped { address: at })?;
        if info.State.0 != sys::MEM_COMMIT {
            return Err(MemError::Unmapped { address: at });
        }
        let chunk_end = sys::region_end(&info).min(end);
        let chunk = chunk_end - at;
        let protect = info.Protect.0;
        if protect & sys::WRITABLE != 0 && protect & sys::PAGE_GUARD == 0 {
            write(at, chunk);
        } else {
            // Keep code executable while it is written: other threads may be
            // running elsewhere on the same page.
            let writable = if protect & sys::EXECUTABLE != 0 {
                sys::PAGE_EXECUTE_READWRITE
            } else {
                sys::PAGE_READWRITE
            };
            let mut previous = 0u32;
            // SAFETY: changes protection of committed pages of this process;
            // `previous` is a valid out-parameter.
            let ok =
                unsafe { sys::VirtualProtect(at as *const c_void, chunk, writable, &mut previous) };
            if ok == 0 {
                return Err(MemError::Protect {
                    address: at,
                    protection: protect,
                });
            }
            write(at, chunk);
            let mut ignored = 0u32;
            // SAFETY: as above; restores what was there.
            unsafe { sys::VirtualProtect(at as *const c_void, chunk, previous, &mut ignored) };
        }
        sys::flush_instructions(at, chunk);
        at = chunk_end;
    }
    Ok(())
}

/// Write `bytes` at `address`, changing page protection as needed.
///
/// Not atomic: a concurrent reader may see part of the write. For code that
/// other threads may be executing use [`crate::code::apply`]; for a single
/// pointer use [`write_pointer`].
///
/// # Safety
/// `address..address + bytes.len()` must be memory whose modification is
/// sound for every other user of it.
pub unsafe fn write_bytes(address: usize, bytes: &[u8]) -> Result<(), MemError> {
    // SAFETY: each sub-range handed to the closure lies inside the target
    // range, now writable; the caller vouches for the write's soundness.
    unsafe {
        with_writable(address, bytes.len(), |at, len| {
            let from = &bytes[at - address..at - address + len];
            std::ptr::copy_nonoverlapping(from.as_ptr(), at as *mut u8, len);
        })
    }
}

/// Atomically replace the pointer-sized, pointer-aligned value at `slot`
/// with `value`, returning what it held. Changes page protection as needed
/// (vtables and IATs live in read-only data).
///
/// # Safety
/// `slot` must be a live pointer-sized slot whose replacement is sound for
/// every user of it.
pub unsafe fn write_pointer(slot: usize, value: usize) -> Result<usize, MemError> {
    if !slot.is_multiple_of(std::mem::align_of::<usize>()) {
        return Err(MemError::Misaligned { address: slot });
    }
    let mut previous = 0;
    // SAFETY: the slot is aligned (checked) and writable inside the closure;
    // an atomic swap keeps concurrent readers from seeing a torn pointer.
    unsafe {
        with_writable(slot, std::mem::size_of::<usize>(), |at, _| {
            previous = AtomicUsize::from_ptr(at as *mut usize).swap(value, Ordering::SeqCst);
        })?;
    }
    Ok(previous)
}

/// Flush the instruction cache for a range of code this process changed by
/// other means.
pub fn flush_instruction_cache(address: usize, len: usize) {
    sys::flush_instructions(address, len);
}
