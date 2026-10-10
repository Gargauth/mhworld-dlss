//! Engine settings that were tuned for the engine's own temporal filter.
//!
//! A DLSS integration inherits a renderer whose choices were all made for a
//! different resolve. Several of them are plainly wrong once DLSS is doing
//! the work, and all but one of them are data -- a field in one of two
//! settings objects. The exception is the mip bias, which rewrites the
//! operand of one instruction (`prepare_exact_bias`).
//!
//! Nothing here is written before `gate` has recognised the build: the
//! offsets below are offsets into this build's objects, and the instructions
//! the objects are reached through are among the sites it checks.
//!
//! The two objects, both reached from the instruction that loads them rather
//! than from an address written down here:
//!
//!   * `rs`  -- render settings, sampler banks, screen-space effects
//!   * `cfg` -- render config, content scale, DLSS and CAS
//!
//! What is here, and why:
//!
//! **Texture mip bias.** Rendering at a fraction of the output means every
//! texture is sampled for the smaller image, and the result is uniformly
//! soft. The usual correction is a sampler bias of log2(render/display). This
//! engine has the machinery -- fifteen complete banks of sampler states,
//! differing only in that bias, chosen by one field -- and then zeroes the
//! field at startup and exposes no option for it. So the retail game runs at
//! bias 0 and an upscaled picture is soft for a reason that has nothing to do
//! with the upscaler.
//!
//! The banks are built once at device initialisation and the choice is baked
//! into each material as it loads, so this has to be set early and cannot be
//! usefully changed later.
//! **Sharpening.** The engine turns its own post-resolve sharpen off the
//! moment DLSS is on, and DLSS itself has not sharpened since 2.5.1. The
//! engine's other sharpener, FidelityFX CAS, runs at the output resolution,
//! but only when its DLSS upscale stage is not usable, and under this mod
//! that stage always is. So `sharpen` below has no caller and no visible
//! effect; it is kept as the record of where those fields are. The `sharpen`
//! setting is this library's own pass after the resolve, in `sharpen.rs`.
//!
//! **Patterns meant for the engine's filter.** Screen-space reflections are
//! resolved with a dither and a g-buffer jitter, and contact shadows with
//! per-pixel noise, all of which the engine's temporal pass is built to
//! absorb. DLSS is not, and reads them as detail that disagrees with its
//! history.
//!
//! The checkerboard alpha (`unroll`) is held at the engine's own distance:
//! drawing alpha whole is not something DLSS needs.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use crate::native::{image, readable, writable, CodeWrite, MAX_WRITE};

/// `mov rax,[rip+disp32]` loading the object these settings live in -- the
/// same one that holds the engine's Halton table at +0xE6A0 -- and the store
/// into it that follows. The displacement is part of the signature: three
/// bytes of opcode would pass for any load at this address.
const SETTINGS_LOAD: usize = 0x2412ef;
const SETTINGS_LOAD_BYTES: [u8; 17] = [
    0x48, 0x8b, 0x05, 0x72, 0x30, 0xf8, 0x04, // mov rax,[rip+0x4f83072]
    0xc7, 0x80, 0x60, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // mov dword [rax+0x260],0
];

/// Every site this module depends on that no other module lists, for `gate`.
/// The render configuration load is `hijack`'s.
pub(crate) static SITES: [crate::gate::Site; 2] = [
    crate::gate::Site::new(
        "the render settings load",
        SETTINGS_LOAD,
        &SETTINGS_LOAD_BYTES,
    ),
    crate::gate::Site {
        what: "the constant sampler bank 1 is built from",
        rva: BANK_ONE_SOURCE,
        bytes: &BANK_ONE_SOURCE_BYTES,
        // The displacement, which is the operand this module rewrites.
        wild: 4..8,
    },
];

/// Both distances, from the registration of their names.
const UNROLL_NEAR: usize = 0xe964;
const UNROLL_FAR: usize = 0xe968;

