//! The engine's own jitter.
//!
//! The projection cannot be offset from outside -- read the camera constant
//! buffer, add a sub-pixel shift to `fProj` and `fViewProj`, hand the shaders
//! a substitute buffer. The picture moves and DLSS resolves it, and the
//! engine's own rendering breaks: whole meshes go black for a frame, with
//! DLSS switched off entirely.
//!
//! The reason is in the game's code. `CBViewProjection` is filled by
//! one function, at RVA 0x23d0e70, which takes the unjittered projection and a
//! separate 4x4 **jitter matrix** as its ninth argument and derives everything
//! else from the product:
//!
//! ```text
//!   jittered    = proj * jitter
//!   fViewProj   = view * jittered          fNoJitterProj = proj
//!   fProj       = jittered                 fNoJitterViewProj = view * proj
//!   fViewProjI, fProjI, fViewProjIViewProjPF, fZToLinear,
//!   fPassThrough, fLODBasePos, and the separate CBViewFrustum buffer
//! ```
//!
//! So offsetting `fProj` after the fact leaves a dozen derived quantities --
//! the inverses above all -- describing a camera that no longer exists. Any
//! shader that round-trips through one of them, which is every deferred pass,
//! computes positions that disagree with the depth buffer. That is the black
//! geometry, and no amount of patching individual fields can fix it, because
//! `CBViewFrustum` is a different buffer and the previous-frame matrices are
//! built from the same product.
//!
//! The engine is already set up to do this properly. The jitter matrix is
//! identity except for two elements, and the caller at RVA 0x228f3b0 -- the
//! only one of the eleven that passes anything but identity -- builds it from
//! two floats in a global render context:
//!
//! ```text
//!   0x228f3f4  mov   rax,[rip+...]      ; the render context
//!   0x228faaf  movss xmm1,[rax+0x38]    ; jitter x
//!   0x228facc  movss xmm1,[rax+0x3c]    ; jitter y
//!              m[3][0] = x / viewport width
//!              m[3][1] = y / viewport height
//! ```
//!
//! Writing those two floats is therefore the whole job: the engine builds the
//! matrix, derives every dependent field consistently, and leaves the
//! `fNoJitter` ones clean -- exactly what DLSS wants to be handed.
//!
//! Writing the two floats patches no code, and the address of the context
//! is read out of the instruction that loads it rather than hardcoded. The
//! *moment* of the write is a different matter: it is made from a detour on
//! the camera setup, further down, and that detour is code written over the
//! engine's. Whether any of it happens is decided in `gate.rs`, for every
//! site this mod depends on at once; nothing here checks a build by itself.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    VirtualQuery, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READWRITE, PAGE_READWRITE, PAGE_WRITECOPY,
};

/// `mov rax,[rip+disp32]` loading the render context pointer, and the two
/// spills that follow it.
///
/// The displacement is part of the signature. It was three bytes of opcode,
/// which any `mov rax,[rip+...]` at this address would have passed; with the
/// displacement it names the one global this build keeps the pointer in.
const LOAD_CONTEXT: usize = 0x228f3f4;
const LOAD_CONTEXT_BYTES: [u8; 17] = [
    0x48, 0x8b, 0x05, 0x05, 0x50, 0xf3, 0x02, // mov rax,[rip+0x2f35005]
    0x44, 0x89, 0x4c, 0x24, 0x60, // mov [rsp+0x60],r9d
    0x44, 0x89, 0x54, 0x24, 0x64, // mov [rsp+0x64],r10d
];
/// `movss xmm1,[rax+0x38]` reading the horizontal jitter, and the division
/// by the viewport width that follows.
const READ_X: usize = 0x228faaf;
const READ_X_BYTES: [u8; 16] = [
    0xf3, 0x0f, 0x10, 0x48, 0x38, // movss xmm1,[rax+0x38]
    0x66, 0x0f, 0x6e, 0xc2, // movd xmm0,edx
    0x0f, 0x5b, 0xc0, // cvtdq2ps xmm0,xmm0
    0xf3, 0x0f, 0x5e, 0xc8, // divss xmm1,xmm0
];
/// `movss xmm1,[rax+0x3c]` reading the vertical jitter.
const READ_Y: usize = 0x228facc;
const READ_Y_BYTES: [u8; 16] = [
    0xf3, 0x0f, 0x10, 0x48, 0x3c, // movss xmm1,[rax+0x3c]
    0x48, 0x8b, 0x43, 0x08, // mov rax,[rbx+0x08]
    0x0f, 0x5b, 0xc0, // cvtdq2ps xmm0,xmm0
    0xf3, 0x0f, 0x5e, 0xc8, // divss xmm1,xmm0
];

/// Where in the render context the two floats sit, taken from the two
/// instructions above rather than assumed.
const JITTER_X: usize = READ_X_BYTES[4] as usize;
const JITTER_Y: usize = READ_Y_BYTES[4] as usize;

/// Address of the pointer to the render context, or zero until it has been
/// worked out.
static SLOT: AtomicUsize = AtomicUsize::new(0);
/// The one way working it out can fail has been reported.
static SLOT_UNREADABLE: AtomicBool = AtomicBool::new(false);
/// Set once the context has been found, so the log says it only once.
static ANNOUNCED: AtomicBool = AtomicBool::new(false);
/// How many samples have been written, which doubles as the sequence index.
static WRITTEN: AtomicU64 = AtomicU64::new(0);

