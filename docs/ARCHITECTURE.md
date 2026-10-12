# mhw-dlss — architecture

This document is the contract for the code. Every crate implements the part
assigned to it here; anything that crosses a crate boundary is described here
first and changed here first.

The previous implementation (tag `v0.2.0`) is the behavioural reference.
Reverse-engineering facts that supersede it are in `docs/re/*.md`, and every
game address used by the code comes from `docs/re/signatures.json`.

## Goals

1. Same product: DLSS-SR by taking over the game's own DLSS 1 path, and DLSS
   frame generation through Streamline. DirectX 12 only. Reverse-engineered
   on Steam build 15539686; any build whose code the gate still recognises.
2. Better code: crates with hard boundaries, pure logic separated from Windows
   glue so that it is unit-tested on any host, no module above ~800 lines,
   no `unsafe` without a `// SAFETY:` line, `clippy -D warnings` clean.
3. Fewer public settings, CamelCase, in sections (see *Settings*).
4. A **dev build** (`--features dev`) that lets an agent drive the running
   game over TCP: state, live settings, memory peek/poke, frame and data
   capture, targeted probes. **The release build contains none of it**: the
   code is behind `#[cfg(feature = "dev")]`, the crate `mhw-dev` is an
   optional dependency, and CI checks the release DLL for dev strings.
5. Professional logging: levels, per-subsystem targets, structured fields,
   rate limiting, rotation.
6. Every runtime question left open by the RE (`docs/re/00-SUMMARY.md` §3)
   has a dev probe that answers it.

## Workspace

```
Cargo.toml                 workspace, shared lints and profile
crates/
  core/      mhw-core      pure Rust, host-testable: log, config, sig, math, frame types
  win/       mhw-win       Windows primitives: memory, detours, IAT/vtable patching, threads, modules
  engine/    mhw-engine    the game: address book, struct views, detours on game functions
  render/    mhw-render    Direct3D 12 interception, tracking, temporal-pass inputs, compute passes, capture
  ngx/       mhw-ngx       NGX ABI and the DLSS-SR feature lifecycle
  fg/        mhw-fg        Streamline binding, proxy swapchain, DLSS-G constants/tags, Reflex/PCL, pacing
  dev/       mhw-dev       dev-only: TCP JSON-RPC server, probes, capture export (optional dependency)
  mod/       mhw-dlss      the cdylib: startup, wiring, policy (which experiment is on)
  launcher/  mhw-dlss-launcher   the injector executable
tools/
  devctl/    devctl.py     agent/CLI client for the dev server (Python, stdlib only)
  standin/   standin       synthetic renderer harness (Windows)
```

Dependency direction (no cycles, no sideways edges except those listed):

```
core  <-  win  <-  engine
               <-  render
               <-  ngx
               <-  fg      (fg may use render's public resource-state API)
dev   ->  core, win, engine, render, ngx, fg     (optional, feature "dev")
mod   ->  everything; dev only with feature "dev"; launcher's pure
          library for the cross-process names (`handoff`)
```

Crates below `mod` never call each other's hooks directly. Cross-cutting events
go through **`mhw_core::bus`** (see below) and `mod` decides policy.

## Build and test

- Toolchain: Rust 1.98.1. Target `x86_64-pc-windows-gnu`.
- On Windows: `cargo build --release` (release) and
  `cargo build --release --features dev` (dev).
- On Linux (cross): `cargo zigbuild --release --target x86_64-pc-windows-gnu
  [--features dev]`. `tools/xbuild.sh` wraps it.
- Unit tests: `cargo test -p mhw-core` (and any crate whose tested logic is
  host-independent) run on the host. Windows-only code is gated by
  `#[cfg(windows)]`; pure logic it needs lives in a `cfg`-free module so it can
  be tested on Linux.
- `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu
  [-F dev] -- -D warnings`, `cargo fmt --check`.
- Development discipline: contract (this file, doc comments) → tests → code.
  Tests cover behaviour, boundaries and invariants (parsing, signature
  scanning, jitter conventions, matrix derivation, state machines), not
  wiring.

## mhw-core (pure, host-testable)

### `log`

