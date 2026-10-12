//! Import Address Table patching, by name and by ordinal, in the regular and
//! the delay-import tables.
//!
//! MHW imports only KERNEL32 statically; d3d11, d3d12, DINPUT8 and 18 more
//! DLLs come in through delay imports, and `D3D12CreateDevice` is imported
//! from d3d12.dll **by ordinal #101** (`docs/re/ReLoadInitDx11.md` §2.2), so a
//! name-only lookup never finds it.
//!
//! A delay slot starts out holding a thunk inside the importing image that
//! calls the delay-load helper, which resolves the import and **overwrites
//! the slot**. Calling that thunk as "the original" would therefore remove
//! the patch on first use. So for an unresolved delay slot the original is
//! resolved here, the way the helper would (`LoadLibraryExA(dll, 0, 0)`, then
//! `GetProcAddress` by name or ordinal), and the thunk is never called. The
//! descriptor's module-handle slot is left alone; the helper loading the DLL
//! again for another import only bumps its reference count.

use std::ffi::{c_void, CString};
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows::core::PCSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleA, GetProcAddress, LoadLibraryExA, LOAD_LIBRARY_FLAGS,
};

use crate::mem::{self, MemError};
use crate::module::Module;
use crate::pe::{self, ImportRef, ImportTable, PeError};

/// Why an import could not be patched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IatError {
    /// The import tables could not be read.
    Pe(PeError),
    /// The module does not import that function from that DLL.
    NotImported,
    /// An unresolved delay import's target could not be resolved.
    Unresolvable,
    /// A slot could not be written.
    Write(MemError),
}

impl fmt::Display for IatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IatError::Pe(error) => write!(f, "import tables unreadable: {error}"),
            IatError::NotImported => f.write_str("not imported"),
            IatError::Unresolvable => f.write_str("delay import could not be resolved"),
            IatError::Write(error) => write!(f, "slot not writable: {error}"),
        }
    }
}

impl std::error::Error for IatError {}

/// One slot changed by [`patch_iat`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IatPatch {
    /// Which table the slot is in.
    pub table: ImportTable,
    /// Address of the slot.
    pub slot: usize,
    /// What the slot held before (a delay thunk, the import, or another
    /// hook). Writing it back with [`mem::write_pointer`] undoes the patch.
    pub previous: usize,
}

/// Point every IAT slot through which `module` imports `dll!import` at
/// `replacement`, after storing the function to call on in `original`
/// (another thread may call the replacement the moment a slot changes).
///
/// A slot already holding `replacement` is left as it is, and so is
/// `original`: storing the slot's content then would make the hook its own
/// original. `original` receives the target of the first slot patched; slots
/// of one import resolve to the same function.
///
/// # Safety
/// `replacement` must have exactly the signature and ABI of the import.
pub unsafe fn patch_iat(
    module: &Module,
    dll: &str,
    import: ImportRef<'_>,
    replacement: *const c_void,
    original: &AtomicUsize,
) -> Result<Vec<IatPatch>, IatError> {
    let slots = pe::find_import(module, module.headers(), dll, import).map_err(IatError::Pe)?;
    if slots.is_empty() {
        return Err(IatError::NotImported);
    }
    let replacement = replacement as usize;
    let mut patched = Vec::with_capacity(slots.len());
    for slot in slots {
        let address = module.address(slot.slot_rva);
        // SAFETY: the slot lies inside the image (found through its tables)
        // and `read` checks it is mapped.
        let current = unsafe { mem::read::<usize>(address) }
            .ok_or(IatError::Write(MemError::Unmapped { address }))?;
        if current == replacement {
            continue;
        }
        let target = match slot.table {
            ImportTable::Delay
                if pe::delay_slot_unresolved(
                    current as u64,
                    module.base() as u64,
                    module.size() as u64,
                ) =>
            {
                resolve(dll, import).ok_or(IatError::Unresolvable)?
            }
            _ => current,
        };
        if patched.is_empty() {
            original.store(target, Ordering::SeqCst);
        }
        // SAFETY: an aligned IAT slot of a loaded image; the caller vouches
        // for the replacement's signature.
        let previous =
            unsafe { mem::write_pointer(address, replacement) }.map_err(IatError::Write)?;
        patched.push(IatPatch {
            table: slot.table,
            slot: address,
            previous,
        });
    }
    Ok(patched)
}

/// Resolve `dll!import` the way the delay-load helper does.
fn resolve(dll: &str, import: ImportRef<'_>) -> Option<usize> {
    let dll = CString::new(dll).ok()?;
    let name;
    let symbol = match import {
        ImportRef::Name(text) => {
            name = CString::new(text).ok()?;
            PCSTR(name.as_ptr().cast())
        }
        // MAKEINTRESOURCEA: an ordinal is passed as a pointer value < 0x10000.
        ImportRef::Ordinal(ordinal) => PCSTR(usize::from(ordinal) as *const u8),
    };
    // SAFETY: `dll` and `name` are NUL-terminated and outlive the calls; an
    // already loaded module is used as is, otherwise it is loaded with the
    // standard search order exactly as the game's helper would.
    unsafe {
        let module = match GetModuleHandleA(PCSTR(dll.as_ptr().cast())) {
            Ok(module) => module,
            Err(_) => LoadLibraryExA(
                PCSTR(dll.as_ptr().cast()),
                HANDLE::default(),
                LOAD_LIBRARY_FLAGS(0),
            )
            .ok()?,
        };
        GetProcAddress(module, symbol).map(|f| f as usize)
    }
}
