//! Standing in for the game's own DLSS.
//!
//! The engine already knows how to render the scene small and leave the UI at
//! full size. Arranging that from outside fails on a stage after the
//! resolve that still believes the picture lives in a sub-rectangle, so this
//! takes the engine's own path instead of building a parallel one.
//!
//! What that path is, in the game's code:
//!
//! ```text
//!   FUN_142383ef0  GetOptimalSettings(out float[3], ngx, w, h, mode)
//!                    asks the driver through DLSSOptimalSettingsCallback and
//!                    returns {scale, flags, sharpness}. flags bit0 = make a
//!                    SuperSampling feature, bit1 = make an ImageSignalProcessing
//!                    one. The scale becomes the engine's render ratio.
//!   FUN_142384420  CreateFeatures(ngx, outW, outH, mode, _, cmdList)
//!                    NVSDK_NGX_D3D12_CreateFeature for feature 1 and feature 9.
//!   FUN_142383cf0  EvaluateSuperSampling(desc, cmdList)   render res -> render res
//!   FUN_142383990  EvaluateImageSignalProcessing(desc, cmdList)
//!                    render res -> display res, writing "UpscaleOut"
//! ```
//!
//! It is a DLSS 1.x integration: two features, where supersampling is only
//! anti-aliasing at the render resolution and the upscale is done by Image
//! Signal Processing -- the half current drivers no longer carry. So the
//! driver refuses, the game hides the option, and the whole path goes unused.
//!
//! DLSS 4 does both halves at once, which maps onto this almost too neatly.
//! The four functions are taken over:
//!
//!   * the optimal settings become ours, which both unhides the option and
//!     makes `render_scale` the engine's render ratio -- the one control of
//!     render resolution, with the game's own Image Quality option held at
//!     High so that it cannot scale the picture a second time;
//!   * feature creation is answered without troubling the driver about
//!     features it does not have;
//!   * the supersampling evaluation only remembers what it was given, since
//!     its inputs are the ones DLSS wants and its output is a render-sized
//!     intermediate we have no use for;
//!   * the image signal processing evaluation, which is the one with the
//!     full-size output, runs DLSS 4 with the resources remembered from the
//!     other.
//!
//! Everything downstream -- post, UI, present -- then runs at full resolution
//! because the engine already switches the chain over to `UpscaleOut` after
//! that call. That switch is the thing worth having and the reason for all of
//! this.

use parking_lot::Mutex;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::gate::Site;
use crate::native::{image, readable, writable, Detour};

/// `GetOptimalSettings`. Seventeen bytes of pushes and a stack adjustment.
const OPTIMAL: usize = 0x2383ef0;
const OPTIMAL_PROLOGUE: [u8; 17] = [
    0x48, 0x89, 0x5c, 0x24, 0x18, // mov [rsp+0x18],rbx
    0x56, // push rsi
    0x57, // push rdi
    0x41, 0x54, // push r12
    0x41, 0x56, // push r14
    0x41, 0x57, // push r15
    0x48, 0x83, 0xec, 0x30, // sub rsp,0x30
];

/// `CreateFeatures`. Exactly fourteen bytes of argument spills.
const CREATE: usize = 0x2384420;
const CREATE_PROLOGUE: [u8; 14] = [
    0x48, 0x89, 0x5c, 0x24, 0x20, // mov [rsp+0x20],rbx
    0x44, 0x89, 0x44, 0x24, 0x18, // mov [rsp+0x18],r8d
    0x89, 0x54, 0x24, 0x10, // mov [rsp+0x10],edx
];

/// `EvaluateSuperSampling`.
const EVAL_SS: usize = 0x2383cf0;
const EVAL_SS_PROLOGUE: [u8; 15] = [
    0x48, 0x89, 0x5c, 0x24, 0x08, // mov [rsp+0x08],rbx
    0x48, 0x89, 0x6c, 0x24, 0x18, // mov [rsp+0x18],rbp
    0x48, 0x89, 0x74, 0x24, 0x20, // mov [rsp+0x20],rsi
];

/// `EvaluateImageSignalProcessing`.
const EVAL_ISP: usize = 0x2383990;
const EVAL_ISP_PROLOGUE: [u8; 15] = [
    0x48, 0x89, 0x5c, 0x24, 0x08, // mov [rsp+0x08],rbx
    0x48, 0x89, 0x6c, 0x24, 0x10, // mov [rsp+0x10],rbp
    0x48, 0x89, 0x74, 0x24, 0x18, // mov [rsp+0x18],rsi
];

/// The enqueue of the supersampling pass.
///
/// It reads the dense motion vector target out of the temporal node and binds
/// it with no check at all:
///
/// ```text
///   uVar6 = *(longlong *)(node + 0x1a8);
///   FUN_1423d0130(list, rwOutput, uVar6);
/// ```
///
/// and that target is only allocated when `FUN_142391f30` runs while the "DLSS
/// is active" byte is already set -- and when the condition is false it leaves
/// the pointer entirely alone rather than clearing it. The engine never trips
/// over this because its own features are created during device
/// initialisation, before the node is ever configured. Taking the path over
/// later moves that order, the node gets configured first, the pointer stays
/// null and binding it is an access violation a few hundred frames in.
///
/// Installing early is the actual fix; this is the guard for anything that
/// still gets the order wrong.
const SS_ENQUEUE: usize = 0x23913f0;
const SS_ENQUEUE_PROLOGUE: [u8; 15] = [
    0x4c, 0x8b, 0xdc, // mov r11,rsp
    0x41, 0x56, // push r14
    0x48, 0x81, 0xec, 0x90, 0x00, 0x00, 0x00, // sub rsp,0x90
    0x48, 0x8b, 0x01, // mov rax,[rcx]
];
/// Where the node keeps the targets that pass binds.
const NODE_DENSE_MOTION: usize = 0x1a8;
const NODE_COLOUR: usize = 0x150;
const NODE_RING: usize = 0x168;

/// The handler for the menu's Image Quality (Resolution Scaling) option.
///
/// ```text
///   if (dlssUsable()) param_1 = '\x02';     // forced to High
///   0 -> 0.75   1 -> 0.9   2 -> 1.0   3,4 -> dynamic, 1.0 down to 0.75
///   ... -> cfg+0x1f0, the engine's own scale: a sub-rectangle of its targets
/// ```
///
/// `dlssUsable` needs the game's own NVIDIA DLSS option to read On, so in a
/// session without it the option's scale stays in force on top of the DLSS
/// ratio. It is detoured to give the engine High every time; see
/// `hook_scaling_option`. Fourteen bytes of prologue, exactly enough.
const SCALING_OPTION: usize = 0x43eeb0;
const SCALING_OPTION_PROLOGUE: [u8; 14] = [
    0x40, 0x53, // push rbx
    0x48, 0x83, 0xec, 0x60, // sub rsp,0x60
    0x0f, 0x29, 0x74, 0x24, 0x50, // movaps [rsp+0x50],xmm6
    0x0f, 0xb6, 0xd9, // movzx ebx,cl
];

/// `mov rax,[rip+disp32]` loading the graphics configuration, which carries
/// the display resolution at +0x198, with the two instructions after it --
/// the second of which is the engine's own read of that field. The address
/// comes out of the instruction, not out of a note, and the instruction is
/// recognised by all seventeen bytes: its opcode alone, which is what used
/// to be compared, is the opcode of every such load in the game.
pub(crate) const CONFIG_LOAD: usize = 0x2290426;
const CONFIG_LOAD_BYTES: [u8; 17] = [
    0x48, 0x8b, 0x05, 0x53, 0x40, 0xf3, 0x02, // mov rax,[rip+0x2f34053]
    0x48, 0x8b, 0xd9, // mov rbx,rcx
    0x48, 0x8b, 0x90, 0x98, 0x01, 0x00, 0x00, // mov rdx,[rax+0x198]
];
const DISPLAY_SIZE: usize = 0x198;

