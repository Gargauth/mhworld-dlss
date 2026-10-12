# mhw-dev

The dev-build control server (`docs/DEV-PROTOCOL.md`, `docs/ARCHITECTURE.md`
§ mhw-dev): newline-delimited JSON over TCP with binary payloads, token
authentication, requests that run at the mod's execution points, the log
stream, memory peek/poke, watches, captures and the probes that answer the
RE's open runtime questions. Client: `tools/devctl`.

The crate exists only in dev builds: `mhw-dlss/dev` makes it a dependency,
and it turns on the `dev` side of every crate it reads (`mhw-core`,
`mhw-engine`, `mhw-render`, `mhw-ngx`, `mhw-fg`). A release DLL contains
none of it; `DEV_BUILD_MARKER` (`MHW_DLSS_DEV_BUILD`) is kept in every dev
DLL so CI can tell them apart.

## Public API

```rust
pub struct DevConfig { pub mod_folder: PathBuf, pub listen: SocketAddr, pub token_file: PathBuf, pub capture_dir: PathBuf }
pub trait Host: Send + Sync {
    fn settings(&self) -> mhw_core::config::Settings;
    fn set_setting(&self, section: &str, key: &str, value: &str) -> Result<Option<Diagnostic>, Diagnostic>;
    fn state(&self) -> serde_json::Value;
}
pub fn start(config: DevConfig, host: &'static dyn Host) -> std::io::Result<()>;
pub fn on_frame(frame: u64);
pub fn on_camera(camera: &CameraFrame, jitter: &JitterSample);
pub unsafe fn on_slot(list: *mut c_void, slot: &SlotContext);
pub fn on_present(frame: u64);
pub struct SlotContext { pub frame: FrameInfo, pub camera: Option<CameraFrame>, pub jitter: Option<JitterSample>, pub resources: Vec<SlotResource>, pub absent: Vec<(&'static str, &'static str)> }
pub struct SlotResource { pub name: &'static str, pub resource: *mut c_void, pub state: u32 }
pub fn trace_event(name: &'static str, frame: u64);
pub static DEV_BUILD_MARKER: &[u8];
```

The pure modules are public for tests and tools: `frame` (framing), `proto`
(requests, errors, params), `expr` (memory expressions, typed values),
`symbols` (address book, write whitelist, field table), `settings` (JSON
form, batch staging), `logline` (ring line → event), `exec` (point queue),
`trace`, `modules` (module log), `token`, `analysis` (probe arithmetic) and
`game` (the game seen by the server).

## Wiring (what `mhw-dlss` does)

1. After settings are read: `start(DevConfig { mod_folder, listen:
   settings.dev.listen, token_file: settings.dev.token_file.into(),
   capture_dir: settings.dev.capture_dir.into() }, &HOST)`. Relative paths
   resolve against `mod_folder`. The token file is created with 32 random
   bytes (`BCryptGenRandom`) as hex on first start and reused afterwards;
   the log names its path, never its value.
