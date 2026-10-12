//! The few kernel32 calls made while other threads are suspended, declared
//! directly so that nothing stands between this code and the system: no
//! error object is built, nothing is allocated, nothing is formatted. A
//! thread stopped at an arbitrary instruction may hold the heap lock or the
//! log lock, and anything that takes either while it is stopped waits for
//! ever. (`windows` wraps failures in `windows::core::Error`, which queries
//! COM error info.)

use std::ffi::c_void;
use windows::Win32::System::Diagnostics::Debug::CONTEXT;
use windows::Win32::System::Memory::MEMORY_BASIC_INFORMATION;

#[link(name = "kernel32")]
extern "system" {
    pub fn VirtualProtect(address: *const c_void, size: usize, new: u32, old: *mut u32) -> i32;
    pub fn VirtualQuery(
        address: *const c_void,
        info: *mut MEMORY_BASIC_INFORMATION,
        length: usize,
    ) -> usize;
    pub fn SuspendThread(thread: *mut c_void) -> u32;
    pub fn ResumeThread(thread: *mut c_void) -> u32;
    pub fn GetThreadContext(thread: *mut c_void, context: *mut CONTEXT) -> i32;
    pub fn FlushInstructionCache(process: *mut c_void, address: *const c_void, size: usize) -> i32;
    pub fn GetCurrentProcess() -> *mut c_void;
}

/// `CONTEXT` with the 16-byte alignment `GetThreadContext` requires (the
/// `windows` definition does not carry it).
#[repr(C, align(16))]
pub struct AlignedContext(pub CONTEXT);

pub const MEM_COMMIT: u32 = 0x1000;
pub const MEM_FREE: u32 = 0x1_0000;
pub const PAGE_NOACCESS: u32 = 0x01;
pub const PAGE_READWRITE: u32 = 0x04;
pub const PAGE_WRITECOPY: u32 = 0x08;
pub const PAGE_EXECUTE: u32 = 0x10;
pub const PAGE_EXECUTE_READ: u32 = 0x20;
pub const PAGE_EXECUTE_READWRITE: u32 = 0x40;
pub const PAGE_EXECUTE_WRITECOPY: u32 = 0x80;
pub const PAGE_GUARD: u32 = 0x100;

/// Any executable protection.
pub const EXECUTABLE: u32 =
    PAGE_EXECUTE | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;
/// Any writable protection.
pub const WRITABLE: u32 =
    PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;

/// The region containing `address`, or `None` when it cannot be queried.
pub fn query(address: usize) -> Option<MEMORY_BASIC_INFORMATION> {
    let mut info = MEMORY_BASIC_INFORMATION::default();
    let size = std::mem::size_of::<MEMORY_BASIC_INFORMATION>();
    // SAFETY: `info` is a valid out-parameter of the declared size; querying
    // any address is allowed and fails cleanly for invalid ones.
    let got = unsafe { VirtualQuery(address as *const c_void, &mut info, size) };
    (got == size).then_some(info)
}

/// End of the region `info` describes.
pub fn region_end(info: &MEMORY_BASIC_INFORMATION) -> usize {
    (info.BaseAddress as usize).saturating_add(info.RegionSize)
}

/// Whether a committed region with protection `protect` can be read.
pub fn readable_protection(protect: u32) -> bool {
    protect != 0 && protect & (PAGE_NOACCESS | PAGE_GUARD) == 0
}

/// Flush the instruction cache for `[address, address + len)`.
pub fn flush_instructions(address: usize, len: usize) {
    // SAFETY: flushing is a hint to the processor about our own process and
    // touches no memory.
    unsafe {
        FlushInstructionCache(GetCurrentProcess(), address as *const c_void, len);
    }
}
