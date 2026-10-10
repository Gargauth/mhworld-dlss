//! Creating and evaluating the DLSS feature.
//!
//! Everything this needs is established elsewhere: the engine's temporal
//! dispatch and its bound colour and motion vector textures; the main depth
//! buffer, found through its depth-stencil view; a sub-pixel jitter, which is
//! the engine's own mechanism given this library's sequence (`native.rs`);
//! and an NGX session that reports SuperSampling as available (`ngx.rs`).
//!
//! The entry points are called through the driver's `_nvngx.dll` exports.
//! Their argument lists follow the public header here, which is reliable for
//! these four: the SDK's own client is a thin forwarder that passes its
//! parameters straight through. `Init` was the exception, and that one was
//! settled by observing the game's call.

use parking_lot::Mutex;
use std::ffi::{c_void, CString};
use std::sync::atomic::{AtomicUsize, Ordering};
use windows::core::PCSTR;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::GetProcAddress;

const SUCCESS: u32 = 0x1;
/// `NVSDK_NGX_DLSS_Feature_Flags`: HDR colour (bit 0), inverted depth (bit 3)
/// and auto exposure (bit 6). Bit 1, low-resolution motion vectors, is added
/// by `flags_for` when upscaling. Bit 2, jittered motion vectors, stays off:
/// the engine's vectors carry no jitter.
const FEATURE_FLAGS: i32 = 0x49;
/// `NVSDK_NGX_Feature_SuperSampling`.
const FEATURE_SUPERSAMPLING: u32 = 1;

/// `NVSDK_NGX_Parameter` vtable slots.
///
/// The header declares eight `Set` overloads, then eight `Get`, then `Reset`.
/// MSVC lays out overloads of one name in **reverse** declaration order,
/// which a write-then-read check caught: an int round-tripped but a float came
/// back as 5.6e-11, because slot 1 is the `ID3D12Resource*` setter rather than
/// the float one, and a float argument arrives in XMM1 where that overload
/// reads RDX.
///
/// Declared order:  u64, f32, f64, u32, i32, ID3D11Resource*, ID3D12Resource*, void*
/// Actual order:    void*, ID3D12Resource*, ID3D11Resource*, i32, u32, f64, f32, u64
const SET_D3D12: usize = 1;
const SET_I32: usize = 3;
const SET_U32: usize = 4;
const SET_F32: usize = 6;
/// The matching `Get` overloads, which start at slot 8 and are reversed too.
const GET_I32: usize = 11;
const GET_F32: usize = 14;

type AllocateParametersFn = unsafe extern "system" fn(*mut *mut c_void) -> u32;
type DestroyParametersFn = unsafe extern "system" fn(*mut c_void) -> u32;
type CreateFeatureFn =
    unsafe extern "system" fn(*mut c_void, u32, *mut c_void, *mut *mut c_void) -> u32;
type EvaluateFeatureFn =
    unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void, *const c_void) -> u32;
type ReleaseFeatureFn = unsafe extern "system" fn(*mut c_void) -> u32;

struct Api {
    allocate: AllocateParametersFn,
    destroy: DestroyParametersFn,
    create: CreateFeatureFn,
    evaluate: EvaluateFeatureFn,
    release: ReleaseFeatureFn,
}

static API: Mutex<Option<Api>> = Mutex::new(None);
/// The created feature, and the parameter block it was created with.
static HANDLE: AtomicUsize = AtomicUsize::new(0);
static PARAMETERS: AtomicUsize = AtomicUsize::new(0);
/// Render dimensions the feature was created for; a change means recreating.
static CREATED_FOR: Mutex<(u32, u32)> = Mutex::new((0, 0));
/// Quality and creation flags the feature was built with. Both are fixed at
/// creation, so tuning them from the configuration file means noticing the
/// change here and building the feature again -- otherwise the file appears to
/// do nothing and the picture is judged against a setting that never applied.
static CREATED_WITH: Mutex<(i32, i32, i32)> = Mutex::new((0, 0, 0));
/// A preset outside the library's range has been reported once.
static PRESET_REFUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

