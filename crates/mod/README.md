# mhw-dlss (cdylib)

The mod itself: `DllMain`, the start-up order, the wiring between the crates
and the policy that turns `mhw-dlss.ini` into decisions. Contract:
`docs/ARCHITECTURE.md` § *mhw-dlss*; each crate's README *Wiring* section is
what this crate does for it.

## Modules

| module | pure | what |
|---|---|---|
| `policy` | yes | `Policy::from_settings(&Settings)`: engine `Params`, input path, DLSS preset/sharpness/`IsHDR`, reset policy, HUD-less, wait latch, mip bias, frame generation `Options`, MV decode; `feature_desc`, `dlss_is_hdr`, `mip_bias`, `latch_write`, `SS_MV_SCALE` |
| `reset` | yes | `reset_reason(policy, cause)`, `CutDetector` (camera cuts from the engine's unjittered camera) |
| `fallback` | yes | `Fallback`: every slot's outcome; no temporal pass for 5 slots → the game's path *holds* until the pass is back; the pass running with nothing evaluated (unusable inputs, failures, the 3-frame pause after a failure) for 12 slots, or the pass relapsing 3 times → *given up*; a settings change that may fix it retries |
| `frames` | yes | `CameraRing` (camera + jitter by render frame), `KeepRing` (references held `KEEP_FRAMES` = 4 frames) |
| `store` | yes | `Store`: settings in force, live/restart groups (`Replaced`), restart groups reported once, generation |
| `ini_edit` | yes | `set_key(text, section, key, value)`: one value changed, everything else kept; `set_key_in_file(bytes, …)`: the same in the file's encoding (UTF-8/UTF-16), `None` when unchanged, refused when undecodable; the overlay writes the result to `mhw-dlss.ini.writing` and renames it over the file |
| `slotnames` | yes | the dev slot point's resources by name and why the others are absent (probe inputs) |
| `page` | yes | the overlay page's model (slider positions, scale snapping) |
| `sys::boot` | Windows | start-up order (below) |
| `sys::procs` | Windows | `KERNEL32!GetProcAddress`, `LoadLibraryExW/A` IAT hooks; `d3d12.dll` #101 delay slot; launcher hand-off |
| `sys::gate` | Windows | engine gate polling (and on renderer lookups), detours with `Busy` retry, stand-down |
| `sys::runtime` | Windows | paths, store, policy, fallback; applying a change to every subsystem |
| `sys::reload` | Windows | hot reload (500 ms) |
| `sys::slot`, `sys::evaluate` | Windows | engine `UpscaleHandler`, render `PassHandler`, one DLSS evaluation (states, NGX, RCAS, references) |
| `sys::live` | Windows | per-frame state: cameras, display, reset decision, supersampling inputs |
| `sys::fg_glue` | Windows | frame generation inputs, HUD-less tag, wait latch, submit marker |
| `sys::swap` | Windows | factory registration point, `CreateSwapChainForHwnd`, `Present` |
| `sys::overlay` | Windows | ReShade add-on settings page (public settings) |
| `sys::devwire` | Windows, `dev` | dev server start, `Host`, the `slot` point |

## Start-up (`DllMain` → thread `mhw-boot`)

Under the launcher the game's main thread is still suspended while this
runs: the launcher injects the mod with a remote `LoadLibraryW`, then waits
for the named event `Local\mhw-dlss-booted` (`handoff::BOOTED_EVENT`, at
most `handoff::BOOT_TIMEOUT` = 10 s) before `ResumeThread`.

0. **`DllMain`** (loader lock: table writes only, no library loads, no
   waits, no log): the executable's `KERNEL32!GetProcAddress`,
   `LoadLibraryExW/A` IAT slots (`procs::install_imports`), so they are in
   before the game's own code runs whoever loaded the mod; then the boot
   thread. Until boot step 7, a renderer lookup (`D3D12*`,
   `CreateDXGIFactory*`, `NVSDK_NGX_D3D12*`, `d3d12.dll` #101) or the
   game's `d3d12.dll` load waits for the boot (`boot::wait_active`, once,
   at most `BOOT_TIMEOUT`); when the mod is not active they pass everything
   through and still load the `renderer` libraries.
1. Mod folder = this DLL's folder. `mhw-dlss.ini` there is read (written
   with `Settings::write_default` when missing; nothing is migrated —
   0.2's file is converted by the release scripts).
2. Log (`[General] LogLevel`, dev: `[Dev] LogTargets`; file in the mod
   folder, `%TEMP%\mhw-dlss` as fall-back), diagnostics, crash logger, the
   import hooks' outcome.
3. `[General] Enabled = false`, or the game's renderer files say DirectX 11:
   stop here (inactive: the boot is finished, the hooks pass through).
4. Frame generation (`[FrameGeneration] Enabled`): `fg::start` — `slInit`
   on its own thread, as early as there are settings (later it loses the
   race with the engine's device and swapchain; the `renderer` libraries
   wait for it to settle) — and `fg::set_options`.
5. NGX init policy (redirect: API 21, mod folder; 10 s limit after a
   Streamline fault); `render::configure_device` (dev), `render::install`
   (its own scratch device; on failure inactive, `fg::stand_down`);
   render's factory hook (`swap::on_factory`), submit observer
   (`fg_glue::on_submit`), temporal-pass handler (`slot::on_pass`); engine
   params, depth alias, pass config and upscale handler;
   `bus::subscribe::<CameraReady>`; HUD-less.
6. `d3d12.dll` #101 delay slot (may load `d3d12.dll`, so not in `DllMain`;
   the gate patches it again once the code is unpacked).