/// Every site this module depends on, for `gate`: the six functions it
/// routes through itself, in the order of `detours`, the load above, and
/// the two ends of the engine's ratio setter, which is called and not
/// changed.
pub(crate) static SITES: [Site; 9] = [
    Site::new(
        "the game's DLSS optimal settings",
        OPTIMAL,
        &OPTIMAL_PROLOGUE,
    ),
    Site::new("the game's DLSS feature creation", CREATE, &CREATE_PROLOGUE),
    Site::new(
        "the game's supersampling evaluation",
        EVAL_SS,
        &EVAL_SS_PROLOGUE,
    ),
    Site::new(
        "the game's upscale evaluation",
        EVAL_ISP,
        &EVAL_ISP_PROLOGUE,
    ),
    Site::new(
        "the supersampling enqueue",
        SS_ENQUEUE,
        &SS_ENQUEUE_PROLOGUE,
    ),
    Site::new(
        "the Resolution Scaling option",
        SCALING_OPTION,
        &SCALING_OPTION_PROLOGUE,
    ),
    Site::new(
        "the render configuration load",
        CONFIG_LOAD,
        &CONFIG_LOAD_BYTES,
    ),
    Site::new(
        "the engine's render ratio setter",
        RATIO_SETTER,
        &RATIO_SETTER_HEAD,
    ),
    Site::new(
        "the render ratio setter's store",
        RATIO_SETTER_TAIL,
        &RATIO_SETTER_TAIL_BYTES,
    ),
];

/// The six functions that make up the game's DLSS path, for `gate` to
/// install together or not at all.
///
/// Together, because they are one mechanism. The feature creation hands the
/// game stand-in handles that only the two evaluations here know not to pass
/// to the driver; the optimal settings tell the game to ask for features
/// only this creation can supply. Any subset of them is worse than none.
pub(crate) fn detours() -> [Detour; 6] {
    [
        Detour {
            site: &SITES[0],
            hook: hook_optimal as *const (),
            trampoline: &T_OPTIMAL,
        },
        Detour {
            site: &SITES[1],
            hook: hook_create as *const (),
            trampoline: &T_CREATE,
        },
        Detour {
            site: &SITES[2],
            hook: hook_eval_supersampling as *const (),
            trampoline: &T_EVAL_SS,
        },
        Detour {
            site: &SITES[3],
            hook: hook_eval_image_signal as *const (),
            trampoline: &T_EVAL_ISP,
        },
        Detour {
            site: &SITES[4],
            hook: hook_ss_enqueue as *const (),
            trampoline: &T_SS_ENQUEUE,
        },
        Detour {
            site: &SITES[5],
            hook: hook_scaling_option as *const (),
            trampoline: &T_SCALING,
        },
    ]
}

/// Handles handed to the game in place of real NGX features.
///
/// They are never dereferenced here -- both evaluations are this mod's --
/// but the game does pass them back to `NVSDK_NGX_D3D12_ReleaseFeature` when
/// it rebuilds its targets, and handing the driver a handle it did not
/// create is an access violation in `_nvngx.dll`. The release entry point is
/// intercepted for these two values instead; recognisable constants are used
/// rather than anything that looks like a pointer so the check cannot match
/// by accident.
///
/// Detouring the engine's own release function is not available:
/// `FUN_1423848d0` has a `jz` four bytes in and `FUN_1423837a0` has one at
/// fourteen, so neither has enough relocatable prologue to patch.
pub const FAKE_SUPERSAMPLING: usize = 0x0000_D155_0000_0001;
pub const FAKE_IMAGE_SIGNAL: usize = 0x0000_D155_0000_0009;

/// Whether a handle is one of ours, and so must not reach the driver.
pub fn is_ours(handle: usize) -> bool {
    handle == FAKE_SUPERSAMPLING || handle == FAKE_IMAGE_SIGNAL
}

/// Offsets into the game's DLSS object, which is 0x48 bytes.
const NGX_SUPERSAMPLING: usize = 0x38;
const NGX_IMAGE_SIGNAL: usize = 0x40;

/// The 0x40-byte descriptor both evaluations take: colour at +0x08, motion
/// at +0x10, output at +0x18, depth at +0x20, the blend rate at +0x28 and
/// the rectangle at +0x2c. Only two of them are read here.
const D_COLOUR: usize = 0x08;
const D_OUTPUT: usize = 0x18;
/// Left, top, right, bottom as four floats: the engine's viewport when the
/// stage was enqueued. Measured as (0, 0, 3801, 2138) with 3801x2138 targets.
const D_RECTANGLE: usize = 0x2c;

static T_OPTIMAL: AtomicUsize = AtomicUsize::new(0);
static T_CREATE: AtomicUsize = AtomicUsize::new(0);
static T_EVAL_SS: AtomicUsize = AtomicUsize::new(0);
static T_EVAL_ISP: AtomicUsize = AtomicUsize::new(0);
static T_SS_ENQUEUE: AtomicUsize = AtomicUsize::new(0);
static T_SCALING: AtomicUsize = AtomicUsize::new(0);

type OptimalFn = unsafe extern "system" fn(*mut u32, usize, u32, u32, i32) -> *mut u32;
type CreateFn = unsafe extern "system" fn(usize, u32, u32, i32, u32, usize) -> u32;
type EvaluateFn = unsafe extern "system" fn(usize, usize) -> u32;
type EnqueueFn = unsafe extern "system" fn(usize) -> u64;
type ScalingOptionFn = unsafe extern "system" fn(u8) -> u64;

/// What this frame is to be resolved from, recorded when the engine's
/// temporal pass is identified and used when the upscale slot comes round.
///
/// Not taken from the supersampling evaluation. Asking the engine for both
/// features puts it in a state it never ships: the supersampling pass gets
/// recorded into the command stream, and the barrier resolve that walks that
/// stream afterwards dereferences every resource in the recorded descriptor
/// -- including a depth target it fetches through a call that cannot be
/// predicted from here, and which can be null. Asking for the upscale alone
/// keeps the engine on a path it supports, and the inputs come from the
/// same identification that drives DLAA.
#[derive(Clone, Copy)]
struct Inputs {
    colour: usize,
    motion: usize,
    depth: usize,
    render: (u32, u32),
    jitter: (f32, f32),
    motion_scale: (f32, f32),
    /// The serials `alive` knew the depth and the motion vectors by when
    /// this was recorded, or `None` for one it had no living witness for.
    depth_serial: Option<u64>,
    motion_serial: Option<u64>,
    /// The command list the temporal dispatch was recorded on. The engine
    /// records a frame's temporal pass and its upscale stage on one list, in
    /// that order (every `[path] frame` block of the 9 October session), so
    /// a slot on the same list is the same frame's.
    list: usize,
}

static INPUTS: Mutex<Option<Inputs>> = Mutex::new(None);
/// How many times the record has been made, and the count at which frame
/// generation last took it: equal means nothing new since.
static NOTED: AtomicU64 = AtomicU64::new(0);
static TAKEN: AtomicU64 = AtomicU64::new(0);

/// What the last frame was resolved from, for anything that needs the depth
/// and motion vectors later in the frame than the pass that identified them.
///
/// Frame generation is evaluated at Present, by which point every list the
/// engine recorded is closed and submitted -- so the inputs have to be carried
/// forward from here rather than identified again.
///
/// Nothing is returned for a record that names a destroyed resource. The
/// record is a pair of bare addresses kept from one point in a frame to
/// another, and the engine rebuilds its targets whenever the render ratio
/// changes; `alive` is how an address is known still to be the resource it
/// was recorded as.
pub fn last_inputs() -> Option<(usize, usize, (u32, u32), (f32, f32), (f32, f32))> {
    let inputs = (*INPUTS.lock())?;
    let living = |resource: usize, serial: Option<u64>| {
        serial.is_some_and(|serial| crate::alive::is(resource, serial))
    };
    (living(inputs.depth, inputs.depth_serial) && living(inputs.motion, inputs.motion_serial))
        .then_some((
            inputs.depth,
            inputs.motion,
            inputs.render,
            inputs.jitter,
            inputs.motion_scale,
        ))
}

/// The same, for frame generation, and at most once per record.
///
/// Frame generation describes one rendered frame with these, so it must not
/// be handed the previous frame's again when no temporal pass has run since:
/// depth and motion vectors a frame old, tagged as this frame's, are worse
/// than none. `None` then, and it switches generation off for the frame.
pub fn frame_inputs() -> Option<(usize, usize, (u32, u32), (f32, f32), (f32, f32))> {
    let noted = NOTED.load(Ordering::Relaxed);
    if TAKEN.swap(noted, Ordering::Relaxed) == noted {
        return None;
    }
    last_inputs()
}

