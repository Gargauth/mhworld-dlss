"""The simulated game behind :mod:`mock_server`: memory layout, settings
schema with validation, synthetic capture data and the method handlers.

Handlers take ``(server, connection, params)`` and return a result, a
``(result, binary)`` tuple, or a list of those for a multi-part response;
they raise :class:`MockError` for an ``ok: false`` answer.
"""

from __future__ import annotations

import math
import struct
import threading
import time
from collections.abc import Callable
from typing import TYPE_CHECKING, Any

from devclient import TYPES, decode_values, encode_values, type_size

if TYPE_CHECKING:
    from mock_server import Connection, MockServer

LEVELS = ["error", "warn", "info", "debug", "trace"]
TARGETS = [
    "boot",
    "gate",
    "engine",
    "jitter",
    "hijack",
    "tune",
    "render",
    "taa",
    "inputs",
    "dlss",
    "ngx",
    "fg",
    "sl",
    "reflex",
    "swap",
    "hudless",
    "dev",
    "perf",
    "config",
    "log",
    "crash",
]

# name -> (address, size); the five resolved engine structs are writable.
STRUCTS = {
    "RenderConfig": (0x7FF6_0001_0000, 0x1000),
    "Camera": (0x7FF6_0002_0000, 0x1000),
    "RenderSettings": (0x7FF6_0003_0000, 0x1000),
    "TaaFilter": (0x7FF6_0004_0000, 0x1000),
    "Device": (0x7FF6_0005_0000, 0x50000),
}
CODE = ("GameCode", 0x1_4000_1000, 0x1000)  # readable, writable only with force
ALIASES = {
    "cfg": "RenderConfig",
    "cam": "Camera",
    "ctx": "Camera",
    "rs": "RenderSettings",
    "settings": "RenderSettings",
    "taa": "TaaFilter",
    "ss_node": "TaaFilter",
    "dev": "Device",
}
FIELDS = {
    "RenderConfig": {
        "frame": (0x10, "u64"),
        "render_width": (0x100, "u32"),
        "render_height": (0x104, "u32"),
        "scale": (0x200, "f32"),
    },
    "Camera": {"view": (0x0, "mat4"), "proj": (0x40, "mat4")},
    "RenderSettings": {"jitter_phases": (0x20, "u32")},
    "TaaFilter": {"word": (0x30, "u16"), "dense_velocity": (0x1A8, "u64")},
    "Device": {"wait_latch": (0x10DA, "u8"), "fg_flag": (0x42718, "u8")},
}

RENDER = (64, 36)
DISPLAY = (128, 72)
RESOURCES = {  # name -> (format, dxgi id, bytes per pixel, size); the slot's resources
    "colour": ("R11G11B10_FLOAT", 26, 4, RENDER),
    "depth": ("R32_FLOAT", 41, 4, RENDER),
    "mv": ("R16G16_FLOAT", 34, 4, RENDER),
    "mv_raw": ("R32_UINT", 42, 4, RENDER),
    "dlss_out": ("R11G11B10_FLOAT", 26, 4, DISPLAY),
    "hudless": ("R10G10B10A2_UNORM", 24, 4, DISPLAY),
    "game_mv": ("R32G32_FLOAT", 16, 8, RENDER),
    "game_depth": ("R32_FLOAT", 41, 4, RENDER),
}


def _enum(*choices: str) -> Callable[[Any], Any]:
    def check(v: Any) -> str:
        for c in choices:
            if str(v).lower() == c.lower():
                return c
        raise ValueError(f"expected one of {', '.join(choices)}")

    return check


def _bool(v: Any) -> bool:
    if isinstance(v, bool):
        return v
    if str(v).lower() in ("true", "1", "yes", "on"):
        return True
    if str(v).lower() in ("false", "0", "no", "off"):
        return False
    raise ValueError("expected true or false")


def _float(lo: float, hi: float) -> Callable[[Any], Any]:
    """A number; out of range is clamped with a note (the file's rule)."""

    def check(v: Any) -> Any:
        f = float(v)
        if not lo <= f <= hi:
            used = min(max(f, lo), hi)
            return Noted(used, f"{v} clamped to {used}")
        return f

    return check


def _int_in(*choices: int) -> Callable[[Any], int]:
    def check(v: Any) -> int:
        i = int(v)
        if i not in choices:
            raise ValueError(f"expected one of {choices}")
        return i

    return check


