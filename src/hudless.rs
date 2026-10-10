//! Finding the finished frame, before the interface is drawn over it.
//!
//! DLSS frame generation's `HUDLess` input wants the image as it will be
//! presented, minus the interface: post-processed, tonemapped, in the back
//! buffer's own colour space. This engine makes that hard in a specific way.
//! There is no interface render target. `uGUI::draw` (`FUN_142353030`) binds
//! no target of its own and draws into whatever is already bound. And the only
//! full-screen composite, `System_CopyToBackBuffer` (`FUN_1422979f0`), runs
//! after the HUD, so the back buffer never holds an interface-free frame.
//!
//! The upscale slot gives an interface-free frame, but that one is
//! pre-tonemap, linear, in the engine's own R11G11B10_FLOAT, which is the
//! right content at the wrong end of the post chain.
//!
//! ## Observed at run time, not located in the executable
//!
//! The image wanted is simply *the contents of whatever render target is bound
//! when the first interface draw happens*. Both halves of that are already
//! observable: `OMSetRenderTargets` is hooked, and a draw hook says when
//! drawing starts. Nothing has to be found in the executable, and the GUI's
//! position in the engine's draw list -- which is runtime data and cannot be
//! read statically -- does not have to be known.
//!
//! So this works in two stages, and the first is pure measurement:
//!
//! * `hudless_survey = 1` records, for every frame, each render target bound
//!   after the upscale slot has run, with its size, its format and how many
//!   draws went into it, and writes the timeline to the log once a second.
//!   One run of the game produces the answer. What to look for is a binding
//!   whose target is the window's size, whose format is the swapchain's or
//!   close to it, and which receives a sudden run of many small draws -- that
//!   run is the HUD, and the target it goes into is the frame we want.
//! * `hudless_occurrence = N` then captures a window-sized target at its Nth
//!   binding of the frame, at the moment of the first draw into it -- the last
//!   instant before the interface exists. The copy is recorded into the
//!   engine's own list, so it needs no queue or fence of its own.
//!
//! ## What the survey shows, and why the key is the occurrence
//!
//! Measured in game, the steady state is three bindings:
//!
//! ```text
//!   slot 1  0xd3d8c820   3840x2160  R11G11B10_FLOAT  10-14 draws
//!   slot 2  0x11b9c8420  3840x2160  R8_UNORM          2 draws
//!   slot 3  0xd3d8c820   3840x2160  R11G11B10_FLOAT  11 -> 26 -> 31 draws
//! ```
//!
//! Slots 1 and 3 are the **same target**, and the third binding's draw count
//! climbs as the player moves and more interface appears. So the image wanted
//! is that target as slot 3 begins: its second binding of the frame.
//!
//! The index is not the key, though. A survey taken while the game loads
//! shows six bindings before it settles to three, so anything selected by raw
//! slot number picks the wrong target during loading, and a frame generation
//! input that is quietly wrong for the first seconds of a session looks like
//! a fault of the interpolator. Counting how many times *that same resource*
//! has already been bound is stable across the extra bindings, which is why
//! `hudless_occurrence` is the setting.
//! ## The format is not the back buffer's, and that is a real problem
//!
//! The target is `R11G11B10_FLOAT`; the swapchain is `B8G8R8A8_UNORM`. So the
//! The target is `R11G11B10_FLOAT`; the swapchain is `B8G8R8A8_UNORM`. So the
//! engine draws its interface into the linear high-range buffer and tonemaps
//! afterwards, in the composite that writes the back buffer -- which means
//! there is **no post-tonemap interface-free image anywhere in this frame**.
//!
//! What this gives is therefore still linear and high-range, but it is after
//! the post chain rather than before it, which is strictly closer to the back
//! buffer than the upscale slot is. Whether DLSS-G accepts a `HUDLess` input
//! whose format and colour space differ from the back buffer's is not
//! documented, which is why `fg_hudless` is a switch of its own and defaults
//! to off.
//!
//! Both are off by default, and the draw hooks they need are installed but
//! cost one relaxed atomic load each when nothing is switched on.

