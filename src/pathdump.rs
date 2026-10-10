//! `path_dump`: one key that says what the engine and this library are doing
//! about render resolution, on every change, so one session answers it.
//!
//! Off by default and read live. Off, it costs one relaxed load at each of
//! four places that run once a frame (the temporal dispatch, the upscale
//! slot, frame generation's hand-over, Present); nothing is added to any
//! per-call hook. On, it writes four kinds of line, each only when what it
//! reports has changed:
//!
//! * `[path] engine:` the engine's own state, read once per Present out of
//!   its configuration object, its target set and its render context. Which
//!   field is which was read from the game executable's code:
//!
//!   ```text
//!   cfg+0x1f0  the engine's own resolution scale, requested (the Resolution
//!              Scaling option, or its dynamic resolution): a sub-rectangle
//!              of the targets
//!   cfg+0x1f4  the same, as applied this frame;  +0x1f8 last frame's
//!   cfg+0x200  the DLSS ratio, requested;  +0x204 the one targets were built
//!              for;  +0x208 the product of the two scales, clamped
//!   cfg+0xa5/+0xa6  mUseDLSS / mUseDLISP;  +0xb0/+0xb4/+0xb8 what the
//!              optimal-settings query answered (scale, flags, sharpness)
//!   cfg+0x7b3b0..  dynamic resolution: on, mode, level 0..5, thresholds
//!   set+0x6b0  the content scale of the target set (the own scale again);
//!              +0x6b4 its DLSS ratio;  +0x6a8 whether it has an UpscaleOut
//!   ```
//!
//! * `[path] dlss:` what `gpu::run_dlss` read and wrote: each resource with
//!   its size and format, the render and output sizes it told NGX, the
//!   jitter, the motion-vector scale and where the resolve went.
//! * `[path] slot:` what the engine handed its upscale stage.
//! * `[path] fg:` what frame generation was given.
//!
//! And, whenever any of the first two changes (or on F10), `[path] frame:`
//! -- every render target bound and every compute dispatch recorded from the
//! temporal dispatch to Present, in recording order, with the upscale slot
//! marked where it fell. That is the list of passes the two resolve routes
//! differ by.
//!
//! Everything printed is read at the moment it is printed. Nothing here
//! writes to the engine.

use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;

use crate::native::readable;

static ARMED: AtomicBool = AtomicBool::new(false);

/// Follow the setting. Called from the configuration's reload.
pub fn follow(settings: &crate::config::Config) {
    if ARMED.swap(settings.path_dump, Ordering::Relaxed) != settings.path_dump {
        crate::log!(
            "[path] path_dump is {}",
            if settings.path_dump {
                "on: the engine's resolution state, what DLSS and frame generation are given, \
                 and one frame's passes are written whenever they change. F10 writes a frame's \
                 passes on demand"
            } else {
                "off"
            }
        );
        if settings.path_dump {
            WANT_FRAME.store(true, Ordering::Relaxed);
        }
    }
}

/// Whether the key is on. One relaxed load.
pub fn armed() -> bool {
    ARMED.load(Ordering::Relaxed)
}

/// Say `text` under `tag` if it differs from what was last said there.
fn say_on_change(last: &Mutex<String>, tag: &str, text: String) -> bool {
    let mut previous = last.lock();
    if *previous == text {
        return false;
    }
    crate::log!("[path] {tag}: {text}");
    *previous = text;
    true
}

fn shape(resource: usize) -> String {
    match crate::gpu::described(resource) {
        Some(info) => format!(
            "{resource:#x} {}x{} {}",
            info.Width,
            info.Height,
            crate::gpu::format_name(info.Format)
        ),
        None if resource == 0 => "none".into(),
        None => format!("{resource:#x} (undescribed)"),
    }
}

/// The same without the address, for the change test: the engine rotates
/// between several textures of one shape and that is not a change.
fn kind(resource: usize) -> String {
    match crate::gpu::described(resource) {
        Some(info) => {
            format!(
                "{}x{} {}",
                info.Width,
                info.Height,
                crate::gpu::format_name(info.Format)
            )
        }
        None => "undescribed".into(),
    }
}

// ---------------------------------------------------------------------------
// The engine's own state
// ---------------------------------------------------------------------------