/// The loaded image of the game.
pub(crate) fn image() -> usize {
    unsafe {
        GetModuleHandleW(windows::core::PCWSTR::null())
            .map(|module| module.0 as usize)
            .unwrap_or(0)
    }
}

/// Whether `length` bytes at `address` can be read.
pub(crate) unsafe fn readable(address: usize, length: usize) -> bool {
    let mut info = MEMORY_BASIC_INFORMATION::default();
    let size = std::mem::size_of::<MEMORY_BASIC_INFORMATION>();
    if VirtualQuery(Some(address as *const _), &mut info, size) != size {
        return false;
    }
    if info.State.0 != 0x1000 {
        return false;
    }
    // Committed is not readable: a page can be committed with no access, or
    // as a guard page, and reading either is the fault this exists to avoid.
    const NO_ACCESS: u32 = 0x01;
    const GUARD: u32 = 0x100;
    if info.Protect.0 == 0 || info.Protect.0 & (NO_ACCESS | GUARD) != 0 {
        return false;
    }
    let end = info.BaseAddress as usize + info.RegionSize;
    address + length <= end
}

/// Whether `length` bytes at `address` can be written.
pub(crate) unsafe fn writable(address: usize, length: usize) -> bool {
    let mut info = MEMORY_BASIC_INFORMATION::default();
    let size = std::mem::size_of::<MEMORY_BASIC_INFORMATION>();
    if VirtualQuery(Some(address as *const _), &mut info, size) != size {
        return false;
    }
    if info.State.0 != 0x1000 {
        return false;
    }
    let ok = info.Protect == PAGE_READWRITE
        || info.Protect == PAGE_EXECUTE_READWRITE
        || info.Protect == PAGE_WRITECOPY;
    let end = info.BaseAddress as usize + info.RegionSize;
    ok && address + length <= end
}

/// Where the render context pointer lives, once the build is known.
///
/// The instructions this reads from are three of the sites `gate` checks, so
/// there is nothing to verify here: before the gate has engaged the answer
/// is "not yet", and that answer is not remembered. This is first called
/// from the first submission on any queue, which on some machines comes
/// before the executable is unpacked; a failure latched then would leave the
/// whole session without jitter.
fn locate() -> usize {
    let known = SLOT.load(Ordering::Relaxed);
    if known != 0 {
        return known;
    }
    if !crate::gate::engaged() {
        return 0;
    }
    let base = image();
    if base == 0 {
        return 0;
    }
    unsafe {
        // The instruction is `mov rax,[rip+disp32]`, seven bytes long, so the
        // displacement is relative to the byte after it.
        let load = base + LOAD_CONTEXT;
        let displacement = ((load + 3) as *const i32).read_unaligned();
        let slot = (load as i64 + 7 + displacement as i64) as usize;
        if !readable(slot, 8) {
            if !SLOT_UNREADABLE.swap(true, Ordering::Relaxed) {
                crate::log!("[eng] the context pointer at {slot:#x} is not mapped");
            }
            return 0;
        }
        crate::log!(
            "[eng] engine jitter found: context pointer at {:#x} (+{:#x} of the image), \
             jitter at +{:#x}/+{:#x}",
            slot,
            slot - base,
            JITTER_X,
            JITTER_Y
        );
        SLOT.store(slot, Ordering::Relaxed);
        slot
    }
}

/// The last context that was checked, so the check does not repeat.
///
/// This runs once per submission, several times a frame, and `VirtualQuery` is
/// a kernel call; doing it every time would put a syscall in the renderer's
/// path for no new information. The pointer is re-checked whenever it changes,
/// which is what matters -- a device reset hands out a different one.
static VALIDATED: AtomicUsize = AtomicUsize::new(0);

/// The render context, once the engine has made one.
///
/// The pointer is null until the renderer starts, so this is not an error
/// worth reporting; callers simply have nothing to do yet.
unsafe fn context() -> Option<*mut u8> {
    let slot = locate();
    if slot == 0 {
        return None;
    }
    let pointer = *(slot as *const usize);
    if pointer == 0 {
        return None;
    }
    if VALIDATED.load(Ordering::Relaxed) != pointer {
        if !writable(pointer + JITTER_X, 8) {
            crate::log!("[eng] the render context at {pointer:#x} is not writable");
            return None;
        }
        VALIDATED.store(pointer, Ordering::Relaxed);
    }
    Some(pointer as *mut u8)
}

/// Where the render context is, for a reader that only looks: no check that
/// it can be written and nothing said if it is not there yet.
pub(crate) fn context_address() -> Option<usize> {
    let slot = SLOT.load(Ordering::Relaxed);
    if slot == 0 {
        return None;
    }
    let pointer = unsafe { *(slot as *const usize) };
    (pointer != 0).then_some(pointer)
}

/// The last value written, so the next write can see whether it survived.
static LAST: AtomicU32 = AtomicU32::new(0);
/// How many times the slot no longer held what was last put there.
///
/// The whole question, when the picture does not move, is whether the engine
/// read the offset and did nothing with it, or never saw it. A write is not
/// evidence; a write that is still there next time is.
static LOST: AtomicU64 = AtomicU64::new(0);
static KEPT: AtomicU64 = AtomicU64::new(0);

