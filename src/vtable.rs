//! COM vtable slot patching.
//!
//! A D3D12 interface's vtable lives in the read-only data of the library
//! that implements it and is shared by every object of that implementation,
//! so patching one slot through any instance redirects the same call on the
//! game's objects too. That is what lets the hooks be installed through a
//! device this library creates for itself, before the game has one.
//!
//! "Every object of that implementation" is the limit of it: a proxy layer
//! above (Streamline, ReShade) has vtables of its own. `swap.rs` patches per
//! vtable for that reason.
//!
//! Slot indices are derived from the `*_Vtbl` layout with `offset_of!` rather
//! than hardcoded, so an interface gaining methods cannot silently shift them.

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use windows::core::Interface;
use windows::Win32::System::Memory::{
    VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS,
};

/// Byte offset of a `*_Vtbl` field, as a slot index.
#[macro_export]
macro_rules! slot {
    ($vtbl:ty, $method:ident) => {
        std::mem::offset_of!($vtbl, $method) / std::mem::size_of::<usize>()
    };
}

/// The vtable backing a COM object.
///
/// # Safety
/// `interface` must be a live COM object: its first word is read as the
/// vtable pointer.
pub unsafe fn of<I: Interface>(interface: &I) -> *mut *mut c_void {
    *(interface.as_raw() as *const *mut *mut c_void)
}

/// Replace one vtable slot, having first put the entry it held in `original`.
///
/// In that order, because the slot is live the moment it is written: another
/// thread can be through it and into the replacement before this function has
/// returned, and a replacement that loads its original would then find zero
/// and call it. On the draw slots, patched while the engine issues thousands
/// of draws a frame, that window is real.
///
/// A slot that already holds `replacement` is left alone and `original` with
/// it: storing the slot's content then would make the hook its own original.
///
/// Writing into another module's read-only data is the point of the exercise;
/// the page protection is restored afterwards so nothing else notices.
///
/// # Safety
/// `vtable` must be a live vtable with more than `index` slots, and
/// `replacement` a function with exactly the ABI and signature of the
/// method in that slot. Every object sharing the vtable is affected, in
/// every thread, at once.
pub unsafe fn hook(
    vtable: *mut *mut c_void,
    index: usize,
    replacement: *const c_void,
    original: &AtomicUsize,
) -> bool {
    let entry = vtable.add(index);
    let width = std::mem::size_of::<*mut c_void>();
    let mut previous = PAGE_PROTECTION_FLAGS(0);

    if VirtualProtect(
        entry as *const c_void,
        width,
        PAGE_EXECUTE_READWRITE,
        &mut previous,
    )
    .is_err()
    {
        return false;
    }

    let held = *entry;
    let ours = std::ptr::eq(held, replacement);
    if !ours {
        original.store(held as usize, Ordering::SeqCst);
        *entry = replacement as *mut c_void;
    }

    let mut restored = PAGE_PROTECTION_FLAGS(0);
    let _ = VirtualProtect(entry as *const c_void, width, previous, &mut restored);

    // Already hooked counts as hooked only if the original is known.
    !ours || original.load(Ordering::SeqCst) != 0
}

/// Write one vtable slot and nothing else, returning what it held.
///
/// For taking a hook out again: the value written is an original that is
/// already known, so there is nothing to store first.
///
/// # Safety
/// As for `hook`: a live vtable with more than `index` slots, and `value` a
/// function with the signature of that method.
pub unsafe fn write(
    vtable: *mut *mut c_void,
    index: usize,
    value: *const c_void,
) -> Option<*mut c_void> {
    let entry = vtable.add(index);
    let width = std::mem::size_of::<*mut c_void>();
    let mut previous = PAGE_PROTECTION_FLAGS(0);

    VirtualProtect(
        entry as *const c_void,
        width,
        PAGE_EXECUTE_READWRITE,
        &mut previous,
    )
    .ok()?;

    let held = *entry;
    *entry = value as *mut c_void;

    let mut restored = PAGE_PROTECTION_FLAGS(0);
    let _ = VirtualProtect(entry as *const c_void, width, previous, &mut restored);

    Some(held)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Different bodies, so the two cannot be merged into one address.
    unsafe extern "system" fn first() -> u32 {
        1
    }
    unsafe extern "system" fn second() -> u32 {
        2
    }

    /// The original is in place before the slot changes, and a second
    /// installation does not make the hook its own original.
    #[test]
    fn the_original_is_stored_and_survives_a_second_installation() {
        let mut table: [*mut c_void; 2] = [first as *mut c_void, std::ptr::null_mut()];
        let original = AtomicUsize::new(0);
        unsafe {
            assert!(hook(
                table.as_mut_ptr(),
                0,
                second as *const c_void,
                &original
            ));
            assert_eq!(table[0] as usize, second as *const c_void as usize);
            assert_eq!(
                original.load(Ordering::SeqCst),
                first as *const c_void as usize
            );

            assert!(hook(
                table.as_mut_ptr(),
                0,
                second as *const c_void,
                &original
            ));
            assert_eq!(
                original.load(Ordering::SeqCst),
                first as *const c_void as usize
            );

            assert_eq!(
                write(table.as_mut_ptr(), 0, first as *const c_void).map(|held| held as usize),
                Some(second as *const c_void as usize)
            );
            assert_eq!(table[0] as usize, first as *const c_void as usize);
        }
    }
}
