//! A stand-in for the game, for testing the mod with companion tools without
//! spending a launch of the game on it.
//!
//! It does what the engine's renderer start-up does, in the engine's order
//! and through the same calls, so the mod's hooks and anything layered above
//! them (ReShade, its add-ons, Streamline) meet what they would meet in the
//! game:
//!
//! 1. load `dxgi.dll` and `d3d12.dll` with `LOAD_LIBRARY_SEARCH_SYSTEM32`;
//! 2. resolve `CreateDXGIFactory2` and `D3D12CreateDevice` with
//!    `GetProcAddress`, through this executable's import table, which is what
//!    the mod patches;
//! 3. a factory, then a device, then a queue;
//! 4. a root signature and a **compute pipeline whose shader reflects like the
//!    engine's temporal pass** (`tInput`, `tHistory`, `rwOuput`), created with
//!    `ID3D12Device::CreateComputePipelineState` on whatever device it was
//!    handed, as the engine does;
//! 5. a swapchain with the game's flags (0x842), and a few presents;
//! 6. a bare-name `LoadLibrary("DINPUT8.dll")`, as the game does later.
//!
//! Then it reads the mod's own log and **checks that the mod recognised the
//! pipeline**. That check is the reason step 4 exists: with an add-on that
//! listens for pipeline events loaded, ReShade creates the engine's
//! pipelines through a different native call, and a stand-in with no
//! pipeline in it passes while a mod that misses that call fails in the
//! game.
//!
//! What it is not: the game. No protection stub, no engine detours, one
//! thread. A pass here means the plumbing meets; it does not mean a picture.
//!
//! The mod knows this program by its file name. In any other executable
//! that is not the game it stands down and does nothing, and this program
//! renamed is such an executable: that is how to see a stand-down here. Under
//! its own name the mod gives it the render hooks and NGX, and no engine
//! detour or setting, there being no engine.
//!
//! The DLSS frame needs NGX to find a DLSS library, and where it looks
//! depends on the configuration: with `fg = 1` Streamline's NGX finds the
//! one in `mhw-dlss\`; with `fg = 0` NGX only looks beside `mhw-dlss.dll`
//! itself. That is why the mod's library is installed in `mhw-dlss\`, where
//! the DLSS library is: measured here, `fg = 0` with both in that folder
//! evaluates a frame, and with `mhw-dlss.dll` beside this program instead
//! the driver reports
//! `SuperSampling.Available = 0`, `FeatureNotFound`. Without a DLSS library
//! NGX can find, the mod says DLSS is not used and this program reports FAIL
//! for the frame, which is then the correct outcome.
//!
//! To use it, make a folder laid out as the game's is:
//!
//! ```text
//!   standin.exe
//!   mhw-dlss\mhw-dlss-launcher.exe  mhw-dlss\mhw-dlss.dll
//!   mhw-dlss\nvngx_dlss.dll
//!   mhw-dlss\sl.*.dll, nvngx_dlssg.dll  (optional; with fg = 1 in mhw-dlss.ini)
//!   mhw-dlss\ReShade64.dll              (optional; `ReShade64.dll  renderer` in the chain)
//!   mhw-dlss\renodx-mhworld.addon64     (optional; makes ReShade take the stream path)
//! ```
//!
//! and run `mhw-dlss\mhw-dlss-launcher.exe --wait <folder>\standin.exe`.
//! The launcher writes `mhw-dlss-loader.ini` in `mhw-dlss\` the first time,
//! and the mod `mhw-dlss.ini` and `mhw-dlss.log`, all in that folder. The
//! verdict is the last line of `standin.result` beside this program, `PASS`
//! or `FAIL`.

