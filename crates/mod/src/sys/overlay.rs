//! A settings page in ReShade's overlay, when ReShade is loaded.
//!
//! The ini file stays the configuration; this page edits it. A change
//! rewrites one line of `mhw-dlss.ini` (`crate::ini_edit`) and the hot
//! reload applies it within half a second, so a value has one path. Only
//! the public settings are offered.
//!
//! ReShade's add-on interface is plain C exports found on its module
//! (`ReShadeRegisterAddon`, `ReShadeRegisterOverlayForAddon`,
//! `ReShadeGetImGuiFunctionTable`); any module may register. The widgets
//! come from a positional table of function pointers, so no ImGui is linked.
//! The slot numbers are those of table version 19250 (ReShade 6.8.0,
//! `include/reshade_overlay.hpp` at `v6.8.0`: 421 pointers, a field's
//! position is its slot). Before anything else the table's `GetVersion`
//! must answer with ImGui's version text, or the table is not used.
//!
//! The callback runs on whichever thread calls ReShade's Present (with
//! frame generation, Streamline's pacing thread): it takes no lock the
//! render path takes and touches the file only when a control is released.

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use mhw_core::config::{Named, Preset, UpscalingMode, FILE_NAME};
use parking_lot::Mutex;
use windows::core::PCSTR;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::GetProcAddress;

use super::runtime;
use crate::ini_edit::{set_key_in_file, EditRefused};
use crate::page::{decimal, level_name_at, level_position, name_at, position, snapped};

/// Add-on API version asked for (ReShade accepts 1 up to its own with the
/// same major part; 20 at 6.8.0).
const API_VERSION: u32 = 18;
/// The ImGui function table the slots below describe.
const TABLE_VERSION: u32 = 19250;

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

/// A pointer to a string literal, exported for ReShade's add-on list.
#[repr(transparent)]
#[derive(Debug)]
pub struct Text(*const u8);
// SAFETY: points at a string literal, never written.
unsafe impl Sync for Text {}

/// The add-on name ReShade shows.
#[no_mangle]
pub static NAME: Text = Text(c"mhw-dlss".as_ptr().cast());
/// The add-on description ReShade shows.
#[no_mangle]
pub static DESCRIPTION: Text = Text(
    c"DLSS 4 upscaling and DLSS frame generation for Monster Hunter: World"
        .as_ptr()
        .cast(),
);

const NOT_TRIED: u8 = 0;
const DONE: u8 = 1;
static STATE: AtomicU8 = AtomicU8::new(NOT_TRIED);
static TABLE: AtomicUsize = AtomicUsize::new(0);
static DRAWING: AtomicBool = AtomicBool::new(false);
static WRITE_FAILED: AtomicBool = AtomicBool::new(false);

/// Frames a released value is shown before the settings are believed
/// again (the reload needs up to half a second to catch up).
const PENDING_FRAMES: u32 = 240;

/// Values being dragged, until the file caught up.
#[derive(Clone, Copy, Debug, Default)]
struct Pending {
    mode: Option<i32>,
    render_scale: Option<f32>,
    preset: Option<i32>,
    sharpness: Option<f32>,
    multiplier: Option<i32>,
    level: Option<i32>,
    frames: u32,
}

static PENDING: Mutex<Pending> = Mutex::new(Pending {
    mode: None,
    render_scale: None,
    preset: None,
    sharpness: None,
    multiplier: None,
    level: None,
    frames: 0,
});

/// # Safety
/// `module` is a loaded module.
unsafe fn export<T>(module: HMODULE, name: &CStr) -> Option<T> {
    // SAFETY: a loaded module and a NUL-terminated name; the caller names
    // the export's type.
    unsafe {
        GetProcAddress(module, PCSTR(name.as_ptr().cast()))
            .map(|entry| std::mem::transmute_copy(&entry))
    }
}

/// The module serving ReShade's add-on interface.
fn reshade_module() -> Option<HMODULE> {
    mhw_win::loaded_modules().into_iter().find_map(|loaded| {
        let module = HMODULE(loaded.base as *mut c_void);
        // SAFETY: a loaded module handle.
        unsafe { export::<usize>(module, c"ReShadeRegisterAddon") }.map(|_| module)
    })
}