/// Record what the temporal pass is being resolved from.
///
/// Every frame, because frame generation reads it every frame: recorded
/// once a session, the tags would name whatever came to live at two old
/// addresses after the engine's first rebuild of its targets.
pub fn note_frame(frame: &crate::dlss::Frame, list: usize) {
    *INPUTS.lock() = Some(Inputs {
        list,
        colour: frame.colour as usize,
        motion: frame.motion as usize,
        depth: frame.depth as usize,
        render: frame.render,
        jitter: frame.jitter,
        motion_scale: frame.motion_scale,
        depth_serial: crate::alive::serial_of(frame.depth as usize),
        motion_serial: crate::alive::serial_of(frame.motion as usize),
    });
    NOTED.fetch_add(1, Ordering::Relaxed);
}

/// The count of records at which the upscale slot last took one, and how
/// many times the one it took had been made on a different command list.
static SLOT_TOOK: AtomicU64 = AtomicU64::new(0);
static OTHER_LIST: AtomicU64 = AtomicU64::new(0);

/// This frame's inputs, for the resolve at the engine's upscale stage, and
/// at most once per record: the jitter, the depth and the motion vectors of
/// the temporal dispatch that preceded this slot, and nothing if no temporal
/// pass has run since the last slot took them. A frame resolved with another
/// frame's jitter is worse than a frame not resolved.
///
/// `list` is the list the slot is being recorded on. The record is expected
/// to have been made on the same one; when it was not, the two may belong to
/// different frames, and that is counted and said rather than assumed away.
pub fn slot_inputs(list: usize) -> Option<(usize, usize, (u32, u32), (f32, f32), (f32, f32))> {
    let noted = NOTED.load(Ordering::Relaxed);
    if SLOT_TOOK.swap(noted, Ordering::Relaxed) == noted {
        return None;
    }
    let recorded_on = (*INPUTS.lock())?.list;
    if recorded_on != list {
        let count = OTHER_LIST.fetch_add(1, Ordering::Relaxed) + 1;
        if count == 1 || count.is_multiple_of(4096) {
            crate::log!(
                "[hij] the upscale slot is on list {list:#x} and the temporal pass whose inputs \
                 it is taking was recorded on {recorded_on:#x} ({count} time(s) this session). \
                 They have always been one list; on two, the jitter and the motion vectors \
                 could be another frame's"
            );
        }
    }
    last_inputs()
}

static FAILURES: AtomicU64 = AtomicU64::new(0);
static SAID_COLOURS: AtomicBool = AtomicBool::new(false);

/// The `ID3D12Resource` behind one of the engine's texture objects.
///
/// Two indirections, both read out of the evaluation functions rather than
/// assumed: the object holds a view at +0x60 and the view holds the resource
/// at +0x20.
pub(crate) unsafe fn resource_of(object: usize) -> usize {
    if object == 0 || !readable(object + 0x60, 8) {
        return 0;
    }
    let view = *((object + 0x60) as *const usize);
    if view == 0 || !readable(view + 0x20, 8) {
        return 0;
    }
    *((view + 0x20) as *const usize)
}

/// The graphics configuration object, derived from the instruction that loads
/// it rather than written down.
unsafe fn config() -> Option<usize> {
    // Where the pointer lives is worked out once. It comes from an
    // instruction in the game's code and a slot in its data, neither of which
    // moves, and working it out costs two `VirtualQuery` system calls -- on a
    // path that runs for every submission and every frame. A failure is not
    // remembered: early on the code is still packed, and it has to be asked
    // again.
    static SLOT: AtomicUsize = AtomicUsize::new(0);
    let mut slot = SLOT.load(Ordering::Relaxed);
    if slot == 0 {
        // Only for the build `gate` has recognised, which is also what says
        // the instruction below is the one expected: it is one of the sites.
        if !crate::gate::engaged() {
            return None;
        }
        let base = image();
        if base == 0 {
            return None;
        }
        let load = base + CONFIG_LOAD;
        let displacement = ((load + 3) as *const i32).read_unaligned();
        slot = (load as i64 + 7 + displacement as i64) as usize;
        if !readable(slot, 8) {
            return None;
        }
        SLOT.store(slot, Ordering::Relaxed);
    }
    let object = *(slot as *const usize);
    (object != 0).then_some(object)
}

/// The configuration object, for `pathdump`, which only reads it.
pub(crate) unsafe fn config_object() -> Option<usize> {
    config()
}

/// The position the Resolution Scaling handler was last called with, before
/// anything was substituted for it; `None` if it has not been called.
pub(crate) fn menu_position() -> Option<u32> {
    match MENU_POSITION.load(Ordering::Relaxed) {
        u32::MAX => None,
        position => Some(position),
    }
}

/// The render ratio this library is asking the engine for.
pub(crate) fn ratio_wanted() -> f32 {
    wanted_ratio().clamp(SMALLEST, LARGEST)
}

/// The display size the engine is at, for tests of whether a texture could
/// be one of its full-frame targets.
pub(crate) fn display_now() -> Option<(u32, u32)> {
    unsafe { display_size() }
}

/// `slot_idle` without its log line, for a reader that only reports.
pub(crate) fn slot_is_idle() -> bool {
    DISPATCHES_WITHOUT_SLOT.load(Ordering::Relaxed) >= 2
}

/// Whether `length` bytes at `offset` of the configuration object can be
/// used, asked of the system once per object rather than once per call.
///
/// The same arrangement as `native::context`: the check is a system call,
/// the object does not change from one frame to the next, and when it does
/// change the pointer changes with it and the check is made again.
unsafe fn checked(
    object: usize,
    offset: usize,
    length: usize,
    for_writing: bool,
    validated: &AtomicUsize,
) -> bool {
    if validated.load(Ordering::Relaxed) == object {
        return true;
    }
    let usable = if for_writing {
        writable(object + offset, length)
    } else {
        readable(object + offset, length)
    };
    if usable {
        validated.store(object, Ordering::Relaxed);
    }
    usable
}

/// The resolution the window is at.
unsafe fn display_size() -> Option<(u32, u32)> {
    static VALIDATED: AtomicUsize = AtomicUsize::new(0);
    let object = config()?;
    if !checked(object, DISPLAY_SIZE, 8, false, &VALIDATED) {
        return None;
    }
    let field = (object + DISPLAY_SIZE) as *const u32;
    let size = (field.read_unaligned(), field.add(1).read_unaligned());
    (size.0 != 0 && size.1 != 0).then_some(size)
}

/// The position the game's Image Quality (Resolution Scaling) option was last
/// applied at, kept only to be said. `u32::MAX` while it has not been.
///
/// It selects nothing. High is the engine's own scale of 1.0, Mid 0.9 and
/// Low 0.75 (the constants the handler loads, and the sizes the temporal
/// dispatch covers when the option is stepped through with nothing holding
/// it), and the two Variable entries turn on a dynamic resolution that moves
/// between 1.0 and 0.75. Render resolution has one control and it is
/// `render_scale`.
static MENU_POSITION: AtomicU32 = AtomicU32::new(u32::MAX);

/// The range the engine itself supports.
///
/// Not a guess and not this mod's: the clamp is in the engine's own code,
/// against the constants at 0x142e4fe54 and 0x142e4fe58. Writing a ratio
/// below it does change the resolution -- and then a command list fails to
/// close, which the engine reports as
/// `E_INVALIDARG : commandList->Close()` before deliberately crashing.
pub const SMALLEST: f32 = 0.5;
pub const LARGEST: f32 = 1.0;
/// What `render_scale` is when the file does not say, or says 0.
pub const DEFAULT_RATIO: f32 = 0.667;

/// What a `render_scale` read from the file is taken as.
///
/// 0 is not a ratio: a file that says 0 gets the default rather than an
/// error. (Development builds read 0 as "follow the game's own option".)
pub fn ratio_from_file(asked: f32) -> f32 {
    if !asked.is_finite() || asked <= 0.0 {
        DEFAULT_RATIO
    } else {
        asked.clamp(SMALLEST, LARGEST)
    }
}

