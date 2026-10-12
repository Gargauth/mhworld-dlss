# mhw-engine

The game side of mhw-dlss: where things are in `MonsterHunterWorld.exe`,
typed views of the engine's objects, and the detours on its functions. The
tables come from Steam build 15539686; the gate finds everything by
signature, so another build works when the code still matches. Contract:
`docs/ARCHITECTURE.md` §mhw-engine; facts: `docs/re/*.md`, addresses
`docs/re/signatures.json`.

## Use (from `mod`)

```rust
let module = mhw_win::Module::main()?;
let game_dir = /* directory of the executable */;
match mhw_engine::check(&module, &game_dir) {          // 1. gate, all or nothing
    Ok(_addresses) => {}
    Err(why) if why.retryable() => { /* code may still be packed: ask again later */ }
    Err(why) => { /* DirectX 11: stand down for good */ }
}
mhw_engine::set_params(params);                         // 2. policy (any time)
mhw_engine::set_upscale_handler(Some(&HANDLER));
mhw_engine::set_depth_alias(params.flags.supersampling());
let installed = mhw_engine::install(InstallOptions { mip_bias: Some(-0.585) })?; // 3.
mhw_core::bus::subscribe::<CameraReady>(on_camera);    // once per render frame
```

`install` returns `InstallError::Patch(PatchError::Busy)` when a thread was
inside a prologue: retry. Install before the game creates its device: the
optimal-settings answer must be in place before the engine builds its TAA
units, and the mip bias before it builds its sampler banks.

## Public API

Pure (all targets, host-tested):

| item | what |
|---|---|
| `sites::{SITES, GLOBALS, FIELDS, ANCHORS, site, global, field}` | generated tables |
| `anchors::{resolve_globals, derive_offsets, check_tables, …}` | globals and offsets from any `sig::Image` |
| `gate::{evaluate, Addresses, StandDown}` | the whole gate over an image |
| `renderer::{renderer, marker_set, Renderer, Dx11Reason}` | the game's DX11/DX12 decision |
| `ratio::{setter_input, stored_ratio, snapped, render_width, full_window}` | the ratio setter's arithmetic + 0.5 fix |
| `mipbias::{bank_one_constant, constant_near, displacement_to, bias_of}` | the bank-1 constant |
| `viewport::{ViewportSnapshot, OncePerFrame, render_size, jitter_for}` | camera-setup glue |
| `params::{Params, DlssFlags}` | what `mod` decides (`Params::RELEASE`) |
| `upscale::{UpscaleHandler, SupersamplingCall, ImageSignalCall, Rect, is_stand_in_handle, FAKE_*, STATE_*}` | the evaluation hand-off |
| `status::{Status, Counters, TuneState, WaitState, MipBias, MipBiasOutcome}` | introspection data |
| `layout::*` | unanchored offsets, documented |

Windows:

| fn | what |
|---|---|
| `check(&Module, &Path) -> Result<&'static Addresses, StandDown>` | gate (renderer files, sites, globals, offsets) |
| `detect_renderer(&Path) -> Renderer` | the renderer files alone |
| `install(InstallOptions) -> Result<Installed, InstallError>` | the eight detours at once, then the mip-bias operand |
| `set_params(Params)` / `params()` | policy, live |
| `set_upscale_handler(Option<&'static dyn UpscaleHandler>)` | the evaluation handler |
| `set_depth_alias(bool)` | the SS path's depth alias (needed with flags 3) |
| `set_wait_latch(bool) -> bool` / `wait_state()` | `device+0x10DA` |
| `take_game_reset() -> bool` | the game's reset latched by the SS enqueue, for DLAA at the temporal pass (EvalISP takes it itself) |
| `maintain()` | the per-frame tuning, now |
| `hold_downsample_volume(bool)` | hold the tonemap filter's `mDownsampleVolume` off (`[Upscaling] DownsampleVolume = false`), under the game's lock; `false` gives the game its value back |
| `status()`, `counters()`, `tune_state()`, `last_camera()`, `last_jitter()`, `gate_result()`, `addresses()`, `is_installed()` | introspection |
| `views::{RenderConfig, Camera, RenderSettings, Tonemap, TaaFilter, Device, viewport_at, resource_of}` | typed, checked object views |
| `ModuleImage` | `mhw_win::Module` as `sig::Image` |
| `dev::{viewports, fields, taa}` (feature `dev`) | raw dumps for probes |

