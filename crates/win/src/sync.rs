//! Small thread and event helpers.

use std::io;
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    CreateEventW, EnterCriticalSection, GetCurrentThreadId, LeaveCriticalSection, ResetEvent,
    SetEvent, WaitForSingleObject, CRITICAL_SECTION, INFINITE,
};

/// An unnamed Win32 event, closed on drop. Usable where a `HANDLE` is
/// needed (fence completion, `SetEventOnCompletion`, waits mixed with the
/// engine's own handles).
#[derive(Debug)]
pub struct Event(HANDLE);

// SAFETY: an event handle may be used from any thread.
unsafe impl Send for Event {}
// SAFETY: as above; all operations on it are thread-safe kernel calls.
unsafe impl Sync for Event {}

/// Result of [`Event::wait`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// The event was signalled.
    Signaled,
    /// The timeout elapsed first.
    TimedOut,
    /// The wait failed (the handle is invalid or abandoned).
    Failed,
}

impl Event {
    /// A new event: manual-reset (stays signalled until [`Event::reset`])
    /// or auto-reset (released waits reset it), initially unsignalled.
    pub fn new(manual_reset: bool) -> io::Result<Event> {
        // SAFETY: unnamed event with default security.
        let handle = unsafe { CreateEventW(None, manual_reset, false, PCWSTR::null()) }
            .map_err(|_| io::Error::last_os_error())?;
        Ok(Event(handle))
    }

    /// Signal the event.
    pub fn set(&self) -> bool {
        // SAFETY: a valid event handle owned by `self`.
        unsafe { SetEvent(self.0) }.is_ok()
    }

    /// Unsignal the event.
    pub fn reset(&self) -> bool {
        // SAFETY: a valid event handle owned by `self`.
        unsafe { ResetEvent(self.0) }.is_ok()
    }

    /// Wait until signalled, at most `timeout` (`None`: indefinitely).
    /// Timeouts beyond ~49 days are clamped just below `INFINITE`.
    pub fn wait(&self, timeout: Option<Duration>) -> Wait {
        let millis = match timeout {
            None => INFINITE,
            Some(t) => u32::try_from(t.as_millis())
                .unwrap_or(INFINITE - 1)
                .min(INFINITE - 1),
        };
        // SAFETY: a valid event handle owned by `self`.
        match unsafe { WaitForSingleObject(self.0, millis) } {
            WAIT_OBJECT_0 => Wait::Signaled,
            WAIT_TIMEOUT => Wait::TimedOut,
            _ => Wait::Failed,
        }
    }

    /// The raw handle, valid as long as `self`.
    pub fn handle(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by `self` and closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Spawn a thread named `mhw-<name>` (the name shows in debuggers and crash
/// dumps; std sets it as the thread description).
pub fn spawn<T, F>(name: &str, f: F) -> io::Result<JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name(format!("mhw-{name}"))
        .spawn(f)
}

/// The calling thread's Win32 id (as in logs, debuggers and Toolhelp).
pub fn current_thread_id() -> u32 {
    // SAFETY: no preconditions.
    unsafe { GetCurrentThreadId() }
}

/// A `CRITICAL_SECTION` another module owns (the game's), held until drop.
#[derive(Debug)]
pub struct ForeignLock(*mut CRITICAL_SECTION);

impl ForeignLock {
    /// Enter the critical section at `address`.
    ///
    /// # Safety
    ///
    /// `address` is an initialised `CRITICAL_SECTION` that outlives the
    /// guard.
    pub unsafe fn enter(address: usize) -> ForeignLock {
        let section = address as *mut CRITICAL_SECTION;
        // SAFETY: the caller's contract.
        unsafe { EnterCriticalSection(section) };
        ForeignLock(section)
    }
}

impl Drop for ForeignLock {
    fn drop(&mut self) {
        // SAFETY: entered by `enter` on this thread.
        unsafe { LeaveCriticalSection(self.0) };
    }
}
