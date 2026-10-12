//! Function detours: a 14-byte `jmp [rip+0]` over a function's entry into
//! a hook, and a trampoline that runs the displaced prologue and jumps back.
//!
//! Two steps, so that several detours go in together:
//!
//! 1. [`Detour::prepare`] reads the live prologue, checks that every
//!    instruction in it is one of the forms [`crate::x64`] can move, builds
//!    the trampoline within `rel32` reach of the function, and stores the
//!    trampoline's address in the caller's `original` before anything is
//!    written, so the hook finds its original from the first call.
//! 2. [`install`] writes every entry patch at once through
//!    [`crate::code::apply`]: other threads suspended, none inside a
//!    prologue, bytes compared again, all or nothing.
//!
//! An installed detour is permanent: its trampoline is never freed, because
//! a thread may be inside it at any time.

use std::ffi::c_void;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use windows::Win32::System::Memory::{VirtualAlloc, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READ};
use windows::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};

use crate::code::{self, CodePatch, PatchError};
use crate::x64::{self, PrologueError, MAX_PROLOGUE, MAX_TRAMPOLINE};
use crate::{mem, sys};

/// Why a detour could not be prepared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetourError {
    /// The function's first bytes are not readable.
    Unreadable,
    /// The prologue contains an instruction that cannot be moved.
    Prologue(PrologueError),
    /// No free memory within `rel32` reach of the function.
    NoMemoryInReach,
    /// The trampoline could not be written.
    Write,
}

impl fmt::Display for DetourError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DetourError::Unreadable => f.write_str("function entry not readable"),
            DetourError::Prologue(error) => write!(f, "prologue cannot be moved: {error}"),
            DetourError::NoMemoryInReach => {
                f.write_str("no free memory within 2 GiB of the function")
            }
            DetourError::Write => f.write_str("trampoline could not be written"),
        }
    }
}

impl std::error::Error for DetourError {}

/// A prepared (and possibly installed) detour.
#[derive(Debug)]
pub struct Detour {
    target: usize,
    trampoline: usize,
    prologue: usize,
    patch: CodePatch,
    original: &'static AtomicUsize,
    installed: bool,
}

impl Detour {
    /// Prepare a detour of the function at `target` into `hook`, storing the
    /// trampoline (the way to call the original) in `original`.
    ///
    /// # Safety
    /// `target` must be the entry of a function whose first
    /// [`Prologue::len`](x64::Prologue) bytes are not a branch target from
    /// inside the function (true of compiler prologues), and `hook` a
    /// function with exactly its ABI and signature that calls the original
    /// only through `original`.
    pub unsafe fn prepare(
        what: &'static str,
        target: usize,
        hook: *const c_void,
        original: &'static AtomicUsize,
    ) -> Result<Detour, DetourError> {
        let mut live = [0u8; MAX_PROLOGUE];
        let available = mem::readable_len(target, MAX_PROLOGUE);
        if available == 0 {
            return Err(DetourError::Unreadable);
        }
        // SAFETY: `available` bytes at `target` are committed and readable.
        if !unsafe { mem::read_bytes(target, &mut live[..available]) } {
            return Err(DetourError::Unreadable);
        }
        let plan = x64::plan(&live[..available]).map_err(DetourError::Prologue)?;
        let prologue = &live[..plan.len];

        let (code, used) =
            x64::trampoline(prologue, (target + plan.len) as u64).ok_or(DetourError::Write)?;
        let trampoline = POOL
            .lock()
            .alloc_near(target)
            .ok_or(DetourError::NoMemoryInReach)?;
        // SAFETY: the slot is ours, inside a committed block; no thread runs
        // it before `original` is published below.
        if unsafe { mem::write_bytes(trampoline, &code[..used]) }.is_err() {
            POOL.lock().free(trampoline);
            return Err(DetourError::Write);
        }

        let entry = x64::entry_patch(hook as u64, plan.len).ok_or(DetourError::Write)?;
        let patch =
            CodePatch::new(what, target, prologue, &entry[..plan.len]).ok_or(DetourError::Write)?;
        original.store(trampoline, Ordering::SeqCst);
        Ok(Detour {
            target,
            trampoline,
            prologue: plan.len,
            patch,
            original,
            installed: false,
        })
    }

    /// The detoured function's entry.
    pub fn target(&self) -> usize {
        self.target
    }

    /// The trampoline: calling it runs the original function.
    pub fn trampoline(&self) -> usize {
        self.trampoline
    }

    /// Bytes of the function's entry the detour replaces.
    pub fn prologue_len(&self) -> usize {
        self.prologue
    }

    /// The entry patch [`install`] writes.
    pub fn patch(&self) -> &CodePatch {
        &self.patch
    }

    /// Whether the entry patch is written.
    pub fn is_installed(&self) -> bool {
        self.installed
    }
}