def _auto_or(conv: Callable[[Any], Any]) -> Callable[[Any], Any]:
    def check(v: Any) -> Any:
        if str(v).lower() == "auto":
            return "Auto"
        if str(v).lower() == "off":
            return "Off"
        return conv(v)

    return check


# section -> key -> (default, validator, needs restart)
SETTINGS: dict[str, dict[str, tuple[Any, Callable[[Any], Any], bool]]] = {
    "General": {
        "Enabled": (True, _bool, True),
        "LogLevel": ("Info", _enum("Error", "Warn", "Info", "Debug", "Trace"), False),
    },
    "Upscaling": {
        "Mode": ("Quality", _enum("DLAA", "Quality", "Balanced", "Performance", "Custom"), False),
        "RenderScale": (0.667, _float(0.5, 1.0), False),
        "Preset": ("Default", _enum("Default", "J", "K", "L", "M"), False),
        "Sharpness": (0.0, _float(0.0, 1.0), False),
    },
    "FrameGeneration": {
        "Enabled": (False, _bool, True),
        "Multiplier": (2, lambda v: _int_range(v, 2, 4), False),
        "ReflexBoost": (False, _bool, False),
    },
    "Dev": {
        "Listen": ("127.0.0.1:47800", str, True),
        "TokenFile": ("dev.token", str, True),
        "LogTargets": ("", str, False),
        "DebugLayer": (False, _bool, True),
        "Dred": (False, _bool, True),
        "CaptureDir": ("captures", str, False),
    },
    "Experimental": {
        "InputPath": ("Pass", _enum("Pass", "Supersampling"), False),
        "JitterScale": ("Display", _enum("Display", "Render", "Off"), False),
        "JitterPhases": (32, lambda v: _int_range(v, 1, 256), False),
        "HudLess": (False, _bool, False),
        "TaaSharpen": ("Off", _enum("Off", "Game"), False),
        "HoldAntiAliasing": (True, _bool, False),
        "WaitLatch": ("Auto", _enum("Auto", "Never", "Always"), False),
        "Reset": ("Auto", _enum("Auto", "GameOnly", "Never"), False),
        "MipBias": ("Auto", _auto_or(_float(-4.0, 4.0)), False),
        "DlssHdr": ("Auto", _auto_or(_bool), False),
    },
}


def _int_range(v: Any, lo: int, hi: int) -> Any:
    i = int(v)
    if not lo <= i <= hi:
        used = min(max(i, lo), hi)
        return Noted(used, f"{v} clamped to {used}")
    return i


class MockError(Exception):
    """Turned into an ``ok: false`` response."""

    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code
        self.message = message


class Noted:
    """A validator's value applied with a note (clamped), like the file's
    diagnostics."""

    def __init__(self, value: Any, note: str):
        self.value = value
        self.note = note


# -- methods ----------------------------------------------------------------


def _frames(p: dict, key: str = "frames", hi: int = 1024) -> int:
    n = p.get(key, 1)
    if not isinstance(n, int) or isinstance(n, bool) or not 1 <= n <= hi:
        raise MockError("bad_request", f"{key} must be 1..{hi}")
    return n


def _wait_frames(s: MockServer, n: int) -> int:
    first = s.frame
    time.sleep(n * s.frame_interval)
    return first


def m_hello(s: MockServer, c: Connection, p: dict) -> dict:
    return {
        "version": "0.3.0-dev",
        "commit": "mock",
        "game_build": "timestamp 0x66d13cb6, image size 0x55d3000",
        "gate": "open",
        "subsystems": {"engine": True, "render": True, "ngx": True, "fg": False, "dev": True},
        "features": ["dev"],
    }


def m_state(s: MockServer, c: Connection, p: dict) -> dict:
    up = s.settings["Upscaling"]
    return {
        "frame": s.frame,
        "settings": s.settings,
        "policy": {"input_path": s.settings["Experimental"]["InputPath"], "upscaler": "dlss"},
        "render_size": list(RENDER),
        "display_size": list(DISPLAY),
        "scale": RENDER[0] / DISPLAY[0],
        "dlss": {"feature": "created", "preset": up["Preset"], "mode": up["Mode"]},
        "fg": {"enabled": s.settings["FrameGeneration"]["Enabled"], "state": "off"},
        "counters": {"evaluate": s.frame - 1000, "resets": 3},
    }