/// Offset the main camera by `x` and `y`, in the engine's own units.
///
/// One unit is half a pixel: the matrix element is the value divided by the
/// viewport size, and a one-pixel shift is two units of normalised device
/// space across a viewport. `jitter()` does that conversion; this is the raw
/// form, for the diagnostic that fixes a constant and looks at the screen.
pub fn write(x: f32, y: f32) -> bool {
    unsafe {
        let Some(context) = context() else {
            return false;
        };
        let slot = context.add(JITTER_X) as *mut f32;

        // Before overwriting it, see what became of the last one.
        let previous = LAST.swap(x.to_bits(), Ordering::Relaxed);
        if previous != 0 {
            if slot.read_unaligned().to_bits() == previous {
                KEPT.fetch_add(1, Ordering::Relaxed);
            } else {
                LOST.fetch_add(1, Ordering::Relaxed);
            }
        }

        slot.write_unaligned(x);
        (context.add(JITTER_Y) as *mut f32).write_unaligned(y);
        let count = WRITTEN.fetch_add(1, Ordering::Relaxed);
        if !ANNOUNCED.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[eng] offsetting the engine's own camera: {x:+.4}, {y:+.4} (engine units)"
            );
        } else if count.is_multiple_of(600) {
            crate::log!(
                "[eng] engine jitter {x:+.4}, {y:+.4} ({count} written, {} survived, \
                 {} overwritten by the engine)",
                KEPT.load(Ordering::Relaxed),
                LOST.load(Ordering::Relaxed)
            );
        }
        true
    }
}

/// Work out the offset the settings ask for and give it to the engine.
///
/// Shared by both callers -- the detour and the submission hook -- so they
/// cannot disagree about what the sequence is.
pub fn apply() -> bool {
    let settings = crate::config::get();
    let index = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let (x, y) = crate::camera::jitter_pixels(index, settings.expert_jitter_phases);
    jitter(
        x * settings.expert_jitter_pixels,
        y * settings.expert_jitter_pixels,
    )
}

/// Length of the Halton sequence before it repeats, as shipped:
/// `expert_jitter_phases` is the key.
pub(crate) const JITTER_PHASES: u32 = 32;
/// Width of the jitter in render pixels, as shipped: samples spread across
/// one pixel, which is the usual choice. It was 0.5 for the external jitter,
/// as a compromise for an artefact that path caused and this one does not.
/// `expert_jitter_pixels` is the key.
pub(crate) const JITTER_PIXELS: f32 = 1.0;

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Writing it at the right moment
// ---------------------------------------------------------------------------

/// The camera setup that reads the two floats, and its prologue.
///
/// Four instructions, nineteen bytes, none of them relative to anything: two
/// pushes with a REX prefix, a third push, and an `lea` off `rsp`. That makes
/// them safe to relocate, which is what a detour needs and what cannot be
/// assumed of an arbitrary function.
const CAMERA_SETUP: usize = 0x228f3b0;
const CAMERA_SETUP_PROLOGUE: [u8; 19] = [
    0x40, 0x55, // push rbp
    0x53, // push rbx
    0x57, // push rdi
    0x48, 0x8d, 0xac, 0x24, 0x60, 0xfe, 0xff, 0xff, // lea rbp,[rsp-0x1a0]
    0x48, 0x81, 0xec, 0xa0, 0x02, 0x00, 0x00, // sub rsp,0x2a0
];

type CameraSetupFn = unsafe extern "system" fn(usize, usize) -> u32;

/// Where the relocated prologue lives.
static TRAMPOLINE: AtomicUsize = AtomicUsize::new(0);
/// How many times the engine's camera setup has run through us.
static RAN: AtomicU64 = AtomicU64::new(0);

/// Supply the offset, then let the engine set its camera up.
///
/// A write at submission time is a guess about ordering. This is not: the
/// engine reads the two floats a few hundred instructions below this point,
/// on this thread, with nothing in between that could clear them.
unsafe extern "system" fn hook_camera_setup(camera: usize, pass: usize) -> u32 {
    let perf = crate::perf::start(crate::perf::Family::Camera);
    if JITTERING.load(Ordering::Relaxed) {
        if crate::config::get().expert_jitter {
            apply();
        } else {
            // `expert_jitter = 0`: nothing is written, the engine clears the
            // two floats itself, and frame generation, which is told the
            // offset last given, is told none.
            LAST_JITTER.store(0, Ordering::Relaxed);
        }
    }
    let count = RAN.fetch_add(1, Ordering::Relaxed);
    if count == 0 {
        crate::log!("[eng] the engine's camera setup is running through us");
    }
    perf.stop();
    let original: CameraSetupFn = std::mem::transmute(TRAMPOLINE.load(Ordering::Relaxed));
    original(camera, pass)
}

/// Whether the camera is being offset.
///
/// Off until the engine has submitted Direct3D 12 work, and off again for
/// good once DLSS is latched unusable. The detour that writes the offset is
/// installed as soon as the build is recognised, which is before the game
/// has chosen a renderer; an offset that nothing resolves is a picture that
/// trembles, on Direct3D 11 as much as with a missing snippet.
static JITTERING: AtomicBool = AtomicBool::new(false);

/// The engine has submitted Direct3D 12 work and DLSS may yet run: offset
/// the camera from now on. One relaxed load when it already is.
pub fn start_jitter() {
    if !JITTERING.load(Ordering::Relaxed) && !crate::gate::dlss_unusable() {
        JITTERING.store(true, Ordering::Relaxed);
    }
}