/// Say, once per value, when the file's `render_scale` is not what is used.
pub fn note_configured(asked: f32, used: f32) {
    static SAID: AtomicU32 = AtomicU32::new(u32::MAX);
    if (asked - used).abs() <= 1e-4
        || SAID.swap(asked.to_bits(), Ordering::Relaxed) == asked.to_bits()
    {
        return;
    }
    if asked <= 0.0 || !asked.is_finite() {
        crate::log!(
            "[hij] render_scale = {asked} in the file: 0 used to mean \"follow the game's Image \
             Quality option\", which no longer exists. Using the default, {used:.3}"
        );
    } else {
        crate::log!(
            "[hij] render_scale = {asked} in the file is outside what the engine supports \
             ({SMALLEST} to {LARGEST}); using {used:.3}"
        );
    }
}

/// The ratio the engine's own setter would store for `ratio` at a display
/// `width` wide: the render width cut down to a multiple of sixteen.
///
/// The same arithmetic as `FUN_14229be60`, in the same order and precision,
/// so that what is told to the engine at start-up (`hook_optimal`) and what
/// its setter stores afterwards are the same number and the engine does not
/// rebuild once for the difference.
fn snapped(ratio: f32, width: u32) -> f32 {
    // The whole window is the whole window. The setter's arithmetic would
    // make 1.0 a shade less at any width that is not a multiple of sixteen
    // (1360 of 1366), and anything less than 1.0 brings the engine's upscale
    // stage back into the frame.
    if width == 0 || ratio >= 1.0 {
        return ratio.min(1.0);
    }
    let width_f = width as f32;
    let render = ((width_f * ratio) as i64 as u32) & !0xf;
    render as f32 / width_f
}

/// Resolves done at the temporal pass, and upscale slots seen without one.
///
/// The resolve lives in the engine's temporal pass, so everything depends on
/// that pass running. Turn anti-aliasing off in the menu and it does not: the
/// hook never fires, nothing writes the upscale target, and the picture stops
/// on its last frame. The same thing was happening whenever that pass was
/// skipped, and it was read as evidence for something else twice.
///
/// Nor can it simply be resolved somewhere else, because the colour, depth
/// and motion vectors are all identified from that pass's own bindings.
/// Without it there is nothing to resolve from.
static RESOLVES: AtomicU64 = AtomicU64::new(0);
/// Temporal dispatches handled, whether or not anything was resolved in
/// them. The difference between this and `RESOLVES` is the difference
/// between "the pass is not running" and "the pass runs and nothing can be
/// resolved in it", which have different answers.
static TEMPORAL_DISPATCHES: AtomicU64 = AtomicU64::new(0);
/// Upscale slots in a row with no temporal dispatch since the last one, and
/// with a dispatch but no resolve.
static SLOTS_WITHOUT_PASS: AtomicU64 = AtomicU64::new(0);
static SLOTS_UNRESOLVED: AtomicU64 = AtomicU64::new(0);
static NO_TEMPORAL_PASS: AtomicBool = AtomicBool::new(false);
/// The dispatch count at which `NO_TEMPORAL_PASS` was last cleared, and how
/// many times in a row it has been set again within a moment of that.
static PASS_BACK_AT: AtomicU64 = AtomicU64::new(0);
static RELAPSES: AtomicU64 = AtomicU64::new(0);

/// Slots without a temporal pass before the engine is asked for the whole
/// window, so the picture keeps moving.
const PATIENCE_WITHOUT_PASS: u64 = 5;
/// Slots with a temporal pass and no resolve before the ratio is given up.
/// Longer: a rebuild of the engine's targets accounts for a few such frames
/// by itself, a failed evaluation is followed by a pause of three, and
/// giving a ratio up lasts until `render_scale` is changed.
const PATIENCE_UNRESOLVED: u64 = 12;
/// The pass going away again within this many dispatches of coming back,
/// this many times running, is not anti-aliasing being switched off.
const RELAPSE_WITHIN: u64 = 30;
const RELAPSES_BORNE: u64 = 3;

/// Note that the temporal pass resolved this frame. Called once a resolve
/// has actually been made, not when one is about to be tried.
pub fn note_resolved() {
    RESOLVES.fetch_add(1, Ordering::Relaxed);
    SLOTS_UNRESOLVED.store(0, Ordering::Relaxed);
}

/// Stop asking the engine for `render_scale` and ask for the whole window,
/// until `render_scale` is changed.
///
/// For when nothing can be resolved at that ratio for a reason that will not
/// pass by itself. The alternative that was there before -- go to the whole
/// window, and come back the moment a resolve succeeds -- succeeds at the
/// whole window, comes back, fails, and has the engine rebuild every target
/// it owns every few frames for as long as the session lasts.
fn give_up_ratio(why: &str) {
    let ratio = crate::config::get().render_scale;
    if ratio >= 1.0 || HELD_AT_FULL.swap(ratio.to_bits(), Ordering::Relaxed) == ratio.to_bits() {
        return;
    }
    crate::log!(
        "[hij] nothing can be resolved at render_scale = {ratio:.3}: {why}. The engine is asked \
         for the whole window (1.0) instead, and stays there until render_scale is changed; it \
         is not tried again by itself, because trying again is a rebuild of every target"
    );
}

/// How much of the window to render, or all of it when there is no temporal
/// pass to resolve in.
fn wanted_ratio() -> f32 {
    // DLSS cannot be used in this session, so nothing is going to resolve a
    // smaller picture: the whole window, which the engine's own temporal
    // pass then fills. One relaxed load.
    if crate::gate::dlss_unusable() {
        return 1.0;
    }
    let configured = crate::config::get().render_scale.clamp(SMALLEST, LARGEST);
    // A ratio that was given up is given up for that ratio. Any other value
    // of `render_scale` is a new question.
    let held = HELD_AT_FULL.load(Ordering::Relaxed);
    if held != 0 {
        if held == configured.to_bits() {
            return 1.0;
        }
        if HELD_AT_FULL.swap(0, Ordering::Relaxed) != 0 {
            SLOTS_UNRESOLVED.store(0, Ordering::Relaxed);
            RELAPSES.store(0, Ordering::Relaxed);
            crate::log!(
                "[hij] render_scale is now {configured:.3}, not the {:.3} that could not be \
                 resolved at; trying it",
                f32::from_bits(held)
            );
        }
    }
    if NO_TEMPORAL_PASS.load(Ordering::Relaxed) {
        return 1.0;
    }
    configured
}

/// The game is applying its Image Quality option: give the engine High.
///
/// Always, whatever the player chose. High is the engine's own scale of 1.0.
/// Any other position is a second scaling on top of the render ratio this
/// library sets through the DLSS path, and the two do not know of each
/// other: the engine draws into a fraction of targets that DLSS has been told
/// are full, and the picture comes out cropped towards a corner, the
/// character off-centre. The engine substitutes High itself only when the
/// game's own NVIDIA DLSS option reads On, which this library does not need
/// and every session so far has run without.
///
/// High also keeps the engine's dynamic resolution off, which the two
/// Variable positions would switch on.
unsafe extern "system" fn hook_scaling_option(position: u8) -> u64 {
    const HIGH: u8 = 2;
    let original: ScalingOptionFn = std::mem::transmute(T_SCALING.load(Ordering::Relaxed));
    let position_seen = position as u32;
    // Not in a session where DLSS has been found unusable. Nothing sets a
    // render ratio through the DLSS path then, so there is no second scaling
    // to keep out, and the option is the player's again, as without the mod.
    if crate::gate::dlss_unusable() {
        let result = original(position);
        if MENU_POSITION.swap(position_seen, Ordering::Relaxed) != position_seen {
            let own = config()
                .filter(|object| readable(object + OWN_SCALE, 4))
                .map(|object| ((object + OWN_SCALE) as *const f32).read_unaligned());
            crate::log!(
                "[hij] the game applied its Image Quality option at position {position_seen}. \
                 DLSS is not used in this session, so the engine's handler was given the \
                 position as it came; its own scale (cfg+0x1f0) now reads {}",
                own.map_or_else(|| "nothing readable".into(), |own| format!("{own:.4}"))
            );
        }
        return result;
    }
    let result = original(HIGH);
    if MENU_POSITION.swap(position_seen, Ordering::Relaxed) != position_seen {
        // What the engine holds after its handler has run, read back, and
        // not what it was asked for.
        let own = config()
            .filter(|object| readable(object + OWN_SCALE, 4))
            .map(|object| ((object + OWN_SCALE) as *const f32).read_unaligned());
        crate::log!(
            "[hij] the game applied its Image Quality option at position {position_seen} ({}). \
             It has no effect under this mod: the engine's handler was given High, and its own \
             scale (cfg+0x1f0) now reads {}. render_scale is the only control",
            match position_seen {
                0 => "Low",
                1 => "Mid",
                2 => "High",
                3 => "Variable, Prioritize Resolution",
                4 => "Variable, Prioritize Performance",
                _ => "not one this build knows",
            },
            own.map_or_else(|| "nothing readable".into(), |own| format!("{own:.4}"))
        );
    }
    result
}