/// Register the page with ReShade, once per process, if ReShade is loaded.
/// Called where the swapchain is created (ReShade is loaded by then and not
/// walking its add-on list on this thread); never under the loader lock.
pub(crate) fn register_once() {
    if !mhw_render::reshade_present() || STATE.swap(DONE, Ordering::AcqRel) != NOT_TRIED {
        return;
    }
    let Some(reshade) = reshade_module() else {
        log::info!(target: "boot", "settings page: this ReShade has no add-on interface");
        return;
    };
    let Some(ours) = super::own_module() else {
        return;
    };
    // SAFETY: ReShade's module and the documented export types.
    let (register, overlay, get_table) = unsafe {
        (
            export::<RegisterAddonFn>(reshade, c"ReShadeRegisterAddon"),
            export::<RegisterOverlayFn>(reshade, c"ReShadeRegisterOverlayForAddon"),
            export::<GetTableFn>(reshade, c"ReShadeGetImGuiFunctionTable"),
        )
    };
    let (Some(register), Some(overlay), Some(get_table)) = (register, overlay, get_table) else {
        log::info!(target: "boot", "settings page: ReShade lacks an add-on export; no page");
        return;
    };
    // The table first: registering with nothing to draw with would leave
    // an empty entry in ReShade's list.
    // SAFETY: ReShade's export with its documented signature.
    let table = unsafe { get_table(TABLE_VERSION) };
    if table.is_null() {
        log::info!(
            target: "boot",
            "settings page: ReShade does not serve ImGui table {TABLE_VERSION}; no page"
        );
        return;
    }
    // SAFETY: slot 2 of a table of at least 421 pointers.
    let version = unsafe {
        let get_version: GetVersionFn = slot_fn(table, GET_VERSION);
        let text = get_version();
        if text.is_null() {
            String::new()
        } else {
            CStr::from_ptr(text).to_string_lossy().into_owned()
        }
    };
    let plausible = version.starts_with("1.")
        && version.len() < 24
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ' ' | '-'));
    if !plausible {
        log::warn!(
            target: "boot",
            "settings page: ImGui table slot {GET_VERSION} answered {version:?}, not a version; \
             the table layout is not the one this build knows, no page"
        );
        return;
    }
    // SAFETY: ReShade's exports; our own module handle.
    unsafe {
        if !register(ours.0, API_VERSION) {
            log::info!(
                target: "boot",
                "settings page: ReShadeRegisterAddon refused (ReShade.log says why)"
            );
            return;
        }
        TABLE.store(table as usize, Ordering::Release);
        // A null title makes it the add-on's own section in the Add-ons tab.
        overlay(ours.0, std::ptr::null(), draw);
    }
    log::info!(target: "boot", "settings page registered with ReShade (ImGui {version})");
}

/// Rewrite one line of the settings file, in its own encoding, through a
/// sibling `mhw-dlss.ini.writing` renamed over it: the hot reload reads the
/// file every 500 ms and must never see it half written. Nothing is
/// written when the value is already that or the file does not decode
/// cleanly (rewriting it would lose the user's text).
fn write(section: &str, key: &str, value: &str) {
    let Some(paths) = runtime::paths() else {
        return;
    };
    let edited =
        std::fs::read(&paths.settings).map(|bytes| set_key_in_file(&bytes, section, key, value));
    let result = match edited {
        Ok(Ok(None)) => return,
        Ok(Ok(Some(bytes))) => {
            let writing = paths.settings.with_extension("ini.writing");
            std::fs::write(&writing, bytes)
                .and_then(|()| std::fs::rename(&writing, &paths.settings))
                .map_err(|error| error.to_string())
        }
        Ok(Err(EditRefused::NotDecodable)) => Err(
            "it is not valid UTF-8/UTF-16 text; edit it by hand (nothing was written)".to_owned(),
        ),
        Err(error) => Err(error.to_string()),
    };
    match result {
        Ok(()) => log::info!(target: "config", "settings page: [{section}] {key} = {value}"),
        Err(error) => {
            if !WRITE_FAILED.swap(true, Ordering::Relaxed) {
                log::warn!(target: "config", "settings page: could not write {FILE_NAME}: {error}");
            }
        }
    }
}

/// Slot `index` of the ImGui table as the function type `F`.
///
/// # Safety
/// `table` is a verified table and slot `index` holds a function of type
/// `F` (a function pointer, one word).
unsafe fn slot_fn<F: Copy>(table: *const usize, index: usize) -> F {
    // SAFETY: caller's contract.
    unsafe { std::mem::transmute_copy::<usize, F>(&*table.add(index)) }
}