static SLOT: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL: AtomicU32 = AtomicU32::new(u32::MAX);
static APPLIED: AtomicU32 = AtomicU32::new(u32::MAX);

/// Where a settings object's pointer lives, from the `mov rax,[rip+disp32]`
/// at `load`, or zero until the build has been recognised.
///
/// "Not yet" is not remembered. The first call comes from the first
/// submission on any queue, which on some machines is before the executable
/// is unpacked; a failure latched then would leave the session with no mip
/// bias. The bytes at `load` are not compared here: `gate` has, as part of
/// the set, before it reports the build as recognised.
fn slot_from(load_rva: usize, cache: &AtomicUsize) -> usize {
    let known = cache.load(Ordering::Relaxed);
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
        let load = base + load_rva;
        let displacement = ((load + 3) as *const i32).read_unaligned();
        let slot = (load as i64 + 7 + displacement as i64) as usize;
        if !readable(slot, 8) {
            return 0;
        }
        cache.store(slot, Ordering::Relaxed);
        slot
    }
}

/// Where the render settings object's pointer lives.
fn locate() -> usize {
    slot_from(SETTINGS_LOAD, &SLOT)
}

/// The render settings object itself, for `pathdump`, which only reads it.
/// `None` until the build has been recognised and the object exists.
pub(crate) unsafe fn settings_object() -> Option<usize> {
    let slot = locate();
    if slot == 0 {
        return None;
    }
    let object = *(slot as *const usize);
    (object != 0).then_some(object)
}

/// Push the distance at which alpha becomes a checkerboard out to `distance`.
///
/// Zero or less restores whatever the engine had, so the comparison can be
/// made both ways while the game runs.
pub fn unroll(distance: f32) {
    // Nothing to do once the wanted value is the applied one, and that is
    // found out before anything else: this is called on every submission,
    // several times a frame, and validating the engine's object with
    // `VirtualQuery` is a system call.
    let original = ORIGINAL.load(Ordering::Relaxed);
    if original != u32::MAX {
        let wanted = if distance > 0.0 {
            distance.to_bits()
        } else {
            original
        };
        if APPLIED.load(Ordering::Relaxed) == wanted {
            return;
        }
    }
    let slot = locate();
    if slot == 0 {
        return;
    }
    unsafe {
        let object = *(slot as *const usize);
        if object == 0 || !writable(object + UNROLL_NEAR, 8) {
            return;
        }
        let near = (object + UNROLL_NEAR) as *mut f32;
        let far = (object + UNROLL_FAR) as *mut f32;

        // Whatever the engine shipped, remembered once so turning this off is
        // a return rather than a different guess.
        if ORIGINAL.load(Ordering::Relaxed) == u32::MAX {
            ORIGINAL.store(far.read_unaligned().to_bits(), Ordering::Relaxed);
            crate::log!(
                "[tun] the engine dithers alpha beyond {:.1} and {:.1}",
                near.read_unaligned(),
                far.read_unaligned()
            );
        }
        let wanted = if distance > 0.0 {
            distance
        } else {
            f32::from_bits(ORIGINAL.load(Ordering::Relaxed))
        };
        if APPLIED.swap(wanted.to_bits(), Ordering::Relaxed) == wanted.to_bits() {
            return;
        }
        near.write_unaligned(wanted);
        far.write_unaligned(wanted);
        crate::log!("[tun] alpha is drawn whole out to {wanted:.1}");
    }
}

// ---------------------------------------------------------------------------
// The texture mip bias the engine never applies
// ---------------------------------------------------------------------------

/// Which of the fifteen sampler banks every material binds through.
///
/// Bank 0 is no bias; 1 to 7 are -log2(2.0) through -log2(5.0) and 8 to 14
/// the same positive. Zeroed at startup and absent from the options file, so
/// the retail game is always bank 0.
const SAMPLER_BANK: usize = 0x1a8;

