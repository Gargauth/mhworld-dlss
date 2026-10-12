//! Loading a library into the game with a remote `LoadLibraryW` thread.

use std::ffi::c_void;
use std::path::Path;
use std::time::Duration;

use mhw_dlss_launcher::handoff::was_loaded;
use mhw_dlss_launcher::layout::same_file_name;
use mhw_dlss_launcher::TARGET;
use windows::core::{s, Error, HRESULT, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, E_FAIL, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, MODULEENTRY32W, TH32CS_SNAPMODULE,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    CreateRemoteThread, GetExitCodeThread, GetProcessId, WaitForSingleObject,
};

use super::ui::wide;

/// How long a library is given to load inside the game before the caller
/// is asked whether to keep waiting. A `DllMain` that never returns would
/// otherwise leave the launcher waiting for ever on a game that is
/// suspended and has no window.
pub const LOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// Why an injection did not happen.
#[derive(Debug)]
pub enum InjectError {
    /// A system call failed, or the library was not loaded.
    Failed(Error),
    /// Still loading after [`LOAD_TIMEOUT`], and the wait was given up.
    TimedOut,
}

impl From<Error> for InjectError {
    fn from(error: Error) -> Self {
        InjectError::Failed(error)
    }
}

/// Run `LoadLibraryW(path)` in `process` and wait for it.
///
/// This program's own address of `LoadLibraryW` is the remote thread's
/// start routine: kernel32.dll is mapped at the same address in every
/// process of a boot session. In a process created suspended the remote
/// thread is the first to run, so the process initialises on it, and it
/// runs to completion before the game's main thread is resumed.
///
/// After each [`LOAD_TIMEOUT`] without completion `keep_waiting` decides.
///
/// # Safety
/// `process` must be a live process handle with `PROCESS_ALL_ACCESS`.
pub unsafe fn inject(
    process: HANDLE,
    path: &Path,
    mut keep_waiting: impl FnMut() -> bool,
) -> Result<(), InjectError> {
    let kernel32_name = wide("kernel32.dll");
    // SAFETY: a NUL-terminated name; kernel32 is always loaded.
    let kernel32 = unsafe { GetModuleHandleW(PCWSTR(kernel32_name.as_ptr())) }?;
    // SAFETY: a valid module handle and a static name.
    let load_library =
        unsafe { GetProcAddress(kernel32, s!("LoadLibraryW")) }.ok_or_else(Error::from_win32)?;
    // SAFETY: caller guarantees the process handle.
    let remote = unsafe { write_wide(process, &path.to_string_lossy()) }?;

    // SAFETY: `LoadLibraryW` takes one pointer and returns a pointer-sized
    // value, which is the shape of a thread start routine on x64; its
    // address is valid in the target (see above). `remote` is the path in
    // the target's memory.
    let thread = unsafe {
        CreateRemoteThread(
            process,
            None,
            0,
            Some(std::mem::transmute::<
                unsafe extern "system" fn() -> isize,
                unsafe extern "system" fn(*mut c_void) -> u32,
            >(load_library)),
            Some(remote),
            0,
            None,
        )
    };
    let thread = match thread {
        Ok(thread) => thread,
        Err(error) => {
            // SAFETY: allocated above in that process; no thread uses it.
            let _ = unsafe { VirtualFreeEx(process, remote, 0, MEM_RELEASE) };
            return Err(error.into());
        }
    };

    let millis = u32::try_from(LOAD_TIMEOUT.as_millis()).unwrap_or(u32::MAX - 1);
    loop {
        // SAFETY: the thread handle created above.
        let outcome = unsafe { WaitForSingleObject(thread, millis) };
        if outcome == WAIT_OBJECT_0 {
            break;
        }
        let error = (outcome != WAIT_TIMEOUT).then(Error::from_win32);
        if error.is_some() || !keep_waiting() {
            // The remote string stays where it is: the thread may still
            // read it.
            // SAFETY: closed once; the thread itself carries on.
            let _ = unsafe { CloseHandle(thread) };
            return Err(error.map_or(InjectError::TimedOut, InjectError::Failed));
        }
    }

    let mut code = 0u32;
    // SAFETY: the finished thread's handle; `code` receives its exit code.
    let exit_code = unsafe { GetExitCodeThread(thread, &mut code) }
        .is_ok()
        .then_some(code);
    // SAFETY: closed once; the remote string is no longer read.
    unsafe {
        let _ = CloseHandle(thread);
        let _ = VirtualFreeEx(process, remote, 0, MEM_RELEASE);
    }
    let listed = match exit_code {
        // SAFETY: caller guarantees the process handle.
        Some(0) | None => unsafe { has_module(process, path) },
        Some(_) => None,
    };
    if exit_code == Some(0) && listed == Some(true) {
        log::info!(
            target: TARGET,
            "{}: LoadLibraryW's result has a zero low half; the game's module list has the \
             library, so it is loaded",
            path.display()
        );
    }
    if was_loaded(exit_code, listed) {
        return Ok(());
    }
    let why = match listed {
        Some(_) => "LoadLibraryW returned null inside the game, and the library is not in its module list",
        None => "LoadLibraryW returned zero inside the game, and its module list could not be read to check",
    };
    Err(InjectError::Failed(Error::new(HRESULT(E_FAIL.0), why)))
}

/// Copy `text` as a NUL-terminated wide string into `process`.
///
/// # Safety
/// `process` must be a live process handle with VM access.
unsafe fn write_wide(process: HANDLE, text: &str) -> windows::core::Result<*mut c_void> {
    let units = wide(text);
    let size = std::mem::size_of_val(units.as_slice());
    // SAFETY: caller guarantees the handle; a fresh read-write allocation.
    let remote = unsafe {
        VirtualAllocEx(
            process,
            None,
            size,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        )
    };
    if remote.is_null() {
        return Err(Error::from_win32());
    }
    // SAFETY: `remote` has `size` writable bytes; `units` has `size` bytes.
    let written = unsafe { WriteProcessMemory(process, remote, units.as_ptr().cast(), size, None) };
    if let Err(error) = written {
        // SAFETY: allocated just above and not used by anything.
        let _ = unsafe { VirtualFreeEx(process, remote, 0, MEM_RELEASE) };
        return Err(error);
    }
    Ok(remote)
}

/// Whether `process` has a module loaded from `path`; `None` when its
/// module list could not be read.
///
/// # Safety
/// `process` must be a live process handle.
unsafe fn has_module(process: HANDLE, path: &Path) -> Option<bool> {
    // SAFETY: caller guarantees the handle.
    let snapshot =
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, GetProcessId(process)) }.ok()?;
    let mut entry = MODULEENTRY32W {
        dwSize: std::mem::size_of::<MODULEENTRY32W>() as u32,
        ..Default::default()
    };
    let wanted = path.to_string_lossy();
    // SAFETY: a valid snapshot; `entry` has its size set.
    let mut more = unsafe { Module32FirstW(snapshot, &mut entry) }.is_ok();
    let listed = more;
    let mut found = false;
    while more {
        let length = entry
            .szExePath
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(entry.szExePath.len());
        let loaded = String::from_utf16_lossy(entry.szExePath.get(..length).unwrap_or_default());
        if same_file_name(&loaded, &wanted) {
            found = true;
            break;
        }
        // SAFETY: as above.
        more = unsafe { Module32NextW(snapshot, &mut entry) }.is_ok();
    }
    // SAFETY: the snapshot handle, closed once.
    let _ = unsafe { CloseHandle(snapshot) };
    listed.then_some(found)
}
