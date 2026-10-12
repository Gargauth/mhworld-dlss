//! The game process, created suspended, and the named events the mod
//! signals.

use std::path::Path;
use std::time::Duration;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CreateEventW, CreateProcessW, ResetEvent, ResumeThread, TerminateProcess, WaitForSingleObject,
    CREATE_SUSPENDED, INFINITE, PROCESS_INFORMATION, STARTUPINFOW,
};

use super::ui::wide;

/// The game, started suspended. Its handles are closed on drop; the game
/// itself carries on.
#[derive(Debug)]
pub struct Game {
    process: HANDLE,
    /// The main thread, until it is resumed.
    thread: Option<HANDLE>,
    /// The process id.
    pub pid: u32,
}

impl Game {
    /// Create the game's process with its main thread suspended, in the
    /// game's own folder (MHW resolves its data relative to it), with this
    /// program's environment.
    pub fn start_suspended(executable: &Path, command_line: &str) -> windows::core::Result<Game> {
        // These buffers must outlive the call: a pointer built from a
        // temporary inside the argument list would dangle before
        // CreateProcessW reads it. The command line must be writable.
        let mut line = wide(command_line);
        let target = wide(&executable.to_string_lossy());
        let directory = executable
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| wide(&dir.to_string_lossy()));
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut info = PROCESS_INFORMATION::default();
        // SAFETY: every pointer is to a live, NUL-terminated buffer above;
        // `info` receives the handles.
        unsafe {
            CreateProcessW(
                PCWSTR(target.as_ptr()),
                PWSTR(line.as_mut_ptr()),
                None,
                None,
                false,
                CREATE_SUSPENDED,
                None,
                match &directory {
                    Some(buffer) => PCWSTR(buffer.as_ptr()),
                    None => PCWSTR::null(),
                },
                &startup,
                &mut info,
            )?;
        }
        Ok(Game {
            process: info.hProcess,
            thread: Some(info.hThread),
            pid: info.dwProcessId,
        })
    }

    /// The process handle, valid as long as `self`.
    pub fn handle(&self) -> HANDLE {
        self.process
    }

    /// Let the main thread run. Returns whether it was resumed.
    pub fn resume(&mut self) -> bool {
        let Some(thread) = self.thread.take() else {
            return false;
        };
        // SAFETY: the main thread's handle, owned by `self` and closed once.
        unsafe {
            let resumed = ResumeThread(thread) != u32::MAX;
            let _ = CloseHandle(thread);
            resumed
        }
    }

    /// End the game (it never left its suspended start).
    pub fn terminate(&self) {
        // SAFETY: the process handle owned by `self`.
        let _ = unsafe { TerminateProcess(self.process, 1) };
    }

    /// Wait until the game exits.
    pub fn wait_exit(&self) {
        // SAFETY: the process handle owned by `self`.
        unsafe { WaitForSingleObject(self.process, INFINITE) };
    }
}

impl Drop for Game {
    fn drop(&mut self) {
        // SAFETY: the handles are owned by `self` and closed once.
        unsafe {
            if let Some(thread) = self.thread.take() {
                let _ = CloseHandle(thread);
            }
            let _ = CloseHandle(self.process);
        }
    }
}

/// A named manual-reset event the mod signals ([`BOOTED_EVENT`],
/// [`RENDERER_READY_EVENT`]). Created before the game starts, so the mod
/// can open it by name the moment it has something to say.
///
/// [`BOOTED_EVENT`]: mhw_dlss_launcher::handoff::BOOTED_EVENT
/// [`RENDERER_READY_EVENT`]: mhw_dlss_launcher::handoff::RENDERER_READY_EVENT
#[derive(Debug)]
pub struct Signal(HANDLE);

impl Signal {
    /// Create the manual-reset event `name`, unsignalled (an event of that
    /// name left signalled by an earlier launch is reset).
    pub fn create(name: &str) -> windows::core::Result<Signal> {
        let name = wide(name);
        // SAFETY: a NUL-terminated name that outlives the call.
        let event = unsafe { CreateEventW(None, true, false, PCWSTR(name.as_ptr())) }?;
        // SAFETY: the event handle just created; owned by the result.
        let _ = unsafe { ResetEvent(event) };
        Ok(Signal(event))
    }

    /// Wait for the signal, at most `timeout`. Returns whether it came.
    pub fn wait(&self, timeout: Duration) -> bool {
        let millis = u32::try_from(timeout.as_millis())
            .unwrap_or(INFINITE - 1)
            .min(INFINITE - 1);
        // SAFETY: the event handle owned by `self`.
        unsafe { WaitForSingleObject(self.0, millis) == WAIT_OBJECT_0 }
    }
}

impl Drop for Signal {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by `self` and closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}
