# devctl — dev-protocol client

Client for the TCP dev server of a `--features dev` build
(`docs/DEV-PROTOCOL.md`): a CLI (`devctl.py`), a Python module for scripts
(`devclient.py`), capture conversion (`capture.py`) and a fake server with
synthetic data (`mock_server.py` + `mock_game.py`) for development and tests.

Python 3.10+, standard library only. `numpy`, if installed, speeds up
`capture --convert`; without it the conversion is pure Python (a few seconds per
megapixel).

## CLI

```
devctl [--host H[:PORT]] [--port P] [--token T | --token-file F] [--timeout S] [--json] COMMAND …
```

`--timeout` defaults from the request: `probe wait` waits `seconds + 10`, frame
counts (`capture`, `probe`, `trace`, `call` with `frames`) `5 + 0.1·frames + 10`
(at least 60), anything else 60.

Defaults: `--host` from `$DEVCTL_HOST` or `127.0.0.1`, port `47800`
(`$DEVCTL_PORT`), token from `$DEVCTL_TOKEN` or the file `$DEVCTL_TOKEN_FILE` /
`dev.token` (the game's `[Dev] TokenFile`; copy it to the client machine).
`--json` (before or after the command) prints one-line JSON, and JSON lines for
streams. Exit status: 0 ok, 1 the server answered an error (`code: message` on
stderr), 2 connection, authentication or usage problem.

| Command | Method | Notes |
|---|---|---|
| `hello`, `state`, `symbols` | same | JSON result |
| `settings [SECTION]` | `settings.get` | |
| `set SECTION KEY VALUE [SECTION KEY VALUE …]` | `settings.set` | values sent as strings (INI syntax); several triples → one atomic request |
| `experiment KEY VALUE` | `experiment.set` | |
| `mem read EXPR [--as T] [--count N \| --len BYTES] [--out FILE]` | `mem.read` | typed → `expr = value` lines; untyped → hex dump or `--out` raw file |
| `mem write EXPR VALUE [--as T] [--force]` | `mem.write` | mat4: 16 numbers `a,b,…`; bytes: hex |
| `watch EXPR… [--as T] [--every N] [--point P] [--name N]` | `watch` | prints `frame=… expr=value …` per event; unwatches on exit |
| `unwatch NAME` | `unwatch` | |
| `log [--level L] [--targets a,b]` | `log.subscribe` | prints the formatted log lines |
| `tail [--lines N]` | `log.tail` | |
| `capture RES… [--frames N] [--point P] [--out DIR] [--convert]` | `capture` | see *Captures* |
| `probe jitter\|reset\|hdr\|mv\|depth [--frames N]`, `probe wait [--seconds S]`, `probe modules` | `probe.*` | JSON result |
| `trace [--frames N]` | `trace` | |
| `call METHOD [PARAMS_JSON]` | any | raw request; prints each response part |
| `repl` | any | see below |

Streams (`watch`, `log`) run until `--count N` events, `--duration S` seconds,
or Ctrl-C. Points: `frame` (default), `camera`, `slot`, `present`.

**Memory expressions**: `BASE[+OFF…][:TYPE]` where `BASE` is a symbol from
`symbols` (or an alias such as `cfg`) or an absolute address (`0x1451c4480`),
offsets are decimal or hex and may be negative, and `TYPE` is one of `u8 u16
u32 u64 f32 f64 mat4 bytes`. Examples: `cfg+0x200:f32`, `Camera:mat4`,
`taa+0x1a8:u64`, `0x1451c4480+8:u32`.

**repl**: reads JSON lines from stdin and writes JSON lines to stdout. Input is
either a request object (`{"method": "state", "params": {}}`, the id is
assigned) or `METHOD [PARAMS_JSON]` (`mem.read {"symbol":"cfg","len":16}`).
Responses are printed with binary payloads replaced by their length; events
are printed as they arrive. `quit` or EOF ends it. Suitable for driving from
another process.

## Python API

Run scripts with `tools/devctl` on `sys.path` (or from that directory).

```python
from devclient import DevClient, DevError

with DevClient.connect("192.168.1.20", token_file="dev.token") as dev:
    print(dev.hello()["version"], dev.state()["scale"])
    dev.experiment_set("JitterScale", "Render")
    print(dev.probe("jitter", frames=64)["verdict"])

    print(dev.mem_read("cfg+0x200:f32"))            # [0.5]
    raw = dev.mem_read("Camera", length=128)        # bytes
    dev.mem_write("dev+0x10da", 1, as_="u8")

    dev.watch("scale", ["cfg+0x200:f32"], every_frames=1)
    for ev in dev.events("watch", timeout=1.0):     # stops after 1 s of silence
        print(ev["frame"], ev["values"])
        break
    dev.unwatch("scale")

    from capture import save_part
    for part in dev.capture(["colour", "mv_raw"], frames=1, point="slot"):
        save_part(part.meta, part.data, "cap", convert=True)
```

- `DevClient.connect(host, port=47800, *, token=None, token_file=None, timeout=30.0, connect_timeout=5.0, on_event=None)`
  connects and authenticates (`AuthError` if refused); `host` may be `host:port`.
  `client.server_info` holds the `auth` result.
- `call(method, params=None, *, timeout=None) -> result`,
  `call_raw(…) -> Response(id, result, binary, raw)`,
  `call_parts(…) -> Iterator[Response]` (multi-part answers).
- Wrappers: `hello()`, `state()`, `settings_get()`,
  `settings_set(section, key, value)`, `settings_set_many([(s, k, v) | dict…])`,
  `experiment_set(key, value)`, `symbols()`,
  `mem_read(target, *, offset=0, length=None, as_=None, count=None) -> bytes | list`,
  `mem_write(target, value, *, as_=None, offset=0, force=False)`,
  `watch(name, exprs, *, every_frames=1, point=None)`, `unwatch(name)`,
  `log_subscribe(level="info", targets=None)`, `log_tail(lines=100) -> list[str]`,
  `capture(resources, *, frames=1, point=None, timeout=None) -> list[CapturePart(meta, data)]`,
  `probe(kind, *, timeout=None, **params)`, `trace(frames=1, *, timeout=None)`.
- Events: `next_event(timeout=None) -> dict | None`, `events(kind=None, timeout=None)`;
  or pass `on_event=callback` (called on the reader thread) instead of queueing.
- Errors: `DevError(code, message, data)` for `ok: false`, `AuthError`,
  `ConnectionClosed`, `ProtocolError`, `TimeoutError`.
- Helpers: `Expr.parse(text)`, `decode_values(bytes, type)`,
  `encode_values(value, type)`, `FrameReader`, `encode_frame`.

Calls are thread-safe; a background thread reads frames and routes responses
by id, so events may arrive at any time between responses.

## Captures

Each part (one per resource and frame) is saved in `--out` as
`<resource>_<frame>.raw` (the bytes as received, rows `row_pitch` apart) and
`<resource>_<frame>.json` (the response metadata plus `raw`, `bytes`, and with
`--convert` the name of the converted file or `null`). `--convert` writes:

| Format | Output |
|---|---|
| `B8G8R8A8_UNORM[_SRGB]`, `R8G8B8A8_UNORM[_SRGB]` | 8-bit RGB PNG (alpha dropped) |
| `R10G10B10A2_UNORM` | 8-bit RGB PNG (10 → 8 bit, alpha dropped) |
| `R11G11B10_FLOAT`, `R16G16B16A16_FLOAT` | PFM `PF` (alpha dropped) |
| `R16G16_FLOAT`, `R32G32_FLOAT` | PFM `PF`, R/G = channels, B = 0 |
| `R32_UINT` (`tGBuffer4`, packed MV) | PFM `PF`: R = high half (x), G = low half (y), B = 0 |
| `R32_FLOAT`, `D32_FLOAT`, `R32_TYPELESS` | PFM `Pf` |

PFM is little endian (scale `-1.0`), rows stored bottom first as the format
requires. `capture.read_pfm` / `capture.read_png_rgb` read them back.

## Wire details the client relies on

Where `docs/DEV-PROTOCOL.md` leaves the shape open, the client (and the mock)
use the following; the server must match:

- Multi-part responses: every part but the last carries `"more": true`; all
  parts share the request id. `capture` sends one part per resource and frame.
- Addresses travel as hex strings (`"addr": "0x1451c4480"`), never as JSON
  numbers (64-bit values exceed double precision); `offset` and `len` are
  integers.
- `mem.read`: without `as` (or `as: "bytes"`) the result is `{addr, len}` plus
  the binary payload; with a type, `{addr, as, values: [...]}` where `len`
  (default: one element) must be a multiple of the type size and `mat4`
  values are lists of 16 floats. `mem.write`: `value` is a number, a list of
  numbers, 16 numbers for `mat4`, or a hex string for `bytes`; result
  `{addr, written}`.
- `watch` event: `{"event": "watch", "name", "frame", "point", "values": [...]}`
  with one value per expression, in order.
- `log` event: `{"event": "log", "seq", "time", "level", "target", "message",
  "fields", "line"}`; `line` is the formatted log line, `fields` its trailing
  `key=value` pairs (string values), `seq` its ring position (a `missed`
  count appears when the ring dropped lines in between). `log.subscribe`
  `targets` is a list (empty = all). `log.tail` result: `{"lines": [str]}`.
- `settings.set` result: `{"changes": [{section, key, value, restart,
  note?}], "restart_required": bool}`; a list request is all-or-nothing.
  Values may be JSON scalars or INI-syntax strings. Validation is the file's:
  text that is not a value is rejected (`bad_request`), a number out of range
  is clamped and applied with a `note`.
- `capture` records at the upscale slot (`point` may only be `slot`, the
  default); `backbuffer` is not a slot resource and answers `not_ready`.
  Part result: `{resource, frame, index, count, format, dxgi, width,
  height, row_pitch, point, saved, sidecar: {frame, camera, jitter, settings}}`
  with `height * row_pitch` bytes (the last row may omit its padding).
  `format` is the DXGI name without prefix (a numeric `dxgi` id is accepted).
- Probe results: every per-frame probe answers `{"frames": [...]}`;
  `probe.jitter` adds `verdict` (`display | render | same_size |
  inconsistent | no_jitter`) and the per-axis median `divisor`
  (`−written / proj[8|9]`); `probe.wait` has `latch` and `calls`, `waits`,
  `timeouts` (`null`: the wait helper is not hooked); `probe.modules` has
  `modules` and `device_created_ms`. `crates/dev/README.md` lists every field.
- Auth failure: the server may answer `unauthorized` before closing; the
  client treats either the error or the bare close as `AuthError`.

## Mock server

```
python3 mock_server.py --listen 127.0.0.1:47800 --token-file dev.token [--frame-ms 16.6]
```

Implements every method with synthetic data in the server's shapes: a frame
counter, five writable engine structs (`RenderConfig`, `Camera`,
`RenderSettings`, `TaaFilter`, `Device`; aliases `cfg cam ctx rs settings taa
ss_node dev`) and a read-only `GameCode` page (writable with `force`), the
settings of `docs/ARCHITECTURE.md` with their validation, a log ring with a
`perf` record every 30 frames, and captures of the slot resources in the
real formats at 64×36 (render) / 128×72 (display), rows padded to 256
bytes. Each handled request is logged (`debug`, target `dev`) before its
response is sent, so subscribers see interleaved events. From Python:
`with MockServer(token="t") as s: DevClient.connect(*s.address, token="t")`.

## Tests

```
python3 -m unittest discover tools/devctl
```

Framing (chunking, truncation, malformed frames), typed values, expressions,
authentication (success, wrong token, silent close, request before auth),
events interleaved with responses and binary payloads, timeouts and closure,
every method through the API and every CLI command against the mock, and
every capture format's decoding and file output. The numpy path is tested
when numpy is installed (`uv run --with numpy python -m unittest discover tools/devctl`).