/// The render settings object, whose two bytes at +0xe9e4 the handler of the
/// menu's NVIDIA DLSS option (`FUN_14043dcb0`) sets.
///
/// Reached through `tune`, from the load `gate` checks with the rest of the
/// build's sites; in the known build that instruction resolves to the slot
/// at +0x51c4368.
unsafe fn settings_object() -> Option<usize> {
    crate::tune::settings_object()
}

/// Read a `T` at `address`, or `None` if that memory is not mapped.
///
/// Checked every time. This runs once per Present with the key on, and a
/// pointer chased through three engine objects is exactly where a check
/// remembered from last frame is worth nothing.
unsafe fn peek<T: Copy>(address: usize) -> Option<T> {
    if address < 0x10000 || !readable(address, std::mem::size_of::<T>()) {
        return None;
    }
    Some((address as *const T).read_unaligned())
}

fn or_unread<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "?".into(), |value| value.to_string())
}

fn float(value: Option<f32>) -> String {
    value.map_or_else(|| "?".into(), |value| format!("{value:.4}"))
}

/// One of the engine's texture objects, as the engine describes it and as
/// D3D does: the packed size at +0x88, and the resource behind it.
unsafe fn texture(object: Option<usize>) -> String {
    let Some(object) = object.filter(|object| *object != 0) else {
        return "none".into();
    };
    let packed: Option<u32> = peek(object + 0x88);
    let resource = crate::hijack::resource_of(object);
    format!(
        "{} (the engine calls it {})",
        shape(resource),
        packed.map_or_else(
            || "?".into(),
            |packed| format!("{}x{}", packed & 0xffff, (packed >> 16) & 0x3fff)
        )
    )
}

static ENGINE_SAID: Mutex<String> = Mutex::new(String::new());
static ENGINE_KEY: Mutex<String> = Mutex::new(String::new());
static PRESENTS: AtomicU64 = AtomicU64::new(0);
static LAST_ENGINE_LINE: AtomicU64 = AtomicU64::new(0);
static ENGINE_CHANGES_HELD: AtomicU64 = AtomicU64::new(0);
/// Presents between engine lines at the least, so the engine's dynamic
/// resolution stepping every few frames cannot flood the log.
const ENGINE_LINE_SPACING: u64 = 20;