7. Gate thread (20 ms for 10 s, then 250 ms, up to 180 s; also asked at
   every renderer/NGX `GetProcAddress`): `engine::check`, then
   `engine::install` (mip bias from policy) retried on `Patch(Busy)` at
   once and on `Patch(NotCode | Changed)` (a page in transition) at the
   next poll, until the give-up time;
   `ngx::session::set_spoof_availability(true)`; stand-down unregisters the
   handlers and `fg::stand_down()`. Then the boot is finished, active:
   waiting hooks go on, the launcher is signalled and resumes the game.
8. Dev server (feature `dev`), hot reload.

## Where DLSS runs

| ratio | `InputPath = Pass` | `InputPath = Supersampling` |
|---|---|---|
| < 1 | temporal pass: colour copied into its output unfiltered (`Skip`); EvalISP: DLSS from the post-processed colour + the pass's decoded MV and depth into the display-size target | EvalSS: inputs kept, colour copied into the history slot; EvalISP: DLSS from the post-processed colour + dense velocity + depth copy |
| 1 (DLAA) | temporal pass: DLSS into its own output (`Skip`) | EvalSS: DLSS into the history slot |

Each evaluation: `Transitions` to NGX's states, `record_on` (engine state
restored, DSV unbound), `ngx::feature::evaluate` (into the RCAS
intermediate when `Sharpness > 0`, then `sharpen::record` into the target),
states back, references held 4 frames, `feature::tick`. Jitter: exactly
the render frame's `JitterSample` (its camera's, or `engine::last_jitter`
when that frame's camera was unusable; never another frame's, none → 0),
`dlss_jitter`; cut detection and frame generation only with the frame's
own camera. Reset
(`[Experimental] Reset`): game flag, camera cut (`Auto`), resize/settings
always. Every slot that should have been evaluated is counted
(`fallback`, 0.2's `PATIENCE_WITHOUT_PASS`/`PATIENCE_UNRESOLVED`): a failed
evaluation pauses attempts for 3 frames; 5 slots without a temporal pass
make the game render the whole window with its own TAA until the pass is
seen again; 12 slots with the pass and nothing evaluated (or the pass going
away again soon after coming back, 3 times) give DLSS up for the game's own
TAA at full resolution (`Policy::dlss = false`, ratio 1; the DLSS detours
stay the mod's, so the game never evaluates its stand-in features) until a
setting that may fix it changes. Below ratio 1 a slot without an evaluation leaves the
display-size target unwritten (the 3D picture stands still on that frame),
which is why the counts are short.

The list NGX gets is on the same side of ReShade as its device (the game's
device as the game holds it): at the upscale point and EvalSS the engine's
list as handed to the detour (the proxy, with ReShade); at the temporal
pass `PassContext::engine_list`, the proxy render found from ReShade's
`Dispatch` on the stack (`render::watch_proxy_lists`, also called at
swapchain creation). If NGX holds a proxy device and that proxy is not
known, DLAA is not evaluated there (the engine's pass runs): a native list
faulted inside D3D12Core in 0.2. `record_on` sets the engine's heaps again
through the proxy after NGX.

`IsHDR` (`DlssHdr = Auto`): before tonemapping (DLAA, temporal-pass or
EvalSS colour) → HDR; at the upscale point (after tonemap) → only when the
game outputs HDR (`[sRender+0x21a]`). 0.2 set it always; `probe.hdr`
measures the range.

## Frame generation

At the slot after DLSS: `fg::submit_frame(FrameInputs { camera, jitter,
reset, depth, motion_vectors, mvec_pixel_scale })` with tag states read on
the list then; HUD-less: `hudless::set_output_transform` (engine gamma,
`fGammaCorrectEx`, PQ, swapchain format) and `begin_frame` at the slot,
`take` → `fg::tag_late(HudLess)` before Present (only when frame generation
runs and `HudLess` is on). `render_submit` on every engine-queue
`ExecuteCommandLists` (fg keeps the first of a frame). Wait latch: before
each Present, `latch_write(fg::wait_latch(policy), …)` →
`engine::set_wait_latch`.

## Swapchain and Present

The factory slot `CreateSwapChainForHwnd` is patched after frame
generation's (so the mod's hook is outermost): `render::note_presenting_queue`,
ReShade detection, overlay registration; the returned chain's `Present` is
patched: `before_present` (HUD-less, latch), dev `on_present`,
`render::note_present`, `ngx::feature::tick`, the original, dev `on_frame`
(the next main-loop iteration starts with the wait helper right after
Present returns).

## Settings at run time

`Store` holds the settings in force; the hot reload, the dev server's
`settings.set` and the overlay (which writes the file) all go through it.
Live groups are applied at once (log filter, engine params/pass config,
frame generation options, HUD-less); preset, sharpness, `IsHDR` and reset
are read per frame. Restart groups (`Enabled`, frame generation on/off,
dev server, GPU debug) are logged once and wait for a restart; a changed
render scale moves the mip bias only at the next start.

## Dev build (`--features dev`)

`mhw_dev::start` with `Host` = the store (`settings`, `set_setting`) and a
state object (`policy`, `fallback`, `generation`, `last_frame`, slot
counters, `ss_mv_scale` and its address — `mem.write … force` changes the
dense-velocity scale live to check it with `probe mv`). Points: `on_camera` (CameraReady),
`on_slot` after each evaluation (`colour, depth, mv, mv_raw, pass_depth,
dlss_out, game_mv, game_depth` as available, chosen by the pure
`slotnames`: `mv_raw`/`pass_depth` are the temporal pass's record — in the
supersampling path the newest live one — so `probe.depth` never compares
the SS depth copy with itself; every name not offered goes in
`SlotContext::absent` with its reason; with their states on the list, and
`hudless`: the previous frame's tagged image), `on_present`,
`on_frame`. Trace events: `evaluate`, `tags`, `hudless`, `submit`.

## Tests

`cargo test -p mhw-dlss` (host; `--features dev` adds the experimental
policy test): policy mapping, reset decisions, cut detection, fallback,
rings, store (live/restart, set), INI edits, page model.

## Known gaps

* Tag states for frame generation are those on the list at the slot, not
  re-read at Present.
* The `hudless` slot resource is the previous frame's image: this frame's
  exists only after the GUI's first draw.
