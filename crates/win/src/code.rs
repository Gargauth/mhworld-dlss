//! Writing over code other threads may be executing: [`ThreadSuspender`] and
//! [`apply`], all-or-nothing.
//!
//! The bytes a detour replaces are the entries of functions the engine is
//! free to be calling. Written from a thread of ours while the engine runs,
//! fourteen bytes go in over several instructions' worth of time, and a
//! thread that fetches the entry half way through executes half of each.
//! So every other thread is suspended for the write, and none may be inside
//! a range being written.

use std::ffi::c_void;
use std::fmt;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::Debug::CONTEXT_CONTROL_AMD64;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, OpenThread, THREAD_GET_CONTEXT,
    THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
};

use crate::sys;

/// Every other thread of this process, suspended until drop.
///
/// While one exists the calling thread must not allocate, log, or take any
/// lock another thread might hold: a suspended thread may own the heap lock
/// or the logger's, and waiting on it never ends. Build everything needed
/// first, suspend, write, drop, then report.
///
/// Not covered: a thread created after the list is taken runs throughout.
/// If the thread list cannot be taken, nothing is suspended and
/// [`ThreadSuspender::suspended`] is zero.
#[derive(Debug)]
pub struct ThreadSuspender {
    threads: Vec<HANDLE>,
    suspended: Vec<bool>,
}

impl ThreadSuspender {
    /// Open and suspend every other thread of the process.
    pub fn suspend_others() -> ThreadSuspender {
        let threads = other_threads();
        let mut suspended = vec![false; threads.len()];
        // From here until drop: no allocation, no logging, no locks.
        for (thread, done) in threads.iter().zip(suspended.iter_mut()) {
            // SAFETY: `thread` is a handle opened with THREAD_SUSPEND_RESUME.
            *done = unsafe { sys::SuspendThread(thread.0) } != u32::MAX;
        }
        ThreadSuspender { threads, suspended }
    }

    /// How many threads are suspended.
    pub fn suspended(&self) -> usize {
        self.suspended.iter().filter(|done| **done).count()
    }

    /// Whether any suspended thread's instruction pointer lies inside one of
    /// `ranges` (`[start, end)`).
    pub fn any_inside(&self, ranges: &[(usize, usize)]) -> bool {
        self.any_ip(|rip| ranges.iter().any(|&(start, end)| rip >= start && rip < end))
    }

    /// Whether `test` holds for any suspended thread's instruction pointer.
    ///
    /// Reading a thread's context is also what makes its suspension
    /// complete: `SuspendThread` only asks, and the context cannot be read
    /// until the thread has actually stopped. A thread whose context cannot
    /// be read is assumed to be elsewhere. `test` runs with the other
    /// threads suspended (no allocation, logging or locks).
    pub fn any_ip(&self, mut test: impl FnMut(usize) -> bool) -> bool {
        // SAFETY: CONTEXT is plain data; all-zero is a valid initial value.
        let mut context: sys::AlignedContext = unsafe { std::mem::zeroed() };
        for (thread, done) in self.threads.iter().zip(&self.suspended) {
            if !*done {
                continue;
            }
            context.0.ContextFlags = CONTEXT_CONTROL_AMD64;
            // SAFETY: the handle has THREAD_GET_CONTEXT; the context is
            // 16-byte aligned as the call requires.
            if unsafe { sys::GetThreadContext(thread.0, &mut context.0) } == 0 {
                continue;
            }
            if test(context.0.Rip as usize) {
                return true;
            }
        }
        false
    }
}

impl Drop for ThreadSuspender {
    fn drop(&mut self) {
        for (thread, done) in self.threads.iter().zip(&self.suspended) {
            if *done {
                // SAFETY: resumes exactly the threads this value suspended.
                unsafe { sys::ResumeThread(thread.0) };
            }
        }
        for thread in &self.threads {
            // SAFETY: each handle was opened by `other_threads` and is
            // closed once, here.
            let _ = unsafe { CloseHandle(*thread) };
        }
    }
}