use parking_lot::Mutex;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

/// Bindings recorded per frame. The HUD is at the end of the frame and the
/// timeline starts at the upscale slot, so this is generous.
const TIMELINE: usize = 48;

/// Set when either the survey or the capture needs the draw hooks to do
/// anything at all. Read once per draw call, which is thousands of times a
/// frame, so it is one relaxed load and not a configuration lookup.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Which engine frame we are in, counted at the upscale slot.
///
/// The kept frame is stamped with this so a consumer can tell a capture made
/// this frame from one made last frame, which is the difference between a
/// correct HUD-less image and a plausible-looking wrong one.
static FRAME: AtomicU64 = AtomicU64::new(0);

/// Keep the draw hooks' fast path honest about what the configuration says.
///
/// Called from the configuration's own reload, so the cost of the switch is
/// paid twice a second rather than per draw.
pub fn follow(settings: &crate::config::Config) {
    // `fg_hudless` belongs in this list and was missing from it, which is a
    // defect of the silent kind: with `fg_hudless = 1` and
    // `hudless_occurrence` left at its default of 0, the draw hooks stayed
    // asleep, nothing was ever captured, `frame()` returned `None`, and the
    // tag was simply never added. No error, no log line, and a run that looks
    // exactly like a run where DLSS-G declined the image.
    let wanted = settings.hudless_survey
        || settings.hudless_slot > 0
        || settings.hudless_occurrence > 0
        || settings.fg_hudless
        // `path_dump` lists a frame's render targets and draws, and reaches
        // them through `note_target` and `note_draw` below so that neither
        // hook gains a test of its own. With none of the keys above set
        // nothing is captured: `chosen` refuses every binding.
        || settings.path_dump;
    ACTIVE.store(wanted, Ordering::Relaxed);
    // The draw hooks exist only to serve this, so they are installed while it
    // is armed and removed while it is not. After the flag when arming and
    // -- since it is the same order -- before the hooks go when disarming,
    // which is harmless either way: a hook that runs with the flag clear
    // returns at once.
    crate::gpu::set_draw_hooks(wanted);
}

/// Whether the capture or the survey is armed, for `gpu::install`, which
/// runs after the settings were first read.
pub fn armed() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// The resource the capture has settled on, once one has been accepted.
///
/// Size and occurrence find the target; **identity is what keeps it**. The
/// loading screen proved why: it binds a window-sized `R8_UNORM` mask, so two
/// different resources satisfied "window-sized, second binding" on alternating
/// frames and the capture flipped between a colour buffer and a one-channel
/// mask. A format check alone would have made that safe without making it
/// stable -- a cutscene introducing another `R11G11B10_FLOAT` target of window
/// size would shift the count again, and that failure would be silent rather
/// than fatal.
static LATCHED: AtomicUsize = AtomicUsize::new(0);

/// Frames since the latched resource was last seen, so a target the engine has
/// genuinely replaced does not latch the capture off for the session.
static MISSES: AtomicU64 = AtomicU64::new(0);

/// How long to wait before accepting that the latched target is gone. Two
/// seconds at 60, which is longer than any legitimate gap within a scene and
/// shorter than a user would notice after a resolution change.
const RELATCH_AFTER: u64 = 120;

/// Candidates refused for their format, which is the measurement of whether
/// the occurrence is picking a stable thing.
static REJECTED: AtomicU64 = AtomicU64::new(0);
static SAID_REJECT: AtomicBool = AtomicBool::new(false);

/// Candidates refused because they were not the latched resource, and frames
/// in which no candidate matched the occurrence at all.
///
/// Separate tallies because they separate the two remaining explanations for a
/// rendered frame without a capture: the engine alternating between interface
/// targets, or the occurrence simply not being reached because fewer interface
/// elements drew. Without these the choice between them is a theory.
static OTHER_RESOURCE: AtomicU64 = AtomicU64::new(0);