use std::ffi::c_void;
use std::io::Write;
use windows::core::{s, w, Interface, GUID, HRESULT, PCSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Reflects like `CSTemporalUpsample`: the three names the mod recognises the
/// pass by, spelling mistake included.
const SHADER: &str = "
Texture2D<float4>   tInput   : register(t0);
Texture2D<float4>   tHistory : register(t1);
RWTexture2D<float4> rwOuput  : register(u0);
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    rwOuput[id.xy] = tInput[id.xy] * 0.5 + tHistory[id.xy] * 0.5;
}
";

/// The mod's log: in its own folder, or beside this program when the mod's
/// library was put there.
fn mod_log(folder: &std::path::Path) -> std::path::PathBuf {
    let in_its_folder = folder.join("mhw-dlss").join("mhw-dlss.log");
    if in_its_folder.exists() {
        in_its_folder
    } else {
        folder.join("mhw-dlss.log")
    }
}

fn main() {
    let folder = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let mut result = std::fs::File::create(folder.join("standin.result")).unwrap();
    let mut say = |line: String| {
        println!("{line}");
        let _ = writeln!(result, "{line}");
    };

    unsafe {
        // The mod's own bootstrap runs on a thread ten milliseconds after it
        // is injected; the game takes far longer than that to get here.
        std::thread::sleep(std::time::Duration::from_millis(1500));

        let dxgi = LoadLibraryExW(w!("dxgi.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).unwrap();
        let d3d12 = LoadLibraryExW(w!("d3d12.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).unwrap();

        type CreateFactory =
            unsafe extern "system" fn(u32, *const GUID, *mut *mut c_void) -> HRESULT;
        let create_factory: CreateFactory =
            std::mem::transmute(GetProcAddress(dxgi, s!("CreateDXGIFactory2")).unwrap());
        let mut raw = std::ptr::null_mut();
        create_factory(0, &IDXGIFactory2::IID, &mut raw)
            .ok()
            .unwrap();
        let factory = IDXGIFactory2::from_raw(raw);
        say(format!("factory {:p}", factory.as_raw()));

        type CreateDevice =
            unsafe extern "system" fn(*mut c_void, i32, *const GUID, *mut *mut c_void) -> HRESULT;
        let create_device: CreateDevice =
            std::mem::transmute(GetProcAddress(d3d12, s!("D3D12CreateDevice")).unwrap());
        let mut raw = std::ptr::null_mut();
        create_device(
            std::ptr::null_mut(),
            D3D_FEATURE_LEVEL_11_0.0,
            &ID3D12Device::IID,
            &mut raw,
        )
        .ok()
        .unwrap();
        let device = ID3D12Device::from_raw(raw);
        say(format!("device {:p}", device.as_raw()));

        let queue: ID3D12CommandQueue = device
            .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })
            .unwrap();
        say(format!("queue {:p}", queue.as_raw()));

        // The pipeline, the way the engine makes its own.
        let mut code: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        // STANDIN_PLAIN_SHADER=1 gives the shader names the mod does not
        // recognise, to see what the mod says when no temporal pass exists.
        let source = if std::env::var_os("STANDIN_PLAIN_SHADER").is_some() {
            SHADER
                .replace("tInput", "first")
                .replace("tHistory", "second")
                .replace("rwOuput", "third")
        } else {
            SHADER.to_string()
        };
        let compiled = D3DCompile(
            source.as_ptr() as *const c_void,
            source.len(),
            PCSTR::null(),
            None,
            None,
            s!("main"),
            s!("cs_5_0"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        );
        if let Err(error) = compiled {
            say(format!("the shader did not compile: {error}"));
        }
        let code = code.expect("compiled shader");

        let ranges = [
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
                NumDescriptors: 2,
                BaseShaderRegister: 0,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: 0,
            },
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
                NumDescriptors: 1,
                BaseShaderRegister: 0,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: 2,
            },
        ];
        let parameters = [D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                    NumDescriptorRanges: ranges.len() as u32,
                    pDescriptorRanges: ranges.as_ptr(),
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        }];
        let mut blob: Option<ID3DBlob> = None;
        D3D12SerializeRootSignature(
            &D3D12_ROOT_SIGNATURE_DESC {
                NumParameters: parameters.len() as u32,
                pParameters: parameters.as_ptr(),
                ..Default::default()
            },
            D3D_ROOT_SIGNATURE_VERSION_1_0,
            &mut blob,
            None,
        )
        .unwrap();
        let blob = blob.unwrap();
        let signature: ID3D12RootSignature = device
            .CreateRootSignature(
                0,
                std::slice::from_raw_parts(
                    blob.GetBufferPointer() as *const u8,
                    blob.GetBufferSize(),
                ),
            )
            .unwrap();
        let pipeline: windows::core::Result<ID3D12PipelineState> = device
            .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&signature),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.GetBufferPointer(),
                    BytecodeLength: code.GetBufferSize(),
                },
                ..Default::default()
            });
        match &pipeline {
            Ok(pipeline) => say(format!("compute pipeline {:p}", pipeline.as_raw())),
            Err(error) => say(format!("compute pipeline failed: {error}")),
        }

        let window: HWND = CreateWindowExW(
            Default::default(),
            w!("STATIC"),
            w!("mhw-dlss stand-in"),
            WS_OVERLAPPEDWINDOW,
            0,
            0,
            640,
            360,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let description = DXGI_SWAP_CHAIN_DESC1 {
            Width: 640,
            Height: 360,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 3,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            // What the game asks for: waitable object, mode switch, tearing.
            Flags: (DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0
                | DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH.0
                | DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0) as u32,
            ..Default::default()
        };
        let chain = factory.CreateSwapChainForHwnd(&queue, window, &description, None, None);
        match &chain {
            Ok(chain) => say(format!("swapchain {:p}", chain.as_raw())),
            Err(error) => say(format!("swapchain failed: {error}")),
        }
        // One dispatch of that pipeline, recorded and submitted the way the
        // engine records its own: descriptor heap, views, heaps set, root
        // signature, table, pipeline, dispatch, close, execute. Each of those
        // is a method the mod patches on the native object, called here on
        // whatever the layers above handed out, so a layer that re-routes one
        // of them shows up as a count that stays at zero. The views are null
        // descriptors, which D3D12 defines: reads give zero, writes go nowhere.
        let mut dispatched = false;
        if let Ok(pipeline) = &pipeline {
            let heap: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 3,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .unwrap();
            let step = device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                as usize;
            let base = heap.GetCPUDescriptorHandleForHeapStart();
            let at = |index: usize| D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: base.ptr + index * step,
            };
            let texture = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D12_TEX2D_SRV {
                        MipLevels: 1,
                        ..Default::default()
                    },
                },
            };
            device.CreateShaderResourceView(None, Some(&texture), at(0));
            device.CreateShaderResourceView(None, Some(&texture), at(1));
            device.CreateUnorderedAccessView(
                None,
                None,
                Some(&D3D12_UNORDERED_ACCESS_VIEW_DESC {
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
                    ..Default::default()
                }),
                at(2),
            );
            let allocator: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .unwrap();
            let list: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                .unwrap();
            list.SetDescriptorHeaps(&[Some(heap.clone())]);
            list.SetComputeRootSignature(&signature);
            list.SetComputeRootDescriptorTable(0, heap.GetGPUDescriptorHandleForHeapStart());
            list.SetPipelineState(pipeline);
            list.Dispatch(1, 1, 1);
            list.Close().unwrap();
            queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
            dispatched = true;
            say("dispatched the pipeline once".to_string());
            // Left alive on purpose until the process exits: the queue has
            // not been waited on.
            std::mem::forget((heap, allocator, list));
        }

        // One DLSS frame, with NGX initialised the way the game initialises
        // it and after the swapchain exists, which is the game's order:
        // Streamline has by then set up its own NGX context on the device it
        // was given.
        let dlss_wanted = std::env::var_os("STANDIN_NO_DLSS").is_none();
        let mut dlss_outcome: Option<u32> = None;
        if dlss_wanted {
            match &pipeline {
                Ok(pipeline) => {
                    dlss_outcome =
                        dlss_frame(&device, &queue, &signature, pipeline, &folder, &mut say)
                }
                Err(_) => say("no pipeline to bind for the DLSS frame".to_string()),
            }
        }

        // Past six hundred presents, because that is when the mod writes its
        // account of what its hooks saw.
        if let Ok(chain) = &chain {
            for _ in 0..620 {
                let _ = chain.Present(0, DXGI_PRESENT(0));
            }
        }

        let input = LoadLibraryW(w!("DINPUT8.dll")).unwrap_or_default();
        let mut buffer = [0u16; 300];
        let length = GetModuleFileNameW(input, &mut buffer) as usize;
        say(format!(
            "DINPUT8.dll -> {}",
            String::from_utf16_lossy(&buffer[..length])
        ));

        std::thread::sleep(std::time::Duration::from_millis(1000));

        // The verdict, from the mod's own account of what it saw.
        let log = std::fs::read_to_string(mod_log(&folder)).unwrap_or_default();
        let mut failures: Vec<&str> = Vec::new();
        if log.is_empty() {
            failures.push("there is no mhw-dlss.log: the mod was not injected");
        }
        if !log.contains("[pso] TemporalUpsample") {
            failures.push(
                "the mod did not recognise the compute pipeline (no `[pso] TemporalUpsample` line): \
                 its pipeline hooks did not see the creation",
            );
        }
        if !log.contains("[stk] swapchain 0") {
            failures.push("the mod did not report a swapchain (no `[stk] swapchain 0` line)");
        }
        if pipeline.is_err() || chain.is_err() {
            failures.push("a creation call failed");
        }
        // The mod's own count of what its command-list hooks saw.
        let saw = log
            .lines()
            .rev()
            .find(|line| line.starts_with("[gpu] hooks saw "));
        let count_before = |line: &str, word: &str| -> Option<u64> {
            let end = line.find(word)?;
            line[..end].trim_end().rsplit(' ').next()?.parse().ok()
        };
        match saw {
            None => failures.push("the mod never wrote its `[gpu] hooks saw` line"),
            Some(line) => {
                say(format!("  mod: {line}"));
                if dispatched && count_before(line, " dispatches") == Some(0) {
                    failures.push("the mod's Dispatch hook did not see the dispatch");
                }
                if dispatched && count_before(line, " list closes") == Some(0) {
                    failures.push("the mod's Close hook did not see the list close");
                }
            }
        }
        if log.contains("NO TEMPORAL PASS RECOGNISED") {
            failures.push("the mod says no temporal pass was recognised");
        }
        if dlss_wanted {
            match dlss_outcome {
                None => failures.push("the DLSS frame could not be set up (see above)"),
                Some(outcome) => {
                    if outcome & 1 == 0 {
                        failures.push("the mod's motion vector decode did not run");
                    }
                    if outcome & 2 == 0 {
                        failures.push("the mod's DLSS evaluation did not succeed");
                    }
                }
            }
            if !log.lines().any(|line| {
                line.starts_with("[dls] CreateFeature") && line.ends_with("-> 0x00000001")
            }) {
                failures.push("no `[dls] CreateFeature ... -> 0x00000001` line in the mod's log");
            }
            if device.GetDeviceRemovedReason().is_err() {
                failures.push("the device was removed");
            }
        }
        for line in log.lines().filter(|line| {
            line.starts_with("[stk]")
                || line.starts_with("[pso] ")
                || line.starts_with("[cmp] settings page")
                || line.starts_with("[gpu] NO TEMPORAL")
                || line.starts_with("[dls] ")
                || line.starts_with("[tst] ")
                || line.starts_with("[cmp] ")
                || line.starts_with("[ngx] Init")
        }) {
            say(format!(
                "  mod: {}",
                line.chars().take(260).collect::<String>()
            ));
        }
        if failures.is_empty() {
            say("PASS".to_string());
        } else {
            for failure in &failures {
                say(format!("  {failure}"));
            }
            say("FAIL".to_string());
        }
        std::process::exit(if failures.is_empty() { 0 } else { 3 });
    }
}

