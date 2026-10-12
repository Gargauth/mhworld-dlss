//! Loaded modules: the live image of a DLL or the executable, its PE
//! headers and safe reads, and the process's module list in load order.

use std::fmt;
use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_BAD_LENGTH, HMODULE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, MODULEENTRY32W, TH32CS_SNAPMODULE,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GetModuleHandleW,
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};

use crate::mem;
use crate::pe::{self, Headers, PeError, RvaSource, Section};

/// A module mapped into this process, with its parsed headers.
///
/// Does not hold a reference on the module: use it for modules that stay
/// loaded (the game executable, system DLLs the game keeps). Reads check
/// that the pages are mapped, so a module unloaded underneath reads as
/// unavailable rather than faulting, barring the race described in
/// [`crate::mem`].
#[derive(Debug, Clone)]
pub struct Module {
    base: usize,
    headers: Headers,
}

/// Why a module could not be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleError {
    /// No module by that name or at that address is loaded.
    NotLoaded,
    /// Its headers could not be parsed.
    Headers(PeError),
}

impl fmt::Display for ModuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModuleError::NotLoaded => f.write_str("module not loaded"),
            ModuleError::Headers(error) => write!(f, "bad module headers: {error}"),
        }
    }
}

impl std::error::Error for ModuleError {}

/// Reads of the header page before `SizeOfImage` is known.
struct HeaderPage(usize);

impl RvaSource for HeaderPage {
    fn bytes_at(&self, rva: u32, max: usize) -> Option<&[u8]> {
        // The headers fit in the first page of any image.
        const PAGE: usize = 0x1000;
        let start = rva as usize;
        let len = max.min(PAGE.checked_sub(start)?);
        let address = self.0.checked_add(start)?;
        let len = mem::readable_len(address, len);
        // SAFETY: `len` bytes at `address` are committed and readable.
        (len > 0).then(|| unsafe { std::slice::from_raw_parts(address as *const u8, len) })
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

impl Module {
    /// The process's executable.
    pub fn main() -> Result<Module, ModuleError> {
        // SAFETY: a null name asks for the executable; no reference taken.
        let handle =
            unsafe { GetModuleHandleW(PCWSTR::null()) }.map_err(|_| ModuleError::NotLoaded)?;
        Module::at(handle.0 as usize)
    }

    /// The loaded module called `name` (e.g. `"d3d12.dll"`), without loading
    /// it.
    pub fn by_name(name: &str) -> Result<Module, ModuleError> {
        let name = wide(name);
        // SAFETY: `name` is NUL-terminated UTF-16 that outlives the call.
        let handle = unsafe { GetModuleHandleW(PCWSTR(name.as_ptr())) }
            .map_err(|_| ModuleError::NotLoaded)?;
        Module::at(handle.0 as usize)
    }

    /// The module containing `address`.
    pub fn containing(address: usize) -> Result<Module, ModuleError> {
        Module::at(module_base_of(address).ok_or(ModuleError::NotLoaded)?)
    }

    /// The module whose image starts at `base` (its `HMODULE`).
    pub fn at(base: usize) -> Result<Module, ModuleError> {
        if base == 0 {
            return Err(ModuleError::NotLoaded);
        }
        let headers = pe::parse_headers(&HeaderPage(base)).map_err(ModuleError::Headers)?;
        Ok(Module { base, headers })
    }

    /// Base address.
    pub fn base(&self) -> usize {
        self.base
    }

    /// `SizeOfImage`.
    pub fn size(&self) -> usize {
        self.headers.size_of_image as usize
    }

    /// Whether `address` lies inside the image.
    pub fn contains(&self, address: usize) -> bool {
        address >= self.base && address - self.base < self.size()
    }

    /// `FileHeader.TimeDateStamp`, the build identity.
    pub fn timestamp(&self) -> u32 {
        self.headers.timestamp
    }

    /// The parsed headers.
    pub fn headers(&self) -> &Headers {
        &self.headers
    }

    /// The section table.
    pub fn sections(&self) -> &[Section] {
        &self.headers.sections
    }

    /// The first section called `name`.
    pub fn section(&self, name: &str) -> Option<&Section> {
        self.headers.section(name)
    }

    /// Exactly `len` bytes at `rva`, if inside the image and readable.
    pub fn read(&self, rva: u32, len: usize) -> Option<&[u8]> {
        self.bytes(rva, len)
    }

    /// The `.text` section's RVA and bytes.
    pub fn text(&self) -> Option<(u32, &[u8])> {
        let text = self.section(".text")?;
        Some((text.rva, self.read(text.rva, text.mapped_len() as usize)?))
    }

    /// Absolute address of `rva`.
    pub fn address(&self, rva: u32) -> usize {
        self.base + rva as usize
    }

    /// RVA of `address`, if inside the image.
    pub fn rva_of(&self, address: usize) -> Option<u32> {
        self.contains(address)
            .then(|| u32::try_from(address - self.base).ok())
            .flatten()
    }

    /// Full path of the module file.
    pub fn path(&self) -> Option<String> {
        module_path(self.base)
    }
}

impl RvaSource for Module {
    fn bytes_at(&self, rva: u32, max: usize) -> Option<&[u8]> {
        let start = rva as usize;
        let len = max.min(self.size().checked_sub(start)?);
        let address = self.base.checked_add(start)?;
        let len = mem::readable_len(address, len);
        // SAFETY: `len` bytes at `address` lie inside the image and are
        // committed and readable; the image stays mapped (type contract).
        (len > 0).then(|| unsafe { std::slice::from_raw_parts(address as *const u8, len) })
    }
}

/// Base address of the module containing `address`, without taking a
/// reference.
pub fn module_base_of(address: usize) -> Option<usize> {
    let mut handle = HMODULE::default();
    // SAFETY: FROM_ADDRESS interprets the name argument as an address;
    // UNCHANGED_REFCOUNT takes no reference; `handle` is an out-parameter.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(address as *const u16),
            &mut handle,
        )
    }
    .ok()?;
    Some(handle.0 as usize)
}

