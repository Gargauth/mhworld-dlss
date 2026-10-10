//! A settings page inside ReShade's overlay, when ReShade is there.
//!
//! The ini file is the configuration and stays the configuration: it works
//! with nothing else installed and it is polled live. This page is only an
//! editor for it. A change here rewrites one line of `mhw-dlss.ini`, and the
//! watcher that already exists applies it within half a second. The page
//! keeps no settings of its own, so there is one path a value can take.
//!
//! ## How it is attached
//!
//! ReShade's add-on interface is plain C exports, found on its module with
//! `GetProcAddress`: `ReShadeRegisterAddon`, `ReShadeRegisterOverlayForAddon`,
//! and `ReShadeGetImGuiFunctionTable`. Any module may register, not only an
//! `.addon64` file. The widgets come from a table of function pointers, so no
//! ImGui is linked here.
//!
//! That table is positional, which is the one sharp edge. The indices below
//! are for table version 19250, the one ReShade 6.8.0 serves to every build,
//! counted from `include/reshade_overlay.hpp` at tag `v6.8.0`: its struct is
//! 421 function pointers and nothing else, so a field's position is its slot.
//! Before any widget is used the table's own `GetVersion` is called and must
//! answer with ImGui's version text; a table that does not is not used, and a
//! ReShade that does not serve 19250 answers the request with null.
//!
//! ## Where the callback runs
//!
//! On whichever thread calls ReShade's `Present`. With frame generation that
//! is Streamline's pacing thread, not the engine's. So the callback takes no
//! lock the render path takes, reads the settings through the same
//! generation-stamped copy everything else does, and touches the file only on
//! the frame a control is released.

use parking_lot::Mutex;
use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};
use windows::core::PCSTR;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::GetProcAddress;

/// The add-on API version asked for. ReShade accepts anything from 1 up to
/// its own (20 at 6.8.0) with the same major part, and RenoDX registers with
/// 18 against the same build.
const API_VERSION: u32 = 18;

/// The ImGui function table this file's slot numbers describe.
const TABLE_VERSION: u32 = 19250;

// Slots in `imgui_function_table_19250`.
const GET_VERSION: usize = 2;
const SEPARATOR: usize = 80;
const TEXT_UNFORMATTED: usize = 103;
const CHECKBOX: usize = 115;
const SLIDER_FLOAT: usize = 144;
const SLIDER_INT: usize = 149;
const BEGIN_DISABLED: usize = 279;
const END_DISABLED: usize = 280;
const IS_ITEM_DEACTIVATED_AFTER_EDIT: usize = 295;

type GetVersionFn = unsafe extern "C" fn() -> *const c_char;
type VoidFn = unsafe extern "C" fn();
type TextFn = unsafe extern "C" fn(*const c_char, *const c_char);
type CheckboxFn = unsafe extern "C" fn(*const c_char, *mut bool) -> bool;
type SliderFloatFn =
    unsafe extern "C" fn(*const c_char, *mut f32, f32, f32, *const c_char, i32) -> bool;
type SliderIntFn =
    unsafe extern "C" fn(*const c_char, *mut i32, i32, i32, *const c_char, i32) -> bool;
type BeginDisabledFn = unsafe extern "C" fn(bool);
type BoolFn = unsafe extern "C" fn() -> bool;

type RegisterAddonFn = unsafe extern "C" fn(*mut c_void, u32) -> bool;
type RegisterOverlayFn =
    unsafe extern "C" fn(*mut c_void, *const c_char, unsafe extern "C" fn(*mut c_void));
type GetTableFn = unsafe extern "C" fn(u32) -> *const usize;

/// What ReShade shows in its add-on list. It reads these two exports from a
/// registering module; without them it falls back to the file name.
#[repr(transparent)]
pub struct Text(*const u8);
// A pointer to a string literal, never written.
unsafe impl Sync for Text {}

#[no_mangle]
pub static NAME: Text = Text(b"mhw-dlss\0".as_ptr());
#[no_mangle]
pub static DESCRIPTION: Text =
    Text(b"DLSS 4 upscaling and DLSS frame generation for Monster Hunter: World\0".as_ptr());