def m_settings_get(s: MockServer, c: Connection, p: dict) -> dict:
    return s.settings


def m_settings_set(s: MockServer, c: Connection, p: Any) -> dict:
    items = p if isinstance(p, list) else [p]
    if not items:
        raise MockError("bad_request", "no changes")
    staged = []
    for it in items:
        if not isinstance(it, dict) or not {"section", "key", "value"} <= it.keys():
            raise MockError("bad_request", "each change needs section, key, value")
        # Sections and keys match case-insensitively, like the file.
        sec = next((x for x in SETTINGS if x.lower() == str(it["section"]).lower()), it["section"])
        key = next((x for x in SETTINGS.get(sec, {}) if x.lower() == str(it["key"]).lower()), it["key"])
        spec = SETTINGS.get(sec, {}).get(key)
        if spec is None:
            raise MockError("bad_request", f"unknown setting [{sec}] {key}")
        try:
            value = spec[1](it["value"])
        except (TypeError, ValueError) as e:
            raise MockError("bad_request", f"[{sec}] {key}: {e}") from None
        note = None
        if isinstance(value, Noted):
            value, note = value.value, value.note
        staged.append((sec, key, value, spec[2], note))
    mode = s.settings["Upscaling"]["Mode"]
    for sec, key, value, _, _ in staged:
        if (sec, key) == ("Upscaling", "Mode"):
            mode = value
    changes = []
    for sec, key, value, restart, note in staged:  # all or nothing
        s.settings[sec][key] = value
        if (sec, key) == ("Upscaling", "RenderScale") and mode != "Custom" and note is None:
            note = "only used with Mode = Custom"
        change = {"section": sec, "key": key, "value": value, "restart": restart}
        if note is not None:
            change["note"] = note
        changes.append(change)
    return {"changes": changes, "restart_required": any(r for *_, r, _ in staged)}


def m_experiment_set(s: MockServer, c: Connection, p: dict) -> dict:
    return m_settings_set(s, c, {"section": "Experimental", "key": p["key"], "value": p["value"]})


def m_symbols(s: MockServer, c: Connection, p: dict) -> dict:
    syms = {n: hex(a) for n, (a, _) in STRUCTS.items()}
    syms[CODE[0]] = hex(CODE[1])
    fields = {st: {f: {"offset": hex(o), "as": t} for f, (o, t) in fs.items()} for st, fs in FIELDS.items()}
    regions = [{"name": n, "addr": hex(a), "len": size, "writable": True} for n, (a, size) in STRUCTS.items()]
    return {"symbols": syms, "aliases": ALIASES, "fields": fields, "regions": regions}


def m_mem_read(s: MockServer, c: Connection, p: dict) -> Any:
    as_ = p.get("as")
    if as_ is not None and as_ != "bytes" and as_ not in TYPES:
        raise MockError("bad_request", f"unknown type {as_!r}")
    size = type_size(as_ or "bytes")
    length = p.get("len", size if as_ else None)
    if not isinstance(length, int) or length <= 0 or length % size or length > 1 << 20:
        raise MockError("bad_request", "len must be a positive multiple of the type size (<= 1 MiB)")
    region, off, addr = s.span(p, length)
    s.sync_memory()
    data = bytes(s.memory[region][off : off + length])
    if as_ is None or as_ == "bytes":
        return ({"addr": hex(addr), "len": length}, data)
    return {"addr": hex(addr), "as": as_, "values": decode_values(data, as_)}


def m_mem_write(s: MockServer, c: Connection, p: dict) -> dict:
    as_ = p["as"]
    if as_ != "bytes" and as_ not in TYPES:
        raise MockError("bad_request", f"unknown type {as_!r}")
    data = encode_values(p["value"], as_)
    region, off, addr = s.span(p, len(data))
    if region not in STRUCTS and not p.get("force"):
        raise MockError("refused", f"{addr:#x} is outside the engine structs (force:true to override)")
    s.memory[region][off : off + len(data)] = data
    return {"addr": hex(addr), "written": len(data)}