/// Handles to every other thread of this process, opened for suspending.
fn other_threads() -> Vec<HANDLE> {
    let mut threads = Vec::with_capacity(128);
    // SAFETY: plain Toolhelp enumeration with a correctly sized entry; every
    // handle opened is returned to the caller, the snapshot is closed.
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else {
            return threads;
        };
        let process = GetCurrentProcessId();
        let this = GetCurrentThreadId();
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut more = Thread32First(snapshot, &mut entry).is_ok();
        while more {
            if entry.th32OwnerProcessID == process && entry.th32ThreadID != this {
                if let Ok(thread) = OpenThread(
                    THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION,
                    false,
                    entry.th32ThreadID,
                ) {
                    threads.push(thread);
                }
            }
            more = Thread32Next(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    threads
}

/// Longest single code patch.
pub const MAX_PATCH: usize = 32;

/// One change to code: where, what must be there at the moment of writing,
/// and what goes there instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodePatch {
    /// What the patch is, for reports.
    pub what: &'static str,
    /// First byte.
    pub address: usize,
    /// Bytes replaced.
    pub len: usize,
    /// Bytes that must be there (first `len`).
    pub expect: [u8; MAX_PATCH],
    /// Bytes written (first `len`).
    pub bytes: [u8; MAX_PATCH],
}

impl CodePatch {
    /// A patch replacing `expect` with `bytes` at `address`; `None` unless
    /// both have the same length of 1..=[`MAX_PATCH`].
    pub fn new(what: &'static str, address: usize, expect: &[u8], bytes: &[u8]) -> Option<Self> {
        let len = expect.len();
        if len == 0 || len > MAX_PATCH || bytes.len() != len {
            return None;
        }
        let mut patch = CodePatch {
            what,
            address,
            len,
            expect: [0; MAX_PATCH],
            bytes: [0; MAX_PATCH],
        };
        patch.expect[..len].copy_from_slice(expect);
        patch.bytes[..len].copy_from_slice(bytes);
        Some(patch)
    }

    /// The same patch, undone.
    pub fn reversed(&self) -> Self {
        CodePatch {
            expect: self.bytes,
            bytes: self.expect,
            ..*self
        }
    }

    fn range(&self) -> (usize, usize) {
        (self.address, self.address.saturating_add(self.len))
    }
}

/// Why a set of patches was not applied. In every case nothing is left
/// written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchError {
    /// A suspended thread was executing inside one of the ranges; retry
    /// later.
    Busy,
    /// Patch `index` is not on committed executable memory right now (an
    /// unpacker may still have the page open for writing).
    NotCode {
        /// Index into the patch list.
        index: usize,
        /// The page protection found.
        protection: u32,
    },
    /// Patch `index`'s page could not be made writable.
    Protect {
        /// Index into the patch list.
        index: usize,
    },
    /// With the page writable, patch `index`'s bytes were not the expected
    /// ones.
    Changed {
        /// Index into the patch list.
        index: usize,
        /// What was there (first `len` bytes).
        found: [u8; MAX_PATCH],
    },
}

impl fmt::Display for PatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PatchError::Busy => f.write_str("a thread was executing inside a patched range"),
            PatchError::NotCode { index, protection } => {
                write!(
                    f,
                    "patch {index} is not on executable code (protection {protection:#x})"
                )
            }
            PatchError::Protect { index } => write!(f, "patch {index}: page not writable"),
            PatchError::Changed { index, .. } => write!(f, "patch {index}: bytes changed"),
        }
    }
}

impl std::error::Error for PatchError {}

/// Make every patch, or none, with the process's other threads suspended.
/// Returns how many threads were suspended for it.
///
/// * If any suspended thread is inside a range, nothing changes (`Busy`).
/// * Each range must be committed executable code at that moment.
/// * With the page writable the bytes are compared again, because the check
///   that led here was made earlier and from outside (`Changed`).
/// * The old protection goes back only if the page still has the one set
///   here; otherwise someone else changed it in between and theirs wins.
/// * A failure part way through reverses the earlier patches.
///
/// # Safety
/// The patches must be sound: replacing `expect` by `bytes` at each address
/// must leave code every thread can execute.
pub unsafe fn apply(patches: &[CodePatch]) -> Result<usize, PatchError> {
    let suspender = ThreadSuspender::suspend_others();
    let busy = suspender.any_ip(|rip| {
        patches.iter().any(|patch| {
            let (start, end) = patch.range();
            rip >= start && rip < end
        })
    });
    let outcome = if busy {
        Err(PatchError::Busy)
    } else {
        // SAFETY: forwarded from the caller; all other threads are stopped.
        unsafe { apply_frozen(patches) }
    };
    let count = suspender.suspended();
    drop(suspender);
    outcome.map(|()| count)
}

unsafe fn apply_frozen(patches: &[CodePatch]) -> Result<(), PatchError> {
    for (index, patch) in patches.iter().enumerate() {
        if let Err(error) = patch_one(index, patch) {
            // Put back what the earlier ones replaced. Their pages were
            // writable a moment ago; one that will not open again is left,
            // and the error names the patch that failed.
            for done in patches[..index].iter().rev() {
                let _ = patch_one(0, &done.reversed());
            }
            return Err(error);
        }
    }
    Ok(())
}

