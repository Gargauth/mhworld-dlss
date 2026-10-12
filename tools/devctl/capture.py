"""Saving captured resources and converting them to viewable files.

Every part is saved as ``<resource>_<frame>.raw`` (the bytes exactly as
received, rows ``row_pitch`` apart) plus ``<resource>_<frame>.json`` (the
response metadata: format, size, pitch and the server's sidecar with frame
info, camera, jitter and settings).

With ``convert=True``:

* 8-bit-displayable formats (``B8G8R8A8``, ``R8G8B8A8``, ``R10G10B10A2``) are
  written as 8-bit RGB PNG (alpha dropped: game buffers often store 0 there);
* float formats are written as PFM (little endian, ``Pf`` for one channel,
  ``PF`` for two or more; a second channel goes to G with B = 0, alpha is
  dropped): ``R11G11B10_FLOAT``, ``R16G16_FLOAT``, ``R16G16B16A16_FLOAT``,
  ``R32G32_FLOAT``, ``R32_FLOAT``, ``D32_FLOAT``, and ``R32_UINT`` which is the
  engine's packed motion vectors (``tGBuffer4``): two halves, horizontal in
  the high 16 bits, vertical in the low 16 bits, decoded to R = x, G = y.

Conversion uses numpy when it is installed and falls back to the standard
library (slower: seconds per megapixel).
"""

from __future__ import annotations

import array
import json
import math
import os
import struct
import sys
import zlib
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

try:  # optional accelerator
    import numpy as np
except ImportError:  # pragma: no cover - depends on the environment
    np = None


@dataclass(frozen=True)
class Format:
    """How one DXGI format is decoded."""

    name: str
    dxgi: int
    bpp: int  # bytes per pixel
    kind: str  # "unorm8" -> PNG, "float" -> PFM
    channels: int  # channels after decoding


_FORMATS = [
    Format("R16G16B16A16_FLOAT", 10, 8, "float", 4),
    Format("R32G32_FLOAT", 16, 8, "float", 2),
    Format("R10G10B10A2_UNORM", 24, 4, "unorm8", 4),
    Format("R11G11B10_FLOAT", 26, 4, "float", 3),
    Format("R8G8B8A8_UNORM", 28, 4, "unorm8", 4),
    Format("R8G8B8A8_UNORM_SRGB", 29, 4, "unorm8", 4),
    Format("R16G16_FLOAT", 34, 4, "float", 2),
    Format("D32_FLOAT", 40, 4, "float", 1),
    Format("R32_FLOAT", 41, 4, "float", 1),
    Format("R32_UINT", 42, 4, "float", 2),
    Format("B8G8R8A8_UNORM", 87, 4, "unorm8", 4),
    Format("B8G8R8A8_UNORM_SRGB", 91, 4, "unorm8", 4),
]
FORMATS = {f.name: f for f in _FORMATS}
"""Convertible formats by DXGI name (without the ``DXGI_FORMAT_`` prefix)."""
FORMATS_BY_ID = {f.dxgi: f for f in _FORMATS}
_ALIASES = {
    "R32_TYPELESS": "R32_FLOAT",
    "B8G8R8A8_TYPELESS": "B8G8R8A8_UNORM",
    "R8G8B8A8_TYPELESS": "R8G8B8A8_UNORM",
    "R10G10B10A2_TYPELESS": "R10G10B10A2_UNORM",
    "R16G16_TYPELESS": "R16G16_FLOAT",
    "R32G32_TYPELESS": "R32G32_FLOAT",
    "R16G16B16A16_TYPELESS": "R16G16B16A16_FLOAT",
}


def lookup_format(fmt: Any) -> Format | None:
    """Resolve a DXGI name (``DXGI_FORMAT_`` prefix optional) or numeric id."""
    if isinstance(fmt, bool):
        return None
    if isinstance(fmt, int):
        return FORMATS_BY_ID.get(fmt)
    if not isinstance(fmt, str):
        return None
    name = fmt.strip().upper().removeprefix("DXGI_FORMAT_")
    if name.isdigit():
        return FORMATS_BY_ID.get(int(name))
    return FORMATS.get(_ALIASES.get(name, name))