const NOT_TRIED: u8 = 0;
const REGISTERED: u8 = 1;
const UNAVAILABLE: u8 = 2;
static STATE: AtomicU8 = AtomicU8::new(NOT_TRIED);
static TABLE: AtomicUsize = AtomicUsize::new(0);

unsafe fn export<T>(module: HMODULE, name: &[u8]) -> Option<T> {
    GetProcAddress(module, PCSTR(name.as_ptr())).map(|entry| std::mem::transmute_copy(&entry))
}

/// Register the page with ReShade, once, if ReShade is loaded.
///
/// Called where a swapchain is created: ReShade has long finished loading by
/// then and is not walking its add-on list on this thread. One attempt per
/// process whatever the outcome, and every outcome is logged with what
/// ReShade answered.
///
/// # Safety
/// Must not be called under the loader lock.
pub unsafe fn register_once() {
    if !crate::reshade::present() {
        return;
    }
    if STATE
        .compare_exchange(NOT_TRIED, UNAVAILABLE, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let reshade = HMODULE(crate::reshade::module() as *mut c_void);
    let Some(ours) = crate::reshade::module_at(register_once as *const c_void) else {
        crate::log!("[cmp] settings page: this library's own module could not be found");
        return;
    };
    let register: Option<RegisterAddonFn> = export(reshade, b"ReShadeRegisterAddon\0");
    let overlay: Option<RegisterOverlayFn> = export(reshade, b"ReShadeRegisterOverlayForAddon\0");
    let get_table: Option<GetTableFn> = export(reshade, b"ReShadeGetImGuiFunctionTable\0");
    let (Some(register), Some(overlay), Some(get_table)) = (register, overlay, get_table) else {
        crate::log!(
            "[cmp] settings page: this ReShade has no add-on interface (RegisterAddon {}, \
             RegisterOverlayForAddon {}, GetImGuiFunctionTable {}), so there is no page. \
             The ini file works as before",
            if register.is_some() {
                "found"
            } else {
                "missing"
            },
            if overlay.is_some() {
                "found"
            } else {
                "missing"
            },
            if get_table.is_some() {
                "found"
            } else {
                "missing"
            }
        );
        return;
    };

    // The table first: registering an add-on that then has nothing to draw
    // with would leave an empty entry in ReShade's list.
    let table = get_table(TABLE_VERSION);
    if table.is_null() {
        crate::log!(
            "[cmp] settings page: ReShadeGetImGuiFunctionTable({TABLE_VERSION}) -> null. This \
             ReShade does not serve the table this build knows, so there is no page"
        );
        return;
    }
    let get_version: GetVersionFn = std::mem::transmute(*table.add(GET_VERSION));
    let reported = get_version();
    let version = if reported.is_null() {
        String::new()
    } else {
        CStr::from_ptr(reported).to_string_lossy().into_owned()
    };
    // ImGui's version text: digits and dots, starting "1.". Anything else
    // means slot 2 is not GetVersion and none of the other slots can be
    // trusted either.
    let plausible = version.starts_with("1.")
        && version.len() < 24
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == ' ' || c == '-');
    if !plausible {
        crate::log!(
            "[cmp] settings page: slot {GET_VERSION} of the ImGui table at {:#x} did not \
             answer with a version (\"{version}\"), so the table is not the layout this \
             build knows and there is no page",
            table as usize
        );
        return;
    }

    let accepted = register(ours.0, API_VERSION);
    if !accepted {
        crate::log!(
            "[cmp] settings page: ReShadeRegisterAddon(module {:#x}, API {API_VERSION}) -> \
             false. ReShade.log has its reason; add-ons may be disabled",
            ours.0 as usize
        );
        return;
    }
    TABLE.store(table as usize, Ordering::Release);
    // A null title makes it the add-on's own settings section in ReShade's
    // Add-ons tab rather than a separate window.
    overlay(ours.0, std::ptr::null(), draw);
    STATE.store(REGISTERED, Ordering::SeqCst);
    crate::log!(
        "[cmp] settings page: ReShadeRegisterAddon(module {:#x}, API {API_VERSION}) -> true; \
         ImGui table {TABLE_VERSION} at {:#x} reports version \"{version}\"; registered as \
         this add-on's settings section (ReShade overlay, Add-ons tab, mhw-dlss)",
        ours.0 as usize,
        table as usize
    );
}