/// Whether this candidate is the kind of image a HUD-less tag may be.
///
/// Strict rather than permissive, and deliberately so. The size test was doing
/// two jobs it cannot do -- finding the right target *and* proving it is a
/// colour image -- and the one it cannot do is the one that crashed the game:
/// `DXGI_FORMAT(61)` is `R8_UNORM`, one channel of eight bits, handed to the
/// plugin as `kBufferTypeHUDLessColor`. This is the same principle as the
/// frame stamp one step earlier: **do not hand over something unverified,
/// because a wrong HUD-less image is worse than none.**
fn acceptable(binding: &Binding, settings: &crate::config::Config) -> bool {
    if settings.hudless_format == 0 || binding.format.0 as u32 == settings.hudless_format {
        return true;
    }
    REJECTED.fetch_add(1, Ordering::Relaxed);
    if !SAID_REJECT.swap(true, Ordering::Relaxed) {
        crate::log!(
            "[hud] refusing {:#x} as the interface-free frame: it is {}x{} {:?}, and the \
             interface buffer is format {}. Size alone cannot tell a colour buffer from a \
             mask, and the loading screen binds a window-sized one",
            binding.resource,
            binding.size.0,
            binding.size.1,
            binding.format,
            settings.hudless_format
        );
    }
    false
}

/// Which binding of the window-sized target to capture before.
///
/// `hudless_occurrence` when it is set, and otherwise 2 -- which is the
/// surveyed answer for this engine, not a guess. **It was surveyed before the
/// proxy swapchain existed and before the engine rendered off-screen, so it is
/// a starting point rather than a fact**; `hudless_survey = 1` prints the
/// timeline and re-measures it, and the capture line says which number was
/// actually used so a stale default cannot pass unnoticed.
fn occurrence(settings: &crate::config::Config) -> usize {
    if settings.hudless_occurrence > 0 {
        settings.hudless_occurrence
    } else {
        2
    }
}

#[derive(Clone, Copy)]
struct Binding {
    resource: usize,
    /// The serial `alive` knew the resource by when it was bound, or `None`
    /// for one it has no living witness for.
    serial: Option<u64>,
    size: (u32, u32),
    format: DXGI_FORMAT,
    draws: u32,
    /// How many times this same resource has been bound this frame, counting
    /// from one. This is what the selection keys on -- see the module comment.
    occurrence: u32,
    /// The list it was bound on, so a draw recorded on a different thread's
    /// list is not counted against it.
    list: usize,
}

struct Timeline {
    bindings: Vec<Binding>,
    /// Frames since the last log line.
    frame: u64,
    /// Whether the upscale slot has fired this frame. Nothing before it is of
    /// interest: the HUD is at the end.
    open: bool,
    /// The slot the capture took this frame, so it happens once.
    captured: bool,
}

static TIMELINE_STATE: Mutex<Option<Timeline>> = Mutex::new(None);

/// Frames between timeline dumps while surveying.
const SURVEY_PERIOD: u64 = 60;

/// The upscale slot has run: everything from here to Present is a candidate.
pub fn open_frame() {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let mut guard = TIMELINE_STATE.lock();
    let state = guard.get_or_insert_with(|| Timeline {
        bindings: Vec::with_capacity(TIMELINE),
        frame: 0,
        open: false,
        captured: false,
    });
    state.bindings.clear();
    state.open = true;
    state.captured = false;
    FRAME.fetch_add(1, Ordering::Relaxed);
}