/// Stop offsetting the camera. The engine overwrites the two floats itself
/// every frame, so stopping is enough to have the offset gone.
pub fn stop_jitter() {
    JITTERING.store(false, Ordering::Relaxed);
}

/// A 14-byte absolute jump: `jmp [rip+0]` followed by the target.
///
/// Not `jmp rel32`: this library and the game are both in a 64-bit address
/// space with no guarantee of being within 2 GB of each other, and a
/// displacement that silently does not fit is the kind of defect that shows up
/// as an unrelated crash.
fn absolute_jump(target: usize) -> [u8; 14] {
    let mut bytes = [0u8; 14];
    bytes[0] = 0xff;
    bytes[1] = 0x25;
    bytes[6..].copy_from_slice(&target.to_le_bytes());
    bytes
}

// ---------------------------------------------------------------------------
// Writing over the game's code
// ---------------------------------------------------------------------------

/// The most bytes one write replaces: longer than the longest prologue.
pub(crate) const MAX_WRITE: usize = 24;

/// One change to the game's code: where, what must be there at the moment of
/// writing, and what goes there instead.
#[derive(Clone, Copy)]
pub(crate) struct CodeWrite {
    pub what: &'static str,
    pub address: usize,
    pub length: usize,
    pub expect: [u8; MAX_WRITE],
    pub bytes: [u8; MAX_WRITE],
}

/// Why a set of writes was not made. In every case nothing is left written.
#[derive(Debug, PartialEq)]
pub(crate) enum WriteError {
    /// A suspended thread was executing inside one of the ranges.
    Busy,
    /// The bytes are not committed, executable code right now.
    NotCode { index: usize, protection: u32 },
    /// The page could not be made writable.
    Protect { index: usize },
    /// With the page writable, the bytes were not the expected ones.
    Changed {
        index: usize,
        found: [u8; MAX_WRITE],
    },
}

/// The few system calls made while other threads are stopped, declared here
/// so that nothing stands between this code and the system: no error object
/// is built, nothing is allocated and nothing is formatted. A thread stopped
/// at an arbitrary instruction may hold the heap's lock or the log's, and
/// anything that takes either while it is stopped waits for ever.
mod frozen {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    extern "system" {
        pub fn VirtualProtect(address: *const c_void, size: usize, new: u32, old: *mut u32) -> i32;
        pub fn VirtualQuery(
            address: *const c_void,
            info: *mut windows::Win32::System::Memory::MEMORY_BASIC_INFORMATION,
            length: usize,
        ) -> usize;
        pub fn SuspendThread(thread: *mut c_void) -> u32;
        pub fn ResumeThread(thread: *mut c_void) -> u32;
        pub fn GetThreadContext(thread: *mut c_void, context: *mut Context) -> i32;
        pub fn FlushInstructionCache(
            process: *mut c_void,
            address: *const c_void,
            size: usize,
        ) -> i32;
        pub fn GetCurrentProcess() -> *mut c_void;
    }

    /// `CONTEXT` for x64, of which only two fields are used. 1232 bytes,
    /// sixteen-byte aligned; `ContextFlags` at 0x30 and `Rip` at 0xf8.
    #[repr(C, align(16))]
    pub struct Context(pub [u8; 1232]);

    pub const CONTEXT_CONTROL: u32 = 0x0010_0001;
    pub const FLAGS: usize = 0x30;
    pub const RIP: usize = 0xf8;

    pub const MEM_COMMIT: u32 = 0x1000;
    pub const PAGE_EXECUTE: u32 = 0x10;
    pub const PAGE_EXECUTE_READ: u32 = 0x20;
    pub const PAGE_EXECUTE_READWRITE: u32 = 0x40;
    pub const PAGE_EXECUTE_WRITECOPY: u32 = 0x80;
    pub const PAGE_GUARD: u32 = 0x100;
}

/// Handles to every other thread of this process, opened for suspending.
///
/// Allocates, so it is done before anything is stopped. A thread created
/// after the list is taken is not on it.
unsafe fn other_threads() -> Vec<*mut c_void> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcessId, GetCurrentThreadId, OpenThread, THREAD_GET_CONTEXT,
        THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
    };
    let mut threads = Vec::with_capacity(64);
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
                threads.push(thread.0);
            }
        }
        more = Thread32Next(snapshot, &mut entry).is_ok();
    }
    let _ = CloseHandle(snapshot);
    threads
}