## The detours

All eight are prepared, then written in one step (`mhw_win::install_detours`).
Each passes the call to the game when `Params::hijack` is off (except the
camera, which then publishes the engine's own jitter and writes none).

| site | does |
|---|---|
| `CAMERA_SETUP` `0x14228f3b0` | per render frame: tuning; before the original, for the main viewport (active, pass 0, no own target, first in the frame): the frame's jitter (`JitterSequence`, `Params::jitter`/`jitter_phases`) to `sCamera+0x38/+0x3c`; after it: `CameraFrame` from the viewport's unjittered `V, P, Vprev, Pprev` → `bus::CameraReady` |
| `OPTIMAL` `0x142383ef0` | `{snapped(ratio, w), flags 2/3, 0}` |
| `CREATE` `0x142384420` | stand-in handles at `ctx+0x38/+0x40`, no driver call |
| `EVAL_SS` `0x142383cf0` | `UpscaleHandler::supersampling` (reset = blend ≤ 0) |
| `EVAL_ISP` `0x142383990` | `UpscaleHandler::image_signal` (reset latched by the enqueue) |
| `SS_ENQUEUE` `0x1423913f0` | latch the game's reset (`*capture+0x38` = `taa+0x185`, or blend ≤ 0), clear `taa+0x187`, depth alias, skip with flags 2 or a missing target |
| `SCALING_OPTION` `0x14043eeb0` | Image Quality given High |
| `FATAL` `0x1421fd060` | log the engine's message (target `engine`), call through |

The handler is called on the engine's render thread inside the recording of
`call.list`; resource states at entry are in the call (`NON_PIXEL_SHADER_RESOURCE`
inputs, `UNORDERED_ACCESS` output). Whoever owns
`NVSDK_NGX_D3D12_ReleaseFeature` must drop `is_stand_in_handle` handles.

## Tuning (once per render frame)

Ratio through the engine's setter with the 0.5 alignment fix (1.0 written
directly); own scale 1.0 and dynamic resolution off; AA word `rs+0xe9e4`
held at `0x0001` (the AA apply handler `0x14043dcb0` is not a gate site, so
the hold is re-applied every frame before the TAA draw, and the game's value
put back when released); SSLR dither/g-buffer jitter and contact/facial
noise off (originals kept and restored); sampler bank 1 once the mip-bias
operand was rewritten.

## Gate and tables

`tools/gen_sites.py [--exe PATH] [--check]` regenerates `src/sites.rs` from
`docs/re/signatures.json`, checking every row against the reference
executable, Steam build 15539686 (`MHW_EXE`, default `MonsterHunterWorld.exe`
two levels above the worktree). Its RVAs are only where the gate looks
first.

The gate does not look at which build the executable is. It needs every
site to match its signature exactly once and verify, every global read out
of a site, and every field read out of the code, its anchors agreeing; all
or nothing. Anchors have their displacement wildcarded where that stays
unique, so a moved field is read from the code. A field whose object also
has unanchored offsets (`layout`; `FieldInfo::layout`) must read the
reference build's value: a move means the layout changed under those, and
the gate stands down. `layout` objects without an anchored field
(`viewport`, `device`, `capture`, `texture`, `native`, `view`) cannot be
checked and are used as the reference build has them.

## Tests

`cargo test -p mhw-engine` (host). `gate::tests::the_reference_executable_passes_the_whole_gate`
runs the full gate on the executable when present, else is skipped.

## Logging targets

`gate`, `engine`, `jitter`, `hijack`, `tune`.

## Open runtime questions (dev probes)

E1 (main viewport rect = display; logged once if not), E2 (viewports per
frame), E7 (`sCamera+0x44` blend; logged once), the SS dense-velocity
convention, the frame-latency wait helper's calls/timeouts (`0x1423ade30` is
not a gate site; counting needs a site and a detour).