/// `FUN_14229be60`, the engine's own setter for the DLSS ratio: `rcx` the
/// configuration object, `xmm1` the ratio. It clamps to 0.5..1.0, cuts the
/// render width down to a multiple of sixteen and stores the result at
/// +0x200. A leaf: no calls, no locks, nothing else written.
///
/// Called, not detoured. It is in `SITES` all the same, by its first
/// twenty-four bytes and its last: a function this library calls in the game
/// is as much a dependence on the build as one it patches.
const RATIO_SETTER: usize = 0x229be60;
const RATIO_SETTER_HEAD: [u8; 24] = [
    0xf3, 0x0f, 0x10, 0x05, 0xf0, 0x3f, 0xbb, 0x00, // movss xmm0,[rip+0xbb3ff0]
    0x0f, 0x2f, 0xc8, // comiss xmm1,xmm0
    0x77, 0x0d, // ja +0x0d
    0xf3, 0x0f, 0x10, 0x15, 0xdf, 0x3f, 0xbb, 0x00, // movss xmm2,[rip+0xbb3fdf]
    0x0f, 0x2f, 0xca, // comiss xmm1,xmm2
];
/// Its end: the cut to a multiple of sixteen, the store at +0x200 and the
/// return, which is what makes it the leaf described above.
const RATIO_SETTER_TAIL: usize = RATIO_SETTER + 0x52;
const RATIO_SETTER_TAIL_BYTES: [u8; 24] = [
    0x0f, 0x57, 0xc0, // xorps xmm0,xmm0
    0x83, 0xe0, 0xf0, // and eax,-0x10
    0xf3, 0x48, 0x0f, 0x2a, 0xc0, // cvtsi2ss xmm0,rax
    0xf3, 0x0f, 0x5e, 0xc1, // divss xmm0,xmm1
    0xf3, 0x0f, 0x11, 0x81, 0x00, 0x02, 0x00, 0x00, // movss [rcx+0x200],xmm0
    0xc3, // ret
];

type RatioSetterFn = unsafe extern "system" fn(usize, f32);

/// The engine's setter, once `gate` has recognised the build -- its bytes are
/// two of the sites checked as a set -- and `None` until then, and for ever
/// in a session that stood down.
unsafe fn ratio_setter() -> Option<RatioSetterFn> {
    let base = image();
    if base == 0 || !crate::gate::engaged() {
        return None;
    }
    Some(std::mem::transmute::<usize, RatioSetterFn>(
        base + RATIO_SETTER,
    ))
}

/// Where the engine keeps the ratio it is rendering at, and the copy it has
/// already built its targets for.
/// The scale the Resolution Scaling option sets, in the same object: 0.75
/// for Low, 0.9 for Mid, 1.0 for High. The option's handler stores it here
/// through its last call (`FUN_14229bde0`), clamped so that this times the
/// built ratio is never under 0.5.
const OWN_SCALE: usize = 0x1f0;
const LIVE_RATIO: usize = 0x200;
const BUILT_RATIO: usize = 0x204;

/// Change the render resolution without anything being restarted.
///
/// The engine compares these two itself, once a frame, and rebuilds its
/// target sets when they differ -- that is how its own dynamic resolution
/// works. So setting the first is the whole job: the resize goes through the
/// engine's own path, with all of its own bookkeeping, and the DLSS feature
/// is recreated for the new size by the code that already does that.
pub fn follow_configured_ratio() {
    let wanted = wanted_ratio();
    unsafe {
        let Some(object) = config() else {
            return;
        };
        static VALIDATED: AtomicUsize = AtomicUsize::new(0);
        if !checked(
            object,
            OWN_SCALE,
            BUILT_RATIO + 4 - OWN_SCALE,
            true,
            &VALIDATED,
        ) {
            return;
        }
        let live = (object + LIVE_RATIO) as *mut f32;
        let built = (object + BUILT_RATIO) as *const f32;
        // Held only while DLSS is in use. Once it has been found unusable
        // this function is still called, to take the engine back to the
        // whole window, and the engine's own scale is then the game's.
        if !crate::gate::dlss_unusable() {
            hold_own_scale(object);
        }

        // The ratio, through the engine's own setter. Asked again only when
        // what is wanted has changed or the engine holds something other than
        // what the setter last left there -- two loads otherwise, on a path
        // that runs for every submission.
        static LAST_WANTED: AtomicU32 = AtomicU32::new(u32::MAX);
        static LAST_STORED: AtomicU32 = AtomicU32::new(u32::MAX);
        let before = live.read_unaligned();
        if LAST_WANTED.load(Ordering::Relaxed) != wanted.to_bits()
            || LAST_STORED.load(Ordering::Relaxed) != before.to_bits()
        {
            let display = display_size();
            let through = match ratio_setter().filter(|_| wanted < 1.0) {
                Some(setter) => {
                    setter(object, wanted);
                    "the engine's own setter"
                }
                None => {
                    live.write_unaligned(snapped(wanted, display.map_or(0, |size| size.0)));
                    "a direct write"
                }
            };
            let stored = live.read_unaligned();
            LAST_WANTED.store(wanted.to_bits(), Ordering::Relaxed);
            LAST_STORED.store(stored.to_bits(), Ordering::Relaxed);
            if stored.to_bits() != before.to_bits() {
                crate::log!(
                    "[hij] render ratio: {wanted:.4} wanted{}; cfg+0x200 read {before:.5} and, \
                     after {through}, reads {stored:.5}{}. Built (cfg+0x204) is {:.5}; the \
                     engine rebuilds its targets when the two differ",
                    if NO_TEMPORAL_PASS.load(Ordering::Relaxed)
                        || HELD_AT_FULL.load(Ordering::Relaxed) != 0
                    {
                        " (the whole window, because nothing can be resolved at render_scale)"
                    } else {
                        ""
                    },
                    display.map_or_else(String::new, |(width, height)| format!(
                        ", which is {}x{} of {width}x{height}",
                        (width as f32 * stored) as u32,
                        (height as f32 * stored) as u32
                    )),
                    built.read_unaligned()
                );
            }
        }

        // What the engine has built, said when it changes, with its own
        // scale as it reads at that moment: the engine resets that scale to
        // 1.0 in the same step that adopts a new ratio, and this is where
        // that is seen to have happened rather than assumed.
        static BUILT_SAID: AtomicU32 = AtomicU32::new(u32::MAX);
        let now_built = built.read_unaligned();
        BUILT.store(now_built.to_bits(), Ordering::Relaxed);
        if BUILT_SAID.swap(now_built.to_bits(), Ordering::Relaxed) != now_built.to_bits() {
            let display = display_size();
            crate::log!(
                "[hij] the engine's targets are built for a ratio of {now_built:.5} \
                 (cfg+0x204){}; its own scale (cfg+0x1f0) reads {:.4}",
                display.map_or_else(String::new, |(width, height)| format!(
                    ": {}x{} of {width}x{height}",
                    (width as f32 * now_built) as u32,
                    (height as f32 * now_built) as u32
                )),
                ((object + OWN_SCALE) as *const f32).read_unaligned()
            );
        }
        if let Some(display) = display_size() {
            DISPLAY.store(
                ((display.0 as u64) << 32) | display.1 as u64,
                Ordering::Relaxed,
            );
            crate::gpu::note_display(display);
        }
    }
}