Facade: the `log` crate (with `kv`). `mhw_core::log::init(LogConfig) ->
Result<Option<PathBuf>, SetLoggerError>` installs the logger and returns where
the file landed (`LogConfig { dirs, level, targets, profile, max_file_bytes }`;
`dirs` are tried in order: mod folder first, fall-backs after it).
`log::set_filter(level, targets)` changes levels while running.

- Levels: `Error, Warn, Info, Debug, Trace`. Global level from settings
  (`[General] LogLevel`); per-target overrides in dev (`[Dev] LogTargets =
  "render=trace,fg=debug"`, parsed by `log::parse_targets`).
- Targets are subsystem names, not module paths (`log::TARGETS`): `boot, gate,
  engine, jitter, hijack, tune, render, taa, inputs, dlss, ngx, fg, sl,
  reflex, swap, hudless, dev, perf, config, log, crash`. Every crate logs with
  `target: "<name>"`.
- Line format: `2026-10-11T15:04:05.123 I [render] message key=value key=value`
  (UTC milliseconds, one-letter level, target in brackets, structured fields
  after the message; values with spaces are quoted; continuation lines of a
  multi-line message are indented by two spaces).
- Sinks: file `mhw-dlss.log` in the mod folder (rotation: previous run →
  `mhw-dlss.previous.log`, size cap 32 MiB per run, then truncation notice);
  dev builds also push records into a bounded in-memory ring (`log::ring`:
  `tail(n)`, `since(seq)`, `next_seq()`) that the dev server streams.
- Redaction: the user profile path is replaced with `%USERPROFILE%` in every
  line (pure function, tested).
- Rate limiting helpers (level is an identifier, the rest is what `log!`
  takes): `log_once!(Warn, target, key, ...)`, `log_every!(n, Debug, target,
  ...)`, `log_changed!(Info, target, key, value, ...)` (logs only when `value`
  differs from the last logged one for `(target, key)`).
- Per-frame code never logs above `Debug` except through these helpers.

### `config`

- File: `mhw-dlss.ini` in the mod folder; INI, sections and keys CamelCase
  (matched case-insensitively), values case-insensitive for enums/bools
  (`true/false/on/off/1/0`), `;` and `#` comments (inline after whitespace),
  optional double quotes, UTF-8 with or without BOM (`parse_bytes` also takes
  UTF-16 with BOM).
- `Settings` is a plain struct produced by `parse(&str, Build) -> (Settings,
  Vec<Diagnostic>)` / `parse_bytes(&[u8], Build)`. Unknown keys, invalid or
  out-of-range (clamped) values, and dev keys in a release build produce
  diagnostics (logged once), never errors. `Build::Dev` exists only with the
  `dev` feature; `Build::CURRENT` is the compiled build.
- `Settings::set(section, key, value, Build)` applies one key with the same
  validation (dev `settings.set`).
- `Settings::experimental() -> &Experimental` is the file's section in a dev
  build and the constant `Experimental::DEFAULT` in release.
- Hot reload: the file is polled every 500 ms by `mod`; `Settings::diff(&old,
  &new) -> Changes` says which `Group`s changed; `Group::reload()` is `Live`
  or `Restart`.
- `Settings::write_default(Build) -> String` writes the commented default file
  (release: public sections only).

Public settings (release and dev):

```ini
[General]
Enabled = true            ; restart
LogLevel = Info           ; Error | Warn | Info | Debug | Trace   (live)

[Upscaling]
Mode = Quality            ; DLAA | Quality | Balanced | Performance | Custom   (live)
RenderScale = 0.667       ; 0.50-1.00, only with Mode = Custom   (live)
Preset = Default          ; Default | J | K | L | M   (live, recreates the feature)
Sharpness = 0.0           ; 0.0-1.0, RCAS after DLSS   (live)
DownsampleVolume = true   ; false: volumetric fog at full resolution   (restart)

[FrameGeneration]
Enabled = false           ; restart
Multiplier = 2            ; 2 | 3 | 4   (live)
ReflexBoost = false       ; live
```

Mode → scale: DLAA 1.0, Quality 0.667, Balanced 0.58, Performance 0.5.
Everything the old `expert_*`, `diag_*`, `fg_*` test keys did is either a
fixed, researched default or a dev setting.