/// Choose the sampler bank, which is the only texture mip bias this engine
/// has.
///
/// Early or not at all: the banks are built once at device initialisation and
/// each material records which one it uses as it loads, so a change after a
/// zone is built does not reach the materials already in it.
pub fn mip_bias(bank: u32) {
    static APPLIED: AtomicU32 = AtomicU32::new(u32::MAX);
    if bank == 0 || bank > 14 {
        return;
    }
    // Applied already: see `unroll` for why this is asked first.
    if APPLIED.load(Ordering::Relaxed) == bank {
        return;
    }
    let slot = locate();
    if slot == 0 {
        return;
    }
    unsafe {
        let object = *(slot as *const usize);
        if object == 0 || !writable(object + SAMPLER_BANK, 4) {
            return;
        }
        let field = (object + SAMPLER_BANK) as *mut u32;
        if field.read_unaligned() == bank || APPLIED.swap(bank, Ordering::Relaxed) == bank {
            return;
        }
        field.write_unaligned(bank);
        // The ladder, unless bank 1 has been rebuilt from a different
        // constant -- in which case that line already said what it is, and
        // printing the stock value here would contradict it.
        if bank == 1 && REBUILT.load(Ordering::Relaxed) {
            crate::log!(
                "[tun] sampler bank 1 selected; what its bias is depends on the [tun] line above"
            );
            return;
        }
        let bias = match bank {
            1..=7 => -(2.0f32 + (bank as f32 - 1.0) * 0.5).log2(),
            _ => (2.0f32 + (bank as f32 - 8.0) * 0.5).log2(),
        };
        crate::log!("[tun] sampler bank {bank}, a texture mip bias of {bias:+.3}");
    }
}

// ---------------------------------------------------------------------------
// The engine's own choice of sampler at each draw, which is not the bank's
// ---------------------------------------------------------------------------

/// `mpPrimaryScene` in the render config, and the attribute word of a scene.
const PRIMARY_SCENE: usize = 0x1f450;
const SCENE_ATTRIBUTE: usize = 0x58;
/// The attribute bit the material bind tests before it swaps samplers.
const DYNAMIC_SAMPLERS: u32 = 0x0080_0000;
/// `mDynamicResolutionMode` in the render config, 0 or 1, which is what
/// that swap picks its group by.
const DYNAMIC_RESOLUTION_MODE: usize = 0x7b3b4;