# --------------------------------------------------------------------------
# Small float tables


def _unsigned_float(bits: int, mbits: int) -> float:
    """Decode an unsigned 5-bit-exponent float (R11G11B10 components)."""
    e, m = bits >> mbits, bits & ((1 << mbits) - 1)
    if e == 0:
        return m / (1 << mbits) * 2.0**-14
    if e == 31:
        return math.nan if m else math.inf
    return (1 + m / (1 << mbits)) * 2.0 ** (e - 15)


_F11 = [_unsigned_float(v, 6) for v in range(1 << 11)]
_F10 = [_unsigned_float(v, 5) for v in range(1 << 10)]
_U10_TO_8 = bytes((v * 255 + 511) // 1023 for v in range(1024))


# --------------------------------------------------------------------------
# Decoding


@dataclass
class Image:
    """Decoded pixels, row-major, top row first, channels interleaved.

    ``pixels`` is ``bytes`` (RGB8) for kind ``unorm8`` and ``array('f')`` /
    a float32 ndarray for kind ``float``.
    """

    width: int
    height: int
    channels: int
    kind: str
    pixels: Any


def _rows(data: bytes, width: int, height: int, bpp: int, row_pitch: int | None) -> bytes:
    """Strip row padding; returns ``width * bpp * height`` tightly packed bytes."""
    tight = width * bpp
    pitch = row_pitch or tight
    if pitch < tight:
        raise ValueError(f"row_pitch {pitch} is smaller than a row ({tight} bytes)")
    need = pitch * (height - 1) + tight if height else 0
    if len(data) < need:
        raise ValueError(f"{len(data)} bytes, need {need} for {width}x{height} pitch {pitch}")
    if pitch == tight:
        return bytes(data[: tight * height])
    return b"".join(data[y * pitch : y * pitch + tight] for y in range(height))


def decode(
    data: bytes,
    fmt: Format,
    width: int,
    height: int,
    row_pitch: int | None = None,
    use_numpy: bool | None = None,
) -> Image:
    """Decode raw texel bytes into an :class:`Image` (see module docs)."""
    raw = _rows(data, width, height, fmt.bpp, row_pitch)
    if use_numpy is None:
        use_numpy = np is not None
    if use_numpy:
        if np is None:
            raise RuntimeError("numpy is not installed")
        pixels = _decode_numpy(raw, fmt)
    else:
        pixels = _decode_py(raw, fmt)
    channels = 3 if fmt.kind == "unorm8" else fmt.channels
    return Image(width, height, channels, fmt.kind, pixels)


def _decode_py(raw: bytes, fmt: Format) -> Any:
    n = len(raw) // fmt.bpp
    name = fmt.name
    if name.startswith(("B8G8R8A8", "R8G8B8A8")):
        out = bytearray(n * 3)
        if name.startswith("B8"):
            out[0::3], out[1::3], out[2::3] = raw[2::4], raw[1::4], raw[0::4]
        else:
            out[0::3], out[1::3], out[2::3] = raw[0::4], raw[1::4], raw[2::4]
        return bytes(out)
    if name == "R10G10B10A2_UNORM":
        words = array.array("I", raw)
        if sys.byteorder == "big":
            words.byteswap()
        t = _U10_TO_8
        out = bytearray(n * 3)
        for i, w in enumerate(words):
            out[3 * i] = t[w & 0x3FF]
            out[3 * i + 1] = t[(w >> 10) & 0x3FF]
            out[3 * i + 2] = t[(w >> 20) & 0x3FF]
        return bytes(out)
    if name == "R11G11B10_FLOAT":
        words = array.array("I", raw)
        if sys.byteorder == "big":
            words.byteswap()
        f11, f10 = _F11, _F10
        out = array.array("f", bytes(4 * 3 * n))
        for i, w in enumerate(words):
            out[3 * i] = f11[w & 0x7FF]
            out[3 * i + 1] = f11[(w >> 11) & 0x7FF]
            out[3 * i + 2] = f10[w >> 22]
        return out
    if name in ("R16G16_FLOAT", "R16G16B16A16_FLOAT"):
        return array.array("f", struct.unpack(f"<{len(raw) // 2}e", raw))
    if name in ("R32G32_FLOAT", "R32_FLOAT", "D32_FLOAT"):
        out = array.array("f", raw)
        if sys.byteorder == "big":
            out.byteswap()
        return out
    if name == "R32_UINT":
        halves = struct.unpack(f"<{len(raw) // 2}e", raw)  # (low=y, high=x) per word
        out = array.array("f", bytes(4 * len(halves)))
        out[0::2] = array.array("f", halves[1::2])
        out[1::2] = array.array("f", halves[0::2])
        return out
    raise ValueError(f"no decoder for {name}")


def _decode_numpy(raw: bytes, fmt: Format) -> Any:
    name = fmt.name
    if name.startswith(("B8G8R8A8", "R8G8B8A8")):
        px = np.frombuffer(raw, np.uint8).reshape(-1, 4)
        order = [2, 1, 0] if name.startswith("B8") else [0, 1, 2]
        return px[:, order].tobytes()
    if name == "R10G10B10A2_UNORM":
        w = np.frombuffer(raw, "<u4")
        t = np.frombuffer(_U10_TO_8, np.uint8)
        rgb = np.stack([t[w & 0x3FF], t[(w >> 10) & 0x3FF], t[(w >> 20) & 0x3FF]], axis=1)
        return rgb.tobytes()
    if name == "R11G11B10_FLOAT":
        w = np.frombuffer(raw, "<u4")
        f11 = np.array(_F11, np.float32)
        f10 = np.array(_F10, np.float32)
        return np.stack([f11[w & 0x7FF], f11[(w >> 11) & 0x7FF], f10[w >> 22]], axis=1).reshape(-1)
    if name in ("R16G16_FLOAT", "R16G16B16A16_FLOAT"):
        return np.frombuffer(raw, "<f2").astype(np.float32)
    if name in ("R32G32_FLOAT", "R32_FLOAT", "D32_FLOAT"):
        return np.frombuffer(raw, "<f4").astype(np.float32)
    if name == "R32_UINT":
        h = np.frombuffer(raw, "<f2").reshape(-1, 2)
        return h[:, ::-1].astype(np.float32).reshape(-1)
    raise ValueError(f"no decoder for {name}")


# --------------------------------------------------------------------------
# Writers


def write_png(path: str | os.PathLike, width: int, height: int, rgb: bytes) -> None:
    """Write 8-bit RGB pixels (top row first) as a PNG."""
    if len(rgb) != width * height * 3:
        raise ValueError("pixel buffer does not match the size")
    stride = width * 3
    scan = b"".join(b"\0" + rgb[y * stride : (y + 1) * stride] for y in range(height))

    def chunk(tag: bytes, body: bytes) -> bytes:
        return struct.pack(">I", len(body)) + tag + body + struct.pack(">I", zlib.crc32(tag + body))

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    with open(path, "wb") as f:
        f.write(
            b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", ihdr)
            + chunk(b"IDAT", zlib.compress(scan, 6))
            + chunk(b"IEND", b"")
        )


def write_pfm(path: str | os.PathLike, width: int, height: int, channels: int, pixels: Any) -> None:
    """Write float pixels (top row first, ``channels`` interleaved) as PFM.

    One channel gives ``Pf``; two or more give ``PF`` with channel 3 zero when
    absent and channel 4 dropped. PFM stores rows bottom first.
    """
    if np is not None and isinstance(pixels, np.ndarray):
        px = pixels.astype("<f4").reshape(height, width, channels)
        if channels == 1:
            out = px
        else:
            out = np.zeros((height, width, 3), "<f4")
            out[:, :, : min(channels, 3)] = px[:, :, :3]
        body = out[::-1].tobytes()
        tag = "Pf" if channels == 1 else "PF"
    else:
        src = array.array("f", pixels)
        if len(src) != width * height * channels:
            raise ValueError("pixel buffer does not match the size")
        if channels == 1:
            dst, tag, oc = src, "Pf", 1
        elif channels == 3:
            dst, tag, oc = src, "PF", 3
        else:
            dst, tag, oc = array.array("f", bytes(4 * 3 * width * height)), "PF", 3
            for c in range(min(channels, 3)):
                dst[c::3] = src[c::channels]
        if sys.byteorder == "big":
            dst = array.array("f", dst)
            dst.byteswap()
        flat = dst.tobytes()
        stride = width * oc * 4
        body = b"".join(flat[y * stride : (y + 1) * stride] for y in range(height - 1, -1, -1))
    with open(path, "wb") as f:
        f.write(f"{tag}\n{width} {height}\n-1.0\n".encode() + body)


# --------------------------------------------------------------------------
# Saving


def part_stem(meta: dict) -> str:
    """File stem for a part: ``<resource>_<frame>`` (frame zero-padded)."""
    resource = "".join(c if c.isalnum() or c in "-_" else "_" for c in str(meta.get("resource", "resource")))
    return f"{resource}_{int(meta.get('frame', 0)):06d}"


def save_part(
    meta: dict,
    data: bytes,
    out_dir: str | os.PathLike,
    *,
    convert: bool = False,
    use_numpy: bool | None = None,
) -> list[Path]:
    """Save one capture part as raw + JSON sidecar (+ PNG/PFM when ``convert``).

    Returns the written paths. A format that cannot be converted is noted in
    the sidecar (``"converted": null``) instead of failing the capture.
    """
    out = Path(out_dir)
    out.mkdir(parents=True, exist_ok=True)
    stem = part_stem(meta)
    raw_path = out / f"{stem}.raw"
    raw_path.write_bytes(data)
    written = [raw_path]
    sidecar = dict(meta)
    sidecar["raw"] = raw_path.name
    sidecar["bytes"] = len(data)
    if convert:
        sidecar["converted"] = None
        fmt = lookup_format(meta.get("format", meta.get("dxgi")))
        width, height = int(meta.get("width", 0)), int(meta.get("height", 0))
        if fmt is not None and width > 0 and height > 0:
            img = decode(data, fmt, width, height, meta.get("row_pitch"), use_numpy)
            if img.kind == "unorm8":
                path = out / f"{stem}.png"
                write_png(path, width, height, img.pixels)
            else:
                path = out / f"{stem}.pfm"
                write_pfm(path, width, height, img.channels, img.pixels)
            sidecar["converted"] = path.name
            written.append(path)
    json_path = out / f"{stem}.json"
    json_path.write_text(json.dumps(sidecar, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    written.append(json_path)
    return written


def read_pfm(path: str | os.PathLike) -> tuple[int, int, int, list[float]]:
    """Read a PFM back (width, height, channels, top-row-first floats)."""
    blob = Path(path).read_bytes()
    lines = blob.split(b"\n", 3)
    tag, size, scale, body = lines[0], lines[1], lines[2], lines[3]
    channels = 1 if tag == b"Pf" else 3
    width, height = map(int, size.split())
    endian = "<" if float(scale) < 0 else ">"
    vals = struct.unpack(f"{endian}{width * height * channels}f", body)
    stride = width * channels
    rows = [vals[y * stride : (y + 1) * stride] for y in range(height)][::-1]
    return width, height, channels, [v for r in rows for v in r]


def read_png_rgb(path: str | os.PathLike) -> tuple[int, int, bytes]:
    """Read back an 8-bit RGB PNG written by :func:`write_png`."""
    blob = Path(path).read_bytes()
    pos, idat, width = 8, b"", 0
    while pos < len(blob):
        (length,) = struct.unpack(">I", blob[pos : pos + 4])
        tag = blob[pos + 4 : pos + 8]
        body = blob[pos + 8 : pos + 8 + length]
        if tag == b"IHDR":
            width, height, depth, ctype = struct.unpack(">IIBB", body[:10])
            if (depth, ctype) != (8, 2):
                raise ValueError("only 8-bit RGB")
        elif tag == b"IDAT":
            idat += body
        pos += 12 + length
    scan = zlib.decompress(idat)
    stride = width * 3 + 1
    rows: Sequence[bytes] = [scan[y * stride : (y + 1) * stride] for y in range(height)]
    if any(r[0] != 0 for r in rows):
        raise ValueError("only filter type 0")
    return width, height, b"".join(r[1:] for r in rows)