Dev-only sections (accepted only when built with `dev`; a release build logs
"ignored, dev build only" once per key):

```ini
[Dev]
Listen = 127.0.0.1:47800  ; dev server address; 0.0.0.0:47800 for LAN
TokenFile = dev.token     ; created with a random token on first start
LogTargets =              ; e.g. render=trace,fg=debug
DebugLayer = false        ; D3D12 debug layer (restart)
Dred = false              ; DRED breadcrumbs (restart)
CaptureDir = captures
DumpShaders = false       ; volumetric/fog shader bytecode to CaptureDir\shaders; compute overrides from its override\<hash>.hlsl (restart)

[Experimental]            ; every key live unless noted; defaults = release behaviour
InputPath = Supersampling ; Supersampling (game SS path, alias depth) | Pass (temporal-pass tracking)
JitterScale = Display     ; Display (2px/rs, RE-correct) | Render (old behaviour) | Off
JitterPhases = 32
HudLess = false           ; HUD-less capture + gamma/PQ conversion for frame generation
TaaSharpen = Off          ; Off | Game
HoldAntiAliasing = true   ; keep the TAA word at 0x0001
WaitLatch = Auto          ; Auto (latch when FG owns the swapchain) | Never | Always
Reset = Auto              ; Auto (game flag + camera cut) | GameOnly | Never
MipBias = Auto            ; Auto | Off | <float>
DlssHdr = Auto            ; Auto | true | false
```

The `[Experimental]` defaults are the release behaviour; the release build
compiles them as constants.

### `sig`

Pure signature scanning over byte slices:

- `Pattern::parse("48 8B 05 ?? ?? ?? ?? C7 80")`; `scan(haystack, &Pattern)
  -> Vec<usize>`; `rip_target(insn_addr, disp_offset, insn_len, bytes) ->
  Option<usize>`; `displacement(bytes, at, size) -> Option<i32>` (struct
  offsets read from a found instruction, sign-extended).
- `Site { name, pattern: &'static str, to_target: isize, known_rva: usize,
  verify: &'static [u8], wild: Range<usize> }`, `resolve_all(image: &impl
  Image, sites) -> Result<Resolved, GateFailure>`: table entries are validated
  first (patterns parse, `verify` non-empty, `wild` inside it); fast path
  checks `verify` at every `known_rva`; otherwise scans `.text` for every
  pattern; requires exactly one match and that `verify` holds at
  `match + to_target` (bytes under the pattern's wildcards are not compared
  there: RIP displacements and branch offsets move with code; `wild` is never
  compared); all-or-nothing, `GateFailure::Sites` names every failing site.
  `Image` is a trait (`section(name) -> Option<Range<usize>>`, `read(rva, len)
  -> Option<&[u8]>`, RVAs as `usize`) implemented by `win` for the live module
  and by tests for byte buffers.
- The site table itself is generated from `docs/re/signatures.json` into
  `crates/engine/src/sites.rs` (checked in; regenerated by
  `tools/gen_sites.py`).

### `math`

`Mat4` (row-major, row vectors `v·M`, matching the engine), `Vec2/3/4`,
inverse, multiply; `halton(index, base)`; `JitterSequence` (Halton 2/3 from
index 1, `sample(frame, render, display, JitterScale) -> JitterSample`); the
engine's `jitter_matrix(units, display)` (J) and `DEPTH_FLIP` (R); `lens(proj)`;
and the **conventions**, each a tested function (`JitterScale` lives in
`math`, `config` re-exports it):

- `engine_jitter_units(px: Vec2, render: Size, display: Size, mode) -> Vec2` —
  what to write to `sCamera+0x38/+0x3c` to move the image by `px` render
  pixels (engine axes, +y up). RE fact: the engine divides by the **display**
  size and one unit is half a display pixel, so `units = 2·px·(display/render)`
  in `Display` mode. `engine_jitter_px(units, render, display)` is the shift a
  write actually produces (`JitterSample.px`, also right in `Render` mode).
- `dlss_jitter(px) -> Vec2` and `streamline_jitter(px) -> Vec2` — both
  `(px.x, −px.y)` in render pixels (NVIDIA convention, +y down).