impl Drop for Detour {
    /// A detour that was never installed gives its trampoline back and
    /// clears `original` (if it still holds this trampoline). An installed
    /// one stays, trampoline and all, for the life of the process.
    fn drop(&mut self) {
        if self.installed {
            return;
        }
        let _ =
            self.original
                .compare_exchange(self.trampoline, 0, Ordering::SeqCst, Ordering::SeqCst);
        POOL.lock().free(self.trampoline);
    }
}

/// Write the entry patches of every detour not yet installed, all or none.
/// Returns how many threads were suspended for it.
///
/// # Safety
/// As for [`Detour::prepare`] for each detour; additionally, no other code
/// may be patching the same functions concurrently.
pub unsafe fn install(detours: &mut [Detour]) -> Result<usize, PatchError> {
    let mut patches = Vec::with_capacity(detours.len());
    patches.extend(detours.iter().filter(|d| !d.installed).map(|d| d.patch));
    if patches.is_empty() {
        return Ok(0);
    }
    // SAFETY: each patch replaces a planned prologue with a jump into a hook
    // whose trampoline is ready (caller's contract from `prepare`).
    let suspended = unsafe { code::apply(&patches) }?;
    for detour in detours.iter_mut() {
        detour.installed = true;
    }
    Ok(suspended)
}

/// Trampoline slot size: a moved prologue and the jump back, rounded up.
const SLOT: usize = MAX_TRAMPOLINE.next_multiple_of(16);
/// One block of trampolines: one allocation-granularity unit.
const BLOCK: usize = 0x1_0000;

/// Trampoline memory, allocated in blocks near the functions detoured.
struct Pool {
    blocks: Vec<Block>,
}

struct Block {
    base: usize,
    used: usize,
    free: Vec<usize>,
}

static POOL: Mutex<Pool> = Mutex::new(Pool { blocks: Vec::new() });

impl Pool {
    /// A trampoline slot within `rel32` reach of `target`.
    fn alloc_near(&mut self, target: usize) -> Option<usize> {
        let (floor, ceiling, granularity) = address_space();
        let (low, high) =
            x64::reach_window(target as u64, BLOCK as u64, floor as u64, ceiling as u64);
        let in_reach = |base: usize| (base as u64) >= low && (base as u64) < high;
        // Reuse a freed slot before carving a new one.
        if let Some(slot) = self
            .blocks
            .iter_mut()
            .filter(|b| in_reach(b.base))
            .find_map(|b| b.free.pop())
        {
            return Some(slot);
        }
        if let Some(block) = self
            .blocks
            .iter_mut()
            .find(|b| in_reach(b.base) && b.used + SLOT <= BLOCK)
        {
            block.used += SLOT;
            return Some(block.base + block.used - SLOT);
        }
        let base = allocate_block_near(target, low as usize, high as usize, granularity)?;
        self.blocks.push(Block {
            base,
            used: SLOT,
            free: Vec::new(),
        });
        Some(base)
    }

    fn free(&mut self, slot: usize) {
        if let Some(block) = self
            .blocks
            .iter_mut()
            .find(|b| slot >= b.base && slot < b.base + BLOCK)
        {
            block.free.push(slot);
        }
    }
}

/// Lowest and highest application address, and the allocation granularity.
fn address_space() -> (usize, usize, usize) {
    let mut info = SYSTEM_INFO::default();
    // SAFETY: `info` is a valid out-parameter.
    unsafe { GetSystemInfo(&mut info) };
    (
        info.lpMinimumApplicationAddress as usize,
        info.lpMaximumApplicationAddress as usize,
        info.dwAllocationGranularity.max(0x1000) as usize,
    )
}

/// Reserve and commit one executable block at a free, granularity-aligned
/// address in `[low, high)`, searching outward from `target`: downward
/// first (images sit high; the space just below is usually free), then up.
fn allocate_block_near(
    target: usize,
    low: usize,
    high: usize,
    granularity: usize,
) -> Option<usize> {
    let try_at = |base: usize| -> Option<usize> {
        // SAFETY: reserving fresh memory at a free address has no effect on
        // existing memory; failure returns null.
        let got = unsafe {
            VirtualAlloc(
                Some(base as *const c_void),
                BLOCK,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_EXECUTE_READ,
            )
        };
        (!got.is_null()).then_some(got as usize)
    };

    // Downward: from the granule below the target, skipping allocations.
    let mut at = (target / granularity * granularity).checked_sub(granularity);
    while let Some(base) = at.filter(|&a| a >= low) {
        let info = sys::query(base)?;
        if info.State.0 == sys::MEM_FREE && sys::region_end(&info) >= base + BLOCK {
            if let Some(block) = try_at(base) {
                return Some(block);
            }
        }
        // Below a free region's start (it was tried), or below an
        // allocation's start (it is all taken).
        let next = if info.State.0 == sys::MEM_FREE {
            base
        } else {
            (info.AllocationBase as usize).min(base)
        };
        at = (next / granularity * granularity).checked_sub(granularity);
    }

    // Upward: from the granule above the target, region by region.
    let mut at = (target / granularity + 1) * granularity;
    while at < high {
        let info = sys::query(at)?;
        if info.State.0 == sys::MEM_FREE && sys::region_end(&info) >= at + BLOCK {
            if let Some(block) = try_at(at) {
                return Some(block);
            }
        }
        at = sys::region_end(&info)
            .max(at + 1)
            .next_multiple_of(granularity);
    }
    None
}

