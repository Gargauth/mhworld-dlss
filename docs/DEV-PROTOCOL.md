# Dev protocol (dev builds only)

Built only with `--features dev`. A release build has no listener, no probes,
no capture code and no `[Dev]`/`[Experimental]` settings.

## Transport

- TCP, address from `[Dev] Listen` (default `127.0.0.1:47800`; use
  `0.0.0.0:47800` to reach it from another machine on the LAN).
- One request per line: UTF-8 JSON object terminated by `\n` (at most 1 MiB;
  at most 4 KiB before authentication). Requests never carry a `"binary"`
  payload: a request announcing one is answered `bad_request` and the
  connection is closed.
- One response per request, same framing. If a response carries binary data,
  its JSON has `"binary": <byte count>` and exactly that many raw bytes follow
  the newline.
- Server-initiated events (log stream, watches) are lines with `"event"`
  instead of `"id"`. Events never block the game: when a client's queue is
  full they are dropped and reported — a log event's `missed` counts lines
  lost before it (fell out of the ring or dropped), a watch event's
  `dropped` counts watch events lost before it, and `state.dev.dropped_events`
  totals the connection's drops.
- Several clients may connect (at most 8 at once, authenticated or not; more
  are closed on accept); each is served by its own thread. Requests that
  touch the game run on the render/main thread at a safe point (see
  *Execution points*) and the response is sent when done.

## Authentication

First request must be `{"id":1,"method":"auth","params":{"token":"…"}}`.
The token is read from `[Dev] TokenFile` (created with 32 random bytes as hex
on first start, logged by path only, never by value). Anything else before a
successful `auth` closes the connection, and so does not completing the
`auth` line within 30 s of connecting (an absolute deadline).

## Request / response

```json
{"id": 7, "method": "state", "params": {}}
{"id": 7, "ok": true, "result": {…}}
{"id": 7, "ok": false, "error": {"code": "not_ready", "message": "…"}}
```

Error codes: `bad_request`, `unauthorized`, `unknown_method`, `not_ready`
(the subsystem has not started), `timeout`, `refused` (e.g. write outside the
allowed ranges), `internal`.

## Execution points

`frame` = run at the start of the next main-loop iteration (before the
wait helper; the mod reaches it right after the engine's Present returns);
`camera` = inside the camera-setup detour after the original; `slot` =
inside the upscale slot (after the frame's DLSS evaluation: EvalISP, or the
temporal pass / EvalSS at ratio 1), with the engine's command list open;
`present` = before Present. A request names its point when it matters; the
default is `frame`.

## Methods

| Method | Params | Result |
|---|---|---|
| `auth` | `token` | `{build, game_build, features}`; `game_build` is the executable's PE timestamp and image size |
| `hello` | – | mod version, git commit, game build (as in `auth`), gate status, which subsystems run |
| `state` | – | snapshot: settings, policy, frame info, render/display size, scale, DLSS feature state, FG state, counters |
| `settings.get` | – | effective settings incl. dev/experimental |
| `settings.set` | `{section, key, value}` or list | applies live (same validation as the file); reports which need restart |
| `symbols` | – | resolved address book (name → address) and struct field table |
| `mem.read` | `{addr \| symbol, offset, len, as}` | bytes (binary) or typed (`u8/u16/u32/u64/f32/f64/mat4`) |
| `mem.write` | `{addr \| symbol, offset, value, as}` | only inside resolved engine structs (`RenderConfig`, `Camera`, `RenderSettings`, `TaaFilter`, `Device`) unless `force:true` |
| `watch` | `{name, expr: [{symbol, offset, as}], every_frames, point}` | starts streaming `{"event":"watch","name",…,"frame",values}` |
| `unwatch` | `{name}` | – |
| `log.subscribe` | `{level, targets}` | streams `{"event":"log",…}` for records that passed the logger's levels (`[General] LogLevel`, `[Dev] LogTargets`); the subscription narrows, never raises, verbosity |
| `log.tail` | `{lines}` | last lines from the ring |
| `capture` | `{resources: [colour, depth, mv, mv_raw, pass_depth, dlss_out, hudless, game_mv, game_depth], frames: n ≤ 16, point: "slot"}` | per resource and frame: format, size, bytes (binary, one response per part) + JSON sidecar (frame info, camera, jitter, settings); also written under `CaptureDir/<stamp>-r<n>/`. Captures are taken only at `slot`; `game_mv`/`game_depth` exist only with Supersampling flags (`InputPath = Supersampling`, flags 3); `mv_raw` (packed `tGBuffer4`) and `pass_depth` (the depth the temporal pass picked from the DSVs) come from the temporal pass record and exist only when that pass ran this frame; a resource not offered at the slot answers `not_ready` naming it, with the mod's reason when it gave one; `hudless` is the previous frame's converted image (frame generation with `HudLess` only); `backbuffer` is not available (`not_ready`) |
| `probe.jitter` | `{frames}` | per frame: written units, phase, proj[8]/proj[9] read back, derived divisor; verdict display/render |
| `probe.reset` | `{frames}` | per frame: game history flag, camera-cut metric, reset sent to DLSS/FG |
| `probe.hdr` | `{frames}` | min/max/mean/percentiles of the colour fed to DLSS (GPU reduction) |
| `probe.mv` | `{frames}` | compare `game_mv` (game dense velocity, `taa+0x1a8`) with `mv_raw` (decoded `tGBuffer4`): best-fit scale/sign, residual; needs both at the same slot (Supersampling flags with the temporal pass running), else `not_ready` |
| `probe.depth` | `{frames}` | compare `game_depth` (the SS depth copy) with `pass_depth` (the temporal pass's picked DSV depth): size, format, difference stats; never `depth`, which in the SS path is `game_depth` itself. `not_ready` when either is absent or both are the same resource |
| `probe.wait` | `{seconds ≤ 60}` | `device+0x10d9/+0x10da/+0x10f0` sampled every frame (presented, latch, waitable); wait-helper `calls`, `waits`, `timeouts` are `null` (the helper `0x1423ade30` is not a gate site, nothing counts them) |
| `probe.modules` | – | module load order with the time a poller first saw each module (ms since process start: 50 ms polls until the device exists, 500 ms after), and whether it was loaded before D3D12 device creation (`null` when the same poll saw both) |
| `experiment.set` | `{key, value}` | shortcut for `settings.set` in `[Experimental]` |
| `trace` | `{frames}` | per-frame timeline of the mod's events (camera, slot, evaluate, tags, markers, present) with QPC timestamps |

## Client

`tools/devctl/devctl.py` (Python 3.10+, stdlib only):

```
devctl --host 192.168.1.20 --token-file dev.token state
devctl … set Experimental JitterScale Render
devctl … capture colour depth mv --frames 1 --out ./cap
devctl … probe jitter --frames 64
devctl … watch cfg+0x200:f32 --every 1
devctl … log --level debug --targets render,fg
devctl … repl            # JSON lines interactively
```

Captures are saved as raw bytes + `.json` sidecar, and converted to PNG/EXR
where the format allows (`--convert`, needs numpy).
