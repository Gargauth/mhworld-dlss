# mhw-render

Direct3D 12 interception, tracking, temporal-pass inputs and our own passes
for mhw-dlss (`docs/ARCHITECTURE.md`, section *mhw-render*). This crate
covers creation interception, the vtable hooks, the tracking registries,
DXBC reflection and pass recognition, the `CSTemporalAA2` inputs
(`InputPath = Pass`), the motion-vector decode pass, RCAS sharpening, the
HUD-less image for frame generation, and (feature `dev`) the capture
service.

Pure logic is `cfg`-free and tested on any host (`cargo test -p mhw-render`,
and `--features dev` for the capture math). Everything touching D3D12 is
Windows-only: hooks and tracking are re-exported at the crate root; the
passes are re-exported into their public modules (`sharpen`, `hudless`,
`capture`) next to their pure parts.

## Modules

| module | what | host-tested |
|---|---|---|
| `dxbc` | DXBC container + `RDEF` reflection (bindings, registers, cbuffer fields, stage) | yes, hand-built fixtures |
| `recognise` | `PassKind`: `CSTemporalAA2`, `CSTemporalUpsample`, `System_CopyToBackBuffer`, `…PQSingle`, by binding shape | yes |
| `stream` | pipeline state stream walk (CS, PS, RTV0), x64 sub-object sizes | yes (+ SDK sizes on Windows) |
| `rootsig` | root signature layout: table offsets incl. `APPEND`, root CBVs | yes |
| `states` | resource states committed in submission order; transition planning | yes |
| `descriptors`, `tracked` | descriptor tables back to resources; lock-free copy filter | yes |
| `upload` | GPU VA ranges readable on the CPU (`Map` or seeded arena) | yes |
| `depth` | scene depth: exact size, newest binding, never a larger shadow map | yes |
| `mvpack` | `tGBuffer4` format, reference decode, decode constants + HLSL | yes |
| `taa` | where `CSTemporalAA2` binds what; render rectangle; frame-size floor | yes |
| `inputs` | `DlssInputs`, `CameraCb`, the `InputBoard` hand-over | yes |
| `names` | DXGI format / resource state names | yes |
| `signature` | DXBC `ISGN` input signatures; the fullscreen VS that links to a given PS | yes |
| `sharpen` | RCAS: constants, writable formats, HLSL, CPU reference of the filter; Windows: `intermediate`, `record` | yes |
| `hudless::detect` | when to copy the scene target (learned from each frame's encode, validated after the fact) | yes |
| `hudless::encode` | `OutputTransform`, the encode's `CBSystem` from reflection, own gamma/PQ shader + reference math; Windows: `enable`, `set_output_transform`, `begin_frame`, `take`, `status` | yes |
| `capture` (dev) | `Item`, `Captured`, `ColourStats`, `CaptureError`; `readback` (pitch, in-flight queue), `stats` (histogram, percentiles, reduction + HLSL); Windows: `record`, `poll`, `record_stats`, `poll_stats` | yes |
| `proxystate` (private) | ReShade proxy lists: this thread's dispatching proxy (nesting guard), the bounded per-proxy heap record | yes |
| `sys::*` (Windows) | hooks, registries, creation, our passes, after-submit work, introspection | compile + Windows-only unit tests |

## Wiring (what `mod` does)

Boot thread, before the game creates its device:

1. `configure_device(DeviceOptions { debug_layer, dred })` (dev settings).
2. `install()` — creates a device of our own (default adapter) and patches
   the device, resource, queue and command-list vtables. Idempotent; the
   device replacement calls it too if `mod` did not.
3. Creation calls (mod owns the `GetProcAddress` IAT hook and the
   `d3d12.dll` ordinal #101 delay-import patch):
   * `intercept(name) -> Option<usize>` for `D3D12CreateDevice`,
     `CreateDXGIFactory2`, `CreateDXGIFactory1`;
   * `intercept_ordinal("d3d12.dll", 101)` (= `D3D12_CREATE_DEVICE_ORDINAL`);
   * `set_original(name, address)` with what the game would have got
     (e.g. `patch_iat`'s `original`). Unset, the original is resolved like
     the delay-load helper (`GetModuleHandle`/`LoadLibraryExW(name, 0, 0)` +
     `GetProcAddress`).
   * `set_factory_hook(Some(f))`: `f(&FactoryCreated)` runs after each
     outermost successful factory creation, on the game's thread, before the
     factory is returned; it may replace `*out` (frame generation's
     `slUpgradeInterface`). Nested creations (Streamline enumerating
     adapters) bypass it.
4. `note_display(size)` when the engine's output size is known/changes.
5. `note_presenting_queue(queue)` from the swapchain-creation hook (first
   argument of `CreateSwapChainForHwnd`), then `detect_reshade()` and
   `watch_proxy_lists()` (see [ReShade](#reshade)); `note_present()` from
   the present hook.
6. Late load only: `seed_upload_region(va, size, cpu)` from the engine
   device fields (`+0x1060`/`+0x1018`, and `+0x1080` when binds use the
   default-heap copy; `ReLoadInitDx11` §2.3).

Per frame:

* `configure_pass(PassConfig { identify, decode: Option<DecodeParams> })`
  — policy. `DecodeParams { y_mask: YMask::Full | DropFlagBit, scale,
  offset_px, clamp_px }`.
* `set_pass_handler(Some(h))` — `h(&PassContext) -> PassVerdict` runs at
  each `CSTemporalAA2` dispatch on the recording thread, after the record was
  published; it may record on `ctx.list` (render restores heaps, compute
  root signature, pipeline and roots afterwards) and may `Skip` the engine's
  dispatch (e.g. after `copy_resource(colour → output)`). NGX is given
  `ctx.engine_list`, the list as the engine holds it (see
  [ReShade](#reshade)).
* At the upscale slot: `take_inputs(list) -> Option<(DlssInputs,
  Association)>` and `record_on(list, Some(depth), |list| …)` to record NGX
  with the engine's state restored; `Transitions::plan(list, &[(res,
  state)])` + `forward`/`back` for the barriers.
* RCAS (`[Upscaling] Sharpness > 0`): `sharpen::intermediate(display,
  format) -> Result<Intermediate { resource, state, size, format }, _>` is
  what DLSS writes instead of the engine's target (it rests in
  `UNORDERED_ACCESS`); after NGX, `sharpen::record(list, intermediate,
  INTERMEDIATE_STATE, target, target_state, sharpness)` writes the engine's
  target, with both left in the stated states and the list's compute state
  restored. `Err(Off)` at sharpness 0; `Err(Unsupported)` for a target the
  pass cannot write (then let DLSS write the target directly).
* Full-resolution fog (`[Upscaling] DownsampleVolume = false`):
  `configure_volume_upsample(true, on_built)` before `install`; the game's
  three `CSVRUpsampleBilateral` pipelines (recognised by bytecode hash) are
  then built from `sys/vr_upsample.hlsl`, a per-pixel upsample weighted by
  the full-resolution depth (`docs/re/ReVolumetrics.md`), whether the
  engine creates them or a layer rebuilds them as pipeline state streams
  (ReShade with add-ons such as RenoDX). `on_built` runs once the first one
  was built; the mod then has the engine `hold_downsample_volume(true)`. A
  replacement whose creation fails is built from the game's shader instead.
* HUD-less (`[Experimental] HudLess`): `hudless::enable(on)` on settings
  change; `hudless::set_output_transform(OutputTransform { gamma:
  [sRender+0x1c8], gamma_ex: [sRender+0x1cc], pq: [sRender+0x21a] != 0,
  format: swapchain format, hdr_white_nits })` each frame (cheap when
  unchanged); `hudless::begin_frame(camera.frame)` at the upscale slot;
  before `Present`, `hudless::take(camera.frame) -> Option<HudlessImage {
  resource, state, size, format }>` → `fg::tag_late(HudLess, Some(TagResource
  { .. }), camera.frame)`. `note_display(size)` is required (the scene
  target must be the window's size). `hudless::status()` for the log.

## Threading and timing

* Hooks run on the engine's recording threads. State hooks take one
  uncontended per-list lock; descriptor copies not touching followed views
  take no lock.
* A `DlssInputs` record is built entirely from one dispatch (textures from
  its table, camera bytes from its own root CBV) and published under one
  lock. `take_inputs` hands each record out once, newest first, preferring
  the caller's list, never older than the last taken; `Association` says
  whether it came from another list.
* Published resources are referenced (AddRef) for four publications. Check
  `is_alive(resource, serial)` before using an old record.
* The decoded MV texture rests in `NON_PIXEL_SHADER_RESOURCE`; readers must
  leave it there.
* Work that must follow an engine list on its queue (a fence signal, our
  HUD-less conversion list) is registered against the list and run by the
  `ExecuteCommandLists` hook right after the original call. Our own lists go
  through the original `ExecuteCommandLists` (the submit observer and frame
  generation's submit counting do not see them).
* Our objects are retired by presents (`note_present`): RCAS intermediates
  8 presents after replacement, HUD-less converters 8 presents and until
  their lists completed. Converted HUD-less images rotate through three
  textures, reused only after the GPU finished the previous conversion.
* Shaders are compiled with `D3DCompile` at first use (`d3dcompiler_47.dll`
  from System32); our pipelines are created while marked as our own, so they
  are never recognised as the engine's.

## ReShade

ReShade does not patch vtables; it hands the engine proxy devices, queues
and lists that forward to the native ones, which carry this crate's hooks.
`unwrap_reshade(p)` / `native_interface(i)` go down to the native object
(ReShade's private `IID_UnwrappedObject`), `is_reshade_proxy(p)` says
whether `p` is a proxy; both are one relaxed load with ReShade absent
(`reshade_present()`, set by `detect_reshade()` at creation calls).

NGX must get the list from the same side of ReShade as the device it was
initialised with: with ReShade loaded that device is ReShade's proxy, NGX's
heaps are proxies, and `SetDescriptorHeaps` with a proxy heap on a native
list faults inside D3D12Core. The temporal pass is entered from a hook on
the *native* list, and nothing leads back from it to its proxy except the
call stack: `watch_proxy_lists()` finds ReShade's proxy device as private
data on the engine's device (via the engine queue), creates a list through
it and hooks `Dispatch` on the proxy list vtable, which holds the proxy as
this thread's dispatching list for the call. `PassContext::engine_list` is
that proxy when it unwraps to `ctx.list`, else `ctx.list` itself. Render
calls the watch after the game's device creation; `mod` calls it again at
swapchain creation (ReShade may wrap the device only after creation
returns). Idempotent; one attempt once ReShade's proxy device is found.
Diagnostics: `proxy_lists_watched()`, `proxy_dispatches()`.

NGX then sets its own heaps through the proxy, and ReShade remembers each
list's heaps; a restore on the native list alone would leave that record
on NGX's heap. The watch also hooks the proxy's `SetDescriptorHeaps` and
keeps the heaps the engine last set on each proxy (at most two; the map is
emptied at 4096 lists). `record_on(proxy, …)` and the pass handler snapshot
them before the work and set them again through the proxy afterwards,
before the native state is restored.

## RCAS

FSR 1 RCAS on the DLSS resolve, before post, HUD and frame generation (DLSS
no longer sharpens; a ReShade sharpener at `Present` would sharpen the HUD
and generated frames too). The picture is linear and high-range, RCAS wants
a perceptual `[0, 1)` signal: each tap goes through `c / (1 + max(c))` and
`sqrt`, undone on the way out. Ring excludes the centre (FSR 1.0), no noise
term, non-finite taps count as black, a non-finite centre passes through.
`Sharpness` scales FSR's lobe directly (1 = FSR maximum = 0 stops,
`amount_from_stops`).

## HUD-less

No UI-free resource exists, only a moment (`ReFramePresent` 5a–5c): the
scene target `primaryScene+0x4a0` just before the GUI viewport's first draw
(post-tonemap, pre-encode). The old mod captured "binding 2 of a
window-sized target", surveyed by hand, latched to a resource; loading
screens and cutscenes change the binding count and its mistakes were silent.

Detection (`hudless::detect`): `OMSetRenderTargets` (first RTV → resource
through `CreateRenderTargetView`) and draws are followed only while enabled.
At the frame's first `System_CopyToBackBuffer*` draw the scene target is
the newest other window-sized colour target bound on that list before the
backbuffer (`setRenderTarget(primaryScene+0x4a0, …)` precedes the encode,
`0x14229a3fe`), and the GUI binding is the last binding of it that got
draws. That `(target, occurrence)` must repeat for 3 frames before captures
start; a capture is made at the first draw into that binding and handed out
by `take` only if the same frame's encode observes the same pair. A failed
copy or a refuted capture yields no image for the frame, never a wrong one.

Conversion: the copy is converted into the swapchain's format by the
engine's own `System_CopyToBackBuffer[PQSingle]` pixel shader (bytecode
kept by the pipeline hooks), with a root signature built from its
reflection (texture table, static linear-clamp samplers, `CBSystem` as root
constants: `fSourceMipLevel = 0`, `fGammaCorrect = 1/gamma`,
`fGammaCorrectEx`) and a vertex shader generated from its `ISGN`
(`SV_Position` + texture coordinate). If that shader is missing or binds
anything else, an own shader does `pow(pow(saturate(c), 1/gamma), gamma_ex)`
(SDR) or BT.709→BT.2020 + `hdr_white_nits` scale + ST 2084 (HDR); both are
INFERENCE (`ReFramePresent` E6/E7). `status().encoder` says which runs.

## RE facts relied on

* `D3D12CreateDevice` is delay-imported by ordinal #101 (`ReLoadInitDx11` §2.2);
  three call sites probe with `out == NULL`.
* The CB arena is mapped once at device init; CPU `Device+0x1018`.
* `tGBuffer4`: R32_UINT, x = high half, y = low half (f16, NDC); pixels =
  `v·(−w/2, +h/2)`; jitter-free (`ReDlss1Path` §2.6, `ReJitterCamera`).
* `CSTemporalAA2` table `root0[u0..8@+0 t0..31@+9]`: `rwOutput` +0, `tBaseMap`
  +9, `tGBuffer4` +13 (fallback when the root signature predates the hooks).
* `System_CopyToBackBuffer[PQSingle]`: `tBaseMap` + `CBSystem{fGammaCorrect}`;
  PQ told apart by an R10G10B10A2 target (INFERENCE).
* `CBSystem = { fSourceMipLevel = 0, fGammaCorrect = 1/[sRender+0x1c8],
  fGammaCorrectEx = [sRender+0x1cc] }` (`0x142297b1d`); `sRender`'s ctor
  sets 2.2 and 1.0 (`0x142290e05`); PQ when `[sRender+0x21a]`.
* `tBaseMap = [[sRender+0x1f450]+0x4a0]+0x58`, bound as RT right before the
  encode (`0x14229a3fe`); `uGUI::draw` binds no target (`ReFramePresent` 5a).

## Dev feature

`dev` adds `resource_record(resource)`, `upload_region(va)` and the
`capture` module. `counters()`, `recognised_pipelines()`,
`pass_bytecode(kind)` and `last_inputs()` are always available (cheap, used
by the periodic log). `mhw-dlss`'s `dev` feature should list
`mhw-render/dev`.

### Capture service

`capture::record(list, request, frame, &[Item { name, resource, state }])`
copies each resource (mip 0 / plane 0 of a single-sample 2D texture, or a
whole buffer) into a pooled readback buffer on the engine's open list,
moving it from `state` to `COPY_SOURCE` and back (no barrier when `state`
already includes `COPY_SOURCE`). The crate's fence is signalled on the
engine's queue right behind that list; `capture::poll()` returns completed
`Captured { request, name, format, width, height, row_pitch, frame, bytes }`
oldest first without waiting (`row_pitch` 256-aligned, `bytes =
row_pitch × height`, `Captured::row(y)` strips the padding; the format of a
depth-stencil plane is its copyable format). At most 64 copies / 1 GiB in
flight, else `NotReady`.

`capture::record_stats(list, request, frame, &item)` runs a compute
reduction over a colour texture (typeless families read through their
float/unorm member) on the engine's list, state restored: per 16×16 group
sums, minima, maxima, non-finite and `> 1.0` counts, and a 258-bin Rec. 709
luma histogram (8 bins per octave, 2^-16..2^16, plus under/overflow). The
CPU finishes it; `capture::poll_stats()` returns `(request, frame,
ColourStats { min, max, mean, p50, p99, p999, above_one, nan_or_inf })`
(percentiles interpolated geometrically inside a bin, so within 9 % of the
true value). At most 8 in flight.