/// The `render_scale` at which nothing could be resolved, as bits, while the
/// engine is being held at the whole window because of it; 0 otherwise.
static HELD_AT_FULL: AtomicU32 = AtomicU32::new(0);

/// The ratio the engine last built its targets for, as last read, and the
/// display size beside it; for the settings page.
static BUILT: AtomicU32 = AtomicU32::new(0);
static DISPLAY: AtomicU64 = AtomicU64::new(0);

/// What is asked for, what the engine has built, and the render size that
/// comes to: `(wanted, built, (width, height))`. `None` until the engine's
/// configuration has been read once.
pub fn ratio_status() -> Option<(f32, f32, (u32, u32))> {
    let built = f32::from_bits(BUILT.load(Ordering::Relaxed));
    let display = DISPLAY.load(Ordering::Relaxed);
    if built <= 0.0 || display == 0 {
        return None;
    }
    let (width, height) = ((display >> 32) as u32, display as u32);
    Some((
        wanted_ratio(),
        built,
        (
            (width as f32 * built) as u32,
            (height as f32 * built) as u32,
        ),
    ))
}

/// The switch of the engine's dynamic resolution, which the two Variable
/// positions of the option set and `FUN_14229bf30` writes.
const DYNAMIC_RESOLUTION: usize = 0x7b3b0;

/// Keep the engine's own resolution scale at 1.0 and its dynamic resolution
/// off, and say what was found whenever it was anything else.
///
/// The handler detour is what normally sees to this. This is for the cases
/// it cannot: the option applied before the detour was in, and anything else
/// in the engine that writes the field. Writing 1.0 is what the engine does
/// to it itself whenever the DLSS ratio changes (`FUN_1422980b0`), and a
/// zero in the switch is all its own setter stores when the option is not a
/// Variable one.
unsafe fn hold_own_scale(object: usize) {
    static DYNAMIC_CHECKED: AtomicUsize = AtomicUsize::new(0);
    static CORRECTED: AtomicU64 = AtomicU64::new(0);
    let own = (object + OWN_SCALE) as *mut f32;
    let found = own.read_unaligned();
    let dynamic = if checked(object, DYNAMIC_RESOLUTION, 1, true, &DYNAMIC_CHECKED) {
        Some((object + DYNAMIC_RESOLUTION) as *mut u8)
    } else {
        None
    };
    let switch = dynamic.map_or(0, |switch| switch.read());
    if (found - 1.0).abs() <= 1e-4 && switch == 0 {
        return;
    }
    own.write_unaligned(1.0);
    if let Some(dynamic) = dynamic {
        dynamic.write(0);
    }
    let count = CORRECTED.fetch_add(1, Ordering::Relaxed) + 1;
    // The first few, then rarely: if something keeps writing it the count is
    // the news, not ten thousand copies of the line.
    if count <= 8 || count.is_multiple_of(1024) {
        crate::log!(
            "[hij] the engine's own resolution scale (cfg+0x1f0) read {found:.4} and its \
             dynamic-resolution switch (cfg+0x7b3b0) read {switch}; a second scaling would crop \
             the picture, so they were put back. They now read {:.4} and {} (corrected {count} \
             time(s) this session)",
            own.read_unaligned(),
            dynamic.map_or(0, |switch| switch.read())
        );
    }
}

/// Answer the question the driver can no longer answer.
///
/// The real one asks `DLSSOptimalSettingsCallback` about DLSS 1 and is told
/// nothing is possible, which leaves the flags at zero -- and zero flags is
/// what makes the game treat DLSS as unusable at this resolution and hide the
/// option. Returning both flags set says: make both features. Which it then
/// does, to us.
///
/// The scale is `render_scale`, so the render resolution is settled here, in
/// our configuration, rather than in a menu the game rewrites on exit.
unsafe extern "system" fn hook_optimal(
    out: *mut u32,
    ngx: usize,
    width: u32,
    height: u32,
    mode: i32,
) -> *mut u32 {
    // Also when DLSS has been found unusable in this session: the question
    // then goes to the driver as the game asked it, and the game is told
    // what it is told without the mod.
    if out.is_null() || crate::gate::dlss_unusable() {
        let original: OptimalFn = std::mem::transmute(T_OPTIMAL.load(Ordering::Relaxed));
        let result = original(out, ngx, width, height, mode);
        if !out.is_null() {
            static SAID: AtomicBool = AtomicBool::new(false);
            if !SAID.swap(true, Ordering::Relaxed) {
                crate::log!(
                    "[hij] the game asked what DLSS could do at {width}x{height} (mode {mode}) \
                     after DLSS was found unusable; its own function answered scale {:.4}, \
                     flags {:#x}",
                    f32::from_bits(out.read_unaligned()),
                    out.add(1).read_unaligned()
                );
            }
        }
        return result;
    }
    // Snapped as the engine's own setter snaps it, for the width the engine
    // is asking about: at start-up this answer becomes both the ratio it
    // renders at and the one it builds for, without passing through that
    // setter, and an unsnapped figure there (2561 of 3840 at 0.667) costs a
    // rebuild the moment the setter stores the snapped one.
    let scale = snapped(wanted_ratio(), width);
    // {float scale; uint32 flags; float sharpness}. Both flags: bit 0 is the
    // supersampling feature, bit 1 the one that upscales.
    out.write_unaligned(scale.to_bits());
    // Only the upscale. Asking for the supersampling feature as well has the
    // engine record its pass into the command stream, and the barrier resolve
    // that walks that stream afterwards dereferences a depth target fetched
    // through a call we cannot predict -- null, and the fault lands in shared
    // resource-state code that names nothing. Bit 1 alone is a configuration
    // the engine actually ships.
    out.add(1).write_unaligned(2);
    out.add(2).write_unaligned(0f32.to_bits());
    // Whenever the answer differs from the last one given, not only the first
    // time: the engine asks again on its own occasions, and a ratio it was
    // told then is one it may have derived something from.
    static ANSWERED: AtomicU64 = AtomicU64::new(u64::MAX);
    let answer =
        ((scale.to_bits() as u64) << 32) | (width as u64 & 0xffff) << 16 | (height as u64 & 0xffff);
    if ANSWERED.swap(answer, Ordering::Relaxed) != answer {
        crate::log!(
            "[hij] the game asked what DLSS could do at {width}x{height} (mode {mode}); \
             telling it {scale:.5} ({}x{}), upscale only",
            (width as f32 * scale) as u32,
            (height as f32 * scale) as u32
        );
    }
    out
}

/// Say the features exist without asking the driver for them.
///
/// It would refuse: feature 9, Image Signal Processing, is the sharpening
/// half of DLSS 1 and is gone. The handles are never dereferenced by anything
/// but the two evaluations, and both of those are ours now, so a non-null
/// marker is enough -- and it has to be non-null, because the game tests
/// exactly that to decide DLSS is running.
unsafe extern "system" fn hook_create(
    ngx: usize,
    out_width: u32,
    out_height: u32,
    mode: i32,
    unused: u32,
    list: usize,
) -> u32 {
    if ngx == 0 {
        let original: CreateFn = std::mem::transmute(T_CREATE.load(Ordering::Relaxed));
        return original(ngx, out_width, out_height, mode, unused, list);
    }
    *((ngx + NGX_SUPERSAMPLING) as *mut usize) = FAKE_SUPERSAMPLING;
    *((ngx + NGX_IMAGE_SIGNAL) as *mut usize) = FAKE_IMAGE_SIGNAL;
    // Once for every rebuild of the engine's targets; bounded for the same
    // reason as the release line in `ngx.rs`.
    static SAID: crate::logger::Throttle = crate::logger::Throttle::new(12, 4, 60);
    crate::log_limited!(
        SAID,
        "[hij] the game wanted DLSS features for {out_width}x{out_height} (mode {mode}); ours stand in"
    );
    1
}

/// Remember what the supersampling pass was given, and do nothing else.
///
/// Its inputs are the ones DLSS 4 wants -- colour before the engine's own
/// temporal filter, dense motion vectors, depth -- but its output is a
/// render-sized intermediate, and resolving into that would throw away the
/// upscale. So the work waits for the evaluation that has the full-size
/// target.
unsafe extern "system" fn hook_eval_supersampling(descriptor: usize, list: usize) -> u32 {
    if descriptor == 0 {
        let original: EvaluateFn = std::mem::transmute(T_EVAL_SS.load(Ordering::Relaxed));
        return original(descriptor, list);
    }
    // Nothing to do. With only the upscale feature asked for, this should
    // never run at all; if it does, doing nothing is the safe answer.
    1
}