/// Say whether the engine replaces the samplers a material was loaded with,
/// when that is first known and whenever it changes.
///
/// Read in a disassembler and not yet seen to happen: the material bind,
/// `FUN_14235f450`, tests bit 0x800000 of the scene's attribute, and with it
/// set binds, in place of the material's own samplers 0 to 5, 10 and 11, one
/// of six groups of sampler bank 0 -- the groups the engine keeps for its
/// dynamic resolution, whose biases are log2 of a scale less one. Bank 0,
/// whatever `SAMPLER_BANK` says: so with the bit set the bias this module
/// builds does not reach those samplers, which are the ones textures are
/// read through. Whether the bit is ever set on this platform could not be
/// told from the code. This is the line that tells.
///
/// The scene read is the configuration's primary scene; the bind tests the
/// scene of its own draw context, which is taken to be the same one.
pub fn note_sampler_override() {
    static CALLS: AtomicU32 = AtomicU32::new(0);
    static SAID: AtomicU32 = AtomicU32::new(u32::MAX);
    // Called several times a frame, and what it reads is validated with a
    // system call: looked at every few hundred calls.
    if !CALLS.fetch_add(1, Ordering::Relaxed).is_multiple_of(512) {
        return;
    }
    unsafe {
        let Some(config) = crate::hijack::config_object() else {
            return;
        };
        if !readable(config + PRIMARY_SCENE, 8) || !readable(config + DYNAMIC_RESOLUTION_MODE, 4) {
            return;
        }
        let scene = ((config + PRIMARY_SCENE) as *const usize).read_unaligned();
        if scene == 0 || !readable(scene + SCENE_ATTRIBUTE, 4) {
            return;
        }
        let attribute = ((scene + SCENE_ATTRIBUTE) as *const u32).read_unaligned();
        let mode = ((config + DYNAMIC_RESOLUTION_MODE) as *const u32).read_unaligned();
        let swapped = attribute & DYNAMIC_SAMPLERS != 0;
        let state = u32::from(swapped) | ((mode & 0xff) << 1);
        if SAID.swap(state, Ordering::Relaxed) == state {
            return;
        }
        if swapped {
            crate::log!(
                "[tun] the primary scene {scene:#x} has attribute {attribute:#010x}: bit \
                 0x800000 is SET. As the engine's material bind reads, samplers 0 to 5, 10 and \
                 11 of every material are then replaced at each draw by group {mode} of sampler \
                 bank 0 (mDynamicResolutionMode = {mode}), a bias of {}, whatever bank \
                 mip_bias selects: the bias this mod builds does NOT reach those samplers",
                match mode {
                    0 => "-1.000",
                    1 => "-1.074",
                    _ => "an unknown amount",
                }
            );
        } else {
            crate::log!(
                "[tun] the primary scene {scene:#x} has attribute {attribute:#010x}: bit \
                 0x800000 is clear (mDynamicResolutionMode = {mode}). As the engine's material \
                 bind reads, materials then keep the samplers of the bank that was selected \
                 when they loaded, so the bias mip_bias builds is the one in force for them"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Sharpening, which DLSS 4 no longer does
// ---------------------------------------------------------------------------

/// The render config, which holds the sharpening and content scale. Reached
/// through the same instruction `hijack` reads the display size from.
const USE_CAS: usize = 0xbc;
const CAS_SHARPNESS: usize = 0xc0;

static CONFIG_SLOT: AtomicUsize = AtomicUsize::new(0);

fn config() -> usize {
    slot_from(crate::hijack::CONFIG_LOAD, &CONFIG_SLOT)
}

/// Turn the engine's output-resolution sharpener on, at `sharpness`.
///
/// The engine switches its own pre-resolve sharpen off when DLSS is enabled,
/// which is correct -- sharpening before an upscaler is the wrong order. It
/// then leaves nothing sharpening afterwards, and DLSS 4 will not do it
/// either, having dropped the parameter. CAS runs at the output resolution,
/// which is where it belongs.
pub fn sharpen(enabled: bool, sharpness: f32) {
    static APPLIED: AtomicU32 = AtomicU32::new(u32::MAX);
    let wanted = if enabled {
        sharpness.clamp(0.0, 1.0)
    } else {
        -1.0
    };
    // Applied already: see `unroll` for why this is asked first.
    if APPLIED.load(Ordering::Relaxed) == wanted.to_bits() {
        return;
    }
    let slot = config();
    if slot == 0 {
        return;
    }
    unsafe {
        let object = *(slot as *const usize);
        if object == 0 || !writable(object + USE_CAS, 8) {
            return;
        }
        if APPLIED.swap(wanted.to_bits(), Ordering::Relaxed) == wanted.to_bits() {
            return;
        }
        *((object + USE_CAS) as *mut bool) = enabled;
        if enabled {
            *((object + CAS_SHARPNESS) as *mut f32) = wanted;
            crate::log!("[tun] sharpening the output with CAS at {wanted:.2}");
        } else {
            crate::log!("[tun] CAS off");
        }
    }
}

// ---------------------------------------------------------------------------
// Patterns the engine's own filter was built to absorb
// ---------------------------------------------------------------------------

/// Screen-space reflections: resolved with a dither, and with the g-buffer
/// sampled on a jittered grid.
const SSLR_DITHER: usize = 0xb43f;
const SSLR_JITTER: usize = 0xb440;
/// Contact shadows: ray offsets dithered per pixel.
const CONTACT_NOISE: usize = 0xb500;
const FACIAL_CONTACT_NOISE: usize = 0xb501;

/// Stop the screen-space effects dithering themselves.
///
/// Each of these is a pattern whose detail only resolves across frames, which
/// the engine's temporal pass is built to do and DLSS is not: it sees pixels
/// that disagree with its history and blends them away, or keeps them and
/// they crawl. The cost of turning them off is a little banding in the
/// reflections and the contact shadows.
pub fn undither(enabled: bool) {
    static APPLIED: AtomicU32 = AtomicU32::new(u32::MAX);
    let slot = locate();
    if slot == 0 {
        return;
    }
    let wanted = u32::from(enabled);
    if APPLIED.swap(wanted, Ordering::Relaxed) == wanted {
        return;
    }
    if !enabled {
        return;
    }
    unsafe {
        let object = *(slot as *const usize);
        if object == 0 || !writable(object + SSLR_DITHER, 2) {
            return;
        }
        *((object + SSLR_DITHER) as *mut bool) = false;
        *((object + SSLR_JITTER) as *mut bool) = false;
        if writable(object + CONTACT_NOISE, 2) {
            *((object + CONTACT_NOISE) as *mut bool) = false;
            *((object + FACIAL_CONTACT_NOISE) as *mut bool) = false;
        }
        crate::log!("[tun] reflections and contact shadows no longer dither themselves");
    }
}

// ---------------------------------------------------------------------------
// Making one of those banks the right amount
// ---------------------------------------------------------------------------

/// `movss xmm0,[rip+disp32]` loading the 2.0 that bank 1's bias is built
/// from, as `-log2(2.0)`, with the spill and the call that follow it. The
/// four displacement bytes are the ones rewritten and are not compared.
const BANK_ONE_SOURCE: usize = 0x23d73ee;
const BANK_ONE_SOURCE_BYTES: [u8; 14] = [
    0xf3, 0x0f, 0x10, 0x05, 0x62, 0x7f, 0xb3, 0x00, // movss xmm0,[rip+0xb37f62]
    0x44, 0x89, 0x6c, 0x24, 0x48, // mov [rsp+0x48],r13d
    0xe8, // call ...
];

/// Where the read-only data sits in this build, for finding a constant to
/// point at. The search is further held to the section the original constant
/// is in and to memory that is actually there; see `constant_near`.
const RDATA: std::ops::Range<usize> = 0x2e13000..0x3e78000;

static REBUILT: AtomicBool = AtomicBool::new(false);
/// The render ratio the instruction was rewritten for, as bits; 0 if it was
/// not.
static BUILT_FOR: AtomicU32 = AtomicU32::new(0);
/// The bias that rewrite makes, as bits.
static BUILT_BIAS: AtomicU32 = AtomicU32::new(0);

/// A ratio this near 1.0 wants a bias under a hundredth of a mip level,
/// which is none.
const NO_BIAS_FROM: f32 = 0.995;

/// What the instruction that builds sampler bank 1 was rewritten for, if it
/// was: `(render ratio, the bias that makes)`. `None` when no bias was asked
/// for, when the ratio at start-up wanted none, and when it could not be
/// built -- in all of which bank 1 holds the engine's stock -1.0 and must not
/// be selected for `mip_bias = 1`.
pub(crate) fn bias_built() -> Option<(f32, f32)> {
    let built = BUILT_FOR.load(Ordering::Relaxed);
    (built != 0).then(|| {
        (
            f32::from_bits(built),
            f32::from_bits(BUILT_BIAS.load(Ordering::Relaxed)),
        )
    })
}

/// What is to be done about bank 1's bias.
pub(crate) enum Bias {
    /// The settings do not ask for it. `mip_bias` is carried so that the
    /// case of asking for the bank without a ratio to build it for can be
    /// said.
    NotWanted {
        mip_bias: u32,
        ratio: f32,
        extra: f32,
    },
    /// The constant it starts from does not read 2.0: the data is not in
    /// place yet, or the instruction has already been rewritten.
    NotReady,
    /// Wanted and ready, and it cannot be done.
    Unavailable(String),
    Ready(BiasPlan),
}

pub(crate) struct BiasPlan {
    pub write: CodeWrite,
    literal: usize,
    value: f32,
    ratio: f32,
    extra: f32,
}

/// The `[start, end)` of the section of the loaded image that holds `rva`.
unsafe fn section_of(base: usize, rva: usize) -> Option<std::ops::Range<usize>> {
    if !readable(base, 0x400) {
        return None;
    }
    let nt = (base as *const u8).add(0x3c).cast::<u32>().read_unaligned() as usize;
    if nt == 0 || nt > 0x300 {
        return None;
    }
    let nt = (base + nt) as *const u8;
    let count = nt.add(6).cast::<u16>().read_unaligned() as usize;
    let optional = nt.add(20).cast::<u16>().read_unaligned() as usize;
    let table = nt.add(24 + optional);
    if count > 96 || !readable(table as usize, count * 40) {
        return None;
    }
    (0..count).find_map(|index| {
        let entry = table.add(index * 40);
        let size = entry.add(8).cast::<u32>().read_unaligned() as usize;
        let start = entry.add(12).cast::<u32>().read_unaligned() as usize;
        (rva >= start && rva < start + size).then_some(start..start + size)
    })
}

/// The float nearest `wanted` in the image's read-only data, within one per
/// cent of it.
///
/// Only memory that is there is read: the range is the section `like` is
/// in, cut to the range known for this build, and it is walked a region at
/// a time, reading only regions the system reports as committed and
/// readable. A gap anywhere in a range taken on trust would be an access
/// violation at start-up.
unsafe fn constant_near(base: usize, like: usize, wanted: f32) -> Option<(usize, f32)> {
    use windows::Win32::System::Memory::{
        VirtualQuery, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS,
    };
    let section = section_of(base, like - base).unwrap_or(RDATA);
    let start = base + section.start.max(RDATA.start);
    let end = base + section.end.min(RDATA.end);
    let mut best: Option<(usize, f32)> = None;
    let mut address = start;
    while address + 4 <= end {
        let mut info = MEMORY_BASIC_INFORMATION::default();
        let size = std::mem::size_of::<MEMORY_BASIC_INFORMATION>();
        if VirtualQuery(Some(address as *const _), &mut info, size) != size {
            break;
        }
        let region_end = (info.BaseAddress as usize + info.RegionSize).min(end);
        if region_end <= address {
            break;
        }
        let usable = info.State.0 == 0x1000
            && info.Protect.0 != 0
            && info.Protect.0 & (PAGE_NOACCESS.0 | PAGE_GUARD.0) == 0;
        if usable {
            // On the same four-byte grid whichever region it is in.
            let mut at = start + (address - start).div_ceil(4) * 4;
            while at + 4 <= region_end {
                let value = (at as *const f32).read_unaligned();
                if (value - wanted).abs() < wanted * 0.01
                    && best.is_none_or(|(_, found)| (value - wanted).abs() < (found - wanted).abs())
                {
                    best = Some((at, value));
                }
                at += 4;
            }
        }
        address = region_end;
    }
    best
}

/// The constant bank 1 has to be built from, as `-log2(constant)`, for the
/// bias a render ratio and `mip_bias_extra` ask for together: log2 of the
/// ratio, less the extra levels. `None` when that is next to no bias, or
/// the ratio is not one.
fn bank_one_constant(ratio: f32, extra: f32) -> Option<f32> {
    let bias = ratio.log2() - extra;
    ((0.3..=1.0).contains(&ratio) && bias <= NO_BIAS_FROM.log2()).then(|| (-bias).exp2())
}

/// The texture mip bias a render ratio and `mip_bias_extra` ask for, or
/// `None` for none. What the settings page holds against `bias_built` to
/// say whether the game has to be restarted.
pub(crate) fn bias_wanted(ratio: f32, extra: f32) -> Option<f32> {
    bank_one_constant(ratio, extra).map(|constant| -constant.log2())
}

/// Work out how to make bank 1 the bias this render ratio wants.
///
/// The banks are a fixed ladder -- `-log2(2.0)`, `-log2(2.5)`, and so on --
/// built from literals at startup. Rendering at two thirds wants
/// `-log2(1.5)`, which is `-0.585`, and the nearest rung is `-1.0`: nearly
/// twice too much, which sharpens the mip selection past what the sample
/// count supports and shows up as aliasing and crawling edges rather than
/// detail.
///
/// So rather than pick a rung, the constant one rung is built from is pointed
/// at a different one. Nothing is invented: the image is searched for a float
/// that already holds the reciprocal of the ratio, and only the displacement
/// of one instruction changes. The shared 2.0 itself is left alone -- other
/// code reads it.
///
/// Nothing is written here. The write is handed to `gate`, which makes it in
/// the same step as the detours, so it is in before the game's own code has
/// run -- which is before the banks are built. The ratio is fixed for the
/// session once they are.
///
/// # Safety
/// `base` must be the game's image, with `BANK_ONE_SOURCE` already checked.
pub(crate) unsafe fn prepare_exact_bias(
    base: usize,
    mip_bias: u32,
    ratio: f32,
    extra: f32,
) -> Bias {
    // `ratio` is `render_scale` as it reads when the detours go in, which is
    // 0.5 to 1.0 by the time it leaves `config`. The bias is log2 of it,
    // which is what rendering small costs the textures, less
    // `mip_bias_extra` levels: an upscaler that accumulates jittered frames
    // resolves more texture detail than one frame holds, and the usual
    // advice for one is a level sharper than the ratio alone asks for. The
    // engine's own ladder is that advice -- its rungs are log2 of a scale
    // less one -- for the scales it has rungs for. Next to no bias is none:
    // nothing is built for it.
    let Some(wanted) = (mip_bias == 1)
        .then(|| bank_one_constant(ratio, extra))
        .flatten()
    else {
        return Bias::NotWanted {
            mip_bias,
            ratio,
            extra,
        };
    };
    let instruction = base + BANK_ONE_SOURCE;
    let current = ((instruction + 4) as *const i32).read_unaligned();
    let source = (instruction as i64 + 8 + current as i64) as usize;
    // The data is in place, and the instruction is as the game shipped it,
    // exactly when the constant it loads reads 2.0.
    if !readable(source, 4) || (source as *const f32).read_unaligned() != 2.0 {
        return Bias::NotReady;
    }

    let Some((literal, value)) = constant_near(base, source, wanted) else {
        return Bias::Unavailable(format!(
            "no constant within one per cent of {wanted:.4} was found in the image to build \
             it from"
        ));
    };
    let Ok(displacement) = i32::try_from(literal as i64 - (instruction + 8) as i64) else {
        return Bias::Unavailable(format!(
            "the constant found at {literal:#x} is out of reach of the instruction"
        ));
    };

    let mut expect = [0u8; MAX_WRITE];
    expect[..4].copy_from_slice(&current.to_le_bytes());
    let mut bytes = [0u8; MAX_WRITE];
    bytes[..4].copy_from_slice(&displacement.to_le_bytes());
    Bias::Ready(BiasPlan {
        write: CodeWrite {
            what: "the constant sampler bank 1 is built from",
            address: instruction + 4,
            length: 4,
            expect,
            bytes,
        },
        literal,
        value,
        ratio,
        extra,
    })
}

/// Say what became of the bias, after `gate` has made its writes.
///
/// What was written, and when. Whether the engine then built bank 1 from it
/// is the engine's doing and is not observed here, so the line does not
/// claim it.
pub(crate) fn bias_written(bias: &Bias, game_was_running: bool) {
    match bias {
        Bias::Ready(plan) => {
            REBUILT.store(true, Ordering::Relaxed);
            BUILT_FOR.store(plan.ratio.to_bits(), Ordering::Relaxed);
            BUILT_BIAS.store((-plan.value.log2()).to_bits(), Ordering::Relaxed);
            crate::log!(
                "[tun] the instruction that builds sampler bank 1 now loads {:.4} from {:#x} \
                 instead of 2.0, which makes a mip bias of {:+.3} for the render_scale read at \
                 start-up, {:.3}, and mip_bias_extra, {:.2}. The bias is fixed for this \
                 session: restart the game for it to follow a different render_scale or \
                 mip_bias_extra. {}",
                plan.value,
                plan.literal,
                -plan.value.log2(),
                plan.ratio,
                plan.extra,
                if game_was_running {
                    "The game's own code was already running when this was written: if it \
                     had built its sampler banks by then, bank 1 has the stock -1.0 instead"
                } else {
                    "Written before any of the game's own code had been seen to run, so \
                     before it builds its sampler banks"
                }
            );
        }
        Bias::NotWanted {
            mip_bias: 1,
            ratio,
            extra,
        } => crate::log!(
            "[tun] mip_bias = 1, the render_scale read at start-up is {ratio:.3} and \
             mip_bias_extra is {extra:.2}: together that is no bias, so the instruction is \
             left alone, sampler bank 1 is not selected and the engine's bank 0 (no bias) \
             stays in use this session. Restart the game for the bias to follow a different \
             render_scale or mip_bias_extra"
        ),
        Bias::NotWanted { .. } => {}
        Bias::NotReady => crate::log!(
            "[tun] the constant the mip bias is built from did not read 2.0 when the detours \
             went in, so the instruction is left alone, sampler bank 1 is not selected and \
             the engine's bank 0 (no bias) stays in use this session"
        ),
        Bias::Unavailable(why) => crate::log!(
            "[tun] the exact mip bias was not built: {why}. Sampler bank 1 is not selected and \
             the engine's bank 0 (no bias) stays in use this session"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::bank_one_constant;

    fn bias(ratio: f32, extra: f32) -> Option<f32> {
        bank_one_constant(ratio, extra).map(|constant| -constant.log2())
    }

    fn near(found: Option<f32>, wanted: f32) -> bool {
        found.is_some_and(|found| (found - wanted).abs() < 0.002)
    }

    /// With no extra the bias is log2 of the ratio, as it was before
    /// `mip_bias_extra` existed, and none at a ratio of 1.0.
    #[test]
    fn without_extra_the_bias_is_the_ratios() {
        assert!(near(bias(0.667, 0.0), -0.584));
        assert!(near(bias(0.58, 0.0), -0.786));
        assert!(near(bias(0.5, 0.0), -1.0));
        assert_eq!(bias(1.0, 0.0), None);
        assert_eq!(bias(0.999, 0.0), None);
    }

    /// The extra is that many levels on top, at every ratio: with 1.0 the
    /// figures are the rungs of the engine's own ladder, and DLAA gets one.
    #[test]
    fn the_extra_is_levels_on_top() {
        assert!(near(bias(0.667, 1.0), -1.584));
        assert!(near(bias(0.5, 1.0), -2.0));
        assert!(near(bias(1.0, 1.0), -1.0));
        assert!(near(bias(1.0, 0.5), -0.5));
        assert!(near(bias(0.75, 2.0), -2.415));
        // The constants: 3.0 for Quality, the stock 2.0 for DLAA.
        assert!(bank_one_constant(0.667, 1.0).is_some_and(|c| (c - 2.998).abs() < 0.01));
        assert_eq!(bank_one_constant(1.0, 1.0), Some(2.0));
        // Not a ratio.
        assert_eq!(bias(0.0, 1.0), None);
        assert_eq!(bias(1.7, 1.0), None);
    }
}