/// A render target was bound.
///
/// Called from `OMSetRenderTargets` with the first target's resource, which is
/// the one the interface would inherit.
pub fn note_target(list: usize, resource: usize, size: (u32, u32), format: DXGI_FORMAT) {
    if !ACTIVE.load(Ordering::Relaxed) || resource == 0 {
        return;
    }
    crate::pathdump::target(list, resource, size, format);
    let mut guard = TIMELINE_STATE.lock();
    let Some(state) = guard.as_mut() else {
        return;
    };
    if !state.open || state.bindings.len() >= TIMELINE {
        return;
    }
    let occurrence = state
        .bindings
        .iter()
        .filter(|binding| binding.resource == resource)
        .count() as u32
        + 1;
    state.bindings.push(Binding {
        resource,
        serial: crate::alive::serial_of(resource),
        size,
        format,
        draws: 0,
        occurrence,
        list,
    });
}

/// Whether a binding is the one the configuration asks for.
///
/// Two keys, and the sturdy one wins. `hudless_occurrence` counts how many
/// times the target has been bound this frame and requires it to be the size
/// of the window; `hudless_slot` is the raw position in the timeline and is
/// kept only because pinning one directly is occasionally what is wanted.
fn chosen(binding: &Binding, index: usize, settings: &crate::config::Config) -> bool {
    // The raw slot override first, because it is the escape hatch and should
    // not be overridden by the occurrence default.
    if settings.hudless_slot > 0 {
        return index + 1 == settings.hudless_slot;
    }
    if settings.hudless_occurrence == 0 && !settings.fg_hudless {
        return false;
    }
    let window = crate::swap::chain().map(|chain| chain.size);
    if window != Some(binding.size) || binding.occurrence as usize != occurrence(settings) {
        return false;
    }
    if !acceptable(binding, settings) {
        return false;
    }
    // Identity, once there is one. A resource that is not the one already
    // accepted is refused even when it satisfies everything else, because
    // "everything else" is what the loading screen satisfied twice over.
    let latched = LATCHED.load(Ordering::Relaxed);
    if latched == 0 {
        LATCHED.store(binding.resource, Ordering::Relaxed);
        crate::log!(
            "[hud] the interface-free frame is {:#x}, {}x{} {:?}, and the capture is \
             latched to that resource rather than to its position",
            binding.resource,
            binding.size.0,
            binding.size.1,
            binding.format
        );
        return true;
    }
    if latched == binding.resource {
        MISSES.store(0, Ordering::Relaxed);
        return true;
    }
    // A different resource. Refused -- but counted, because the engine does
    // legitimately replace this target on a resolution change, and latching
    // the capture off for the rest of the session would be its own silent
    // failure.
    OTHER_RESOURCE.fetch_add(1, Ordering::Relaxed);
    if MISSES.fetch_add(1, Ordering::Relaxed) >= RELATCH_AFTER {
        crate::log!(
            "[hud] {:#x} has not been bound for {RELATCH_AFTER} candidate frames, so the \
             capture re-latches to {:#x}, {}x{} {:?}",
            latched,
            binding.resource,
            binding.size.0,
            binding.size.1,
            binding.format
        );
        LATCHED.store(binding.resource, Ordering::Relaxed);
        MISSES.store(0, Ordering::Relaxed);
        return true;
    }
    false
}

/// Which engine frame we are in: the number `open_frame` counts at the
/// upscale slot, so it advances once per *rendered* frame and not once per
/// Present. Consumers need it to tell those apart.
pub fn frame_index() -> u64 {
    FRAME.load(Ordering::Relaxed)
}

/// Candidates refused for their format, and candidates refused for being a
/// resource other than the latched one.
pub fn rejected() -> (u64, u64) {
    (
        REJECTED.load(Ordering::Relaxed),
        OTHER_RESOURCE.load(Ordering::Relaxed),
    )
}

