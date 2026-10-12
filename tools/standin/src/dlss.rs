//! One DLSS frame through the mod, with NGX initialised the way the game
//! initialises it.
//!
//! The mod stands in for `NVSDK_NGX_D3D12_Init` through its `GetProcAddress`
//! hook, exactly as in the game. The frame itself needs the mod's selftest
//! export (below); a build without it skips the frame and says so.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::core::{s, w, Interface, PCWSTR};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryExW};

use crate::scene::{self, Scene};
use crate::{context, Result, Say};

/// What `mhw_dlss_selftest_frame(list, &SelftestFrame) -> u32` takes. The
/// answer is a bit set: 1 motion vectors decoded, 2 DLSS evaluated, 4 armed
/// (a layer above the mod defers the frame to the next dispatch; then
/// `mhw_dlss_selftest_result() -> u32` gives the bits).
#[repr(C)]
struct SelftestFrame {
    colour: *mut c_void,
    depth: *mut c_void,
    motion: *mut c_void,
    output: *mut c_void,
    render: [u32; 2],
    display: [u32; 2],
    flags: u32,
}

/// Bits of the selftest answer.
pub const DECODED: u32 = 1;
/// See [`DECODED`].
pub const EVALUATED: u32 = 2;
const ARMED: u32 = 4;

/// The game's arguments, from the mod's log of its call: application id
/// 0x5f8a85f, `L"DLSS"`, the device, API version 0x12, then two stack words.
const APP_ID: u64 = 0x05F8_A85F;
const API_VERSION: usize = 0x12;
const STACK_WORDS: [usize; 2] = [42567, 0];

/// Run the frame. `Ok(None)`: this build of the mod has no selftest export.
pub fn frame(
    scene: &Scene,
    pipeline: &ID3D12PipelineState,
    folder: &Path,
    say: &mut Say<'_>,
) -> Result<Option<u32>> {
    let path =
        driver_ngx().ok_or("no nvngx.dll in the driver store; set STANDIN_NGX to its path")?;
    let wide: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    type Init = unsafe extern "system" fn(u64, *const u16, *mut c_void, usize, usize, usize) -> u32;
    // SAFETY: a NUL-terminated path; the entry point is transmuted to the
    // six-word shape the game calls it with (`mhw_ngx::init::GameInit`).
    let result = unsafe {
        let ngx = context(
            LoadLibraryExW(PCWSTR(wide.as_ptr()), None, Default::default()),
            "load nvngx.dll",
        )?;
        let init = GetProcAddress(ngx, s!("NVSDK_NGX_D3D12_Init"))
            .ok_or("nvngx.dll has no NVSDK_NGX_D3D12_Init")?;
        let init: Init = std::mem::transmute(init);
        init(
            APP_ID,
            w!("DLSS").as_ptr(),
            scene.device.as_raw(),
            API_VERSION,
            STACK_WORDS[0],
            STACK_WORDS[1],
        )
    };
    say(format!("NVSDK_NGX_D3D12_Init -> {result:#010x}"));
    wait_for_ngx(folder)?;

    // SAFETY: looking up exports of a loaded module by name.
    let (selftest, result_of) = unsafe {
        let ours = context(GetModuleHandleW(w!("mhw-dlss.dll")), "find mhw-dlss.dll")?;
        (
            GetProcAddress(ours, s!("mhw_dlss_selftest_frame")),
            GetProcAddress(ours, s!("mhw_dlss_selftest_result")),
        )
    };
    let Some(selftest) = selftest else {
        say(
            "this build of the mod has no mhw_dlss_selftest_frame export: DLSS frame skipped"
                .to_owned(),
        );
        return Ok(None);
    };
    type Selftest = unsafe extern "system" fn(*mut c_void, *const SelftestFrame) -> u32;
    type SelftestResult = unsafe extern "system" fn() -> u32;
    // SAFETY: the export's documented signature.
    let selftest: Selftest = unsafe { std::mem::transmute(selftest) };

    let render = (1280u32, 720u32);
    let display = (1920u32, 1080u32);
    let readable = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
    let colour = texture(
        scene,
        render,
        DXGI_FORMAT_R11G11B10_FLOAT,
        D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        readable,
    )?;
    let depth = texture(
        scene,
        render,
        DXGI_FORMAT_R32_TYPELESS,
        D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
        readable,
    )?;
    let motion = texture(
        scene,
        render,
        DXGI_FORMAT_R32_UINT,
        D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        readable,
    )?;
    let output = texture(
        scene,
        display,
        DXGI_FORMAT_R11G11B10_FLOAT,
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
    )?;

    // The list in the state the engine's is in when its temporal pass comes
    // round. The mod has to leave that heap bound when it is done.
    let heap = scene::null_table(&scene.device)?;
    let (allocator, list) = scene::bound_list(scene, pipeline, &heap)?;
    say(format!(
        "asking the mod for a DLSS frame on the list {:p}",
        list.as_raw()
    ));
    let request = SelftestFrame {
        colour: colour.as_raw(),
        depth: depth.as_raw(),
        motion: motion.as_raw(),
        output: output.as_raw(),
        render: [render.0, render.1],
        display: [display.0, display.1],
        flags: 1,
    };
    // With a layer above the mod the frame is only armed by this call and
    // runs inside the dispatch that follows, which is where the game's runs.
    // Either way the dispatch is then made with the heap this program set,
    // so a heap left unrestored shows.
    // SAFETY: a recording list and live resources in the documented states.
    let mut outcome = unsafe { selftest(list.as_raw(), &request) };
    say(format!(
        "the mod answered {outcome:#x} ({ARMED:#x} means armed for the dispatch)"
    ));
    // SAFETY: as above.
    unsafe { list.Dispatch(1, 1, 1) };
    if outcome == ARMED {
        if let Some(result_of) = result_of {
            // SAFETY: the export's documented signature.
            outcome = unsafe {
                std::mem::transmute::<unsafe extern "system" fn() -> isize, SelftestResult>(
                    result_of,
                )()
            };
        }
    }
    say(format!(
        "the mod returned {outcome:#x} ({DECODED} decoded, {EVALUATED} evaluated)"
    ));
    scene::submit(scene, &list)?;

    // Wait for the GPU, so a fault the frame causes happens here and not at
    // some later, unrelated call.
    // SAFETY: a live device and queue; a fresh fence waited on to 1.
    unsafe {
        let fence: ID3D12Fence = context(
            scene.device.CreateFence(0, D3D12_FENCE_FLAG_NONE),
            "CreateFence",
        )?;
        context(scene.queue.Signal(&fence, 1), "Signal")?;
        let _ = fence.SetEventOnCompletion(1, None);
        say(format!(
            "the GPU finished the DLSS frame; device removed reason {:?}",
            scene.device.GetDeviceRemovedReason()
        ));
    }
    std::mem::forget((colour, depth, motion, output, allocator, list, heap));
    Ok(Some(outcome))
}