/// A value the user is dragging, until the file has caught up with it.
///
/// Without this a slider would snap back for half a second after release:
/// the page shows the settings, and the settings change only when the watcher
/// has read the file again.
#[derive(Clone, Copy, Default)]
struct Pending {
    render_scale: Option<f32>,
    multiplier: Option<i32>,
    /// Frames left before a pending value is dropped whether or not the file
    /// caught up, so a write that failed cannot pin a control for good.
    frames: u32,
}

static PENDING: Mutex<Pending> = Mutex::new(Pending {
    render_scale: None,
    multiplier: None,
    frames: 0,
});

/// How long a released value is shown before the settings are believed again.
const PENDING_FRAMES: u32 = 240;

/// Set while the callback is on a stack, and how many times it has refused
/// to run. ReShade calls it once per overlay frame; a second entry while one
/// is running would be ReShade re-entering through something this page
/// called, and it is declined rather than nested.
static DRAWING: AtomicBool = AtomicBool::new(false);
static DECLINED: AtomicU32 = AtomicU32::new(0);
/// After this many declined entries the page stops drawing for the session.
const MAX_DECLINED: u32 = 16;
static WRITE_FAILED: AtomicBool = AtomicBool::new(false);

struct Drawing;

impl Drawing {
    fn enter() -> Option<Drawing> {
        if DRAWING.swap(true, Ordering::Acquire) {
            if DECLINED.fetch_add(1, Ordering::Relaxed) + 1 == MAX_DECLINED {
                crate::log!(
                    "[cmp] settings page: its callback was entered {MAX_DECLINED} times \
                     while already running, so it stops drawing for this session"
                );
            }
            return None;
        }
        Some(Drawing)
    }
}

impl Drop for Drawing {
    fn drop(&mut self) {
        DRAWING.store(false, Ordering::Release);
    }
}

fn write(key: &str, value: String) {
    match crate::config::write_key(key, &value) {
        Ok(()) => crate::log!("[cmp] settings page: wrote {key} = {value} to the ini file"),
        Err(error) => {
            if !WRITE_FAILED.swap(true, Ordering::Relaxed) {
                crate::log!("[cmp] settings page: could not write {key} = {value}: {error}");
            }
        }
    }
}

/// The render scales people mean when they drag near them. 0.667 and not
/// two thirds: it is the figure the file and the documentation use, and at
/// 3840x2160 it is the one that gives the engine 2560x1440 after its own
/// rounding, where 0.666 gives 2544x1431.
const COMMON_SCALES: [f32; 6] = [0.5, 0.58, 0.667, 0.75, 0.85, 1.0];

/// How near a common scale a value has to be to become it. A slider a few
/// hundred pixels wide moves in steps of about 0.002, so this is a few pixels
/// either side, and everything between the common scales stays reachable.
const SNAP_WITHIN: f32 = 0.008;

/// `scale` within the range the engine supports, moved onto a common scale
/// if it is within `SNAP_WITHIN` of one.
fn snapped(scale: f32) -> f32 {
    let scale = if scale.is_finite() {
        scale.clamp(0.5, 1.0)
    } else {
        1.0
    };
    COMMON_SCALES
        .iter()
        .copied()
        .find(|common| (scale - common).abs() <= SNAP_WITHIN)
        .unwrap_or(scale)
}

/// What the page says about frame generation, and what its box does.
struct FrameGenerationNote {
    /// What this session is.
    session: &'static str,
    /// Which of its two jobs "Generate frames" is doing.
    control: &'static str,
    /// Said under the boxes when the file and the session disagree.
    restart: Option<&'static str>,
}

