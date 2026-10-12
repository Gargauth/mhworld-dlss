# mhw-fg

DLSS frame generation (DLSS-G) for Monster Hunter: World through NVIDIA
Streamline 2.14.1: the hand-declared C ABI, loading and initialising
Streamline, the swapchain interception, `sl::Constants` from the engine's
camera, resource tags, Reflex/PCL markers with frame-token discipline, and
pacing. Contract: `docs/ARCHITECTURE.md` §mhw-fg; RE facts:
`docs/re/ReFramePresent.md`, `docs/re/ReJitterCamera.md`.

Depends on `mhw-core` and `mhw-win` only. Policy (whether frame generation
runs, multiplier, OTA, signature check, wait latch, sleep point, frame
limit) comes from the mod as parameters; this crate reads no settings.

## Layout

| Module | Host-tested | What |
|---|---|---|
| `abi` | yes | `#[repr(C)]` structures, GUIDs, versions and enums of the v2.14.1 headers; tests pin every offset Streamline reads |
| `constants` | yes | `sl::Constants` from `CameraFrame` + `JitterSample` via `mhw_core::math` |
| `token` | yes | frame-token / marker state machine (`FrameLoop`) |
| `options` | yes | `Options`, per-frame DLSS-G/Reflex requests, send-on-change (`Sent`), resize hold |
| `pacing` | yes | sleep point, Reflex frame limit, wait-latch policy |
| `setup` | yes | plugin folder (+ `nvngx_dlisp.dll` refusal), `dlisp_beside` (the snippet beside a foreign NGX host, for `mod`'s warning), identity route, preference flags, signer policy, Streamline log thinning |
| `tags` | yes | tag kinds, explicit-state resources, `ResourceTag` building |
| `status` | yes | read-only introspection types |
| `sl` | Windows | `sl.interposer.dll`, `slInit` thread, every call into Streamline |
| `trust` | Windows | Authenticode check of the interposer |
| `swap` | Windows | factory / swapchain hooks |
| `frame` | Windows | per-frame glue |

## Public API (Windows; pure modules are public too)

```rust
// Startup (boot thread, only when [FrameGeneration] Enabled = true)
pub struct StartConfig { pub mod_folder: PathBuf, pub identity: Identity,
    pub allow_ota: bool, pub verify_signature: bool, pub log_level: SlLogLevel }
pub fn start(config: StartConfig) -> bool;          // false: already started
pub fn wait_settled(timeout: Duration) -> bool;
pub fn init_state() -> InitState;
pub fn ready() -> bool;
pub fn faulted() -> bool;                           // slInit/slSetD3DDevice raised
pub fn stand_down();                                // gate failure
pub fn shutdown();                                  // process exit

// Factory / swapchain
pub unsafe fn upgrade_factory(factory: *mut c_void) -> bool;
pub fn chain() -> Option<ChainInfo>;
pub fn owns_swapchain() -> bool;
pub fn wait_latch(policy: mhw_core::config::WaitLatch) -> bool;
pub fn wait_latch_wanted(policy: WaitLatch, fg_owns_swapchain: bool) -> bool; // pure

// Options (live, any thread; applied at the next Present)
pub fn set_options(options: Options);
pub fn options() -> Options;
pub fn note_reset();

// Per frame
pub fn frame_start();                               // SleepPoint::IterationStart only
pub fn render_submit();
pub unsafe fn submit_frame(inputs: &FrameInputs, list: *mut c_void) -> Submitted;
pub unsafe fn tag_late(kind: LateKind, resource: Option<TagResource>, frame: u64) -> bool;

// Introspection
pub fn status() -> Status;
#[cfg(feature = "dev")] pub fn last_constants() -> Option<abi::Constants>;
```

`FrameInputs { camera: CameraFrame, jitter: JitterSample, reset: bool,
depth: Option<TagResource>, motion_vectors: Option<TagResource>,
mvec_pixel_scale: Vec2 }`; `TagResource { resource: *mut c_void
(ID3D12Resource), state: u32 (D3D12_RESOURCE_STATES at Present), size: Size,
format: u32 (DXGI_FORMAT) }`; `Submitted::{Inactive, NoInputs, Filed{index},
Duplicate{index}, Rejected}`; `LateKind::{HudLess, UiAlpha}`.

## How the mod wires it

| When / where (thread) | Call |
|---|---|
| boot, before the engine creates its device | `start(StartConfig { … })` |
| gate decides to stand down | `stand_down()` |
| render's factory registration point (engine thread) | `upgrade_factory(factory)` for every factory |
| render sees `ExecuteCommandLists` on the engine's graphics queue (main thread) | `render_submit()` |
| engine upscale slot, after DLSS-SR (render thread) | `submit_frame(&FrameInputs { … }, list)` |
| after the HUD-less / UI-alpha pass (render thread) | `tag_late(kind, Some(resource), camera.frame)` |
| engine iteration start, only with `SleepPoint::IterationStart` | `frame_start()` |
| settings change | `set_options(Options { ..Options::from_settings(&s.frame_generation) })` |
| swapchain exists (first Present) and on `WaitLatch` changes | `if wait_latch(policy) { engine::latch() }` |
| ngx: the game's NGX init | give it a time limit if `faulted()` |
| process exit | `shutdown()` |

`Present` and `ResizeBuffers` are hooked here; nothing else is needed for
them. `GetProcAddress` interception: none (render owns
`CreateDXGIFactory2`). The mod's `dev` feature must list `mhw-fg/dev`.

## Behaviour and its reasons

**Loading.** `slInit` runs on thread `mhw-sl-init` at load: doing it inside
the engine's device or factory call delayed the swapchain past the
interception (black screen). The plugins are looked for in the mod folder
(beside `nvngx_dlss.dll`, so Streamline's NGX client shares the game's
snippet set) and then in `sl/`. A folder holding `nvngx_dlisp.dll` is
refused with a message naming the file: NGX initialises that 2019 snippet in
Streamline's context, it faults inside `slSetD3DDevice` and `sl.dlss_g`
never starts. With `verify_signature` the interposer must carry a valid
Authenticode signature with `CN=O=NVIDIA Corporation` (offline check), as
the fetch script required at install. Preferences: manual hooking, DXGI
factory proxy, frame-based tagging, `DisableCLStateTracking`; OTA
(`eAllowOTA | eLoadDownloadedPlugins`) only with `allow_ota`. Identity:
application id 100182111 (the game's own), or the engine/project route
when both strings are given. Streamline's log lines arrive under target
`sl`; after 4000 start-up lines each message template is written on its
1st, 10th, 100th … occurrence.

**Swapchain.** Created once by the engine (`CreateSwapChainForHwnd` on its
graphics queue); mode, size and HDR go through `ResizeBuffers` on the same
chain. At that creation the device is taken from the queue, given to
Streamline, DLSS-G loaded and bound; only then is a counted copy of the
factory upgraded and the chain created through the proxy. Any failure keeps
DXGI's chain and the game unchanged. `ResizeBuffers` turns DLSS-G off at
once (the guide: off before any window manipulation), holds it off for 3
presents, resets history and reads back size and format (B8G8R8A8 SDR,
R10G10B10A2 HDR).

**Constants.** `cameraViewToClip = P·R` (unjittered, depth flipped as the
depth buffer), `clipToPrevClip = mhw_core::math::clip_to_prev_clip` (the
camera's unjittered prev matrices), `jitterOffset = streamline_jitter(px)`
(render pixels incl. the display/render factor, +y down), `mvecScale =
pixel_scale / mvec size`, depth inverted, camera motion included, vectors
neither jittered nor dilated, lens and basis from the matrices (invalid,
never guessed, when they cannot be read). Jitter of another frame than the
camera is refused.

**Tokens and markers** (`token.rs`). The index advances only at the end of
Present and only if constants were filed under it (a present without a
rendered frame keeps the token instead of leaving a hole the plugin answers
by switching off). Constants once per index; each marker once per index;
Reflex sleep once per present cycle. `PRESENT_START` follows the frame's
constants by construction (render thread done before the main thread
submits and presents). The first submit after a mode change can be the
DLSS feature recreation; that frame's simulation/render split is then
early, which is accepted (`useMarkersToOptimize = false`).

**Options.** Sent from the present thread and only on change. `eOff` uses
`eRetainResourcesWhenOff`, so on/off (`Options::enabled`) is live; loading
the plugin is decided once at swapchain creation (restart). Generation is
off, first reason first: disabled, no depth/MV this frame (their tags are
removed), window not in the foreground (`pause_unfocused`), resize hold,
back buffer below `minWidthOrHeight`. `numFramesToGenerate = multiplier − 1`
(2–4), capped by `numFramesToGenerateMax`. Reflex is always low latency
(DLSS-G requires it), with boost on request. `slDLSSGGetState` is sampled
once a second on the present thread.

**Pacing.** The game's own limiter caps rendered frames: shown fps = game
cap × multiplier. Alternatively `FrameLimit::OutputFps(f)` sets Reflex's
`frameLimitUs = multiplier / f` and the game cap goes to unlimited. The
engine's wait helper waits on the frame-latency waitable after the Reflex
sleep; `wait_latch(WaitLatch::Auto)` is true exactly when frame generation
owns the swapchain, and the mod then sets `Device+0x10DA = 1` (permanent,
like the engine's own 1 s timeout).

**Tags.** All `eValidUntilPresent`; `TagResource::state` is the state at
Present (mandatory per the header). HUD-less and UI alpha are filed at
Present, only for the frame whose constants are under the current token;
a HUD-less format differing from the back buffer's is logged.

## Tests

`cargo test -p mhw-fg` (host): ABI offsets, constants assembly, token
machine, options/limit/latch, plugin location and identity, tags.