/// Make every write, or none of them, with the process's other threads
/// suspended. Returns how many threads were suspended for it.
///
/// What this is for: these bytes are the entries of functions the engine is
/// free to be calling. Written from a thread of ours while the engine's run,
/// fourteen bytes go in over several instructions' worth of time, and a
/// thread that fetches the entry half way through executes half of each. So:
///
/// * every other thread is suspended first, and each one's instruction
///   pointer is read. If any is inside a range to be written the whole
///   attempt is abandoned and nothing has changed (`Busy`);
/// * each range must be committed, executable code at that moment. A page
///   the unpacker still has open for writing is not in its final state and is
///   left alone (`NotCode`);
/// * with the page made writable the bytes are compared again, because the
///   check that led here was made earlier and from outside (`Changed`);
/// * afterwards the page's old protection is put back only if the page still
///   has the protection this function gave it;
/// * a failure part way through puts back what the earlier writes replaced.
///
/// What is left uncovered: a thread created after the list of threads was
/// taken runs throughout; a thread whose context cannot be read is assumed
/// What is left uncovered: a thread created after the list of threads was
/// taken runs throughout; a thread whose context cannot be read is assumed
/// to be elsewhere; and a thread that was suspended one instruction short
/// of a call into one of these functions simply calls the new bytes, which
/// is what is wanted. If the list of threads cannot be taken at all the
/// write is made with nothing suspended, and the count returned is zero.
pub(crate) unsafe fn write_code(writes: &[CodeWrite]) -> Result<usize, WriteError> {
    let threads = other_threads();
    let mut suspended = vec![false; threads.len()];

    // From here to the last `ResumeThread`: no allocation, no logging, no
    // lock of any kind.
    for (thread, done) in threads.iter().zip(suspended.iter_mut()) {
        *done = frozen::SuspendThread(*thread) != u32::MAX;
    }
    let outcome = write_while_frozen(writes, &threads, &suspended);
    for (thread, done) in threads.iter().zip(&suspended) {
        if *done {
            frozen::ResumeThread(*thread);
        }
    }

    let count = suspended.iter().filter(|done| **done).count();
    for thread in threads {
        let _ = windows::Win32::Foundation::CloseHandle(windows::Win32::Foundation::HANDLE(thread));
    }
    outcome.map(|()| count)
}

unsafe fn write_while_frozen(
    writes: &[CodeWrite],
    threads: &[*mut c_void],
    suspended: &[bool],
) -> Result<(), WriteError> {
    // Reading a thread's context is also what makes its suspension complete:
    // `SuspendThread` only asks, and the context cannot be read until the
    // thread has actually stopped.
    let mut context = frozen::Context([0u8; 1232]);
    for (thread, done) in threads.iter().zip(suspended) {
        if !*done {
            continue;
        }
        context.0[frozen::FLAGS..frozen::FLAGS + 4]
            .copy_from_slice(&frozen::CONTEXT_CONTROL.to_le_bytes());
        if frozen::GetThreadContext(*thread, &mut context) == 0 {
            continue;
        }
        let mut rip = [0u8; 8];
        rip.copy_from_slice(&context.0[frozen::RIP..frozen::RIP + 8]);
        let rip = u64::from_le_bytes(rip) as usize;
        if writes
            .iter()
            .any(|write| rip >= write.address && rip < write.address + write.length)
        {
            return Err(WriteError::Busy);
        }
    }

    for (index, write) in writes.iter().enumerate() {
        if let Err(error) = write_one(index, write) {
            // Put back what the earlier ones replaced. Their pages were
            // writable a moment ago; one that will not open again is left,
            // and the caller's report says which write failed.
            for done in writes[..index].iter().rev() {
                let undo = CodeWrite {
                    expect: done.bytes,
                    bytes: done.expect,
                    ..*done
                };
                let _ = write_one(0, &undo);
            }
            return Err(error);
        }
    }
    Ok(())
}

unsafe fn write_one(index: usize, write: &CodeWrite) -> Result<(), WriteError> {
    use windows::Win32::System::Memory::MEMORY_BASIC_INFORMATION;
    let address = write.address as *const c_void;
    let size = std::mem::size_of::<MEMORY_BASIC_INFORMATION>();
    let executable = frozen::PAGE_EXECUTE
        | frozen::PAGE_EXECUTE_READ
        | frozen::PAGE_EXECUTE_READWRITE
        | frozen::PAGE_EXECUTE_WRITECOPY;

    let mut info = MEMORY_BASIC_INFORMATION::default();
    if frozen::VirtualQuery(address, &mut info, size) != size {
        return Err(WriteError::NotCode {
            index,
            protection: 0,
        });
    }
    let end = info.BaseAddress as usize + info.RegionSize;
    if info.State.0 != frozen::MEM_COMMIT
        || info.Protect.0 & executable == 0
        || info.Protect.0 & frozen::PAGE_GUARD != 0
        || write.address + write.length > end
    {
        return Err(WriteError::NotCode {
            index,
            protection: info.Protect.0,
        });
    }

    let mut previous = 0u32;
    if frozen::VirtualProtect(
        address,
        write.length,
        frozen::PAGE_EXECUTE_READWRITE,
        &mut previous,
    ) == 0
    {
        return Err(WriteError::Protect { index });
    }
    let target = std::slice::from_raw_parts_mut(write.address as *mut u8, write.length);
    let outcome = if target == &write.expect[..write.length] {
        target.copy_from_slice(&write.bytes[..write.length]);
        Ok(())
    } else {
        let mut found = [0u8; MAX_WRITE];
        found[..write.length].copy_from_slice(target);
        Err(WriteError::Changed { index, found })
    };
    // The old protection goes back only over the protection set above. If it
    // is something else by now, somebody changed it in between and theirs is
    // the newer intention.
    let mut now = MEMORY_BASIC_INFORMATION::default();
    if frozen::VirtualQuery(address, &mut now, size) == size
        && now.Protect.0 == frozen::PAGE_EXECUTE_READWRITE
        && previous != frozen::PAGE_EXECUTE_READWRITE
    {
        let mut ignored = 0u32;
        frozen::VirtualProtect(address, write.length, previous, &mut ignored);
    }
    frozen::FlushInstructionCache(frozen::GetCurrentProcess(), address, write.length);
    outcome
}

/// A function to route through this library: its entry, the hook that takes
/// its place, and where the way back to the original is kept.
pub(crate) struct Detour {
    pub site: &'static crate::gate::Site,
    pub hook: *const (),
    pub trampoline: &'static AtomicUsize,
}

