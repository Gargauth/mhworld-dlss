//! Import Address Table patching.
//!
//! MonsterHunterWorld.exe statically imports only KERNEL32.dll and pulls
//! d3d11.dll / d3d12.dll / DINPUT8.dll in by name at runtime, so every renderer
//! entry point the game uses is resolved through KERNEL32!GetProcAddress.
//! Patching that one IAT slot is enough to intercept all of them.
//!
//! Every address here comes out of the target image's own headers, so each RVA
//! is bounds-checked against SizeOfImage before it is dereferenced: a bad
//! handle or a malformed header must fail the patch, not fault the game.

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::Diagnostics::Debug::{
    IMAGE_DIRECTORY_ENTRY_IMPORT, IMAGE_NT_HEADERS64,
};
use windows::Win32::System::Memory::{VirtualProtect, PAGE_PROTECTION_FLAGS, PAGE_READWRITE};
use windows::Win32::System::SystemServices::{
    IMAGE_DOS_HEADER, IMAGE_IMPORT_BY_NAME, IMAGE_IMPORT_DESCRIPTOR,
};

const IMAGE_ORDINAL_FLAG64: u64 = 1 << 63;

struct Image {
    base: *const u8,
    size: usize,
}

impl Image {
    /// Validate the PE headers of a loaded module and capture its extent.
    unsafe fn open(module: HMODULE) -> Option<(Self, *const IMAGE_NT_HEADERS64)> {
        let base = module.0 as *const u8;
        if base.is_null() {
            return None;
        }

        let dos = base as *const IMAGE_DOS_HEADER;
        if (*dos).e_magic != 0x5A4D {
            return None;
        }

        let lfanew = (*dos).e_lfanew;
        if lfanew < 0 {
            return None;
        }

        let nt = base.offset(lfanew as isize) as *const IMAGE_NT_HEADERS64;
        if (*nt).Signature != 0x0000_4550 {
            return None;
        }

        let size = (*nt).OptionalHeader.SizeOfImage as usize;
        if size == 0 {
            return None;
        }

        Some((Image { base, size }, nt))
    }

    /// Resolve an RVA, refusing anything that would read past the image.
    unsafe fn at<T>(&self, rva: u32) -> Option<*const T> {
        let rva = rva as usize;
        if rva == 0 || rva.checked_add(std::mem::size_of::<T>())? > self.size {
            return None;
        }
        Some(self.base.add(rva) as *const T)
    }

    /// Read a NUL-terminated string at an RVA, bounded by the image extent.
    unsafe fn str_at(&self, rva: u32) -> Option<&str> {
        let start = rva as usize;
        if start == 0 || start >= self.size {
            return None;
        }
        let bytes = std::slice::from_raw_parts(self.base.add(start), self.size - start);
        let end = bytes.iter().position(|&b| b == 0)?;
        std::str::from_utf8(&bytes[..end]).ok()
    }
}

/// Overwrite the IAT entry for `dll!func` in `module` with `replacement`,
/// returning the original pointer, which is in `original` before the entry
/// changes: the replacement can be called from another thread at once.
///
/// # Safety
/// `module` must be the base of a loaded PE image, and `replacement` a
/// function with the signature of the import it replaces.
pub unsafe fn patch(
    module: HMODULE,
    dll: &str,
    func: &str,
    replacement: *const c_void,
    original: &AtomicUsize,
) -> Option<*const c_void> {
    let (image, nt) = Image::open(module)?;

    let dir = (*nt).OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT.0 as usize];
    let mut desc: *const IMAGE_IMPORT_DESCRIPTOR = image.at(dir.VirtualAddress)?;
    let wanted_dll = dll.to_ascii_lowercase();

    while (*desc).Name != 0 {
        let this_dll = image.str_at((*desc).Name)?.to_ascii_lowercase();
        if this_dll == wanted_dll {
            // OriginalFirstThunk keeps the import names; FirstThunk is the live
            // IAT the loader has already overwritten with resolved addresses.
            // A bound import has no name table, so fall back to the IAT itself.
            let name_rva = match (*desc).Anonymous.OriginalFirstThunk {
                0 => (*desc).FirstThunk,
                rva => rva,
            };
            let mut names: *const u64 = image.at(name_rva)?;
            let mut slots = image.at::<*const c_void>((*desc).FirstThunk)? as *mut *const c_void;

            while *names != 0 {
                if *names & IMAGE_ORDINAL_FLAG64 == 0 {
                    let by_name: *const IMAGE_IMPORT_BY_NAME =
                        image.at((*names & 0xFFFF_FFFF) as u32)?;
                    // IMAGE_IMPORT_BY_NAME is a Hint u16 followed by the name.
                    let name_offset = (by_name as usize - image.base as usize
                        + std::mem::offset_of!(IMAGE_IMPORT_BY_NAME, Name))
                        as u32;
                    if image.str_at(name_offset) == Some(func) {
                        return write_slot(slots, replacement, original);
                    }
                }
                names = names.add(1);
                slots = slots.add(1);
            }
        }
        desc = desc.add(1);
    }
    None
}

/// Swap one pointer-sized IAT slot, restoring the original page protection.
unsafe fn write_slot(
    slot: *mut *const c_void,
    replacement: *const c_void,
    kept: &AtomicUsize,
) -> Option<*const c_void> {
    let width = std::mem::size_of::<*const c_void>();
    let mut previous = PAGE_PROTECTION_FLAGS(0);

    VirtualProtect(slot as *const c_void, width, PAGE_READWRITE, &mut previous).ok()?;
    let original = *slot;
    if original != replacement {
        kept.store(original as usize, Ordering::SeqCst);
        *slot = replacement;
    }

    let mut restored = PAGE_PROTECTION_FLAGS(0);
    let _ = VirtualProtect(slot as *const c_void, width, previous, &mut restored);

    Some(original)
}