/// Resolve and upscale in one, where the engine expected only a sharpen.
///
/// This is the evaluation whose output is `UpscaleOut`, at the window's
/// resolution, and after it the engine switches the rest of the frame over to
/// that target. So everything downstream -- post, UI, present -- is at full
/// resolution without anything else having to be arranged.
unsafe extern "system" fn hook_eval_image_signal(descriptor: usize, list: usize) -> u32 {
    let perf = crate::perf::start(crate::perf::Family::UpscaleSlot);
    let result = eval_image_signal(descriptor, list);
    perf.stop();
    result
}

unsafe fn eval_image_signal(descriptor: usize, list: usize) -> u32 {
    if descriptor == 0 {
        let original: EvaluateFn = std::mem::transmute(T_EVAL_ISP.load(Ordering::Relaxed));
        return original(descriptor, list);
    }
    // With `path_dump` on: what the engine handed this stage, when it
    // changes, and a mark in the frame being recorded.
    crate::pathdump::slot(descriptor);
    let Some(inputs) = *INPUTS.lock() else {
        return 1;
    };
    let output = resource_of(*((descriptor + D_OUTPUT) as *const usize));
    let Some(display) = display_size() else {
        return 1;
    };
    if inputs.colour == 0 || inputs.motion == 0 || inputs.depth == 0 || output == 0 {
        if FAILURES.fetch_add(1, Ordering::Relaxed) == 0 {
            crate::log!(
                "[hij] cannot resolve: colour={:#x} motion={:#x} depth={:#x} output={:#x}",
                inputs.colour,
                inputs.motion,
                inputs.depth,
                output
            );
        }
        return 1;
    }
    // Which colour the resolve reads, said once with the alternative beside
    // it. Ours is the texture the engine's temporal pass reads; the
    // descriptor's is whatever the post chain is holding when the upscale
    // slot comes round. The first is the one used.
    let from_descriptor = resource_of(*((descriptor + D_COLOUR) as *const usize));
    let colour = inputs.colour;
    if !SAID_COLOURS.swap(true, Ordering::Relaxed) {
        // Sizes too, because the whole question is whether what this writes
        // is the thing that reaches the screen. An output that turns out to
        // be the render size rather than the window would mean the engine
        // reallocated it and the upscale has nowhere to go.
        let shape = |resource: usize| {
            crate::gpu::described(resource)
                .map(|info| format!("{}x{}", info.Width, info.Height))
                .unwrap_or_else(|| "unknown".into())
        };
        crate::log!(
            "[hij] colour candidates: identified {:#x} ({}), upscale slot {:#x} ({}), using {:#x}",
            inputs.colour,
            shape(inputs.colour),
            from_descriptor,
            shape(from_descriptor),
            colour
        );
        crate::log!(
            "[hij] writing into {:#x} ({}), and the window is {}x{}",
            output,
            shape(output),
            display.0,
            display.1
        );
    }

    *UPSCALE_TARGET.lock() = Some((output, display, crate::alive::serial_of(output)));
    SLOT_RUNS.fetch_add(1, Ordering::Relaxed);
    // From here to Present is where the post chain finishes and the interface
    // is drawn, which is the window the interface-free frame hunt watches.
    crate::hudless::open_frame();

    // The resolve itself is not done here. The temporal pass does it, earlier
    // in the frame, writing straight into the target this slot has just
    // named (`gpu::run_dlss`), so by now the target is already written.
    //
    // This slot runs whether or not the temporal pass does, which makes it
    // the clock for noticing that the pass has stopped -- turning
    // anti-aliasing off in the menu is enough. A few frames of grace, in case
    // of an ordinary gap, then back to rendering the whole window so the
    // picture keeps moving instead of freezing on its last frame.
    //
    // Two different things can be missing, and they are told apart by
    // whether the temporal pass was dispatched at all since the last slot.
    // Not dispatched: there is no pass, which is what anti-aliasing being
    // off looks like, and it ends when a dispatch is next seen. Dispatched
    // and nothing resolved: the pass runs and its inputs cannot be used at
    // this ratio, and that does not end by itself.
    static SEEN: AtomicU64 = AtomicU64::new(0);
    static DISPATCHES_SEEN: AtomicU64 = AtomicU64::new(0);
    let resolves = RESOLVES.load(Ordering::Relaxed);
    let dispatches = TEMPORAL_DISPATCHES.load(Ordering::Relaxed);
    let dispatched = DISPATCHES_SEEN.swap(dispatches, Ordering::Relaxed) != dispatches;
    if SEEN.swap(resolves, Ordering::Relaxed) != resolves {
        SLOTS_WITHOUT_PASS.store(0, Ordering::Relaxed);
    } else if dispatched {
        SLOTS_WITHOUT_PASS.store(0, Ordering::Relaxed);
        if SLOTS_UNRESOLVED.fetch_add(1, Ordering::Relaxed) + 1 == PATIENCE_UNRESOLVED {
            give_up_ratio(&format!(
                "the temporal pass ran on each of the last {PATIENCE_UNRESOLVED} frames and \
                 nothing was resolved in any of them (the [dls] lines above say what stopped it)"
            ));
        }
    } else if SLOTS_WITHOUT_PASS.fetch_add(1, Ordering::Relaxed) + 1 == PATIENCE_WITHOUT_PASS
        && !NO_TEMPORAL_PASS.swap(true, Ordering::Relaxed)
    {
        // Gone again within a moment of coming back, several times running:
        // the pass is there at the whole window and not at `render_scale`,
        // and following it would rebuild for ever.
        let since = dispatches.wrapping_sub(PASS_BACK_AT.load(Ordering::Relaxed));
        let relapses = if PASS_BACK_AT.load(Ordering::Relaxed) != 0 && since <= RELAPSE_WITHIN {
            RELAPSES.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            RELAPSES.store(0, Ordering::Relaxed);
            0
        };
        if relapses >= RELAPSES_BORNE {
            NO_TEMPORAL_PASS.store(false, Ordering::Relaxed);
            give_up_ratio(&format!(
                "the engine's temporal pass ran at the whole window and stopped within \
                 {RELAPSE_WITHIN} frames of going back to this ratio, {relapses} times running"
            ));
        } else {
            crate::log!(
                "[hij] no temporal pass for {PATIENCE_WITHOUT_PASS} upscale slots -- \
                 anti-aliasing is off, or the engine is not drawing a scene -- so the engine is \
                 asked for the whole window until the pass is seen again, instead of the \
                 picture standing still"
            );
        }
    }
    // Everything frame generation needs is identified and the frame is still
    // ours, which makes this the hand-over point.
    //
    // The native list. With ReShade loaded the engine's list is a proxy, and
    // everything downstream of here keys on the native address: the barrier
    // tracking in `states.rs` is filled from vtable hooks entered with native
    // pointers, and the tags hand Streamline a list to record copies on.
    static LIST: crate::reshade::Boundary = crate::reshade::Boundary::new(
        "the engine's command list at the upscale slot",
        "the state tracking and the frame generation tags get the native list",
    );
    let engine_list = list as *mut c_void;
    let list = LIST.native(engine_list);
    // The list as the engine holds it, kept in view for anything below that
    // has to hand NGX a list from the same side of ReShade as NGX's device.
    // Only when the two differ, which is when ReShade wraps the engine's
    // lists.
    let _held = (engine_list != list).then(|| crate::reshade::hold(engine_list));
    // The resolve, from the colour the engine holds at this stage: the frame
    // after the passes the engine runs between its temporal pass and here,
    // which carry the picture's tone. The size of what is to be resolved is
    // the rectangle the engine put in the descriptor for its own upscaler.
    let corner =
        |index: usize| ((descriptor + D_RECTANGLE + index * 4) as *const f32).read_unaligned();
    let (left, top, right, bottom) = (corner(0), corner(1), corner(2), corner(3));
    let rectangle = (left == 0.0 && top == 0.0 && right >= 1.0 && bottom >= 1.0)
        .then_some((right as u32, bottom as u32));
    crate::gpu::resolve_at_slot(list, from_descriptor, output, display, rectangle);
    crate::fg::note_frame_inputs(list, display);
    1
}