/// `loaded`: Streamline is up in this session. `asked_for`: `fg` was 1 when
/// the game started. `fg`: what the file says now.
fn frame_generation_note(loaded: bool, asked_for: bool, fg: bool) -> FrameGenerationNote {
    if loaded {
        return FrameGenerationNote {
            session: "Frame generation is loaded in this session.",
            control: "\"Generate frames\" switches it on and off now, without a restart (fg_interpolate).",
            restart: (!fg).then_some(
                "fg = 0 is in mhw-dlss.ini: frame generation will not be loaded the next time the game starts. Until then it stays loaded.",
            ),
        };
    }
    FrameGenerationNote {
        session: if asked_for {
            "Frame generation was asked for when the game started (fg = 1) and did not start; mhw-dlss.log says why."
        } else {
            "Frame generation was not loaded when the game started (fg = 0)."
        },
        control: "Whether it is loaded is decided at start-up, so the box below only sets fg in mhw-dlss.ini.",
        restart: match (fg, asked_for) {
            (true, false) => Some(
                "fg = 1 is in mhw-dlss.ini. Restart the game for frame generation to take effect.",
            ),
            (false, true) => Some(
                "fg = 0 is in mhw-dlss.ini: frame generation will not be asked for at the next start.",
            ),
            _ => None,
        },
    }
}