unsafe fn resolve<T>(module: HMODULE, name: &[u8]) -> Option<T> {
    GetProcAddress(module, PCSTR(name.as_ptr())).map(|entry| std::mem::transmute_copy(&entry))
}

/// Bind the NGX entry points once.
///
/// # Safety
/// `module` must be a loaded module that serves NGX: the names are resolved
/// from it and called later with the argument lists of the public SDK
/// headers.
pub unsafe fn bind(module: HMODULE) -> bool {
    if API.lock().is_some() {
        return true;
    }
    match load(module) {
        Some(api) => {
            *API.lock() = Some(api);
            true
        }
        None => false,
    }
}

/// Resolve every entry point, or none: a half-bound API is worse than no API.
unsafe fn load(module: HMODULE) -> Option<Api> {
    Some(Api {
        allocate: resolve(module, b"NVSDK_NGX_D3D12_AllocateParameters\0")?,
        destroy: resolve(module, b"NVSDK_NGX_D3D12_DestroyParameters\0")?,
        create: resolve(module, b"NVSDK_NGX_D3D12_CreateFeature\0")?,
        evaluate: resolve(module, b"NVSDK_NGX_D3D12_EvaluateFeature\0")?,
        release: resolve(module, b"NVSDK_NGX_D3D12_ReleaseFeature\0")?,
    })
}

/// Call one of the parameter object's setters through its vtable.
unsafe fn set<T>(parameters: *mut c_void, name: &str, slot: usize, value: T) {
    if parameters.is_null() {
        return;
    }
    let vtable = *(parameters as *const *const usize);
    let entry = *vtable.add(slot);
    let call: unsafe extern "system" fn(*mut c_void, PCSTR, T) = std::mem::transmute(entry);
    if let Ok(key) = CString::new(name) {
        call(parameters, PCSTR(key.as_ptr() as *const u8), value);
    }
}

/// Read an int back out of a parameter object.
unsafe fn get_i32(parameters: *mut c_void, name: &str) -> Option<i32> {
    let vtable = *(parameters as *const *const usize);
    let call: unsafe extern "system" fn(*mut c_void, PCSTR, *mut i32) -> u32 =
        std::mem::transmute(*vtable.add(GET_I32));
    let key = CString::new(name).ok()?;
    let mut value = 0i32;
    (call(parameters, PCSTR(key.as_ptr() as *const u8), &mut value) == SUCCESS).then_some(value)
}

unsafe fn get_f32(parameters: *mut c_void, name: &str) -> Option<f32> {
    let vtable = *(parameters as *const *const usize);
    let call: unsafe extern "system" fn(*mut c_void, PCSTR, *mut f32) -> u32 =
        std::mem::transmute(*vtable.add(GET_F32));
    let key = CString::new(name).ok()?;
    let mut value = 0f32;
    (call(parameters, PCSTR(key.as_ptr() as *const u8), &mut value) == SUCCESS).then_some(value)
}

/// Check that the assumed vtable slots are the right ones, and say whether
/// they were.
///
/// `NVSDK_NGX_Parameter` is a C++ object with eight `Set` overloads and eight
/// `Get`, and the slot numbers here were worked out from the header's
/// declaration order. An int and a float are written and read back. If
/// either does not come back, the slots are not what this code thinks, and
/// everything set after this point would be stored under the wrong type.
///
/// Returns whether both matched; a feature is not created through slots that
/// failed.
unsafe fn verify_slots(parameters: *mut c_void) -> bool {
    set(parameters, "mhw.probe.int", SET_I32, 1234i32);
    set(parameters, "mhw.probe.float", SET_F32, 0.5f32);
    let int = get_i32(parameters, "mhw.probe.int");
    let float = get_f32(parameters, "mhw.probe.float");
    // Once when it holds, which it does on every driver this has met; every
    // time it does not, which is at most the few attempts before DLSS is
    // given up for the session.
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let held = int == Some(1234) && float == Some(0.5);
    if !held || !SAID.swap(true, Ordering::Relaxed) {
        crate::log!("[dls] vtable check: int {int:?} (want 1234), float {float:?} (want 0.5)");
    }
    held
}