/// Full path of the module at `base`.
pub fn module_path(base: usize) -> Option<String> {
    let mut buffer = [0u16; 1024];
    // SAFETY: `buffer` is a valid output buffer of its length.
    let len = unsafe { GetModuleFileNameW(HMODULE(base as *mut _), &mut buffer) } as usize;
    (len > 0 && len < buffer.len()).then(|| String::from_utf16_lossy(&buffer[..len]))
}

/// One entry of [`loaded_modules`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedModule {
    /// Position in load order (0 = the executable).
    pub order: usize,
    /// Base address.
    pub base: usize,
    /// Mapped size.
    pub size: usize,
    /// File name, e.g. `d3d12.dll`.
    pub name: String,
    /// Full path.
    pub path: String,
}

impl LoadedModule {
    /// The directory the module was loaded from.
    pub fn directory(&self) -> Option<&Path> {
        Path::new(&self.path).parent()
    }
}

fn utf16_until_nul(units: &[u16]) -> String {
    let len = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..len])
}

/// Every module loaded in this process, in load order.
///
/// Toolhelp lists modules in the loader's `InLoadOrderModuleList` order (the
/// executable, then ntdll, kernel32, …), taken under the loader lock, so the
/// result is a consistent snapshot. Do not call from `DllMain`: the snapshot
/// waits for the loader lock.
pub fn loaded_modules() -> Vec<LoadedModule> {
    let mut modules = Vec::new();
    // SAFETY: plain Toolhelp enumeration with a correctly sized entry; the
    // snapshot is closed before returning.
    unsafe {
        // ERROR_BAD_LENGTH means a module was loading during the snapshot;
        // the documented remedy is to try again.
        let mut snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, 0);
        for _ in 0..8 {
            match &snapshot {
                Err(error) if error.code() == ERROR_BAD_LENGTH.to_hresult() => {
                    snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, 0);
                }
                _ => break,
            }
        }
        let Ok(snapshot) = snapshot else {
            return modules;
        };
        let mut entry = MODULEENTRY32W {
            dwSize: std::mem::size_of::<MODULEENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Module32FirstW(snapshot, &mut entry).is_ok();
        while more {
            modules.push(LoadedModule {
                order: modules.len(),
                base: entry.modBaseAddr as usize,
                size: entry.modBaseSize as usize,
                name: utf16_until_nul(&entry.szModule),
                path: utf16_until_nul(&entry.szExePath),
            });
            more = Module32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    modules
}