/// Build the trampoline for a detour and describe the write that installs it.
///
/// The site's bytes are both what is moved and what must be found: every one
/// of them is position-independent -- no rip-relative operand, no relative
/// branch -- because they are copied elsewhere and run there. Each site was
/// read in a disassembler to confirm that.
///
/// The trampoline's address is stored before anything is written, so the
/// hook finds its original from the first call.
pub(crate) unsafe fn prepare_detour(base: usize, detour: &Detour) -> Option<CodeWrite> {
    use windows::Win32::System::Memory::{
        VirtualAlloc, VirtualProtect, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READ,
        PAGE_PROTECTION_FLAGS, PAGE_READWRITE,
    };
    let prologue = detour.site.bytes;
    if prologue.len() < 14 || prologue.len() > MAX_WRITE {
        return None;
    }
    let entry = base + detour.site.rva;

    // The relocated prologue, then a jump back to what follows it.
    let scratch = VirtualAlloc(
        None,
        prologue.len() + 14,
        MEM_COMMIT | MEM_RESERVE,
        PAGE_READWRITE,
    );
    if scratch.is_null() {
        return None;
    }
    std::ptr::copy_nonoverlapping(prologue.as_ptr(), scratch as *mut u8, prologue.len());
    let back = absolute_jump(entry + prologue.len());
    std::ptr::copy_nonoverlapping(
        back.as_ptr(),
        (scratch as *mut u8).add(prologue.len()),
        back.len(),
    );
    let mut previous = PAGE_PROTECTION_FLAGS(0);
    if VirtualProtect(
        scratch,
        prologue.len() + 14,
        PAGE_EXECUTE_READ,
        &mut previous,
    )
    .is_err()
    {
        let _ = windows::Win32::System::Memory::VirtualFree(
            scratch,
            0,
            windows::Win32::System::Memory::MEM_RELEASE,
        );
        return None;
    }
    detour.trampoline.store(scratch as usize, Ordering::SeqCst);

    // The jump out of the engine's function into ours, then whatever the
    // fourteen bytes leave over as no-ops, so a disassembly never shows half
    // an instruction.
    let mut expect = [0u8; MAX_WRITE];
    expect[..prologue.len()].copy_from_slice(prologue);
    let mut bytes = [0x90u8; MAX_WRITE];
    bytes[..14].copy_from_slice(&absolute_jump(detour.hook as usize));
    Some(CodeWrite {
        what: detour.site.what,
        address: entry,
        length: prologue.len(),
        expect,
        bytes,
    })
}

/// Throw away a trampoline whose detour was never written.
pub(crate) unsafe fn discard_detour(detour: &Detour) {
    let scratch = detour.trampoline.swap(0, Ordering::SeqCst);
    if scratch != 0 {
        let _ = windows::Win32::System::Memory::VirtualFree(
            scratch as *mut c_void,
            0,
            windows::Win32::System::Memory::MEM_RELEASE,
        );
    }
}

/// Every site this module depends on, for `gate`.
pub(crate) static SITES: [crate::gate::Site; 5] = [
    crate::gate::Site::new("the render context load", LOAD_CONTEXT, &LOAD_CONTEXT_BYTES),
    crate::gate::Site::new("the horizontal jitter read", READ_X, &READ_X_BYTES),
    crate::gate::Site::new("the vertical jitter read", READ_Y, &READ_Y_BYTES),
    crate::gate::Site::new("the camera setup", CAMERA_SETUP, &CAMERA_SETUP_PROLOGUE),
    crate::gate::Site::new("the fatal error reporter", FATAL, &FATAL_PROLOGUE),
];

/// The two functions this module routes through itself, for `gate`.
pub(crate) fn detours() -> [Detour; 2] {
    [
        Detour {
            site: &SITES[3],
            hook: hook_camera_setup as *const (),
            trampoline: &TRAMPOLINE,
        },
        Detour {
            site: &SITES[4],
            hook: hook_fatal as *const (),
            trampoline: &FATAL_TRAMPOLINE,
        },
    ]
}

// ---------------------------------------------------------------------------
// Naming the engine's own fatal errors
// ---------------------------------------------------------------------------

/// The engine's fatal error reporter, and its prologue.
///
/// This is where a crash reported by Windows at this address comes from. It
/// is not a defect; it is the engine's assert:
///
/// ```text
///   1421fd060  mov rax,rsp             ; format the message into 1 KiB
///   ...        call [param_1+0x18]     ; hand it to the registered handler
///   1421fd0c9  mov dword [0],0xdeadbeef
/// ```
///
/// A deliberate null write with a sentinel, fallen into whether or not the
/// handler returns. The second argument is a printf format string, and the
/// twenty-eight call sites name what went wrong -- `ERR11 : Invalid DXGI
/// call.`, `ERR22 : Failed to initialize DirectX12.`, `ERR08 : Memory
/// overrun.` -- so logging that one pointer turns an address in the Windows
/// event log into a sentence.
const FATAL: usize = 0x21fd060;
const FATAL_PROLOGUE: [u8; 18] = [
    0x48, 0x8b, 0xc4, // mov rax,rsp
    0x48, 0x89, 0x50, 0x10, // mov [rax+0x10],rdx
    0x4c, 0x89, 0x40, 0x18, // mov [rax+0x18],r8
    0x4c, 0x89, 0x48, 0x20, // mov [rax+0x20],r9
    0x53, // push rbx
    0x56, // push rsi
    0x57, // push rdi
];