/// A draw was recorded. Returns the slot to capture, if this is the moment.
///
/// The decision is made here rather than in the hook because it needs the
/// timeline: the capture happens at the *first* draw into the chosen binding,
/// which is the last instant at which that target still holds the frame
/// without any interface in it.
fn note_draw_inner(list: usize) -> Option<Kept> {
    let settings = crate::config::get();
    let mut guard = TIMELINE_STATE.lock();
    let state = guard.as_mut()?;
    if !state.open {
        return None;
    }
    // The most recent binding on this list, which is what this draw goes into.
    let index = state
        .bindings
        .iter()
        .rposition(|binding| binding.list == list)?;
    let binding = state.bindings[index];
    state.bindings[index].draws += 1;
    // The *first* draw only: that is the last instant at which the target
    // still holds the frame without any interface in it.
    if state.captured || binding.draws != 0 || !chosen(&binding, index, &settings) {
        return None;
    }
    state.captured = true;
    Some(Kept {
        resource: binding.resource,
        serial: binding.serial,
        size: binding.size,
        format: binding.format,
        occurrence: binding.occurrence,
        slot: index + 1,
    })
}

/// What a capture was actually taken from: the binding's own numbers, read
/// off the timeline, and not the setting that selected it.
struct Kept {
    resource: usize,
    serial: Option<u64>,
    size: (u32, u32),
    format: DXGI_FORMAT,
    occurrence: u32,
    slot: usize,
}

/// The binding the last capture was taken before, as `occurrence`, or 0 for
/// no capture yet.
///
/// `hudless_occurrence` is read live, and an A/B between two values of it was
/// judged by eye with nothing in the log to say the second value had ever
/// been used. So the line below is printed whenever the binding a capture was
/// *actually taken before* differs from the previous capture's -- from the
/// timeline, at the copy, so it cannot report a setting that selected
/// nothing.
static LAST_OCCURRENCE: AtomicUsize = AtomicUsize::new(0);

/// A draw was recorded onto `list`; capture the frame if this is the boundary.
///
/// # Safety
/// `list` must be the open `ID3D12GraphicsCommandList` the draw is going onto.
pub unsafe fn note_draw(list: *mut c_void) {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    crate::pathdump::draw(list as usize);
    let Some(kept) = note_draw_inner(list as usize) else {
        return;
    };
    let Kept {
        resource,
        serial,
        size,
        format,
        occurrence,
        slot,
    } = kept;
    if !keep(list, resource, serial, size, format) {
        return;
    }
    let previous = LAST_OCCURRENCE.swap(occurrence as usize, Ordering::Relaxed);
    if previous != occurrence as usize {
        let settings = crate::config::get();
        crate::log!(
            "[hud] copied {resource:#x}, {}x{} {format:?}, just before the first draw of its \
             binding {occurrence} of the frame (timeline slot {slot}){}; the ini reads \
             hudless_occurrence = {}, hudless_slot = {}{}",
            size.0,
            size.1,
            if previous == 0 {
                String::from(" -- the first capture of the session")
            } else {
                format!(" -- the previous capture was before binding {previous}")
            },
            settings.hudless_occurrence,
            settings.hudless_slot,
            if settings.hudless_occurrence == 0 && settings.hudless_slot == 0 {
                ", so 2 is the built-in default and not a measurement of this run"
            } else {
                ""
            }
        );
    }
    if !SAID_CAPTURE.swap(true, Ordering::Relaxed) {
        let swapchain = crate::swap::chain().map(|chain| chain.format);
        if swapchain.is_some_and(|wanted| wanted != format) {
            crate::log!(
                "[hud] note: this is {format:?} and the swapchain is {:?}, so the interface-free \
                 frame is linear and high-range rather than display-referred -- the engine \
                 tonemaps after the interface, in the composite that writes the back buffer",
                swapchain.unwrap()
            );
        }
    }
}

static SAID_CAPTURE: AtomicBool = AtomicBool::new(false);