unsafe fn engine_state() -> Option<(String, String)> {
    let cfg = crate::hijack::config_object()?;
    let f = |offset: usize| peek::<f32>(cfg + offset);
    let u = |offset: usize| peek::<u32>(cfg + offset);
    let b = |offset: usize| peek::<u8>(cfg + offset);

    let device: Option<usize> = peek(cfg + 0x78);
    let ngx_object: Option<usize> = device.and_then(|device| peek(device + 0x42700));
    let ngx_ready: Option<u8> = device.and_then(|device| peek(device + 0x42718));
    let settings = settings_object();
    let menu_dlss: Option<(u8, u8)> = settings
        .and_then(|object| Some((peek::<u8>(object + 0xe9e4)?, peek::<u8>(object + 0xe9e5)?)));

    let set: Option<usize> = peek::<usize>(cfg + 0x1f450).filter(|set| *set != 0);
    let sf = |offset: usize| set.and_then(|set| peek::<f32>(set + offset));
    let su = |offset: usize| set.and_then(|set| peek::<u32>(set + offset));
    let sp = |offset: usize| set.and_then(|set| peek::<usize>(set + offset));
    // The chain's current colour and the screen target are one object further
    // in: the tail of `FUN_1423c3790` reads `*(*(set+0x420)+0x58)` and
    // `*(*(set+0x460)+0x58)`.
    let via = |offset: usize| {
        sp(offset)
            .filter(|p| *p != 0)
            .and_then(|p| peek::<usize>(p + 0x58))
    };

    let context = crate::native::context_address();
    let cf = |offset: usize| context.and_then(|context| peek::<f32>(context + offset));
    let ci = |offset: usize| context.and_then(|context| peek::<i32>(context + offset));

    let levels: Vec<String> = (0..6).map(|index| float(f(0x7b3cc + index * 4))).collect();
    let position = crate::hijack::menu_position();

    // What decides whether this is a change: the scales, the sizes and the
    // switches. Not the jitter, which is new every frame.
    let key = format!(
        "{:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?}",
        f(0x1f0).map(f32::to_bits),
        f(0x1f4).map(f32::to_bits),
        f(0x200).map(f32::to_bits),
        f(0x204).map(f32::to_bits),
        (b(0xa5), b(0xa6), b(0xbc)),
        u(0xb4),
        f(0xb0).map(f32::to_bits),
        (u(0x198), u(0x19c)),
        (b(0x7b3b0), u(0x7b3b4), u(0x7b3bc)),
        menu_dlss,
        ngx_ready,
        set.and_then(|set| peek::<u8>(set + 0x6a8)),
        sf(0x6b0).map(f32::to_bits),
        sf(0x6b4).map(f32::to_bits),
        (ci(0xd50), ci(0xd54), ci(0xd58), ci(0xd5c)),
        position,
        sp(0x6a0),
        su(0x58),
    );

    let text = format!(
        "own scale cfg+0x1f0 {} (applied +0x1f4 {}, previous +0x1f8 {}, changed +0x1fc {}); \
         DLSS ratio cfg+0x200 {} (built +0x204 {}); product +0x208 {} | display cfg+0x198 {}x{}, \
         window cfg+0x1f448 {}x{} | mUseDLSS +0xa5 {}, mUseDLISP +0xa6 {}, DLSS mode +0xa8 {}, \
         optimal answer +0xb0 scale {} flags {} sharpness {}, +0xac {}, +0xc4 {}, CAS +0xbc {}, \
         flags +0x1dc {} | NGX object {} ready +0x42718 {} | menu NVIDIA DLSS bytes \
         settings+0xe9e4/5 {} | dynamic resolution: on +0x7b3b0 {}, active +0x7b3b1 {}, mode \
         +0x7b3b4 {}, level +0x7b3bc {}, thresholds {} / {} / {}, levels [{}] | target set {}: \
         flags +0x58 {}, content scale +0x6b0 {}, DLSS ratio +0x6b4 {}, product +0x6b8 {}, \
         built for {}x{} at {} x {}, has UpscaleOut +0x6a8 {}; chain colour {}; screen target \
         {}; UpscaleOut {} | render context: rectangle +0xd50 ({}, {}, {}, {}), jitter +0x38 \
         ({}, {}) engine units | menu Resolution Scaling position last given to the handler: {} \
         | this library: render_scale {} in the ini, ratio wanted {:.4}",
        float(f(0x1f0)),
        float(f(0x1f4)),
        float(f(0x1f8)),
        or_unread(b(0x1fc)),
        float(f(0x200)),
        float(f(0x204)),
        float(f(0x208)),
        or_unread(u(0x198)),
        or_unread(u(0x19c)),
        or_unread(u(0x1f448)),
        or_unread(u(0x1f44c)),
        or_unread(b(0xa5)),
        or_unread(b(0xa6)),
        or_unread(u(0xa8)),
        float(f(0xb0)),
        or_unread(u(0xb4)),
        float(f(0xb8)),
        float(f(0xac)),
        float(f(0xc4)),
        or_unread(b(0xbc)),
        u(0x1dc).map_or_else(|| "?".into(), |flags| format!("{flags:#x}")),
        ngx_object.map_or_else(|| "?".into(), |object| format!("{object:#x}")),
        or_unread(ngx_ready),
        menu_dlss.map_or_else(|| "?".into(), |(a, b)| format!("{a},{b}")),
        or_unread(b(0x7b3b0)),
        or_unread(b(0x7b3b1)),
        or_unread(u(0x7b3b4)),
        or_unread(u(0x7b3bc)),
        float(f(0x7b3c0)),
        float(f(0x7b3c4)),
        float(f(0x7b3c8)),
        levels.join(", "),
        set.map_or_else(|| "none".into(), |set| format!("{set:#x}")),
        su(0x58).map_or_else(|| "?".into(), |flags| format!("{flags:#x}")),
        float(sf(0x6b0)),
        float(sf(0x6b4)),
        float(sf(0x6b8)),
        or_unread(su(0x6c0)),
        or_unread(su(0x6c4)),
        float(sf(0x6c8)),
        float(sf(0x6cc)),
        or_unread(set.and_then(|set| peek::<u8>(set + 0x6a8))),
        texture(via(0x420)),
        texture(via(0x460)),
        texture(sp(0x6a0)),
        or_unread(ci(0xd50)),
        or_unread(ci(0xd54)),
        or_unread(ci(0xd58)),
        or_unread(ci(0xd5c)),
        float(cf(0x38)),
        float(cf(0x3c)),
        match position {
            Some(position) => position.to_string(),
            None => "never called".into(),
        },
        crate::config::get().render_scale,
        crate::hijack::ratio_wanted(),
    );
    Some((key, text))
}