type FatalFn = unsafe extern "system" fn(usize, usize, usize, usize);

static FATAL_TRAMPOLINE: AtomicUsize = AtomicUsize::new(0);

/// Read a C string, with a bound, from memory that may not hold one.
unsafe fn c_string(address: usize) -> String {
    if address == 0 || !readable(address, 1) {
        return format!("<{address:#x} is not readable>");
    }
    let mut end = 0;
    while end < 512 && *(address as *const u8).add(end) != 0 {
        end += 1;
    }
    String::from_utf8_lossy(std::slice::from_raw_parts(address as *const u8, end)).into_owned()
}

/// Say what the engine is about to die of, then let it.
///
/// Deliberately not suppressed. By the time this runs a D3D call has already
/// failed and the engine has decided the renderer is unrecoverable; carrying
/// on would trade a named crash for an unnamed one.
///
/// Only the first four arguments are forwarded. The messages here take at most
/// two, which arrive in r8 and r9, so nothing is lost -- but this is a
/// variadic function, and one with more would print rubbish for the rest.
unsafe extern "system" fn hook_fatal(context: usize, format: usize, a3: usize, a4: usize) {
    crate::log!("[fatal] the engine is aborting: {}", c_string(format));
    crate::log!(
        "[fatal] arguments {a3:#x} {a4:#x}; offsets written {}, camera setups {}",
        written(),
        ran()
    );
    let original: FatalFn = std::mem::transmute(FATAL_TRAMPOLINE.load(Ordering::Relaxed));
    original(context, format, a3, a4)
}

/// How many times the engine's camera setup has run through the detour.
pub fn ran() -> u64 {
    RAN.load(Ordering::Relaxed)
}

/// Offset the main camera by a sub-pixel amount, in pixels.
pub fn jitter(x: f32, y: f32) -> bool {
    // Remember it in pixels. The value DLSS has to be told is this one, and
    // reading it back out of the engine later would be reading whatever the
    // engine's own jitter writer had since put there -- which, with the TAA
    // node alive, is a different sample from a different table.
    LAST_JITTER.store(
        ((x.to_bits() as u64) << 32) | y.to_bits() as u64,
        Ordering::Relaxed,
    );
    write(x * 2.0, y * 2.0)
}

static LAST_JITTER: AtomicU64 = AtomicU64::new(0);

/// The sub-pixel offset last given to the engine, in render pixels.
pub fn last_jitter() -> (f32, f32) {
    let packed = LAST_JITTER.load(Ordering::Relaxed);
    (
        f32::from_bits((packed >> 32) as u32),
        f32::from_bits(packed as u32),
    )
}