/// Present has happened: close the frame and say what was seen.
pub fn close_frame() {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let mut guard = TIMELINE_STATE.lock();
    let Some(state) = guard.as_mut() else {
        return;
    };
    state.open = false;
    state.frame += 1;
    if !crate::config::get().hudless_survey || state.frame % SURVEY_PERIOD != 0 {
        return;
    }
    let window = crate::swap::chain().map(|chain| chain.size);
    crate::log!(
        "[hud] render targets bound between the upscale slot and Present \
         (the window is {}):",
        window.map_or_else(|| "unknown".into(), |size| format!("{}x{}", size.0, size.1))
    );
    let settings = crate::config::get();
    for (index, binding) in state.bindings.iter().enumerate() {
        let matches = window == Some(binding.size);
        crate::log!(
            "[hud]   slot {:>2}  {:#x}  {:>5}x{:<5} {:?}  binding {} of this target  \
             {} draw(s){}{}",
            index + 1,
            binding.resource,
            binding.size.0,
            binding.size.1,
            binding.format,
            binding.occurrence,
            binding.draws,
            if matches { "   <- window sized" } else { "" },
            if chosen(binding, index, &settings) {
                "   <== KEPT"
            } else {
                ""
            }
        );
    }
}

// ---------------------------------------------------------------------------
// The kept frame
// ---------------------------------------------------------------------------

struct Held {
    texture: ID3D12Resource,
    size: (u32, u32),
    format: DXGI_FORMAT,
    /// The engine frame this was captured in, or 0 for never.
    ///
    /// A frame stamp and not a flag set once: a stale HUD-less image is not
    /// a missing one. DLSS-G derives which pixels are interface by comparing
    /// it against the back buffer, so an image from the wrong frame marks
    /// everything that moved in between as interface, and the result looks
    /// like an interpolation fault rather than like a bad input.
    frame: u64,
}

unsafe impl Send for Held {}
unsafe impl Sync for Held {}

static HELD: Mutex<Option<Held>> = Mutex::new(None);
static KEPT: AtomicU64 = AtomicU64::new(0);
static FAILED: AtomicBool = AtomicBool::new(false);
static SAID_STATE: AtomicBool = AtomicBool::new(false);
static SAID_DEAD: AtomicBool = AtomicBool::new(false);
static SAID_CHANGED: AtomicBool = AtomicBool::new(false);

/// How many interface-free frames have been kept.
pub fn kept() -> u64 {
    KEPT.load(Ordering::Relaxed)
}

/// The kept frame, for the interpolation to read.
///
/// Returns nothing until one has been written and its shape matches what is
/// being asked for -- handing frame generation a texture of the wrong size is
/// the kind of mistake that shows as a status code at best.
pub fn frame(expected: (u32, u32)) -> Option<ID3D12Resource> {
    let guard = HELD.lock();
    let held = guard.as_ref()?;
    // This frame's capture only. Handing over last frame's is worse than
    // handing over none: without the tag DLSS-G falls back to interpolating
    // the finished image, which is the documented behaviour; with a stale tag
    // it masks confidently against the wrong reference.
    let current = FRAME.load(Ordering::Relaxed);
    (held.frame != 0 && held.frame == current && held.size == expected)
        .then(|| held.texture.clone())
}

/// The format of the kept frame, so the tag can state it rather than passing
/// `UNKNOWN` and leaving Streamline to guess.
pub fn format() -> Option<DXGI_FORMAT> {
    HELD.lock().as_ref().map(|held| held.format)
}