struct Widgets {
    separator: VoidFn,
    text: TextFn,
    checkbox: CheckboxFn,
    slider_float: SliderFloatFn,
    slider_int: SliderIntFn,
    begin_disabled: BeginDisabledFn,
    end_disabled: VoidFn,
    released: BoolFn,
}

impl Widgets {
    /// # Safety
    /// `table` is a verified version-19250 table.
    unsafe fn of(table: *const usize) -> Widgets {
        // SAFETY: caller's contract: each slot holds the function its
        // field's type names.
        unsafe {
            Widgets {
                separator: slot_fn(table, SEPARATOR),
                text: slot_fn(table, TEXT_UNFORMATTED),
                checkbox: slot_fn(table, CHECKBOX),
                slider_float: slot_fn(table, SLIDER_FLOAT),
                slider_int: slot_fn(table, SLIDER_INT),
                begin_disabled: slot_fn(table, BEGIN_DISABLED),
                end_disabled: slot_fn(table, END_DISABLED),
                released: slot_fn(table, IS_ITEM_DEACTIVATED_AFTER_EDIT),
            }
        }
    }

    fn say(&self, line: &str) {
        let range = line.as_bytes().as_ptr_range();
        // SAFETY: ImGui reads `[start, end)`.
        unsafe { (self.text)(range.start.cast(), range.end.cast()) };
    }

    /// An enumeration slider showing the value's name.
    fn choice(&self, label: &CStr, at: &mut i32, last: i32, name: fn(i32) -> &'static str) -> bool {
        let shown = CString::new(name(*at)).unwrap_or_default();
        // SAFETY: NUL-terminated strings; ImGui writes `at`.
        unsafe { (self.slider_int)(label.as_ptr(), at, 0, last, shown.as_ptr(), 0) }
    }

    fn released(&self) -> bool {
        // SAFETY: an ImGui query about the last item.
        unsafe { (self.released)() }
    }
}

fn last<T: Named>() -> i32 {
    i32::try_from(T::NAMES.len()).unwrap_or(1) - 1
}

/// The page. Called by ReShade with its effect runtime (not used).
unsafe extern "C" fn draw(_runtime: *mut c_void) {
    let table = TABLE.load(Ordering::Acquire) as *const usize;
    if table.is_null() || DRAWING.swap(true, Ordering::Acquire) {
        return;
    }
    if let Some(mut pending) = PENDING.try_lock() {
        // SAFETY: verified at registration.
        let ui = unsafe { Widgets::of(table) };
        page(&ui, &mut pending);
    }
    DRAWING.store(false, Ordering::Release);
}