/// Windows-only: run on a Windows host (`cargo test -p mhw-win`).
#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Memory::{VirtualFree, MEM_RELEASE, PAGE_EXECUTE_READWRITE};

    /// `f(x) = x + 1`, with a prologue of accepted forms 16 bytes long:
    /// push rbx; sub rsp,0x20; mov rbx,rcx; mov [rsp+0x10],rbx; mov rax,rbx;
    /// then add rax,1; add rsp,0x20; pop rbx; ret.
    const PLUS_ONE: [u8; 26] = [
        0x53, 0x48, 0x83, 0xec, 0x20, 0x48, 0x89, 0xcb, 0x48, 0x89, 0x5c, 0x24, 0x10, 0x48, 0x8b,
        0xc3, 0x48, 0x83, 0xc0, 0x01, 0x48, 0x83, 0xc4, 0x20, 0x5b, 0xc3,
    ];

    static ORIGINAL: AtomicUsize = AtomicUsize::new(0);

    extern "system" fn times_ten(x: usize) -> usize {
        let original: extern "system" fn(usize) -> usize =
            // SAFETY: the trampoline runs the original `f`, same signature.
            unsafe { std::mem::transmute(ORIGINAL.load(Ordering::SeqCst)) };
        original(x) * 10
    }

    #[test]
    fn a_detoured_function_runs_the_hook_which_runs_the_original() {
        // SAFETY: a private executable page holding a complete function.
        unsafe {
            let page = VirtualAlloc(
                None,
                0x1000,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_EXECUTE_READWRITE,
            );
            assert!(!page.is_null());
            std::ptr::copy_nonoverlapping(PLUS_ONE.as_ptr(), page.cast::<u8>(), PLUS_ONE.len());
            let f: extern "system" fn(usize) -> usize = std::mem::transmute(page);
            assert_eq!(f(4), 5);

            let target = page as usize;
            let mut detours =
                [
                    Detour::prepare("plus one", target, times_ten as *const c_void, &ORIGINAL)
                        .unwrap(),
                ];
            assert_eq!(detours[0].prologue_len(), 16);
            assert!(x64::rel32_reaches(
                detours[0].trampoline() as u64,
                target as u64
            ));
            assert_eq!(ORIGINAL.load(Ordering::SeqCst), detours[0].trampoline());
            assert_eq!(f(4), 5, "nothing written before install");

            install(&mut detours).unwrap();
            assert!(detours[0].is_installed());
            assert_eq!(f(4), 50);
            // A detoured entry cannot be detoured again.
            assert!(matches!(
                Detour::prepare("again", target, times_ten as *const c_void, &ORIGINAL),
                Err(DetourError::Prologue(_))
            ));
            // The installed detour stays, so the page stays mapped for it.
        }
    }

    #[test]
    fn a_dropped_detour_frees_its_trampoline_and_clears_the_original() {
        static DROPPED: AtomicUsize = AtomicUsize::new(0);
        // SAFETY: a private executable page holding a complete function.
        unsafe {
            let page = VirtualAlloc(
                None,
                0x1000,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_EXECUTE_READWRITE,
            );
            std::ptr::copy_nonoverlapping(PLUS_ONE.as_ptr(), page.cast::<u8>(), PLUS_ONE.len());
            let detour = Detour::prepare(
                "plus one",
                page as usize,
                times_ten as *const c_void,
                &DROPPED,
            )
            .unwrap();
            drop(detour);
            assert_eq!(DROPPED.load(Ordering::SeqCst), 0);
            let _ = VirtualFree(page, 0, MEM_RELEASE);
        }
    }

    #[test]
    fn the_pool_reuses_freed_slots_and_stays_in_reach() {
        let mut pool = Pool { blocks: Vec::new() };
        let target =
            a_dropped_detour_frees_its_trampoline_and_clears_the_original as *const () as usize;
        let first = pool.alloc_near(target).unwrap();
        let second = pool.alloc_near(target).unwrap();
        assert_eq!(second, first + SLOT);
        pool.free(first);
        assert_eq!(pool.alloc_near(target), Some(first));
        for slot in [first, second] {
            assert!(x64::rel32_reaches(slot as u64, target as u64));
            assert!(x64::rel32_reaches(target as u64, (slot + SLOT) as u64));
        }
    }
}