/// The page. Called by ReShade with its effect runtime, which is not used.
unsafe extern "C" fn draw(_runtime: *mut c_void) {
    let table = TABLE.load(Ordering::Acquire) as *const usize;
    if table.is_null() || DECLINED.load(Ordering::Relaxed) >= MAX_DECLINED {
        return;
    }
    // Bound to a variable: `Drawing::enter().is_none()` would create the guard
    // and drop it in the same statement.
    let Some(_drawing) = Drawing::enter() else {
        return;
    };
    // Never waited for. The only other holder is this function.
    let Some(mut pending) = PENDING.try_lock() else {
        return;
    };

    let separator: VoidFn = std::mem::transmute(*table.add(SEPARATOR));
    let text: TextFn = std::mem::transmute(*table.add(TEXT_UNFORMATTED));
    let checkbox: CheckboxFn = std::mem::transmute(*table.add(CHECKBOX));
    let slider_float: SliderFloatFn = std::mem::transmute(*table.add(SLIDER_FLOAT));
    let slider_int: SliderIntFn = std::mem::transmute(*table.add(SLIDER_INT));
    let begin_disabled: BeginDisabledFn = std::mem::transmute(*table.add(BEGIN_DISABLED));
    let end_disabled: VoidFn = std::mem::transmute(*table.add(END_DISABLED));
    let released: BoolFn = std::mem::transmute(*table.add(IS_ITEM_DEACTIVATED_AFTER_EDIT));
    let say = |line: &str| {
        let range = line.as_bytes().as_ptr_range();
        text(range.start as *const c_char, range.end as *const c_char);
    };

    let settings = crate::config::get();
    if pending.frames > 0 {
        pending.frames -= 1;
        if pending.frames == 0 {
            *pending = Pending::default();
        }
    }

    // Render scale: the one control of render resolution. The slider shows
    // what the file asks for; the lines under it say what the engine was
    // observed to have built, which is a shade less because the engine cuts
    // the render width down to a multiple of sixteen, and can be the whole
    // window when nothing can be resolved at the ratio asked for.
    let mut scale = pending.render_scale.unwrap_or(settings.render_scale);
    if slider_float(
        c"Render scale".as_ptr(),
        &mut scale,
        0.5,
        1.0,
        c"%.3f".as_ptr(),
        0,
    ) {
        // Snapped as it is dragged, so the figure on the slider is the one
        // that will be written.
        scale = snapped(scale);
        pending.render_scale = Some(scale);
        pending.frames = PENDING_FRAMES;
    }
    if released() {
        write("render_scale", format!("{:.3}", snapped(scale)));
    }
    say("1.0 DLAA, 0.85, 0.75, 0.667 Quality, 0.58 Balanced, 0.5 Performance.");
    say("The slider settles on those when it is brought close to one; any other value is kept as set.");
    match crate::hijack::ratio_status() {
        Some((wanted, built, (width, height))) => {
            say(&format!(
                "Asked for {:.3}; the engine has built {built:.4}, which is {width}x{height}.",
                settings.render_scale
            ));
            if (wanted - settings.render_scale).abs() > 1e-4 {
                say(&format!(
                    "Held at {wanted:.3} for now: nothing can be resolved at the ratio asked for (the log says why)."
                ));
            }
        }
        None => say("The engine's render ratio has not been read yet."),
    }
    say("The game's own Image Quality option does nothing while the mod is loaded.");
    // The texture mip bias is a start-up quantity: the engine builds its
    // samplers once. So it is shown against the scale it was built for, and
    // the page says a restart is what makes it follow a new one.
    if settings.mip_bias == 1 {
        match crate::hijack::mip_bias_built() {
            Some((ratio, bias)) => {
                say(&format!(
                    "Texture mip bias {bias:+.3}: fixed at start-up, for the render scale read \
                     then ({ratio:.3})."
                ));
                say(if (ratio - settings.render_scale).abs() > 0.02 {
                    "The render scale has changed since. Restart the game for the bias to follow it."
                } else {
                    "It does not follow the slider. Restart the game after changing the render \
                     scale for the bias to follow it."
                });
            }
            None => say(
                "Texture mip bias: none in this session (the render scale read at start-up was \
                 1.0, or the log says why not). Restart the game for it to follow a new render \
                 scale.",
            ),
        }
    }

    separator();

    // Frame generation is loaded or not when the game starts: the swapchain
    // is created through Streamline or it is not. So "Generate frames" is one
    // of two controls, and the page says which. In a session that loaded it,
    // the box is the live switch (`fg_interpolate`), and a second box sets
    // whether it is loaded next time. In a session that did not, the box
    // sets `fg` itself, which only the next start reads.
    let loaded = crate::streamline::ready();
    let note = frame_generation_note(loaded, crate::streamline::asked_for(), settings.fg);
    say(note.session);
    say(note.control);
    if loaded {
        // Generation on and off. The swapchain stays Streamline's either
        // way; this asks the plugin for generated frames or for none.
        let mut generate = settings.fg_interpolate;
        if checkbox(c"Generate frames".as_ptr(), &mut generate) {
            write(
                "fg_interpolate",
                if generate { "1" } else { "0" }.to_string(),
            );
        }
        let mut at_start = settings.fg;
        if checkbox(
            c"Load frame generation when the game starts".as_ptr(),
            &mut at_start,
        ) {
            write("fg", if at_start { "1" } else { "0" }.to_string());
        }
    } else {
        let mut at_start = settings.fg;
        if checkbox(
            c"Generate frames (from the next start of the game)".as_ptr(),
            &mut at_start,
        ) {
            write("fg", if at_start { "1" } else { "0" }.to_string());
        }
    }
    if let Some(restart) = note.restart {
        say(restart);
    }
    // What a session with frame generation loaded can change as it runs.
    begin_disabled(!loaded);
    let mut multiplier = pending.multiplier.unwrap_or(settings.fg_multiplier as i32);
    if slider_int(
        c"Frames presented per rendered frame".as_ptr(),
        &mut multiplier,
        2,
        4,
        c"%d".as_ptr(),
        0,
    ) {
        pending.multiplier = Some(multiplier);
        pending.frames = PENDING_FRAMES;
    }
    if released() {
        write("fg_multiplier", multiplier.clamp(2, 4).to_string());
    }
    let mut boost = settings.reflex_boost;
    if checkbox(c"Reflex boost".as_ptr(), &mut boost) {
        write("reflex_boost", if boost { "1" } else { "0" }.to_string());
    }
    end_disabled();

    separator();

    // What is. The render size is the one the DLSS feature was last built
    // for, which follows every change of render scale.
    let render = crate::dlss::created_for();
    let output = crate::swap::chain().map(|chain| chain.size);
    match (render, output) {
        (Some(render), Some(output)) => say(&format!(
            "Rendering {}x{}, output {}x{}.",
            render.0, render.1, output.0, output.1
        )),
        (None, Some(output)) => say(&format!(
            "Output {}x{}; no upscaled frame has been seen yet.",
            output.0, output.1
        )),
        _ => say("No swapchain of the game's has been seen yet."),
    }
    match crate::streamline::last_state() {
        Some((status, presented)) => say(&format!(
            "Frame generation is {}: status {status:#x}, {presented} presented per rendered \
             frame at the last sample.",
            if settings.fg_interpolate {
                "running"
            } else {
                "loaded, with generation off"
            }
        )),
        None => say("Frame generation is not running in this session."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slot numbers, against the table they were counted from. A reminder
    /// more than a proof: the header is not in this repository, so what this
    /// pins is that nobody changes one number without the others.
    #[test]
    fn slots_are_the_19250_tables() {
        assert_eq!(TABLE_VERSION, 19250);
        assert_eq!(
            [
                GET_VERSION,
                SEPARATOR,
                TEXT_UNFORMATTED,
                CHECKBOX,
                SLIDER_FLOAT,
                SLIDER_INT,
                BEGIN_DISABLED,
                END_DISABLED,
                IS_ITEM_DEACTIVATED_AFTER_EDIT
            ],
            [2, 80, 103, 115, 144, 149, 279, 280, 295]
        );
    }

    #[test]
    fn the_slider_lands_on_the_common_scales_and_keeps_the_rest() {
        // What the user ended on, and its neighbours.
        assert_eq!(snapped(0.666), 0.667);
        assert_eq!(snapped(0.6712), 0.667);
        assert_eq!(snapped(0.5795), 0.58);
        assert_eq!(snapped(0.994), 1.0);
        assert_eq!(snapped(0.503), 0.5);
        assert_eq!(snapped(0.755), 0.75);
        assert_eq!(snapped(0.846), 0.85);
        // Between them a value is kept as it is.
        for kept in [0.52, 0.6, 0.64, 0.7, 0.8, 0.9, 0.95] {
            assert_eq!(snapped(kept), kept);
        }
        // Out of range and nonsense are brought into range, as before.
        assert_eq!(snapped(0.2), 0.5);
        assert_eq!(snapped(1.7), 1.0);
        assert_eq!(snapped(f32::NAN), 1.0);
        // What is written parses back to the scale meant.
        for common in COMMON_SCALES {
            assert_eq!(
                format!("{:.3}", snapped(common)).parse::<f32>().unwrap(),
                common
            );
        }
    }

    #[test]
    fn the_page_says_which_job_the_frame_generation_box_is_doing() {
        // Started without it: the box writes fg, and a restart is asked for
        // only once the file says 1.
        let off = frame_generation_note(false, false, false);
        assert!(off.session.contains("was not loaded"));
        assert!(off.control.contains("only sets"));
        assert!(off.restart.is_none());
        let wanted = frame_generation_note(false, false, true);
        assert!(wanted
            .restart
            .is_some_and(|line| line.contains("Restart the game")));
        // Started with it: the box is the live switch, and fg = 0 in the
        // file is said to be for the next start.
        let on = frame_generation_note(true, true, true);
        assert!(on.control.contains("without a restart"));
        assert!(on.restart.is_none());
        let leaving = frame_generation_note(true, true, false);
        assert!(leaving
            .restart
            .is_some_and(|line| line.contains("next time")));
        // Asked for and not started: said as that, not as fg = 0.
        let failed = frame_generation_note(false, true, true);
        assert!(failed.session.contains("did not"));
        assert!(failed.restart.is_none());
    }

    #[test]
    fn the_guard_declines_a_second_entry_and_releases() {
        let first = Drawing::enter();
        assert!(first.is_some());
        assert!(Drawing::enter().is_none());
        drop(first);
        assert!(Drawing::enter().is_some());
    }
}