fn page(ui: &Widgets, pending: &mut Pending) {
    let settings = runtime::settings();
    if pending.frames > 0 {
        pending.frames -= 1;
        if pending.frames == 0 {
            *pending = Pending::default();
        }
    }
    let hold = |pending: &mut Pending| pending.frames = PENDING_FRAMES;

    let mut mode = pending.mode.unwrap_or(position(settings.upscaling.mode));
    if ui.choice(
        c"Mode",
        &mut mode,
        last::<UpscalingMode>(),
        name_at::<UpscalingMode>,
    ) {
        pending.mode = Some(mode);
        hold(pending);
    }
    if ui.released() {
        write("Upscaling", "Mode", name_at::<UpscalingMode>(mode));
    }
    let custom = name_at::<UpscalingMode>(mode) == UpscalingMode::Custom.name();
    // SAFETY: ImGui calls with NUL-terminated strings and live pointers.
    unsafe { (ui.begin_disabled)(!custom) };
    let mut scale = pending
        .render_scale
        .unwrap_or(settings.upscaling.render_scale);
    // SAFETY: as above.
    if unsafe {
        (ui.slider_float)(
            c"Render scale".as_ptr(),
            &mut scale,
            0.5,
            1.0,
            c"%.3f".as_ptr(),
            0,
        )
    } {
        scale = snapped(scale);
        pending.render_scale = Some(scale);
        hold(pending);
    }
    if ui.released() {
        write("Upscaling", "RenderScale", &decimal(snapped(scale), 3));
    }
    // SAFETY: paired with begin_disabled.
    unsafe { (ui.end_disabled)() };
    ui.say(
        "DLAA 1.0, Quality 0.667, Balanced 0.58, Performance 0.5; Custom uses the render scale.",
    );

    let mut preset = pending
        .preset
        .unwrap_or(position(settings.upscaling.preset));
    if ui.choice(
        c"DLSS preset",
        &mut preset,
        last::<Preset>(),
        name_at::<Preset>,
    ) {
        pending.preset = Some(preset);
        hold(pending);
    }
    if ui.released() {
        write("Upscaling", "Preset", name_at::<Preset>(preset));
    }
    let mut sharpness = pending.sharpness.unwrap_or(settings.upscaling.sharpness);
    // SAFETY: as above.
    if unsafe {
        (ui.slider_float)(
            c"Sharpness".as_ptr(),
            &mut sharpness,
            0.0,
            1.0,
            c"%.2f".as_ptr(),
            0,
        )
    } {
        pending.sharpness = Some(sharpness);
        hold(pending);
    }
    if ui.released() {
        let value = if sharpness.is_finite() {
            sharpness.clamp(0.0, 1.0)
        } else {
            0.0
        };
        write("Upscaling", "Sharpness", &decimal(value, 2));
    }
    ui.say("0 is off. Applied after DLSS, before the interface is drawn.");
    let mut full_fog = !settings.upscaling.downsample_volume;
    // SAFETY: ImGui call.
    if unsafe { (ui.checkbox)(c"Full-resolution volumetric fog".as_ptr(), &mut full_fog) } {
        write(
            "Upscaling",
            "DownsampleVolume",
            if full_fog { "false" } else { "true" },
        );
    }
    ui.say(
        "Smooths stair-stepped edges where objects meet fog. Takes effect when the game starts \
         again.",
    );

    // SAFETY: ImGui call.
    unsafe { (ui.separator)() };
    let running = runtime::fg_started() && mhw_fg::ready();
    let mut enabled = settings.frame_generation.enabled;
    // SAFETY: as above.
    if unsafe { (ui.checkbox)(c"Frame generation".as_ptr(), &mut enabled) } {
        write(
            "FrameGeneration",
            "Enabled",
            if enabled { "true" } else { "false" },
        );
    }
    ui.say(if running {
        "Running in this session. Loading it or not is decided when the game starts."
    } else {
        "Not running in this session; turning it on takes effect when the game starts again."
    });
    // SAFETY: as above.
    unsafe { (ui.begin_disabled)(!running) };
    let mut multiplier = pending
        .multiplier
        .unwrap_or(i32::try_from(settings.frame_generation.multiplier).unwrap_or(2));
    // SAFETY: as above.
    if unsafe {
        (ui.slider_int)(
            c"Frames shown per rendered frame".as_ptr(),
            &mut multiplier,
            2,
            4,
            c"%d".as_ptr(),
            0,
        )
    } {
        pending.multiplier = Some(multiplier);
        hold(pending);
    }
    if ui.released() {
        write(
            "FrameGeneration",
            "Multiplier",
            &multiplier.clamp(2, 4).to_string(),
        );
    }
    let mut boost = settings.frame_generation.reflex_boost;
    // SAFETY: as above.
    if unsafe { (ui.checkbox)(c"Reflex boost".as_ptr(), &mut boost) } {
        write(
            "FrameGeneration",
            "ReflexBoost",
            if boost { "true" } else { "false" },
        );
    }
    // SAFETY: paired with begin_disabled.
    unsafe { (ui.end_disabled)() };

    // SAFETY: ImGui call.
    unsafe { (ui.separator)() };
    let mut level = pending
        .level
        .unwrap_or(level_position(settings.general.log_level));
    if ui.choice(c"Log level", &mut level, 4, level_name_at) {
        pending.level = Some(level);
        hold(pending);
    }
    if ui.released() {
        write("General", "LogLevel", level_name_at(level));
    }
    if let Some(frame) = super::live::last_frame() {
        ui.say(&format!(
            "Rendering {}x{}, output {}x{}{}.",
            frame.render.w,
            frame.render.h,
            frame.display.w,
            frame.display.h,
            if runtime::given_up() {
                "; DLSS is off (the game's own anti-aliasing runs, see the log)"
            } else {
                ""
            }
        ));
    } else {
        ui.say("No upscaled frame yet.");
    }
}