def m_watch(s: MockServer, c: Connection, p: dict) -> dict:
    name = p["name"]
    exprs = p["expr"]
    every = _frames(p, "every_frames", 10_000)
    point = p.get("point", "frame")
    if not isinstance(exprs, list) or not exprs:
        raise MockError("bad_request", "expr must be a non-empty list")
    for e in exprs:
        if e.get("as") not in TYPES:
            raise MockError("bad_request", f"watch expression needs a type: {e}")
        s.read_typed(e, e["as"])  # validate now, fail the request not the stream
    if name in c.watches:
        c.watches.pop(name).set()
    stop = threading.Event()
    c.watches[name] = stop

    def run() -> None:
        while not stop.wait(every * s.frame_interval) and c.alive:
            try:
                values = [s.read_typed(e, e["as"])[0] for e in exprs]
            except MockError:
                return
            c.send({"event": "watch", "name": name, "frame": s.frame, "point": point, "values": values})

    threading.Thread(target=run, name=f"watch-{name}", daemon=True).start()
    return {"name": name, "count": len(exprs), "every_frames": every, "point": point}


def m_unwatch(s: MockServer, c: Connection, p: dict) -> dict:
    stop = c.watches.pop(p["name"], None)
    if stop is None:
        raise MockError("bad_request", f"no watch named {p['name']!r}")
    stop.set()
    return {"name": p["name"]}


def m_log_subscribe(s: MockServer, c: Connection, p: dict) -> dict:
    level = str(p.get("level", "info")).lower()
    if level not in LEVELS:
        raise MockError("bad_request", f"level must be one of {LEVELS}")
    targets = p.get("targets") or []
    if isinstance(targets, str):
        targets = [t for t in targets.split(",") if t]
    unknown = [t for t in targets if t not in TARGETS]
    if unknown:
        raise MockError("bad_request", f"unknown targets {unknown}")
    c.log_filter = (LEVELS.index(level), set(targets))
    return {"level": level, "targets": targets}


def m_log_tail(s: MockServer, c: Connection, p: dict) -> dict:
    n = _frames(p, "lines", 100_000)
    with s.lock:
        recs = list(s.ring)[-n:]
    return {"lines": [r["line"] for r in recs]}


def _texel(fmt: str, x: int, y: int) -> bytes:
    if fmt == "R11G11B10_FLOAT":
        r = (((x % 15) + 1) << 6) | (y % 64)
        g = (((y % 15) + 1) << 6) | (x % 64)
        b = (15 << 5) | ((x + y) % 32)
        return struct.pack("<I", r | (g << 11) | (b << 22))
    if fmt == "R32_FLOAT":
        return struct.pack("<f", 1.0 / (1 + x + y))
    if fmt == "R16G16_FLOAT":
        return struct.pack("<2e", x / 64.0, -y / 64.0)
    if fmt == "R32_UINT":  # high half = x motion, low half = y motion
        return struct.pack("<2e", -y / 64.0, x / 64.0)
    if fmt == "R10G10B10A2_UNORM":
        return struct.pack("<I", (x * 8 % 1024) | ((y * 8 % 1024) << 10) | (512 << 20) | (3 << 30))
    if fmt == "B8G8R8A8_UNORM":
        return bytes((y * 3 % 256, 128, x * 2 % 256, 0))
    if fmt == "R32G32_FLOAT":
        return struct.pack("<2f", x / 128.0, -y / 128.0)
    raise ValueError(fmt)


def synth_resource(resource: str) -> tuple[dict, bytes]:
    """Synthetic texels for one resource, rows padded to 256 bytes."""
    fmt, dxgi, bpp, (w, h) = RESOURCES[resource]
    pitch = (w * bpp + 255) // 256 * 256
    rows = [b"".join(_texel(fmt, x, y) for x in range(w)).ljust(pitch, b"\0") for y in range(h)]
    meta = {"format": fmt, "dxgi": dxgi, "width": w, "height": h, "row_pitch": pitch}
    return meta, b"".join(rows)