2. `on_frame(frame)` at the start of each main-loop iteration (before the
   wait helper), `on_camera(&camera, &jitter)` from the `CameraReady`
   subscriber, `on_slot(list, &slot)` inside the upscale handler with the
   engine's list open (resources: `colour, depth, mv, mv_raw, pass_depth,
   dlss_out, hudless, game_mv, game_depth` as available, each with its
   current state on `list`; `SlotContext::absent` lists the ones not offered
   at this slot with the reason, which a capture asking for them reports), `on_present(frame)` before Present. Each point is one atomic
   load when nothing is queued for it (`on_camera` and `on_slot` also copy
   the camera/frame info under an uncontended lock for the sidecars and
   probes).
3. `trace_event(name, frame)` at the mod's other timeline events
   (`evaluate`, `tags`, `markers`, …); the four points record `frame`,
   `camera`, `slot`, `present` themselves. Recording happens only while a
   `trace` request runs.

`Host::set_setting` must apply a value exactly like the file would and
report restarts through the same `Diagnostic`s; `Host::state` returns the
mod's policy and per-frame state as a JSON object (its keys are merged into
the top level of `state`).

## Capture

`src/capture_glue.rs` is the only code naming `mhw_render::capture`; it is
always compiled in Windows dev builds. Outside the game (host tests without
a fake capture service) `capture`, `probe.hdr`, `probe.mv` and
`probe.depth` answer `not_ready`.

## Threads

`mhw-dev-listen` accepts; each client gets a reader thread
(`mhw-dev-client`: auth, then requests one after another) and a writer
thread (`mhw-dev-write`) fed by a bounded queue, so game threads that send
watch events never block on a socket (a full queue drops events and counts
them). `mhw-dev-log` streams the log ring every 50 ms. `mhw-dev-modules`
polls the module list (50 ms until the D3D12 device exists, 500 ms after).
Requests that touch the game queue a task at their point and wait for it
(5 s for one-shot requests, longer for multi-frame ones; `timeout` when the
point does not come, e.g. in a loading screen).

## Methods

Requests, framing and errors as in `docs/DEV-PROTOCOL.md`; the shapes below
are what `tools/devctl` relies on (its mock answers the same).

| method | params | result |
|---|---|---|
| `auth` | `token` | `{build: "dev", game_build, features}`; `game_build`: `"timestamp 0x…, image size 0x…"` from the executable's PE header |
| `hello` | – | `{version, commit, game_build, gate, subsystems: {engine, render, ngx, fg, dev}, features}` (`commit` from `MHW_DLSS_COMMIT` at build time) |
| `state` | – | the host's object plus `frame`, `settings`, `last_slot`, `last_jitter`, `last_camera_frame`, `last_camera_cut`, `introspection: {engine, render, ngx, fg}`, `dev: {listen, clients, pending}` |
| `settings.get` | – | section → key → value, file spellings |
| `settings.set` | `{section, key, value}` or a list | `{changes: [{section, key, value, restart, note?}], restart_required}`; validated as a batch first (all or nothing), then applied through the host |
| `experiment.set` | `{key, value}` | as `settings.set` on `[Experimental]` |
| `symbols` | – | `{symbols, aliases, fields, regions}`: struct addresses, `site.<NAME>`, `global.<NAME>`; aliases `cfg cam ctx rs settings taa ss_node dev`; fields per struct with offset (this build) and type; the writable regions |
| `mem.read` | `{addr \| symbol, offset, len, as, point}` | untyped: `{addr, len}` + binary; typed: `{addr, as, values}`. Any readable address (checked reads); `len` ≤ 1 MiB |
| `mem.write` | `{addr \| symbol, offset, value, as, force, point}` | `{addr, written}`; only inside one resolved engine struct unless `force` |
| `watch` | `{name, expr: [{symbol\|addr, offset, as} \| "cfg+0x200:f32"], every_frames, point}` | `{name, count, every_frames, point}`; events `{event: "watch", name, frame, point, values, dropped?}` (addresses resolved once, `null` for an unreadable value; `dropped`: watch events lost on a full outbox before this one) |
| `unwatch` | `{name}` | `{name}` |
| `log.subscribe` | `{level, targets}` | `{level, targets}`; events `{event: "log", seq, time, level, target, message, fields, line, missed?}` (`missed`: lines lost before this one, out of the ring or dropped on a full outbox); filters the ring, which holds only records that passed the logger's levels |
| `log.tail` | `{lines}` | `{lines}` (up to the ring's 4096) |
| `capture` | `{resources, frames ≤ 16, point: "slot"}` | one part per resource and frame: `{resource, frame, index, count, format, dxgi, width, height, row_pitch, point, saved, sidecar: {frame, camera, jitter, settings}}` + binary; files under `CaptureDir/<stamp>-r<n>/<resource>_<frame>.raw/.json` |
| `probe.jitter` | `{frames ≤ 1024}` | `{frames: [{frame, phase, written, px, proj8, proj9, divisor}], verdict, divisor, render, display}` |
| `probe.reset` | `{frames}` | `{frames: [{frame, reason, reset_dlss, game_reset, game_resets, history_invalid, camera_cut: {position_jump, turn, projection_change, cut}, fg, reset_fg}]}` |
| `probe.hdr` | `{frames ≤ 16}` | `{frames: [{frame, min, max, mean, p50, p99, p999, above_one, nonfinite}]}` |
| `probe.mv` | `{frames ≤ 16}` | `game_mv` vs `mv_raw`: `{frames: [{frame, scale, sign, residual, fit, scale_ndc, samples, textures}]}` |
| `probe.depth` | `{frames ≤ 16}` | `game_depth` (copy) vs `pass_depth` (dsv): `{frames: [{frame, copy, dsv, diff, flipped_fraction}]}`; `not_ready` if they are the same resource |
| `probe.wait` | `{seconds ≤ 60}` | `{seconds, frames, samples, latch, latched_frames, presented_frames, waitable, calls, waits, timeouts, note}` |
| `probe.modules` | – | `{modules: [{order, name, path, base, ms, seen_at_start, before_device, loaded}], device_created_ms, dev_started_ms, resolution_ms, polls}` |
| `trace` | `{frames ≤ 256}` | `{qpc_frequency, frames: [{frame, events: [{what, qpc}]}]}` |

## Probes: what they measure

- **jitter** (slot): the units the camera detour wrote against `fProj[8]`,
  `fProj[9]` of the camera buffer the temporal pass bound. The engine builds
  `J` with row 3 `= (u/D_w, u/D_h)` and `fProj[8|9] = −u/D`
  (`ReJitterCamera.md` §2.4), so `divisor = −written / proj`. The verdict
  compares the per-axis median with the display and render sizes (1 %):
  `display` is the RE's claim, `render` the 0.2 assumption, `same_size` when
  the two sizes are equal, `no_jitter` when nothing is measurable
  (`JitterScale = Off`, phase centre, no camera buffer).
- **reset** (slot): the frame's reset reason and whether DLSS was reset, the
  engine's game-reset counter delta, `taa+0x185`, the camera-cut metric
  against `CUT_THRESHOLDS` and frame generation's last constants.
- **hdr** (slot): the render crate's GPU reduction of the colour DLSS gets.
- **mv** (slot): captures of the game's dense velocity (`game_mv`) and
  `tGBuffer4` (`mv_raw`); per axis the least-squares scale of game velocity
  per decoded render pixel (`ndc·(−w/2, +h/2)`, the decode DLSS is given),
  its sign, the RMS residual, and the scale against raw NDC.
- **depth** (slot): captures of the supersampling path's depth copy
  (`game_depth`) and the picked DSV depth (`depth`): sizes, formats, value
  statistics, `|copy − dsv|` statistics and the fraction of texels that
  match better flipped (`1 − dsv`).
- **wait** (frame): `device+0x10d9/+0x10da/+0x10f0` sampled every frame. The
  wait helper `0x1423ade30` is not a gate site, so calls, waits and
  timeouts are `null` until it gets a site and a detour.
- **modules**: Windows keeps no cheap per-module load time, so a poller
  records when it first saw each module and when render first counted a
  created device; same-poll or pre-server ambiguity is `before_device:
  null`.

## Tests

`cargo test -p mhw-dev` (host): framing, requests, expressions and typed
values, the whitelist, settings staging, log-line parsing, the point queue,
the trace buffer, the module log, the token file, every probe's arithmetic,
and `server::tests`: the server on localhost against a fake mod and a fake
game (memory, camera buffer, capture service) driven with JSON lines through
auth, settings, memory, watches, the log, captures, every probe and `trace`.

To develop a client without the game: `MHW_DEV_SERVE_SECS=600 cargo test -p
mhw-dev serve_fake_game -- --ignored --nocapture` prints `HOST:PORT TOKEN`
and serves the fake game; `tools/devctl` connects to it with `--host` and
`--token`.

## Logging

Target `dev`: start (address, token file path), clients, requests (`debug`,
fields `method`, `id`), settings and memory writes made by clients (`info`).