fn patch_one(index: usize, patch: &CodePatch) -> Result<(), PatchError> {
    let address = patch.address as *const c_void;
    let Some(info) = sys::query(patch.address) else {
        return Err(PatchError::NotCode {
            index,
            protection: 0,
        });
    };
    let protect = info.Protect.0;
    if info.State.0 != sys::MEM_COMMIT
        || protect & sys::EXECUTABLE == 0
        || protect & sys::PAGE_GUARD != 0
        || patch.address.saturating_add(patch.len) > sys::region_end(&info)
    {
        return Err(PatchError::NotCode {
            index,
            protection: protect,
        });
    }

    let mut previous = 0u32;
    // SAFETY: committed executable pages of this process (checked above);
    // `previous` is a valid out-parameter.
    if unsafe {
        sys::VirtualProtect(
            address,
            patch.len,
            sys::PAGE_EXECUTE_READWRITE,
            &mut previous,
        )
    } == 0
    {
        return Err(PatchError::Protect { index });
    }
    // SAFETY: the range is committed and now writable; every other thread is
    // suspended outside it, so no one else reads or writes it meanwhile.
    let target = unsafe { std::slice::from_raw_parts_mut(patch.address as *mut u8, patch.len) };
    let outcome = if target == &patch.expect[..patch.len] {
        target.copy_from_slice(&patch.bytes[..patch.len]);
        Ok(())
    } else {
        let mut found = [0u8; MAX_PATCH];
        found[..patch.len].copy_from_slice(target);
        Err(PatchError::Changed { index, found })
    };
    if let Some(now) = sys::query(patch.address) {
        if now.Protect.0 == sys::PAGE_EXECUTE_READWRITE && previous != sys::PAGE_EXECUTE_READWRITE {
            let mut ignored = 0u32;
            // SAFETY: restores the protection this function replaced.
            unsafe { sys::VirtualProtect(address, patch.len, previous, &mut ignored) };
        }
    }
    sys::flush_instructions(patch.address, patch.len);
    outcome
}

/// Windows-only: run on a Windows host (`cargo test -p mhw-win`).
#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Memory::{
        VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READ,
        PAGE_READWRITE,
    };

    #[test]
    fn a_failing_patch_leaves_every_patch_unwritten() {
        // SAFETY: a private page; nothing executes it.
        unsafe {
            let page =
                VirtualAlloc(None, 0x1000, MEM_RESERVE | MEM_COMMIT, PAGE_EXECUTE_READ) as usize;
            assert_ne!(page, 0);
            let good = CodePatch::new("good", page, &[0; 4], &[1; 4]).unwrap();
            let stale = CodePatch::new("stale", page + 16, &[9; 4], &[2; 4]).unwrap();
            let error = apply(&[good, stale]).unwrap_err();
            assert!(matches!(error, PatchError::Changed { index: 1, .. }));
            assert_eq!(std::slice::from_raw_parts(page as *const u8, 4), &[0; 4]);

            assert!(apply(&[good]).is_ok());
            assert_eq!(std::slice::from_raw_parts(page as *const u8, 4), &[1; 4]);
            // Protection is back to what it was.
            assert_eq!(sys::query(page).unwrap().Protect.0, sys::PAGE_EXECUTE_READ);
            let _ = VirtualFree(page as *mut c_void, 0, MEM_RELEASE);

            let data =
                VirtualAlloc(None, 0x1000, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) as usize;
            let patch = CodePatch::new("data", data, &[0; 4], &[1; 4]).unwrap();
            assert!(matches!(
                apply(&[patch]),
                Err(PatchError::NotCode { index: 0, .. })
            ));
            let _ = VirtualFree(data as *mut c_void, 0, MEM_RELEASE);
        }
    }

    #[test]
    fn patch_lengths_are_checked() {
        assert!(CodePatch::new("empty", 0x1000, &[], &[]).is_none());
        assert!(CodePatch::new("uneven", 0x1000, &[0; 2], &[0; 3]).is_none());
        assert!(CodePatch::new("long", 0x1000, &[0; MAX_PATCH + 1], &[0; MAX_PATCH + 1]).is_none());
        let patch = CodePatch::new("ok", 0x1000, &[1, 2], &[3, 4]).unwrap();
        assert_eq!(&patch.reversed().bytes[..2], &[1, 2]);
    }

    #[test]
    fn the_suspender_stops_and_resumes_other_threads() {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let worker =
            std::thread::spawn(move || while !flag.load(std::sync::atomic::Ordering::Relaxed) {});
        let suspender = ThreadSuspender::suspend_others();
        assert!(suspender.suspended() >= 1);
        assert!(suspender.any_ip(|_| true));
        assert!(!suspender.any_inside(&[(0, 0)]));
        drop(suspender);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        worker.join().unwrap();
    }
}