/// Present has returned: say the engine's state if it changed, and close the
/// frame being recorded.
pub fn at_present() {
    if !armed() {
        return;
    }
    let presents = PRESENTS.fetch_add(1, Ordering::Relaxed) + 1;
    close_frame();
    let Some((key, text)) = (unsafe { engine_state() }) else {
        say_on_change(
            &ENGINE_SAID,
            "engine",
            "the configuration object cannot be read yet".into(),
        );
        return;
    };
    let mut previous = ENGINE_KEY.lock();
    if *previous == key {
        return;
    }
    // A change. Held back if the last line is too recent, and then said with
    // the number of changes that went by unreported.
    if presents.saturating_sub(LAST_ENGINE_LINE.load(Ordering::Relaxed)) < ENGINE_LINE_SPACING
        && !previous.is_empty()
    {
        ENGINE_CHANGES_HELD.fetch_add(1, Ordering::Relaxed);
        return;
    }
    *previous = key;
    drop(previous);
    LAST_ENGINE_LINE.store(presents, Ordering::Relaxed);
    let held = ENGINE_CHANGES_HELD.swap(0, Ordering::Relaxed);
    crate::log!(
        "[path] engine (present {presents}{}): {text}",
        if held > 0 {
            format!(", after {held} Presents on which the state differed and was not written")
        } else {
            String::new()
        }
    );
    *ENGINE_SAID.lock() = text;
    WANT_FRAME.store(true, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// What the resolve read and wrote
// ---------------------------------------------------------------------------

/// What `gpu::run_dlss` is about to hand NGX.
pub struct Resolve {
    /// Whether this is the resolve at the engine's upscale stage. The fields
    /// that only the temporal pass knows are zero then.
    pub at_slot: bool,
    pub colour: usize,
    pub depth: usize,
    /// The engine's packed vectors, and what NGX is given (the decode's
    /// output, or the engine's own when the decode failed).
    pub motion_engine: usize,
    pub motion: usize,
    /// The temporal pass's own output, and the texture actually written.
    pub pass_output: usize,
    pub written: usize,
    pub groups: (u32, u32),
    pub render: (u32, u32),
    pub display: (u32, u32),
    pub jitter: (f32, f32),
    pub decode_scale: (f32, f32),
    pub motion_scale: (f32, f32),
    pub slotless: bool,
}

static RESOLVE_KEY: Mutex<String> = Mutex::new(String::new());

/// Say what the resolve reads and writes, when its shape changes.
pub fn resolve(r: &Resolve) {
    if !armed() {
        return;
    }
    let key = format!(
        "{} {} {} {} {} {} {} {:?} {:?} {:?} {:?} {}",
        r.at_slot,
        kind(r.colour),
        kind(r.depth),
        kind(r.motion_engine),
        kind(r.motion),
        kind(r.pass_output),
        kind(r.written),
        r.groups,
        r.render,
        r.display,
        r.decode_scale,
        r.slotless
    );
    let mut previous = RESOLVE_KEY.lock();
    if *previous == key {
        return;
    }
    *previous = key;
    drop(previous);
    if r.at_slot {
        crate::log!(
            "[path] dlss: resolved at the engine's upscale stage. Reads colour {} (the engine's \
             own, after its passes); depth {}; motion vectors {} (this frame's temporal pass \
             decoded them); writes {}. Told NGX: render {}x{}, output {}x{}, jitter ({:.4}, \
             {:.4}) render pixels, motion scale ({}, {})",
            shape(r.colour),
            shape(r.depth),
            shape(r.motion),
            shape(r.written),
            r.render.0,
            r.render.1,
            r.display.0,
            r.display.1,
            r.jitter.0,
            r.jitter.1,
            r.motion_scale.0,
            r.motion_scale.1
        );
    } else {
        crate::log!(
            "[path] dlss: resolved at the temporal pass into {}. Reads colour {}; depth {}; \
             motion vectors {} (decoded from the engine's {} with scale ({}, {})); writes {}; \
             the pass's own output is {}. Told NGX: render {}x{} (the dispatch is {}x{} groups, \
             {}x{} pixels), output {}x{}, jitter ({:.4}, {:.4}) render pixels, motion scale \
             ({}, {})",
            if r.slotless {
                "the pass's own output (no upscale stage in the frame)"
            } else {
                "the engine's full-size upscale target"
            },
            shape(r.colour),
            shape(r.depth),
            shape(r.motion),
            shape(r.motion_engine),
            r.decode_scale.0,
            r.decode_scale.1,
            shape(r.written),
            shape(r.pass_output),
            r.render.0,
            r.render.1,
            r.groups.0,
            r.groups.1,
            r.groups.0 * 16,
            r.groups.1 * 16,
            r.display.0,
            r.display.1,
            r.jitter.0,
            r.jitter.1,
            r.motion_scale.0,
            r.motion_scale.1
        );
    }
    WANT_FRAME.store(true, Ordering::Relaxed);
}

static SLOT_KEY: Mutex<String> = Mutex::new(String::new());

/// The engine's upscale stage has run with this descriptor: colour at +0x08,
/// motion at +0x10, output at +0x18, depth at +0x20, the rectangle as four
/// floats at +0x2c and a sharpness at +0x3c.
///
/// # Safety
/// `descriptor` must be the 0x40-byte descriptor the engine passed.
pub unsafe fn slot(descriptor: usize) {
    if !armed() {
        return;
    }
    mark("the engine's upscale stage (the slot this library stands in for)");
    let object = |offset: usize| peek::<usize>(descriptor + offset);
    let rect: Vec<String> = (0..4)
        .map(|index| float(peek::<f32>(descriptor + 0x2c + index * 4)))
        .collect();
    let resource =
        |offset: usize| object(offset).map_or(0, |object| crate::hijack::resource_of(object));
    let key = format!(
        "{} {} {} {} {}",
        kind(resource(0x08)),
        kind(resource(0x10)),
        kind(resource(0x18)),
        kind(resource(0x20)),
        rect.join(",")
    );
    let mut previous = SLOT_KEY.lock();
    if *previous == key {
        return;
    }
    *previous = key;
    drop(previous);
    crate::log!(
        "[path] slot: the engine handed its upscale stage colour {}; motion {}; output {}; \
         depth {}; rectangle ({}); sharpness {}",
        texture(object(0x08)),
        texture(object(0x10)),
        texture(object(0x18)),
        texture(object(0x20)),
        rect.join(", "),
        float(peek::<f32>(descriptor + 0x3c))
    );
    WANT_FRAME.store(true, Ordering::Relaxed);
}

static FG_KEY: Mutex<String> = Mutex::new(String::new());

/// What frame generation is being given for this frame.
#[allow(clippy::too_many_arguments)]
pub fn frame_generation(
    depth: usize,
    motion: usize,
    states: &[(u32, u32)],
    render: (u32, u32),
    display: (u32, u32),
    mvec_scale: Option<(f32, f32)>,
    jitter: (f32, f32),
    decoded: bool,
) {
    if !armed() {
        return;
    }
    let at = if crate::hijack::slot_is_idle() {
        "the temporal dispatch"
    } else {
        "the upscale slot"
    };
    let key = format!(
        "{} {} {:?} {:?} {:?} {decoded} {at}",
        kind(depth),
        kind(motion),
        render,
        display,
        mvec_scale.map(|scale| (scale.0.to_bits(), scale.1.to_bits())),
    );
    let mut previous = FG_KEY.lock();
    if *previous == key {
        return;
    }
    *previous = key;
    drop(previous);
    crate::log!(
        "[path] fg: filed at {at}. Depth tag {}, motion-vector tag {} ({}), each with its whole \
         texture as the extent; states (kind, state) {:?}; mvecScale {}; jitterOffset ({:.4}, \
         {:.4}) as last written to the engine, in render pixels; DLSS-G options: back buffer \
         {}x{}, depth and vectors {}x{}",
        shape(depth),
        shape(motion),
        if decoded {
            "this library's decode, in pixels"
        } else {
            "the engine's own, undecoded"
        },
        states,
        mvec_scale.map_or_else(
            || "not sent: no camera".into(),
            |scale| format!("({}, {})", scale.0, scale.1)
        ),
        jitter.0,
        jitter.1,
        display.0,
        display.1,
        render.0,
        render.1
    );
}

// ---------------------------------------------------------------------------
// One frame's passes
// ---------------------------------------------------------------------------

/// Set when a frame's passes should be written: at the next temporal
/// dispatch the recording starts, and Present writes it out.
static WANT_FRAME: AtomicBool = AtomicBool::new(false);
static RECORDING: AtomicBool = AtomicBool::new(false);

const ENTRIES: usize = 400;

enum Entry {
    Target {
        list: usize,
        resource: usize,
        size: (u32, u32),
        format: DXGI_FORMAT,
        draws: u32,
    },
    Dispatch {
        list: usize,
        groups: (u32, u32, u32),
        what: String,
    },
    Mark(String),
}

static FRAME: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

/// Ask for the next frame's passes. F10 does this.
pub fn request_frame() {
    if armed() {
        WANT_FRAME.store(true, Ordering::Relaxed);
    }
}

/// The temporal dispatch is being handled. Returns whether a frame's passes
/// are now being recorded, so the caller can let compute dispatches through.
pub fn begin_frame(note: &str) -> bool {
    if !armed() || !WANT_FRAME.swap(false, Ordering::Relaxed) {
        return false;
    }
    let mut frame = FRAME.lock();
    frame.clear();
    frame.push(Entry::Mark(format!(
        "the engine's temporal dispatch: {note}"
    )));
    RECORDING.store(true, Ordering::Relaxed);
    true
}

/// A point in the frame worth naming.
pub fn mark(what: &str) {
    if !RECORDING.load(Ordering::Relaxed) {
        return;
    }
    let mut frame = FRAME.lock();
    if frame.len() < ENTRIES {
        frame.push(Entry::Mark(what.into()));
    }
}

/// A render target was bound. Reached through `hudless::note_target`, behind
/// its own switch, so the bind hook itself gains nothing.
pub fn target(list: usize, resource: usize, size: (u32, u32), format: DXGI_FORMAT) {
    if !RECORDING.load(Ordering::Relaxed) {
        return;
    }
    let mut frame = FRAME.lock();
    if frame.len() < ENTRIES {
        frame.push(Entry::Target {
            list,
            resource,
            size,
            format,
            draws: 0,
        });
    }
}

/// A draw was recorded on `list`. Reached through `hudless::note_draw`.
pub fn draw(list: usize) {
    if !RECORDING.load(Ordering::Relaxed) {
        return;
    }
    let mut frame = FRAME.lock();
    let found = frame.iter_mut().rev().find_map(|entry| match entry {
        Entry::Target {
            list: bound, draws, ..
        } if *bound == list => Some(draws),
        _ => None,
    });
    if let Some(draws) = found {
        *draws += 1;
    }
}

/// A compute dispatch was recorded.
pub fn dispatch(list: usize, groups: (u32, u32, u32), what: impl FnOnce() -> String) {
    if !RECORDING.load(Ordering::Relaxed) {
        return;
    }
    let what = what();
    let mut frame = FRAME.lock();
    if frame.len() < ENTRIES {
        frame.push(Entry::Dispatch { list, groups, what });
    }
}

/// Whether a frame is being recorded, for the dispatch hook's burst branch.
pub fn recording() -> bool {
    RECORDING.load(Ordering::Relaxed)
}

fn close_frame() {
    if !RECORDING.swap(false, Ordering::Relaxed) {
        return;
    }
    let frame = std::mem::take(&mut *FRAME.lock());
    crate::log!(
        "[path] frame: {} entries from the temporal dispatch to Present, in the order they were \
         RECORDED. The engine records on several threads, so entries on different lists are \
         not in execution order relative to each other; entries on one list are",
        frame.len()
    );
    for (index, entry) in frame.iter().enumerate() {
        match entry {
            Entry::Mark(what) => crate::log!("[path]   {index:>3}  ---- {what} ----"),
            Entry::Target {
                list,
                resource,
                size,
                format,
                draws,
            } => crate::log!(
                "[path]   {index:>3}  list {list:#x}  target {resource:#x} {}x{} {}  {draws} \
                 draw(s)",
                size.0,
                size.1,
                crate::gpu::format_name(*format)
            ),
            Entry::Dispatch { list, groups, what } => crate::log!(
                "[path]   {index:>3}  list {list:#x}  compute {}x{}x{}  {what}",
                groups.0,
                groups.1,
                groups.2
            ),
        }
    }
    if frame.len() >= ENTRIES {
        crate::log!("[path]   (the frame had more than {ENTRIES} entries; the rest are not shown)");
    }
}