/// Wait for the mod to say NGX is ready (it binds NGX's entry points on its
/// own thread once it has seen the initialisation).
fn wait_for_ngx(folder: &Path) -> Result<()> {
    let log = crate::mod_log(folder);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.lines().any(|line| line.contains("[ngx] NGX is ready")) {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err("the mod never said `[ngx] NGX is ready`".to_owned());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The driver's NGX loader, where the game finds it: `STANDIN_NGX`, or the
/// newest `nv_dispi*` folder of the driver store.
fn driver_ngx() -> Option<PathBuf> {
    if let Some(given) = std::env::var_os("STANDIN_NGX") {
        return Some(given.into());
    }
    let store = PathBuf::from(std::env::var_os("SystemRoot")?)
        .join("System32")
        .join("DriverStore")
        .join("FileRepository");
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(store)
        .ok()?
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .starts_with("nv_dispi")
        })
        .filter_map(|entry| {
            let candidate = entry.path().join("nvngx.dll");
            let modified = std::fs::metadata(&candidate).ok()?.modified().ok()?;
            Some((modified, candidate))
        })
        .collect();
    found.sort();
    found.pop().map(|(_, path)| path)
}

fn texture(
    scene: &Scene,
    size: (u32, u32),
    format: DXGI_FORMAT,
    flags: D3D12_RESOURCE_FLAGS,
    state: D3D12_RESOURCE_STATES,
) -> Result<ID3D12Resource> {
    let mut resource: Option<ID3D12Resource> = None;
    // SAFETY: a live device, valid descriptions and out-pointer.
    context(
        unsafe {
            scene.device.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                    Width: u64::from(size.0),
                    Height: size.1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: format,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Flags: flags,
                    ..Default::default()
                },
                state,
                None,
                &mut resource,
            )
        },
        "CreateCommittedResource",
    )?;
    resource.ok_or_else(|| "CreateCommittedResource returned nothing".to_owned())
}