/// How many samples have been handed to the engine.
pub fn written() -> u64 {
    WRITTEN.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// The output size, as observed
// ---------------------------------------------------------------------------

/// The size of the target the temporal pass writes into.
///
/// Kept for its one log line, which says what the pass was observed writing.
/// Nothing is derived from it: the render ratio is the engine's own, set
/// through `hijack`.
pub fn note_output_size(size: (u32, u32)) {
    if size.0 == 0 || size.1 == 0 {
        return;
    }
    // Said when a larger size than any before is seen, not on every change.
    let packed = ((size.0 as u64) << 32) | size.1 as u64;
    let mut known = OUTPUT_SIZE.load(Ordering::Relaxed);
    loop {
        let bigger = (size.0 as u64) * (size.1 as u64) > (known >> 32) * ((known as u32) as u64);
        if !bigger {
            return;
        }
        match OUTPUT_SIZE.compare_exchange_weak(known, packed, Ordering::SeqCst, Ordering::Relaxed)
        {
            Ok(_) => {
                crate::log!("[taa] the temporal pass writes {}x{}", size.0, size.1);
                return;
            }
            Err(seen) => known = seen,
        }
    }
}

static OUTPUT_SIZE: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Memory::{
        VirtualAlloc, VirtualFree, VirtualProtect, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE,
        PAGE_EXECUTE_READ, PAGE_PROTECTION_FLAGS, PAGE_READONLY, PAGE_READWRITE,
    };

    /// A page holding `content`, left with `protection`.
    unsafe fn page(content: &[u8], protection: PAGE_PROTECTION_FLAGS) -> usize {
        let memory = VirtualAlloc(None, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        assert!(!memory.is_null());
        std::ptr::copy_nonoverlapping(content.as_ptr(), memory as *mut u8, content.len());
        let mut previous = PAGE_PROTECTION_FLAGS(0);
        VirtualProtect(memory, 4096, protection, &mut previous).unwrap();
        memory as usize
    }

    unsafe fn contents(address: usize, length: usize) -> Vec<u8> {
        std::slice::from_raw_parts(address as *const u8, length).to_vec()
    }

    unsafe fn protection(address: usize) -> u32 {
        let mut info = MEMORY_BASIC_INFORMATION::default();
        VirtualQuery(
            Some(address as *const _),
            &mut info,
            std::mem::size_of_val(&info),
        );
        info.Protect.0
    }

    fn write_of(address: usize, expect: &[u8], bytes: &[u8]) -> CodeWrite {
        let mut write = CodeWrite {
            what: "a test write",
            address,
            length: expect.len(),
            expect: [0; MAX_WRITE],
            bytes: [0; MAX_WRITE],
        };
        write.expect[..expect.len()].copy_from_slice(expect);
        write.bytes[..bytes.len()].copy_from_slice(bytes);
        write
    }

    #[test]
    fn code_is_written_and_the_page_is_left_as_it_was() {
        unsafe {
            let memory = page(&[1, 2, 3, 4, 5, 6], PAGE_EXECUTE_READ);
            let outcome = write_code(&[write_of(memory + 1, &[2, 3, 4], &[9, 8, 7])]);
            assert!(outcome.is_ok(), "{outcome:?}");
            assert_eq!(contents(memory, 6), [1, 9, 8, 7, 5, 6]);
            assert_eq!(protection(memory), PAGE_EXECUTE_READ.0);
            let _ = VirtualFree(memory as *mut _, 0, MEM_RELEASE);
        }
    }

    #[test]
    fn bytes_that_are_not_the_expected_ones_are_not_written_over() {
        unsafe {
            let memory = page(&[1, 2, 3, 4], PAGE_EXECUTE_READ);
            let outcome = write_code(&[write_of(memory, &[1, 2, 9], &[7, 7, 7])]);
            let mut found = [0u8; MAX_WRITE];
            found[..3].copy_from_slice(&[1, 2, 3]);
            assert_eq!(outcome, Err(WriteError::Changed { index: 0, found }));
            assert_eq!(contents(memory, 4), [1, 2, 3, 4]);
            assert_eq!(protection(memory), PAGE_EXECUTE_READ.0);
            let _ = VirtualFree(memory as *mut _, 0, MEM_RELEASE);
        }
    }

    #[test]
    fn a_failure_part_way_puts_the_earlier_writes_back() {
        unsafe {
            let memory = page(&[1, 2, 3, 4, 5, 6, 7, 8], PAGE_EXECUTE_READ);
            let outcome = write_code(&[
                write_of(memory, &[1, 2], &[0xaa, 0xbb]),
                write_of(memory + 2, &[3, 4], &[0xcc, 0xdd]),
                write_of(memory + 4, &[0, 0], &[0xee, 0xff]),
            ]);
            assert!(
                matches!(outcome, Err(WriteError::Changed { index: 2, .. })),
                "{outcome:?}"
            );
            assert_eq!(contents(memory, 8), [1, 2, 3, 4, 5, 6, 7, 8]);
            let _ = VirtualFree(memory as *mut _, 0, MEM_RELEASE);
        }
    }

    #[test]
    fn memory_that_is_not_executable_code_is_refused() {
        unsafe {
            // What a page looks like while an unpacker is still filling it.
            let memory = page(&[1, 2, 3, 4], PAGE_READWRITE);
            let outcome = write_code(&[write_of(memory, &[1, 2], &[7, 7])]);
            assert_eq!(
                outcome,
                Err(WriteError::NotCode {
                    index: 0,
                    protection: PAGE_READWRITE.0
                })
            );
            assert_eq!(contents(memory, 4), [1, 2, 3, 4]);
            let _ = VirtualFree(memory as *mut _, 0, MEM_RELEASE);

            let memory = page(&[1, 2, 3, 4], PAGE_READONLY);
            let outcome = write_code(&[write_of(memory, &[1, 2], &[7, 7])]);
            assert!(
                matches!(outcome, Err(WriteError::NotCode { index: 0, .. })),
                "{outcome:?}"
            );
            let _ = VirtualFree(memory as *mut _, 0, MEM_RELEASE);
        }
    }

    /// A detour written by these functions runs: the hook is entered, and the
    /// trampoline carries on into the original.
    #[test]
    fn a_detour_reaches_its_hook_and_the_trampoline_reaches_the_original() {
        // `mov eax, 0x11111111` three times and `ret`: fifteen bytes of
        // position-independent prologue, then the return.
        static ORIGINAL: [u8; 16] = [
            0xb8, 0x11, 0x11, 0x11, 0x11, 0xb8, 0x22, 0x22, 0x22, 0x22, 0xb8, 0x33, 0x33, 0x33,
            0x33, 0xc3,
        ];
        static WAY_BACK: AtomicUsize = AtomicUsize::new(0);
        static ENTERED: AtomicU32 = AtomicU32::new(0);
        unsafe extern "system" fn hook() -> u32 {
            ENTERED.fetch_add(1, Ordering::SeqCst);
            let original: unsafe extern "system" fn() -> u32 =
                std::mem::transmute(WAY_BACK.load(Ordering::SeqCst));
            original() + 1
        }
        unsafe {
            let memory = page(&ORIGINAL, PAGE_EXECUTE_READ);
            let function: unsafe extern "system" fn() -> u32 = std::mem::transmute(memory);
            assert_eq!(function(), 0x3333_3333);

            let site: &'static crate::gate::Site = Box::leak(Box::new(crate::gate::Site {
                what: "a test function",
                rva: 0,
                bytes: &ORIGINAL[..15],
                wild: 0..0,
            }));
            let detour = Detour {
                site,
                hook: hook as *const (),
                trampoline: &WAY_BACK,
            };
            let write = prepare_detour(memory, &detour).expect("a trampoline");
            assert!(write_code(&[write]).is_ok());

            assert_eq!(function(), 0x3333_3334);
            assert_eq!(ENTERED.load(Ordering::SeqCst), 1);
            assert_eq!(protection(memory), PAGE_EXECUTE_READ.0);
        }
    }
}
