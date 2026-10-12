//! COM vtable slot patching.
//!
//! A D3D12 interface's vtable lives in the read-only data of the library
//! implementing it and is shared by every object of that implementation, so
//! patching one slot through any instance redirects the call on the game's
//! objects too. That is what lets hooks go in through a device the mod
//! creates for itself, before the game has one. "Every object of that
//! implementation" is the limit: a proxy layer above (Streamline, ReShade)
//! has vtables of its own and must be patched separately.

use std::ffi::c_void;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::mem::{self, MemError};

/// Slot index of `method` in the `windows` crate's `*_Vtbl` layout:
/// `slot!(ID3D12Device_Vtbl, CreateCommandQueue)`. Derived with
/// `offset_of!` so an interface gaining methods cannot silently shift it.
#[macro_export]
macro_rules! slot {
    ($vtbl:ty, $method:ident) => {
        ::std::mem::offset_of!($vtbl, $method) / ::std::mem::size_of::<usize>()
    };
}

/// The vtable of a COM object.
///
/// # Safety
/// `object` must point to a live COM object (its first word is its vtable).
pub unsafe fn vtable_of(object: *const c_void) -> *mut usize {
    // SAFETY: caller guarantees a live object, whose first word is the
    // vtable pointer.
    unsafe { *(object as *const *mut usize) }
}

/// Why a slot was not patched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VtableError {
    /// The slot could not be written.
    Write(MemError),
    /// The slot already holds the replacement but no original is known:
    /// it was patched by something that did not record one.
    OursWithoutOriginal,
}

impl fmt::Display for VtableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VtableError::Write(error) => write!(f, "vtable slot not writable: {error}"),
            VtableError::OursWithoutOriginal => {
                f.write_str("slot already hooked but its original is unknown")
            }
        }
    }
}

impl std::error::Error for VtableError {}

/// Point slot `index` of `vtable` at `replacement`, having first stored the
/// function it held in `original`.
///
/// In that order, because the slot is live the moment it is written:
/// another thread can be through it and into the replacement before this
/// returns, and a replacement that loads its original must not find zero.
/// On the draw slots, patched while the engine issues thousands of draws a
/// frame, that window is real.
///
/// A slot already holding `replacement` is left alone, and `original` with
/// it: storing the slot's content then would make the hook its own original.
///
/// # Safety
/// `vtable` must be a live vtable with more than `index` slots, and
/// `replacement` a function with exactly the ABI and signature of the method
/// in that slot. Every object sharing the vtable is affected, in every
/// thread, at once.
pub unsafe fn patch_vtable(
    vtable: *mut usize,
    index: usize,
    replacement: *const c_void,
    original: &AtomicUsize,
) -> Result<(), VtableError> {
    // SAFETY: caller guarantees `index` is inside the vtable.
    let slot = unsafe { vtable.add(index) } as usize;
    // SAFETY: as above; `read` also checks the slot is mapped.
    let held = unsafe { mem::read::<usize>(slot) }
        .ok_or(VtableError::Write(MemError::Unmapped { address: slot }))?;
    if held == replacement as usize {
        return match original.load(Ordering::SeqCst) {
            0 => Err(VtableError::OursWithoutOriginal),
            _ => Ok(()),
        };
    }
    original.store(held, Ordering::SeqCst);
    // SAFETY: caller guarantees a live slot and a compatible replacement.
    let previous =
        unsafe { mem::write_pointer(slot, replacement as usize) }.map_err(VtableError::Write)?;
    if previous != held {
        // Changed between the read and the write (another hook went in):
        // chain to what was actually replaced.
        original.store(previous, Ordering::SeqCst);
    }
    Ok(())
}

/// Write slot `index` of `vtable` and nothing else, returning what it held:
/// for taking a hook out again with an original that is already known.
///
/// # Safety
/// As for [`patch_vtable`].
pub unsafe fn write_vtable(
    vtable: *mut usize,
    index: usize,
    value: *const c_void,
) -> Result<usize, MemError> {
    // SAFETY: caller guarantees `index` is inside a live vtable and `value`
    // is compatible.
    unsafe { mem::write_pointer(vtable.add(index) as usize, value as usize) }
}

/// Windows-only: run on a Windows host (`cargo test -p mhw-win`).
#[cfg(test)]
mod tests {
    use super::*;

    // Different bodies, so the two cannot be merged into one address.
    extern "system" fn first() -> u32 {
        1
    }
    extern "system" fn second() -> u32 {
        2
    }

    #[test]
    fn the_original_is_stored_and_survives_a_second_installation() {
        let (one, two) = (first as *const c_void, second as *const c_void);
        let mut table = [one as usize, 0];
        let original = AtomicUsize::new(0);
        // SAFETY: a two-slot table of functions with one signature.
        unsafe {
            patch_vtable(table.as_mut_ptr(), 0, two, &original).unwrap();
            assert_eq!(table[0], two as usize);
            assert_eq!(original.load(Ordering::SeqCst), one as usize);

            patch_vtable(table.as_mut_ptr(), 0, two, &original).unwrap();
            assert_eq!(original.load(Ordering::SeqCst), one as usize);

            assert_eq!(write_vtable(table.as_mut_ptr(), 0, one), Ok(two as usize));
            assert_eq!(table[0], one as usize);

            let unknown = AtomicUsize::new(0);
            table[1] = two as usize;
            assert_eq!(
                patch_vtable(table.as_mut_ptr(), 1, two, &unknown),
                Err(VtableError::OursWithoutOriginal)
            );
        }
    }
}