def m_capture(s: MockServer, c: Connection, p: dict) -> list:
    resources = p.get("resources")
    if not isinstance(resources, list) or not resources:
        raise MockError("bad_request", "resources must be a non-empty list")
    unknown = [r for r in resources if r not in RESOURCES and r != "backbuffer"]
    if unknown:
        raise MockError("bad_request", f"unknown resources {unknown} (known: {', '.join(RESOURCES)}, backbuffer)")
    if p.get("point", "slot") != "slot":
        raise MockError("bad_request", "captures record on the engine's command list: point must be slot")
    if "backbuffer" in resources:
        # Not a resource of the upscale slot, where captures record.
        raise MockError("not_ready", f"backbuffer is not available at the slot (have: {', '.join(RESOURCES)})")
    frames = _frames(p, hi=16)
    point = "slot"
    stamp = time.strftime("%Y%m%d-%H%M%S", time.gmtime())
    first = _wait_frames(s, frames)
    cam = s.read_typed({"symbol": "Camera"}, "mat4", 2)
    parts = []
    for f in range(frames):
        frame = first + f
        phase = frame % s.settings["Experimental"]["JitterPhases"]
        jitter = [math.sin(phase) * 0.5, math.cos(phase) * 0.5]
        for r in resources:
            meta, data = synth_resource(r)
            meta.update(
                resource=r,
                frame=frame,
                point=point,
                index=len(parts),
                count=frames * len(resources),
                saved=f"captures/{stamp}/{r}_{frame:06d}.raw",
                sidecar={
                    "frame": {"number": frame, "phase": phase},
                    "camera": {"view": cam[0], "proj": cam[1]},
                    "jitter": jitter,
                    "settings": s.settings,
                },
            )
            parts.append((meta, data))
    return parts


def m_probe_jitter(s: MockServer, c: Connection, p: dict) -> dict:
    """The engine divides the written units by the display size:
    ``proj[8] = -units.x / display.w`` (ReJitterCamera §2.4)."""
    n = _frames(p)
    first = _wait_frames(s, n)
    phases = s.settings["Experimental"]["JitterPhases"]
    rows = []
    for i in range(n):
        ph = (first + i) % phases
        wx, wy = math.sin(ph) * 0.5, math.cos(ph) * 0.5
        rows.append(
            {
                "frame": first + i,
                "phase": ph,
                "written": [wx, wy],
                "px": [wx * RENDER[0] / (2 * DISPLAY[0]), wy * RENDER[1] / (2 * DISPLAY[1])],
                "proj8": -wx / DISPLAY[0],
                "proj9": -wy / DISPLAY[1],
                "divisor": [float(DISPLAY[0]), float(DISPLAY[1])],
            }
        )
    return {
        "frames": rows,
        "verdict": "display",
        "divisor": [float(DISPLAY[0]), float(DISPLAY[1])],
        "render": list(RENDER),
        "display": list(DISPLAY),
    }


def m_probe_reset(s: MockServer, c: Connection, p: dict) -> dict:
    n = _frames(p)
    first = _wait_frames(s, n)
    rows = []
    for i in range(n):
        reset = i == 0
        rows.append(
            {
                "frame": first + i,
                "reason": "GameHistory" if reset else "None",
                "reset_dlss": reset,
                "game_reset": None if i == 0 else False,
                "game_resets": 3,
                "history_invalid": reset,
                "camera_cut": {
                    "frame": first + i,
                    "position_jump": 0.01 * i,
                    "turn": 0.0,
                    "projection_change": 0.0,
                    "cut": False,
                },
                "fg": {"frame": first + i, "reset": reset},
                "reset_fg": reset,
            }
        )
    return {"frames": rows}


def m_probe_hdr(s: MockServer, c: Connection, p: dict) -> dict:
    n = _frames(p, hi=16)
    first = _wait_frames(s, n)
    return {
        "frames": [
            {
                "frame": first + i,
                "min": [0.0, 0.0, 0.0],
                "max": [12.5, 9.0, 6.0],
                "mean": [0.31, 0.29, 0.25],
                "p50": 0.22,
                "p99": 2.4,
                "p999": 6.0,
                "above_one": 0.02,
                "nonfinite": 0,
            }
            for i in range(n)
        ]
    }


def m_probe_mv(s: MockServer, c: Connection, p: dict) -> dict:
    n = _frames(p, hi=16)
    first = _wait_frames(s, n)
    fit = [
        {"scale": 0.5, "sign": 1, "residual": 1e-4, "relative": 1e-3},
        {"scale": -0.5, "sign": -1, "residual": 1e-4, "relative": 1e-3},
    ]
    textures = {
        "game": {"format": "R32G32_FLOAT", "size": list(RENDER)},
        "raw": {"format": "R32_UINT", "size": list(RENDER)},
    }
    return {
        "frames": [
            {
                "frame": first + i,
                "scale": [0.5, -0.5],
                "sign": [1, -1],
                "residual": 1.4e-4,
                "fit": fit,
                "scale_ndc": [-16.0, -9.0],
                "samples": RENDER[0] * RENDER[1],
                "textures": textures,
            }
            for i in range(n)
        ],
        "rule": "game = scale * decoded tGBuffer4 in render pixels",
    }