- `clip_to_prev_clip(cam: &CameraFrame) -> Option<Mat4>` =
  `inv(V·P·R)·(Vprev·Pprev·R)` from the camera's own **unjittered** matrices
  (the engine's prev copies, which its motion vectors use); row vectors.
- `camera_cut(prev, cur) -> bool` — position jump, view-direction change or
  projection-scale change above `CUT_THRESHOLDS`, or non-finite values;
  `camera_cut_metric` gives the numbers for `probe.reset`.

### `frame` (shared data types)

```rust
pub struct Size { pub w: u32, pub h: u32 }
pub struct CameraFrame {          // from the engine viewport, unjittered
    pub view: Mat4, pub proj: Mat4, pub prev_view: Mat4, pub prev_proj: Mat4,
    pub view_inv: Mat4, pub position: Vec3, pub near: f32, pub far: f32,
    pub fov_y: f32, pub frame: u64,
}   // CameraFrame::new(view, proj, prev_view, prev_proj, frame) derives the rest
pub struct JitterSample { pub px: Vec2, pub units: Vec2, pub phase: u32, pub frame: u64 }
pub struct FrameInfo { pub frame: u64, pub render: Size, pub display: Size, pub scale: f32, pub reset: ResetReason }
pub enum ResetReason { None, GameHistory, CameraCut, Settings, Resize }
```

### `bus`

A tiny synchronous event registry so lower crates can announce things without
knowing who listens: `bus::subscribe::<E>(fn(&E))`, `bus::publish(&E)`,
`bus::subscribers::<E>()`. Handlers run on the publishing thread, in
subscription order, without a lock held.
Events (defined in core): `FrameBegin(FrameInfo)`, `CameraReady(CameraFrame,
JitterSample)`, `UpscaleSlot(...)` is *not* on the bus — direct calls between
engine and render go through `mod` via registered callbacks (see Wiring).

## mhw-win

Windows primitives, each small and documented: `Module` (base, size,
sections, implements `sig::Image`), memory read/write with protection,
`Detour` (14-byte `jmp [rip]` with trampoline, prologue relocation for the
exact prologues in the site table), `patch_iat` (by name **and ordinal**),
`patch_vtable`, `ThreadSuspender` (suspend all other threads for atomic
install), `loaded_modules()` with load order, `Event`/`Thread` helpers, crash
VEH that logs fault address + module + offset.

Specifics the rest of the workspace relies on:

- `patch_iat` covers the regular **and the delay-import** tables (MHW's
  `D3D12CreateDevice` is delay-imported by ordinal #101). An unresolved delay
  slot holds a thunk that would overwrite the patch, so its original is
  resolved like the delay helper does (`LoadLibraryExA(dll, 0, 0)` +
  `GetProcAddress`).
- The detour relocator accepts exactly the instruction forms of the detoured
  prologues (`push`, `mov`, `lea`, `sub rsp`, `movaps` store, `movzx`) and
  rejects RIP-relative operands and branches; trampolines live within
  `rel32` reach of the function. Prepare all detours, then install them in
  one `install_detours` call (all or nothing, other threads suspended).
- Until `win` depends on `core`, `Module` exposes `sections()`, `section()`,
  `read(rva, len)` and `text()`, and `mod` adapts it to `sig::Image`.
- The crash VEH logs under target `crash`, once per faulting address.

## mhw-engine

- `sites.rs` (generated) and `Addresses` (resolved once by the gate).
- Struct views with named fields (reflection names from RE where known):
  `RenderConfig` (`0x1451c4480`), `Camera` (`0x1451c4400`, viewports),
  `RenderSettings` (`0x1451c4368`), `TaaFilter`, `Device` (`+0x10D9/DA/F0`).
- Gate, independent of the build: `sig::resolve_all` (every site), globals
  out of the sites, struct offsets out of the code (`anchors`; a field whose
  object also has unanchored `layout` offsets must not have moved); DX12
  detection from `graphics_option.ini` (BOM-aware, missing key = DX11) and
  the fallback marker `rendering_api_fallback.txt`.
- Detours:
  - camera setup `0x14228f3b0`: after the original, read the main viewport's
    unjittered matrices and rect, write the jitter **once per frame for the
    main viewport**, publish `CameraReady`.
  - DLSS 1 hijack: GetOptimalSettings, CreateFeatures, EvalSS, EvalISP,
    SS enqueue, Image Quality handler — semantics as in `main` plus the RE
    facts: game Reset flag (`taa+0x185` path), flags 2 vs 3 policy, optional
    supersampling path with the depth alias (`+0xf0 := +0x60`, AddRef).
  - fatal reporter: log and re-raise.
- Tune: TAA sharpen off, AA word hold, mip bias (sampler bank 1 operand),
  SSLR/contact-noise defaults, render ratio via setter `0x14229be60` with the
  16-alignment fix at 0.5.
- Wait latch: write `Device+0x10DA = 1` when told to.

## mhw-render

- Interception: D3D12 device/queue/command-list/resource vtables via a device
  of our own; `CreatePipelineState`/`CreateComputePipelineState` (ReShade
  routes around the latter); DXBC RDEF reflection to recognise
  `CSTemporalAA2`, `CSTemporalUpsample`, `System_CopyToBackBuffer*`.
- Tracking: root signatures, descriptor tables/heaps, mapped constant buffers,
  resource states in submission order, liveness, the engine's queue.
- Temporal-pass inputs (`InputPath = Pass`): colour, MV (`tGBuffer4` decode
  compute pass), depth (exact-size DSV, newest first) → `DlssInputs`.
- Compute passes: MV decode, RCAS sharpen, HUD-less convert (gamma/PQ like
  `System_CopyToBackBuffer`), all compiled with `D3DCompile` at first use.
- Capture service (used by dev only, compiled in release as nothing): copy
  named resources into readback buffers on the engine's queue and hand back
  bytes + format.

## mhw-ngx

NGX ABI (parameter vtable with MSVC overload order), session (hook the game's
`NVSDK_NGX_D3D12_Init`, API version 21, data path, adapter LUID), DLSS feature:
create/evaluate/release with a 16-frame retire queue, presets, flags
(`IsHDR` from policy, `MVLowRes`, `DepthInverted`, `AutoExposure`), reset.

## mhw-fg

Streamline 2.14.1 binding (`#[repr(C)]` ABI, tested offsets), slInit on its own
thread, proxy DXGI factory upgrade, swapchain hooks (create once,
`ResizeBuffers` → reset + format), DLSS-G options, constants (from
`CameraFrame` + `JitterSample` via `math`), tags (depth, MV, HUD-less), Reflex
and PCL markers, frame token discipline, frame limit.

## mhw-dev (feature `dev` only)

TCP server, newline-delimited JSON request/response, binary payloads framed
after the JSON line. Auth by token. Specified in `docs/DEV-PROTOCOL.md`.
Methods expose state, settings, memory, captures, probes, log stream.

## mhw-dlss (cdylib) — wiring and policy

`DllMain` patches the executable's `GetProcAddress`/`LoadLibraryEx*` IAT
slots (table writes only) and starts the `mhw-boot` thread
(`crates/mod/README.md` is the precise order): settings (default file
written when missing), log, crash logger; stop for `Enabled = false` or
DirectX 11; `fg::start` (if enabled); NGX init policy, render
`configure_device`/`install`, render's factory hook, submit observer and
temporal-pass handler; engine params, depth alias and `UpscaleHandler`;
`bus::subscribe::<CameraReady>`; the `d3d12.dll` #101 delay slot; the gate
thread (`engine::check` until it decides, also asked at every renderer/NGX
lookup; `engine::install` retried on `Busy`, and on `NotCode`/`Changed` at
the next poll); then the boot is finished: renderer lookups held by the
import hooks go on and the launcher, waiting on `handoff::BOOTED_EVENT`
(at most `BOOT_TIMEOUT`), resumes the game; `dev` (if built);
the 500 ms hot reload. `Policy` (pure, `mod::policy`) is the one mapping
from settings to every subsystem; a `Store` serves the reload, the dev
server and the ReShade page (which writes the file).

- Below ratio 1: the temporal pass hands its colour through unfiltered
  (`copy_resource`, `Skip`); EvalISP evaluates DLSS from the post-processed
  colour with depth/MV from `render::take_inputs(list)` (`Pass`) or from the
  frame's EvalSS call (`Supersampling`), into the engine's target (via the
  RCAS intermediate when sharpening). At ratio 1 (no upscale point) DLSS
  resolves into the temporal pass's output (or EvalSS's history slot).
  Every slot that should have been evaluated feeds `mod::fallback`: 5
  slots without a temporal pass hold the game's own TAA at full resolution
  until the pass is back; 12 with the pass and nothing evaluated (a failure
  pauses 3 frames) give DLSS up (`Policy::dlss = false`, ratio 1; the DLSS
  detours stay the mod's, since the game's DLSS 1 path would evaluate the
  stand-ins and tear its DLSS state down) until a
  relevant setting changes. At the temporal pass NGX gets
  `PassContext::engine_list` (ReShade's proxy list when NGX holds its
  proxy device).
- `CameraReady` → camera ring (jitter and camera of the slot's frame, cut
  detection), `render::note_display`, dev `on_camera`. `fg` gets its
  constants through `FrameInputs` at the slot.
- The mod patches `CreateSwapChainForHwnd` (after `fg::upgrade_factory`, so
  outermost) and the returned chain's `Present`: engine queue for render,
  HUD-less tag and wait latch before Present, `render::note_present`, dev
  `present`/`frame` points.

## Error handling

No panics across FFI: every `extern "system"` entry point is wrapped in
`ffi_guard` (catches panics in debug, logs, passes through to the original).
Release profile keeps `panic = "abort"`; hooks therefore avoid panicking APIs
(`get` over indexing, checked arithmetic on game data).

## Crate APIs as built (wave 2)

The sections above describe intent; each crate's `README.md` is the precise
API and its "Wiring" section is what `mhw-dlss` must do:
`crates/engine/README.md`, `crates/render/README.md`, `crates/ngx/README.md`,
`crates/fg/README.md`. Changes against the intent above:

- engine: the jitter is written **before** the camera-setup original (the
  original reads `sCamera+0x38` to build the jitter matrix); the camera is
  read and `CameraReady` published after it. The upscale handler is
  `engine::UpscaleHandler` (`supersampling`, `image_signal`), called on the
  render thread inside the recording of `call.list`. `views::RenderConfig`
  reads the output encode (`output_gamma` `+0x1c8`, `output_gamma_ex`
  `+0x1cc`, `output_pq` `+0x21a`) for the HUD-less conversion. The mip bias
  is an `InstallOptions` field (start-up only), not a `Params` field.
- render: owns `D3D12CreateDevice` (by name and delay-import ordinal 101) and
  `CreateDXGIFactory1/2`; `set_factory_hook` is where `mod` calls
  `fg::upgrade_factory`. Inputs for the Pass path come from
  `render::take_inputs(list)` (the `InputBoard` behind it); the temporal
  pass calls `mod`'s `PassHandler`, which may `Skip` the engine's dispatch.
  `set_submit_observer` is the `ExecuteCommandLists` callback point
  (`Submission { queue, engine, lists }`). Passes contract:
  `hudless::begin_frame(frame)` at the slot and `hudless::take(frame) ->
  Option<HudlessImage { resource, state, size, format }>` before Present;
  `note_display(size)` is required (HUD-less detection needs the window
  size); `OutputTransform { gamma, gamma_ex, pq, format, hdr_white_nits }`
  (engine encode + swapchain format; nits only for the own PQ shader);
  `sharpen::intermediate` needs the engine's queue (`NotReady` before the
  swapchain exists) and rests in `UNORDERED_ACCESS`.
- ngx: `mod` calls the real `GetProcAddress`, then
  `ngx::session::set_original(name, module, address)`; if that returns true
  the game gets `ngx::session::intercept(name)`. Init policy is a function
  pointer set by `mod`.
- fg: no dependency on render/engine; `mod` passes `FrameInputs` at the
  upscale slot, calls `render_submit` on every engine graphics-queue
  `ExecuteCommandLists` (fg keeps the first of a frame), tags the HUD-less
  image with `tag_late` before Present (from its own `Present` hook, which
  runs before fg's), and writes the wait latch when
  `fg::wait_latch(policy)` says so (`mod::policy::latch_write`).