/// Keep the supersampling pass from binding a target that was never made.
unsafe extern "system" fn hook_ss_enqueue(node_list: usize) -> u64 {
    let original: EnqueueFn = std::mem::transmute(T_SS_ENQUEUE.load(Ordering::Relaxed));
    if node_list == 0 || !readable(node_list + 8, 8) {
        return 0;
    }
    let node = *((node_list + 8) as *const usize);
    if node == 0 {
        return 0;
    }
    // Every target the pass binds, not only the one that was null last time.
    // It takes no precautions of its own -- all it checks is that the DLSS
    // object exists -- so any absent target is an access violation deep in the
    // engine's resource state tracking, at an address that says nothing about
    // which one was missing. Naming them here is the whole point.
    let at = |offset: usize| -> usize {
        if readable(node + offset, 8) {
            *((node + offset) as *const usize)
        } else {
            0
        }
    };
    let targets = [
        ("colour", at(NODE_COLOUR)),
        ("dense motion", at(NODE_DENSE_MOTION)),
        ("history 0", at(NODE_RING)),
        ("history 1", at(NODE_RING + 8)),
        ("history 2", at(NODE_RING + 16)),
    ];
    if targets.iter().any(|(_, resource)| *resource == 0) {
        if SKIPPED.fetch_add(1, Ordering::Relaxed) == 0 {
            crate::log!(
                "[hij] the supersampling pass is missing a target, so it is skipped rather than left to bind nothing: {}",
                targets
                    .iter()
                    .map(|(name, resource)| format!("{name}={resource:#x}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        return 0;
    }
    if PASSED.fetch_add(1, Ordering::Relaxed) == 0 {
        crate::log!("[hij] the supersampling pass has every target it binds");
    }
    original(node_list)
}

static SKIPPED: AtomicU64 = AtomicU64::new(0);
static PASSED: AtomicU64 = AtomicU64::new(0);

/// The sampler bank to select: the configured one, except that `mip_bias =
/// 1` selects bank 1 only once the instruction it is built from has been
/// rewritten for the ratio.
///
/// Without that rewrite bank 1 is the engine's stock -1.0 -- a full mip
/// level of bias, which is wrong for every ratio but 0.5 and worst at 1.0.
/// The rewrite itself is `tune::prepare_exact_bias`, made by `gate` in the
/// same step as the detours, from `render_scale` as it reads then: the
/// engine builds its sampler banks once, with the device, so the bias is a
/// start-up quantity however live the ratio is.
pub fn mip_bias_bank() -> u32 {
    let settings = crate::config::get();
    if settings.mip_bias != 1 {
        return settings.mip_bias;
    }
    u32::from(crate::tune::bias_built().is_some())
}

/// What the mip bias was built for, if it was: `(ratio, bias)`.
pub fn mip_bias_built() -> Option<(f32, f32)> {
    crate::tune::bias_built()
}

/// The full-size target the upscale slot writes, once it has been seen.
///
/// Remembered so the resolve can happen somewhere else. The engine only
/// reveals which resource it is by passing it to that slot, so the first
/// frame goes without -- after which it is known and stable.
/// With the serial `alive` knew it by, so that a different texture later
/// created at the same address is not taken for it.
static UPSCALE_TARGET: Mutex<Option<(usize, (u32, u32), Option<u64>)>> = Mutex::new(None);

/// Where to write, and how big it is, for a resolve done elsewhere.
pub fn upscale_target() -> Option<(usize, (u32, u32))> {
    let (target, size, serial) = (*UPSCALE_TARGET.lock())?;
    // Only while it is the texture the slot named. The engine discards it at
    // every rebuild, and what is remembered is an address.
    serial
        .is_some_and(|serial| crate::alive::is(target, serial))
        .then_some((target, size))
}

/// How many times the upscale slot has run, the count at the last temporal
/// dispatch, and how many dispatches in a row have found it unchanged.
static SLOT_RUNS: AtomicU64 = AtomicU64::new(0);
static SLOT_RUNS_SEEN: AtomicU64 = AtomicU64::new(0);
static DISPATCHES_WITHOUT_SLOT: AtomicU64 = AtomicU64::new(0);
static SAID_SLOT_IDLE: AtomicBool = AtomicBool::new(false);

/// A temporal dispatch is being handled: note whether the upscale slot has
/// run since the last one. Once per dispatch.
pub fn note_dispatch() {
    let dispatches = TEMPORAL_DISPATCHES.fetch_add(1, Ordering::Relaxed) + 1;
    // The pass is running, whatever becomes of this frame: that is what ends
    // the whole-window spell that its absence began. A successful resolve
    // does not, because a resolve at the whole window is one too.
    if NO_TEMPORAL_PASS.swap(false, Ordering::Relaxed) {
        PASS_BACK_AT.store(dispatches, Ordering::Relaxed);
        crate::log!(
            "[hij] the temporal pass is being dispatched again; the engine is asked for \
             render_scale again"
        );
    }
    let runs = SLOT_RUNS.load(Ordering::Relaxed);
    let seen = SLOT_RUNS_SEEN.swap(runs, Ordering::Relaxed);
    if runs == seen {
        DISPATCHES_WITHOUT_SLOT.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // The slot has run since the last temporal dispatch. Once, and it is
    // this frame's or the last one's: the stage is in the frame. More than
    // once, and it ran on frames this pass was no part of -- anti-aliasing
    // was off, or a loading screen -- so the count says nothing about the
    // frame now being recorded, and the target remembered from then may be
    // one the engine has since discarded. The built ratio is the better
    // witness for that one dispatch: the engine has no upscale stage at 1.0.
    //
    // (The built ratio is not used as the mode outright, at every dispatch.
    // The engine's own test for running the stage has six more terms than
    // the ratio -- mUseDLISP, the flags of the optimal-settings answer, the
    // features having been created, the target set having an UpscaleOut and
    // not being in its temporal-upsample mode -- it is made on another thread
    // at another point of the frame, and against the target set of the view
    // being drawn, which has not been shown to be the one at cfg+0x1f450.
    // Counting what was observed needs none of that to be right.)
    let after_a_gap = runs.wrapping_sub(seen) > 1;
    let built = f32::from_bits(BUILT.load(Ordering::Relaxed));
    if after_a_gap && built >= 1.0 {
        DISPATCHES_WITHOUT_SLOT.store(2, Ordering::Relaxed);
        crate::log!(
            "[hij] the temporal pass is running again after {} upscale slots without it, and \
             the engine's targets are built for a ratio of {built:.4}, which has no upscale \
             stage: the resolve goes to the pass's own output from this frame",
            runs.wrapping_sub(seen)
        );
        return;
    }
    DISPATCHES_WITHOUT_SLOT.store(0, Ordering::Relaxed);
    if SAID_SLOT_IDLE.swap(false, Ordering::Relaxed) {
        crate::log!(
            "[hij] the engine's upscale stage is running again; the resolve goes to its \
             full-size target and it drives frame generation"
        );
    }
}

/// Whether the engine has stopped running its upscale stage.
///
/// It does at a render ratio of 1.0: with nothing to upscale the stage is
/// not part of the frame, the slot this module hooks never fires, and the
/// finished picture is whatever the temporal pass itself wrote. Everything
/// that was hung on the slot then has to happen at the temporal dispatch
/// instead -- the resolve into the pass's own output, and frame generation's
/// inputs -- or the picture stops, which is what `render_scale = 1.0` did.
///
/// Two dispatches without the slot in between, not one: the two are recorded
/// on the engine's own schedule and a frame's slot can land after the next
/// frame's dispatch has begun.
pub fn slot_idle() -> bool {
    let idle = DISPATCHES_WITHOUT_SLOT.load(Ordering::Relaxed) >= 2;
    if idle && !SAID_SLOT_IDLE.swap(true, Ordering::Relaxed) {
        crate::log!(
            "[hij] the engine's upscale stage has not run for two temporal passes -- a \
             render ratio of 1.0 has none -- so the resolve goes to the temporal pass's own \
             output and that pass drives frame generation"
        );
    }
    idle
}