/// The driver's NGX loader, where the game finds it.
fn driver_ngx() -> Option<std::path::PathBuf> {
    if let Some(given) = std::env::var_os("STANDIN_NGX") {
        return Some(given.into());
    }
    let store = std::path::PathBuf::from(std::env::var_os("SystemRoot")?)
        .join("System32")
        .join("DriverStore")
        .join("FileRepository");
    let mut found: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(store).ok()?.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .to_ascii_lowercase()
            .starts_with("nv_dispi")
        {
            continue;
        }
        let candidate = entry.path().join("nvngx.dll");
        if let Ok(metadata) = std::fs::metadata(&candidate) {
            found.push((metadata.modified().ok()?, candidate));
        }
    }
    found.sort();
    found.pop().map(|(_, path)| path)
}

unsafe fn texture(
    device: &ID3D12Device,
    size: (u32, u32),
    format: DXGI_FORMAT,
    flags: D3D12_RESOURCE_FLAGS,
    state: D3D12_RESOURCE_STATES,
) -> ID3D12Resource {
    let mut resource: Option<ID3D12Resource> = None;
    device
        .CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            },
            D3D12_HEAP_FLAG_NONE,
            &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: size.0 as u64,
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
        .unwrap();
    resource.unwrap()
}

/// What `mhw_dlss_selftest_frame` takes; see `src/selftest.rs` in the mod.
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