/// The creation flags, with the one bit that depends on the resolutions.
///
/// `MVLowRes` (0x2) tells DLSS the motion vector texture is the size of the
/// render target rather than the output. It is handed vectors decoded into a
/// texture of our own at the render size, so the bit is right exactly when
/// upscaling -- and at DLAA, where the two are equal, it makes no difference
/// either way. Deriving it here rather than leaving it in the file means the
/// quality level can be changed while the game runs without the flags quietly
/// becoming wrong for it.
fn flags_for(configured: i32, frame: &Frame) -> i32 {
    const MV_LOW_RES: i32 = 0x2;
    if frame.render.0 < frame.display.0 || frame.render.1 < frame.display.1 {
        configured | MV_LOW_RES
    } else {
        configured & !MV_LOW_RES
    }
}

/// The preset matching a render resolution, when the configuration asks for
/// the choice to be made from what the engine actually rendered.
///
/// The engine decides the render resolution -- nothing here can make it draw
/// at less than it was told to -- so the useful thing is to name the preset
/// whose ratio matches, rather than to assert one and have DLSS scale for a
/// ratio it is not being given.
///
/// `NVSDK_NGX_PerfQuality_Value`: 0 max performance, 1 balanced, 2 max
/// quality, 3 ultra performance, 5 DLAA.
fn preset_for(render: (u32, u32), display: (u32, u32)) -> i32 {
    if display.0 == 0 {
        return 5;
    }
    let ratio = render.0 as f32 / display.0 as f32;
    const PRESETS: &[(f32, i32)] = &[(1.0, 5), (0.667, 2), (0.588, 1), (0.5, 0), (0.333, 3)];
    PRESETS
        .iter()
        .min_by(|(a, _), (b, _)| {
            (a - ratio)
                .abs()
                .partial_cmp(&(b - ratio).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(_, preset)| *preset)
        .unwrap_or(5)
}

/// The preset to ask for, from the number in the settings, or why not.
///
/// The numbers are `NVSDK_NGX_DLSS_Hint_Render_Preset`, read from
/// `nvsdk_ngx_defs.h` in the SDK the 310.9.1 library ships in
/// (`external/ngx-sdk/include` of the Streamline 2.14.1 archive):
///
/// ```text
///    0       default: the driver chooses
///    1 - 4   A to D, removed ("use preset J or K")
///    5, 6    E and F, deprecated
///    7 - 9   G, H, I: "do not use, reverts to default behavior"
///   10       J
///   11       K   the transformer model, default for DLAA/Quality/Balanced
///   12       L   default for Ultra Performance
///   13       M   default for Performance
///   14, 15   N, O: "do not use"
/// ```
///
/// Only the four that mean something are passed on: asking this library for
/// a preset that has been removed, or for a number it does not define, is at
/// best the default and at worst a crash.
fn usable_preset(configured: i32) -> Result<i32, &'static str> {
    match configured {
        0 => Ok(0),
        10..=13 => Ok(configured),
        1..=4 => Err("presets A to D were removed from DLSS"),
        5 | 6 => Err("presets E and F are deprecated"),
        7..=9 | 14 | 15 => Err("that preset is reserved and reverts to the default"),
        _ => Err("there is no such preset"),
    }
}

/// Inputs for one evaluation.
pub struct Frame {
    pub colour: *mut c_void,
    pub output: *mut c_void,
    pub depth: *mut c_void,
    pub motion: *mut c_void,
    pub render: (u32, u32),
    pub display: (u32, u32),
    pub jitter: (f32, f32),
    pub motion_scale: (f32, f32),
    pub reset: bool,
}

/// Create the feature if it does not exist, then evaluate it.
///
/// # Safety
/// `list` must be an open `ID3D12GraphicsCommandList`, and every resource in
/// `frame` must be live.
pub unsafe fn evaluate(list: *mut c_void, frame: &Frame) -> bool {
    // Not at all once DLSS has been found unusable: no feature is created,
    // nothing is recorded and nothing is logged. One relaxed load.
    if crate::gate::dlss_unusable() {
        return false;
    }
    // A module other than the driver's serves NGX and has been let go. That
    // also latches the flag above, so this is the same answer asked of the
    // module that owns the reason. Never true while the driver serves NGX.
    if crate::ngxhost::refused() {
        return false;
    }
    let guard = API.lock();
    let Some(api) = guard.as_ref() else {
        return false;
    };
    // The list NGX is given, which is not always the list this was called
    // with. See `list_for_ngx`.
    let native = list;
    let Some(list) = list_for_ngx(native) else {
        return false;
    };
    // The engine's heaps on that list, noted before NGX replaces them.
    let engines_heaps = if list != native {
        crate::reshade::held_heaps(list)
    } else {
        None
    };

    let settings = crate::config::get();
    // The quality level follows the ratio the engine actually rendered at,
    // which `render_scale` sets; asserting one here would have DLSS scale for
    // a ratio it is not being given.
    let quality = preset_for(frame.render, frame.display);
    let flags = flags_for(FEATURE_FLAGS, frame);
    let wanted = (quality, flags, settings.dlss_preset);
    if (HANDLE.load(Ordering::Relaxed) == 0
        || *CREATED_FOR.lock() != frame.render
        || *CREATED_WITH.lock() != wanted)
        && !create(api, list, frame)
    {
        return false;
    }

    let parameters = PARAMETERS.load(Ordering::Relaxed) as *mut c_void;
    let handle = HANDLE.load(Ordering::Relaxed) as *mut c_void;
    if parameters.is_null() || handle.is_null() {
        return false;
    }

    set(parameters, "Color", SET_D3D12, frame.colour);
    set(parameters, "Output", SET_D3D12, frame.output);
    set(parameters, "Depth", SET_D3D12, frame.depth);
    set(parameters, "MotionVectors", SET_D3D12, frame.motion);
    set(parameters, "Jitter.Offset.X", SET_F32, frame.jitter.0);
    set(parameters, "Jitter.Offset.Y", SET_F32, frame.jitter.1);
    set(parameters, "MV.Scale.X", SET_F32, frame.motion_scale.0);
    set(parameters, "MV.Scale.Y", SET_F32, frame.motion_scale.1);
    // Always: the feature is created for the render size, and the subrect is
    // how an evaluation says which part of the supplied textures to read.
    set(
        parameters,
        "DLSS.Render.Subrect.Dimensions.Width",
        SET_U32,
        frame.render.0,
    );
    set(
        parameters,
        "DLSS.Render.Subrect.Dimensions.Height",
        SET_U32,
        frame.render.1,
    );
    set(parameters, "Reset", SET_I32, i32::from(frame.reset));

    // Markers either side of the call. The first evaluation ended the process
    // without returning a code, and these say whether control ever came back.
    static ENTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let first = !ENTERED.swap(true, Ordering::Relaxed);
    if first {
        crate::log!("[dls] entering EvaluateFeature");
    }
    let result = (api.evaluate)(list, handle, parameters, std::ptr::null());
    if first {
        crate::log!("[dls] EvaluateFeature returned {result:#010x}");
    }
    if list != native {
        // NGX set its own descriptor heaps through ReShade's proxy, and
        // ReShade remembers the heaps of every list it wraps. The caller
        // restores the engine's state on the native list underneath, which
        // ReShade does not see, so the engine's heaps are first set again the
        // way the engine set them.
        let restored = crate::reshade::restore_held_heaps(list, engines_heaps);
        static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !SAID.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[cmp] after NGX, {restored} of the engine's descriptor heaps were set again \
                 on its list through ReShade{}",
                if restored == 0 {
                    " (the engine had set none on this list)"
                } else {
                    ""
                }
            );
        }
    }
    if result != SUCCESS {
        // A failure that repeats does so every few frames.
        static SAID: crate::logger::Throttle = crate::logger::Throttle::new(5, 2, 60);
        crate::log_limited!(
            SAID,
            "[dls] EvaluateFeature -> {result:#010x}{}",
            crate::ngx::status(result)
        );
        return false;
    }
    true
}