def m_probe_depth(s: MockServer, c: Connection, p: dict) -> dict:
    n = _frames(p, hi=16)
    first = _wait_frames(s, n)
    values = {"count": 2304, "nonfinite": 0, "min": 0.01, "max": 1.0, "mean": 0.1, "p50": 0.05, "p99": 0.5}
    return {
        "frames": [
            {
                "frame": first + i,
                "copy": {"size": list(RENDER), "format": "R32_FLOAT", "values": values},
                "dsv": {"size": list(RENDER), "format": "D32_FLOAT", "values": values},
                "diff": {"count": 2304, "nonfinite": 0, "min": 0.0, "max": 0.0, "mean": 0.0, "p50": 0.0, "p99": 0.0},
                "flipped_fraction": 0.0,
            }
            for i in range(n)
        ]
    }


def m_probe_wait(s: MockServer, c: Connection, p: dict) -> dict:
    seconds = p.get("seconds", 5)
    if not isinstance(seconds, (int, float)) or isinstance(seconds, bool) or not 0 < seconds <= 60:
        raise MockError("bad_request", "seconds must be in (0, 60]")
    time.sleep(min(seconds, 0.05))
    frames = max(1, int(seconds / s.frame_interval))
    return {
        "seconds": seconds,
        "frames": frames,
        "samples": frames,
        "latch": False,
        "latched_frames": 0,
        "presented_frames": frames,
        "waitable": "0x5a4",
        # The wait helper is not a gate site: calls/waits/timeouts need its detour.
        "calls": None,
        "waits": None,
        "timeouts": None,
        "note": "the wait helper 0x1423ade30 is not a gate site",
    }


def m_probe_modules(s: MockServer, c: Connection, p: dict) -> dict:
    mods = [
        ("MonsterHunterWorld.exe", 0.0),
        ("dxgi.dll", 0.8),
        ("mhw-dlss.dll", 0.9),
        ("d3d12.dll", 1.2),
        ("nvngx_dlss.dll", 2.5),
        ("sl.interposer.dll", 2.6),
    ]
    return {
        "modules": [
            {
                "order": i,
                "name": n,
                "path": f"C:\\Game\\{n}",
                "base": hex(0x7FF8_0000_0000 + i * 0x100_0000),
                "ms": t * 1000,
                "seen_at_start": t < 1.0,
                "before_device": t < 1.3,
                "loaded": True,
            }
            for i, (n, t) in enumerate(mods)
        ],
        "device_created_ms": 1300.0,
        "dev_started_ms": 950.0,
        "resolution_ms": 50.0,
        "polls": 40,
    }


def m_trace(s: MockServer, c: Connection, p: dict) -> dict:
    n = _frames(p, hi=256)
    first = _wait_frames(s, n)
    freq = 10_000_000
    names = ["camera", "slot", "evaluate", "tags", "markers", "present"]
    frames = [
        {
            "frame": first + i,
            "events": [{"what": w, "qpc": (first + i) * 166_667 + k * 1000} for k, w in enumerate(names)],
        }
        for i in range(n)
    ]
    return {"qpc_frequency": freq, "frames": frames}


METHODS: dict[str, Callable[..., Any]] = {
    "hello": m_hello,
    "state": m_state,
    "settings.get": m_settings_get,
    "settings.set": m_settings_set,
    "experiment.set": m_experiment_set,
    "symbols": m_symbols,
    "mem.read": m_mem_read,
    "mem.write": m_mem_write,
    "watch": m_watch,
    "unwatch": m_unwatch,
    "log.subscribe": m_log_subscribe,
    "log.tail": m_log_tail,
    "capture": m_capture,
    "probe.jitter": m_probe_jitter,
    "probe.reset": m_probe_reset,
    "probe.hdr": m_probe_hdr,
    "probe.mv": m_probe_mv,
    "probe.depth": m_probe_depth,
    "probe.wait": m_probe_wait,
    "probe.modules": m_probe_modules,
    "trace": m_trace,
}
"""Every protocol method except ``auth`` (handled at connection start)."""