/// Initialise NGX as the game does and ask the mod for one DLSS frame on a
/// list made by whatever device this program was handed.
///
/// The game's own call, from the mod's log of it: application id 100182111,
/// a wide string, the device, API version 18, 42567, null. The mod stands in
/// for that entry point through its `GetProcAddress` hook, exactly as it does
/// in the game.
unsafe fn dlss_frame(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    signature: &ID3D12RootSignature,
    pipeline: &ID3D12PipelineState,
    folder: &std::path::Path,
    say: &mut dyn FnMut(String),
) -> Option<u32> {
    let Some(path) = driver_ngx() else {
        say("no nvngx.dll in the driver store; set STANDIN_NGX to its path".to_string());
        return None;
    };
    let wide: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let ngx = match LoadLibraryExW(
        windows::core::PCWSTR(wide.as_ptr()),
        None,
        Default::default(),
    ) {
        Ok(ngx) => ngx,
        Err(error) => {
            say(format!("could not load {}: {error}", path.display()));
            return None;
        }
    };
    type Init = unsafe extern "system" fn(u64, *const u16, *mut c_void, usize, usize, usize) -> u32;
    let Some(init) = GetProcAddress(ngx, s!("NVSDK_NGX_D3D12_Init")) else {
        say("nvngx.dll has no NVSDK_NGX_D3D12_Init".to_string());
        return None;
    };
    let init: Init = std::mem::transmute(init);
    let result = init(
        100182111,
        w!("DLSS").as_ptr(),
        device.as_raw(),
        18,
        42567,
        0,
    );
    say(format!("NVSDK_NGX_D3D12_Init -> {result:#010x}"));

    // The mod binds NGX's feature entry points on a thread of its own once
    // the initialisation has been seen.
    let log_path = mod_log(&folder);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        if log.contains("[ngx] feature entry points bound") {
            break;
        }
        if std::time::Instant::now() > deadline {
            say("the mod never bound NGX's feature entry points".to_string());
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let ours = GetModuleHandleW(w!("mhw-dlss.dll")).ok()?;
    type Selftest = unsafe extern "system" fn(*mut c_void, *const SelftestFrame) -> u32;
    let Some(selftest) = GetProcAddress(ours, s!("mhw_dlss_selftest_frame")) else {
        say("this build of the mod has no mhw_dlss_selftest_frame export".to_string());
        return None;
    };
    let selftest: Selftest = std::mem::transmute(selftest);

    let render = (1280u32, 720u32);
    let display = (1920u32, 1080u32);
    let readable = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
    let colour = texture(
        device,
        render,
        DXGI_FORMAT_R11G11B10_FLOAT,
        D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        readable,
    );
    let depth = texture(
        device,
        render,
        DXGI_FORMAT_R32_TYPELESS,
        D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
        readable,
    );
    let motion = texture(
        device,
        render,
        DXGI_FORMAT_R32_UINT,
        D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        readable,
    );
    let output = texture(
        device,
        display,
        DXGI_FORMAT_R11G11B10_FLOAT,
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
    );

    let allocator: ID3D12CommandAllocator = device
        .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
        .unwrap();
    let list: ID3D12GraphicsCommandList = device
        .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
        .unwrap();
    say(format!(
        "asking the mod for a DLSS frame on the list {:p}",
        list.as_raw()
    ));
    let frame = SelftestFrame {
        colour: colour.as_raw(),
        depth: depth.as_raw(),
        motion: motion.as_raw(),
        output: output.as_raw(),
        render: [render.0, render.1],
        display: [display.0, display.1],
        flags: 1,
    };
    // The list in the state the engine's is in when its temporal pass comes
    // round: its own descriptor heap set, root signature, table and pipeline
    // bound. The mod has to leave that heap bound when it is done.
    let heap: ID3D12DescriptorHeap = device
        .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            NumDescriptors: 3,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            NodeMask: 0,
        })
        .unwrap();
    let step =
        device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
    let base = heap.GetCPUDescriptorHandleForHeapStart();
    let at = |index: usize| D3D12_CPU_DESCRIPTOR_HANDLE {
        ptr: base.ptr + index * step,
    };
    let view = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MipLevels: 1,
                ..Default::default()
            },
        },
    };
    device.CreateShaderResourceView(None, Some(&view), at(0));
    device.CreateShaderResourceView(None, Some(&view), at(1));
    device.CreateUnorderedAccessView(
        None,
        None,
        Some(&D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
            ..Default::default()
        }),
        at(2),
    );
    list.SetDescriptorHeaps(&[Some(heap.clone())]);
    list.SetComputeRootSignature(signature);
    list.SetComputeRootDescriptorTable(0, heap.GetGPUDescriptorHandleForHeapStart());
    list.SetPipelineState(pipeline);

    // With a layer above the mod the frame is only armed by this call, and
    // runs from inside the dispatch that follows, which is where the game's
    // runs. Without one it runs at once. Either way the dispatch is then made
    // with the heap this program set, so a heap left unrestored shows.
    let mut outcome = selftest(list.as_raw(), &frame);
    say(format!(
        "the mod answered {outcome:#x} to the request (4 means armed for the dispatch)"
    ));
    list.Dispatch(1, 1, 1);
    if outcome == 4 {
        type SelftestResult = unsafe extern "system" fn() -> u32;
        if let Some(result) = GetProcAddress(ours, s!("mhw_dlss_selftest_result")) {
            let result: SelftestResult = std::mem::transmute(result);
            outcome = result();
        }
    }
    say(format!(
        "the mod returned {outcome:#x} (1 decoded, 2 evaluated)"
    ));
    std::mem::forget(heap);
    list.Close().unwrap();
    queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);

    // Wait for the GPU to have run it, so a fault it causes happens here and
    // not at some later, unrelated call.
    let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
    queue.Signal(&fence, 1).unwrap();
    let _ = fence.SetEventOnCompletion(1, None);
    say(format!(
        "the GPU finished the DLSS frame; device removed reason {:?}",
        device.GetDeviceRemovedReason()
    ));
    std::mem::forget((colour, depth, motion, output, allocator, list, fence));
    Some(outcome)
}