/// Feature creations that have failed in a row.
static CREATE_FAILURES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// How many of those end the attempts for the session.
///
/// A creation is retried a few frames after it fails, so without a bound a
/// machine on which it cannot succeed created and destroyed a parameter
/// block, ran a real `CreateFeature` on the engine's command list and wrote
/// half a dozen log lines every fourth frame for as long as the game ran,
/// while the engine went on rendering small for a resolve that never came.
const CREATE_ATTEMPTS: u32 = 5;

/// Count a failed creation, and give up for the session at the limit.
fn creation_failed(result: u32) {
    let failures = CREATE_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
    if failures >= CREATE_ATTEMPTS {
        crate::gate::declare_unusable(&format!(
            "creating the DLSS feature failed {failures} times in a row, the last time with \
             {result:#010x}{}",
            crate::ngx::status(result)
        ));
    }
}

static SAID_NGX_LIST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static SAID_NO_HELD_LIST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The command list to hand NGX for a pass recorded on `native`.
///
/// NGX creates descriptor heaps and command objects on the device it holds
/// and uses them with the list it is given, so the two must come from the
/// same side of any proxy layer. With ReShade under Streamline, NGX's device
/// is ReShade's proxy -- Streamline initialised NGX with the device it was
/// given -- and NGX then called `SetDescriptorHeaps` with one of ReShade's
/// proxy heaps on the native list it had from here: an access violation
/// inside D3D12Core at the first DLSS frame of the second run in the game.
///
/// So: if the device NGX holds is a ReShade proxy, NGX gets the engine's list
/// as the engine holds it, which is the proxy `reshade::held_list` finds on
/// the call stack. If that proxy cannot be found the evaluation is declined
/// and says so, because a native list there is known to take the process
/// down and an unresolved frame is not.
///
/// Without ReShade this is one relaxed load and the argument back.
unsafe fn list_for_ngx(native: *mut c_void) -> Option<*mut c_void> {
    if !crate::reshade::present() {
        return Some(native);
    }
    // The device NGX holds: Streamline's when frame generation is on, since
    // its NGX context is the one created first, and otherwise the one the
    // game's own initialisation passed. With ReShade loaded early they are
    // the same proxy.
    let given = crate::streamline::given_device();
    let device = if given != 0 {
        given
    } else {
        crate::ngx::init_device()
    };
    let (unwrapped, device_is_proxy) = crate::reshade::unwrap(device as *mut c_void);
    if !device_is_proxy {
        return Some(native);
    }
    match crate::reshade::held_list(native) {
        Some(held) => {
            if !SAID_NGX_LIST.swap(true, Ordering::Relaxed) {
                crate::log!(
                    "[cmp] NGX holds the device {device:#x}, a ReShade proxy over {:#x}, so \
                     it is given the engine's list as the engine holds it, {:#x} ({}), and \
                     not the native {:#x} underneath",
                    unwrapped as usize,
                    held as usize,
                    crate::reshade::owner(held),
                    native as usize
                );
            }
            Some(held)
        }
        None => {
            if !SAID_NO_HELD_LIST.swap(true, Ordering::Relaxed) {
                crate::log!(
                    "[cmp] NGX holds the device {device:#x}, a ReShade proxy, and the proxy \
                     for the native list {:#x} is not on this thread's stack ({} dispatches \
                     have come through ReShade's list proxy so far). DLSS is NOT evaluated: \
                     a native list handed to NGX here is a crash inside D3D12Core, and an \
                     unresolved frame is not",
                    native as usize,
                    crate::reshade::proxy_dispatches()
                );
            }
            None
        }
    }
}

