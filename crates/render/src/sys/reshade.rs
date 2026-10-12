//! Sharing the process with ReShade: unwrapping its proxy objects.
//!
//! ReShade does not patch D3D12's vtables; it hands the engine *proxy*
//! device, queue and command list objects that forward to the native ones.
//! This crate's vtable hooks sit on the native implementation and are
//! entered with native `this` pointers, while anything handed to us by the
//! engine (a list at the upscale slot, a device from a creation call we own)
//! may be a proxy. Every ReShade proxy answers `QueryInterface` for one
//! private IID with the object it wraps; that is applied at those boundaries
//! and nowhere per draw.
//!
//! Never unwrap something that may be Streamline's proxy with this:
//! Streamline forwards IIDs it does not know to what it wraps, so the walk
//! would silently skip its layer.

use std::ffi::{c_void, CStr};
use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::{Interface, GUID, HRESULT, PCSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::GetProcAddress;

/// `IID_UnwrappedObject` (ReShade `source/com_utils.hpp`, 6.x).
pub const IID_UNWRAPPED: GUID = GUID::from_u128(0x7f2c9a11_3b4e_4d6a_812f_5e9cd37a1b42);

/// Layers any walk follows before stopping (one is expected).
const MAX_LAYERS: usize = 4;

static PRESENT: AtomicBool = AtomicBool::new(false);

type QueryFn = unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;

/// Whether a ReShade module has been seen. One relaxed load.
pub fn reshade_present() -> bool {
    PRESENT.load(Ordering::Relaxed)
}

/// Look for ReShade among the loaded modules (a module exporting
/// `ReShadeVersion`, the way ReShade recognises itself). A module walk:
/// call at creation calls, never per frame or under the loader lock.
/// Returns whether it is present.
pub fn detect_reshade() -> bool {
    for loaded in mhw_win::loaded_modules() {
        let module = HMODULE(loaded.base as *mut c_void);
        // SAFETY: a loaded module handle and a NUL-terminated name.
        let Some(export) =
            (unsafe { GetProcAddress(module, PCSTR(c"ReShadeVersion".as_ptr().cast())) })
        else {
            continue;
        };
        if !PRESENT.swap(true, Ordering::Relaxed) {
            // The export is a `const char*` variable: its address holds the
            // address of the text.
            // SAFETY: ReShade stays loaded for the process; the variable is
            // a pointer to a NUL-terminated string or null.
            let version = unsafe {
                let text = *(export as usize as *const *const std::ffi::c_char);
                if text.is_null() {
                    "unknown".to_string()
                } else {
                    CStr::from_ptr(text).to_string_lossy().into_owned()
                }
            };
            log::info!(target: "render", "ReShade is loaded: {} (version {version})", loaded.path);
        }
        return true;
    }
    reshade_present()
}

/// One layer down, if `pointer` is a ReShade proxy. Balanced: ReShade adds
/// a reference to what it returns and it is released here at once (the
/// proxy keeps the native object alive as long as itself).
///
/// # Safety
/// `pointer` must be a live COM object.
pub(super) unsafe fn peel(pointer: *mut c_void) -> Option<*mut c_void> {
    // SAFETY: a COM object's first word is its vtable; slot 0 is
    // QueryInterface and slot 2 Release.
    unsafe {
        let vtable = *(pointer as *const *const usize);
        let query: QueryFn = std::mem::transmute::<usize, QueryFn>(*vtable);
        let mut inner: *mut c_void = std::ptr::null_mut();
        if query(pointer, &IID_UNWRAPPED, &mut inner).is_err() || inner.is_null() {
            return None;
        }
        let release: ReleaseFn =
            std::mem::transmute::<usize, ReleaseFn>(*(*(inner as *const *const usize)).add(2));
        release(inner);
        Some(inner)
    }
}

/// Whether `object` is a ReShade proxy (it unwraps to another object).
/// `false` for null and with ReShade absent (one relaxed load).
///
/// # Safety
/// `object` must be null or a live COM object.
pub unsafe fn is_reshade_proxy(object: *mut c_void) -> bool {
    if object.is_null() || !reshade_present() {
        return false;
    }
    // SAFETY: caller guarantees a live object.
    unsafe { peel(object) }.is_some_and(|inner| inner != object)
}

/// The native object behind ReShade proxies, or `pointer` itself. With
/// ReShade absent this is one atomic load.
///
/// # Safety
/// `pointer` must be null or a live COM object.
pub unsafe fn unwrap_reshade(pointer: *mut c_void) -> *mut c_void {
    if pointer.is_null() || !reshade_present() {
        return pointer;
    }
    let mut current = pointer;
    for _ in 0..MAX_LAYERS {
        // SAFETY: `current` is live: the argument, or an object a live
        // proxy holds.
        match unsafe { peel(current) } {
            Some(inner) if inner != current => current = inner,
            _ => break,
        }
    }
    current
}

/// [`unwrap_reshade`] for a typed interface; the native object implements
/// `I` because the proxy stood in for it.
///
/// # Safety
/// As [`unwrap_reshade`].
pub unsafe fn native_interface<I: Interface>(object: I) -> I {
    // SAFETY: `object` is a live COM object.
    let raw = unsafe { unwrap_reshade(object.as_raw()) };
    if raw == object.as_raw() {
        return object;
    }
    // SAFETY: `raw` is the live native object the proxy wraps.
    unsafe { I::from_raw_borrowed(&raw) }
        .cloned()
        .unwrap_or(object)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_iid_is_reshades() {
        let bytes = IID_UNWRAPPED.to_u128().to_be_bytes();
        assert_eq!(
            bytes,
            [
                0x7f, 0x2c, 0x9a, 0x11, 0x3b, 0x4e, 0x4d, 0x6a, 0x81, 0x2f, 0x5e, 0x9c, 0xd3, 0x7a,
                0x1b, 0x42
            ]
        );
    }
}