/// Copy the bound target aside, on the engine's own list.
unsafe fn keep(
    list: *mut c_void,
    resource: usize,
    serial: Option<u64>,
    size: (u32, u32),
    format: DXGI_FORMAT,
) -> bool {
    if FAILED.load(Ordering::Relaxed) {
        return false;
    }
    // The resource comes from a table of render target views that outlives
    // the resources in it, and its size and format from a record made when
    // it was created. Neither says it still exists. A copy from a destroyed
    // resource, or one of another size that has taken its address, is a
    // command the engine's list cannot be closed with, and the engine aborts
    // on that. So: the resource must be the living one it was when it was
    // bound, and what it says about itself now must be what is about to be
    // copied.
    if !serial.is_some_and(|serial| crate::alive::is(resource, serial)) {
        if !SAID_DEAD.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[hud] the target {resource:#x} chosen for the interface-free copy is not a \
                 resource known to be alive; not copying it"
            );
        }
        return false;
    }
    {
        let raw = resource as *mut c_void;
        let Some(origin) = ID3D12Resource::from_raw_borrowed(&raw) else {
            return false;
        };
        let now = origin.GetDesc();
        if (now.Width as u32, now.Height) != size || now.Format != format {
            if !SAID_CHANGED.swap(true, Ordering::Relaxed) {
                crate::log!(
                    "[hud] the target {resource:#x} is {}x{} {:?} and was recorded as {}x{} \
                     {format:?}; not copying it",
                    now.Width,
                    now.Height,
                    now.Format,
                    size.0,
                    size.1
                );
            }
            return false;
        }
    }
    let mut guard = HELD.lock();
    let stale = match guard.as_ref() {
        Some(held) => held.size != size || held.format != format,
        None => true,
    };
    if stale {
        let Some(device) = crate::bindings::device() else {
            return false;
        };
        match build(&device, size, format) {
            Ok(held) => {
                crate::log!("[hud] keeping a {}x{} {format:?} frame", size.0, size.1);
                *guard = Some(held);
            }
            Err(error) => {
                FAILED.store(true, Ordering::Relaxed);
                crate::log!("[hud] could not make the kept frame: {error}");
                return false;
            }
        }
    }
    let Some(held) = guard.as_mut() else {
        return false;
    };

    // The engine leaves a render target it is about to draw into in
    // RENDER_TARGET, but that is an expectation and not a fact, so it is taken
    // from the tracking like every other transition in this library.
    let Some(moves) = crate::states::plan(
        list as usize,
        &[(resource, D3D12_RESOURCE_STATE_COPY_SOURCE)],
    ) else {
        if !SAID_STATE.swap(true, Ordering::Relaxed) {
            crate::log!("[hud] the state of {resource:#x} was never observed; not copying it");
        }
        return false;
    };
    let Some(recorder) = ID3D12GraphicsCommandList::from_raw_borrowed(&list) else {
        return false;
    };
    let raw = resource as *mut c_void;
    let Some(origin) = ID3D12Resource::from_raw_borrowed(&raw) else {
        return false;
    };

    let forward: Vec<D3D12_RESOURCE_BARRIER> = moves
        .iter()
        .map(|step| crate::states::barrier(step, false))
        .collect();
    if !forward.is_empty() {
        recorder.ResourceBarrier(&forward);
    }
    recorder.CopyResource(&held.texture, origin);
    let back: Vec<D3D12_RESOURCE_BARRIER> = moves
        .iter()
        .map(|step| crate::states::barrier(step, true))
        .collect();
    if !back.is_empty() {
        recorder.ResourceBarrier(&back);
    }
    held.frame = FRAME.load(Ordering::Relaxed);
    KEPT.fetch_add(1, Ordering::Relaxed);
    true
}

fn build(
    device: &ID3D12Device,
    size: (u32, u32),
    format: DXGI_FORMAT,
) -> windows::core::Result<Held> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let description = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Alignment: 0,
        Width: size.0 as u64,
        Height: size.1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: D3D12_RESOURCE_FLAG_NONE,
    };
    let mut texture: Option<ID3D12Resource> = None;
    unsafe {
        device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &description,
            // Left here for its life. It is read by frame generation as a
            // shader resource, and the transition for that is done on our own
            // queue where the state is known exactly.
            D3D12_RESOURCE_STATE_COPY_DEST,
            None,
            &mut texture,
        )?;
    }
    Ok(Held {
        texture: texture.ok_or_else(windows::core::Error::from_win32)?,
        size,
        format,
        frame: 0,
    })
}

/// The state the kept frame is left in between captures, so the present path
/// can transition it for reading and put it back.
pub const RESTING_STATE: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_COPY_DEST;