/// The render size the DLSS feature that exists now was created for.
///
/// This is what is being rendered, as of the last frame DLSS took: the
/// feature is rebuilt whenever the size changes.
pub fn created_for() -> Option<(u32, u32)> {
    if HANDLE.load(Ordering::Relaxed) == 0 {
        return None;
    }
    let size = *CREATED_FOR.lock();
    (size.0 != 0).then_some(size)
}

/// Whether NGX's entry points have been bound.
pub fn bound() -> bool {
    API.lock().is_some()
}

/// Every object about to be handed to NGX, with the module that implements it.
///
/// Written *before* `CreateFeature`, every time a feature is created, because
/// the one time this mattered the process died inside that call: an access
/// violation in D3D12Core, which is what handing D3D12 an object it did not
/// make looks like, and the log ended one line short of saying which. A
/// wrapped object here now shows as a module name that is not D3D12Core.dll,
/// on a line that exists whether or not the call returns.
///
/// The devices matter as much as the list. NGX creates its own heaps and
/// lists on the device it holds and then uses them with the list it is
/// given, so the two have to be on the same side of any proxy layer.
unsafe fn describe_objects(list: *mut c_void, frame: &Frame) {
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct3D12::{ID3D12Device, ID3D12GraphicsCommandList};
    let named = |pointer: *mut c_void| {
        format!(
            "{:#x} ({})",
            pointer as usize,
            crate::reshade::owner(pointer)
        )
    };
    let lists_device = ID3D12GraphicsCommandList::from_raw_borrowed(&list)
        .and_then(|list| {
            let mut device: Option<ID3D12Device> = None;
            list.GetDevice(&mut device).ok()?;
            device
        })
        .map(|device| named(device.as_raw()))
        .unwrap_or_else(|| "unknown".to_string());
    let ngx_device = crate::ngx::init_device();
    let streamline_device = crate::streamline::given_device();
    crate::log!(
        "[dls] before CreateFeature: list {}, whose device is {lists_device}; NGX was \
         initialised by this library with device {}; Streamline was given {}; colour {}, \
         depth {}, motion {}, output {}",
        named(list),
        if ngx_device == 0 {
            "none".to_string()
        } else {
            named(ngx_device as *mut c_void)
        },
        if streamline_device == 0 {
            "no device".to_string()
        } else {
            named(streamline_device as *mut c_void)
        },
        named(frame.colour),
        named(frame.depth),
        named(frame.motion),
        named(frame.output)
    );
}

