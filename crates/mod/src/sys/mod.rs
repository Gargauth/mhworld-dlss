//! Everything that runs inside the game (Windows only): `DllMain`, the boot
//! sequence and the wiring between the crates.
//!
//! * [`boot`]: start-up order; [`procs`]: the executable's imports;
//!   [`gate`]: the engine gate and its detours;
//! * [`runtime`]: settings store, policy, fallback, applying changes;
//!   [`reload`]: the file's hot reload;
//! * [`slot`] + [`evaluate`]: DLSS at the upscale point / temporal pass;
//!   [`live`]: per-frame state; [`fg_glue`]: frame generation's inputs;
//!   [`swap`]: factory, swapchain creation and Present;
//! * [`overlay`]: ReShade settings page; `devwire` (feature `dev`): the
//!   dev server.

mod boot;
#[cfg(feature = "dev")]
mod devwire;
mod evaluate;
mod fg_glue;
mod gate;
mod live;
mod overlay;
mod procs;
mod reload;
mod runtime;
mod slot;
mod swap;

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows::Win32::Foundation::{BOOL, HMODULE, TRUE};
use windows::Win32::System::LibraryLoader::DisableThreadLibraryCalls;
use windows::Win32::System::SystemServices::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH};

/// This library's module handle.
static OWN: AtomicUsize = AtomicUsize::new(0);

/// This library's module.
pub(crate) fn own_module() -> Option<HMODULE> {
    let base = OWN.load(Ordering::Acquire);
    (base != 0).then_some(HMODULE(base as *mut c_void))
}

/// The loader's entry point: patches the executable's KERNEL32 imports
/// (table writes only, before the game's own code can run), starts the
/// boot thread and returns. Under the loader lock nothing may load a
/// library or wait on a thread; a thread created here starts once the lock
/// is released.
///
/// # Safety
/// Called by the Windows loader.
#[no_mangle]
pub unsafe extern "system" fn DllMain(module: HMODULE, reason: u32, reserved: *mut c_void) -> BOOL {
    match reason {
        DLL_PROCESS_ATTACH => {
            // SAFETY: our own module handle.
            let _ = unsafe { DisableThreadLibraryCalls(module) };
            let base = module.0 as usize;
            OWN.store(base, Ordering::Release);
            procs::install_imports();
            if mhw_win::spawn("boot", move || boot::run(base)).is_err() {
                // Nothing will wire the mod: the hooks pass through.
                boot::finish(false);
            }
        }
        // An explicit unload (not process exit, where the other threads are
        // already gone and Streamline's could not finish its shutdown).
        DLL_PROCESS_DETACH if reserved.is_null() && runtime::fg_started() => mhw_fg::shutdown(),
        _ => {}
    }
    TRUE
}