unsafe fn create(api: &Api, list: *mut c_void, frame: &Frame) -> bool {
    release(api);

    let mut parameters: *mut c_void = std::ptr::null_mut();
    let result = (api.allocate)(&mut parameters);
    if result != SUCCESS || parameters.is_null() {
        crate::log!(
            "[dls] AllocateParameters -> {result:#010x}{}",
            crate::ngx::status(result)
        );
        creation_failed(result);
        return false;
    }

    // The slot numbers are the driver's object's. Another module's object is
    // its own class: OptiScaler's derives from the SDK's declaration and is
    // built by the same compiler, so the layout matches by construction, and
    // this is where that is checked rather than assumed. Whoever's object it
    // is, one that fails is not written through.
    if !verify_slots(parameters) {
        (api.destroy)(parameters);
        if crate::ngxhost::foreign() {
            let host = crate::ngxhost::answerer();
            crate::ngxhost::give_up(
                "an int and a float written to the parameter object it allocated did not read \
                 back through the driver's vtable slots",
            );
            crate::gate::declare_unusable(&format!(
                "the parameter object {host} allocated did not give back the values written to \
                 it through the vtable slots this mod uses, so nothing can safely be set on it"
            ));
        } else {
            crate::gate::declare_unusable(
                "NGX's parameter object did not give back the values written to it through the \
                 vtable slots this mod uses, so this driver lays that object out differently and \
                 nothing can safely be set on it",
            );
        }
        return false;
    }

    let settings = crate::config::get();
    set(parameters, "Width", SET_U32, frame.render.0);
    set(parameters, "Height", SET_U32, frame.render.1);
    set(parameters, "OutWidth", SET_U32, frame.display.0);
    set(parameters, "OutHeight", SET_U32, frame.display.1);
    let quality = preset_for(frame.render, frame.display);
    set(parameters, "PerfQualityValue", SET_I32, quality);
    set(parameters, "RTXValue", SET_I32, 0i32);
    set(
        parameters,
        "DLSS.Feature.Create.Flags",
        SET_I32,
        flags_for(FEATURE_FLAGS, frame),
    );

    // The preset is fixed when the feature is built. Each quality level has its
    // own hint and only the one in use matters, but setting them all costs
    // nothing and saves having to map the level back to a name here.
    let preset = match usable_preset(settings.dlss_preset) {
        Ok(preset) => preset,
        Err(why) => {
            if !PRESET_REFUSED.swap(true, Ordering::Relaxed) {
                crate::log!(
                    "[dls] dlss_preset = {} is ignored and the preset is left to the driver: \
                     {why}. The presets that can be asked for are 10 (J), 11 (K), 12 (L) and \
                     13 (M)",
                    settings.dlss_preset
                );
            }
            0
        }
    };
    if preset != 0 {
        for level in [
            "DLAA",
            "Quality",
            "Balanced",
            "Performance",
            "UltraPerformance",
            "UltraQuality",
        ] {
            set(
                parameters,
                &format!("DLSS.Hint.Render.Preset.{level}"),
                SET_U32,
                preset as u32,
            );
        }
    }

    describe_objects(list, frame);

    let mut handle: *mut c_void = std::ptr::null_mut();
    let result = (api.create)(list, FEATURE_SUPERSAMPLING, parameters, &mut handle);
    crate::log!(
        "[dls] CreateFeature {}x{} -> {}x{} quality={} flags={:#x} preset={} -> {result:#010x}",
        frame.render.0,
        frame.render.1,
        frame.display.0,
        frame.display.1,
        quality,
        flags_for(FEATURE_FLAGS, frame),
        // The preset that was set on the parameters, which is not always
        // the number in the settings.
        preset
    );
    if result != SUCCESS || handle.is_null() {
        (api.destroy)(parameters);
        // The driver is asked again on a later frame, five times in all.
        // Another module is not: one refusal and it is left alone, and DLSS
        // is given up for the session with it, so that the engine is taken
        // back to the whole window instead of rendering small for nothing.
        if crate::ngxhost::foreign() {
            let host = crate::ngxhost::answerer();
            let said = format!(
                "CreateFeature returned {result:#010x}{} with handle {:#x}",
                crate::ngx::status(result),
                handle as usize
            );
            crate::ngxhost::give_up(&format!("its {said}"));
            crate::gate::declare_unusable(&format!(
                "NGX is served by {host}, whose {said}; a module that is not the driver's is \
                 asked once"
            ));
        } else {
            creation_failed(result);
        }
        return false;
    }
    CREATE_FAILURES.store(0, Ordering::Relaxed);
    if crate::ngxhost::foreign() {
        crate::ngxhost::note_feature(handle, get_i32(parameters, "OptiScaler"));
    }

    PARAMETERS.store(parameters as usize, Ordering::Relaxed);
    HANDLE.store(handle as usize, Ordering::Relaxed);
    *CREATED_FOR.lock() = frame.render;
    *CREATED_WITH.lock() = (
        quality,
        flags_for(FEATURE_FLAGS, frame),
        settings.dlss_preset,
    );
    true
}

unsafe fn release(api: &Api) {
    let handle = HANDLE.swap(0, Ordering::Relaxed);
    if handle != 0 {
        (api.release)(handle as *mut c_void);
    }
    let parameters = PARAMETERS.swap(0, Ordering::Relaxed);
    if parameters != 0 {
        (api.destroy)(parameters as *mut c_void);
    }
}

#[cfg(test)]
mod tests {
    use super::usable_preset;

    #[test]
    fn only_the_presets_the_header_gives_a_meaning_are_passed_on() {
        assert_eq!(usable_preset(0), Ok(0));
        for preset in 10..=13 {
            assert_eq!(usable_preset(preset), Ok(preset));
        }
        for preset in (1..=9).chain(14..=20).chain([-1]) {
            assert!(usable_preset(preset).is_err(), "{preset} should be refused");
        }
    }
}
