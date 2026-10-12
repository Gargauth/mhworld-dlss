"""Client library for the mod's dev protocol (``docs/DEV-PROTOCOL.md``).

Stdlib only. The wire format is newline-delimited JSON over TCP; a frame whose
JSON carries ``"binary": n`` is followed by exactly ``n`` raw bytes. Frames with
``"id"`` are responses, frames with ``"event"`` are server-initiated events
(log stream, watches) and may arrive between any two responses.

A response with ``"more": true`` is one part of a multi-part answer (capture
sends one response per resource and frame); the parts share the request id and
the last one has no ``more``.

Typical use from a script::

    from devclient import DevClient

    with DevClient.connect("192.168.1.20", token_file="dev.token") as dev:
        print(dev.state()["frame"])
        dev.experiment_set("JitterScale", "Render")
        parts = dev.capture(["colour", "mv"], frames=1)
"""

from __future__ import annotations

import itertools
import json
import os
import queue
import re
import socket
import struct
import threading
import traceback
from collections.abc import Callable, Iterator
from dataclasses import dataclass, field
from typing import Any

DEFAULT_PORT = 47800
"""Port of the default ``[Dev] Listen`` address."""

MAX_LINE = 16 * 1024 * 1024
"""Longest JSON line accepted; a longer one means the stream is out of sync."""

MAX_BINARY = 1024 * 1024 * 1024
"""Largest binary payload accepted (a 4K RGBA32F frame is ~130 MiB)."""

POINTS = ("frame", "camera", "slot", "present")
"""Execution points a request may name; the server defaults to ``frame``."""

ERROR_CODES = (
    "bad_request",
    "unauthorized",
    "unknown_method",
    "not_ready",
    "timeout",
    "refused",
    "internal",
)
"""Error codes the protocol defines."""


class DevError(Exception):
    """The server answered ``ok: false``."""

    def __init__(self, code: str, message: str = "", data: Any = None):
        super().__init__(f"{code}: {message}" if message else code)
        self.code = code
        self.message = message
        self.data = data


class AuthError(DevError):
    """Authentication was refused (error response or connection closed)."""


class ProtocolError(Exception):
    """The byte stream does not follow the framing rules."""


class ConnectionClosed(Exception):
    """The connection ended while a response was still expected."""


# --------------------------------------------------------------------------
# Framing


@dataclass
class Frame:
    """One decoded frame: the JSON object and its binary payload, if any."""

    msg: dict
    binary: bytes | None = None


def encode_frame(msg: dict, binary: bytes | None = None) -> bytes:
    """Serialise one frame. ``binary`` sets ``msg["binary"]`` to its length."""
    if binary is not None:
        msg = dict(msg, binary=len(binary))
    line = json.dumps(msg, separators=(",", ":"), ensure_ascii=False).encode()
    if b"\n" in line:  # json.dumps never emits a raw newline; keep it that way
        raise ProtocolError("newline in encoded JSON")
    return line + b"\n" + (binary or b"")


class FrameReader:
    """Incremental frame decoder over a ``recv(n) -> bytes`` callable.

    ``recv`` returning ``b""`` means end of stream. Chunk boundaries may fall
    anywhere, including inside the JSON line or the binary payload.
    """

    def __init__(self, recv: Callable[[int], bytes]):
        self._recv = recv
        self._buf = bytearray()
        self._eof = False

    def _fill(self) -> bool:
        if self._eof:
            return False
        chunk = self._recv(1 << 16)
        if not chunk:
            self._eof = True
            return False
        self._buf += chunk
        return True

    def read_frame(self) -> Frame | None:
        """Return the next frame, or ``None`` at a clean end of stream."""
        scan = 0
        while True:
            nl = self._buf.find(b"\n", scan)
            if nl >= 0:
                break
            scan = len(self._buf)
            if scan > MAX_LINE:
                raise ProtocolError(f"line longer than {MAX_LINE} bytes")
            if not self._fill():
                if self._buf:
                    raise ProtocolError("stream ended inside a JSON line")
                return None
        line = bytes(self._buf[:nl])
        del self._buf[: nl + 1]
        try:
            msg = json.loads(line)
        except (UnicodeDecodeError, json.JSONDecodeError) as e:
            raise ProtocolError(f"invalid JSON line: {e}") from e
        if not isinstance(msg, dict):
            raise ProtocolError("frame is not a JSON object")
        size = msg.get("binary")
        if size is None:
            return Frame(msg)
        if not isinstance(size, int) or isinstance(size, bool) or not 0 <= size <= MAX_BINARY:
            raise ProtocolError(f"invalid binary length {size!r}")
        while len(self._buf) < size:
            if not self._fill():
                raise ProtocolError(f"stream ended inside a binary payload ({len(self._buf)}/{size} bytes)")
        data = bytes(self._buf[:size])
        del self._buf[:size]
        return Frame(msg, data)


# --------------------------------------------------------------------------
# Typed values

TYPES: dict[str, tuple[str, int]] = {
    "u8": ("B", 1),
    "u16": ("H", 2),
    "u32": ("I", 4),
    "u64": ("Q", 8),
    "f32": ("f", 4),
    "f64": ("d", 8),
    "mat4": ("16f", 64),
}
"""Typed views of memory: struct code and size of one element (little endian)."""


def type_size(as_: str) -> int:
    """Size in bytes of one element of ``as_``; ``bytes`` is 1."""
    if as_ == "bytes":
        return 1
    try:
        return TYPES[as_][1]
    except KeyError:
        raise ValueError(f"unknown type {as_!r} (one of {', '.join([*TYPES, 'bytes'])})") from None


def decode_values(data: bytes, as_: str) -> list:
    """Decode little-endian ``data`` as consecutive ``as_`` elements.

    Scalars decode to numbers, ``mat4`` to lists of 16 floats (memory order).
    Trailing bytes that do not fill a whole element are an error.
    """
    if as_ == "bytes":
        return list(data)
    code, size = TYPES.get(as_, (None, 0))
    if code is None:
        type_size(as_)  # raises with the list of types
    if len(data) % size:
        raise ValueError(f"{len(data)} bytes is not a whole number of {as_} ({size} bytes)")
    if as_ == "mat4":
        return [list(struct.unpack_from("<16f", data, o)) for o in range(0, len(data), size)]
    return list(struct.unpack(f"<{len(data) // size}{code}", data))


def encode_values(value: Any, as_: str) -> bytes:
    """Encode a scalar, a list of scalars, a mat4 (16 numbers) or a list of
    mat4s as little-endian ``as_``. ``bytes`` accepts bytes or a hex string."""
    if as_ == "bytes":
        if isinstance(value, (bytes, bytearray)):
            return bytes(value)
        if isinstance(value, str):
            return bytes.fromhex(value)
        raise ValueError("bytes value must be bytes or a hex string")
    code, _ = TYPES.get(as_, (None, 0))
    if code is None:
        type_size(as_)
    if as_ == "mat4":
        mats = value if value and isinstance(value[0], (list, tuple)) else [value]
        out = b""
        for m in mats:
            if len(m) != 16:
                raise ValueError(f"mat4 needs 16 numbers, got {len(m)}")
            out += struct.pack("<16f", *m)
        return out
    items = value if isinstance(value, (list, tuple)) else [value]
    if as_[0] in "u":
        items = [int(v) for v in items]
    try:
        return struct.pack(f"<{len(items)}{code}", *items)
    except struct.error as e:
        raise ValueError(f"{value!r} does not fit {as_}: {e}") from None


# --------------------------------------------------------------------------
# Memory expressions

_EXPR = re.compile(
    r"^\s*(?P<base>[A-Za-z_][\w.]*|0[xX][0-9a-fA-F]+|\d+)"
    r"(?P<offs>(?:\s*[+-]\s*(?:0[xX][0-9a-fA-F]+|\d+))*)"
    r"(?::(?P<as>\w+))?\s*$"
)
_OFFSET = re.compile(r"([+-])\s*(0[xX][0-9a-fA-F]+|\d+)")


@dataclass
class Expr:
    """A memory location: a symbol or an absolute address, plus an offset.

    Text form: ``cfg+0x200:f32``, ``0x1451c4480+8:u32``, ``Camera:mat4``.
    """

    symbol: str | None = None
    addr: int | None = None
    offset: int = 0
    as_: str | None = None

    @classmethod
    def parse(cls, text: str, default_as: str | None = None) -> Expr:
        """Parse the text form; ``default_as`` applies when no ``:type`` is given."""
        m = _EXPR.match(text)
        if not m:
            raise ValueError(f"bad memory expression {text!r} (e.g. cfg+0x200:f32)")
        base = m["base"]
        offset = sum((1 if sign == "+" else -1) * int(num, 0) for sign, num in _OFFSET.findall(m["offs"]))
        as_ = m["as"] or default_as
        if as_ is not None:
            type_size(as_)
        if base[0].isdigit():
            return cls(addr=int(base, 0), offset=offset, as_=as_)
        return cls(symbol=base, offset=offset, as_=as_)

    def params(self) -> dict:
        """The ``{addr | symbol, offset, as}`` object the protocol expects."""
        p: dict[str, Any] = {"offset": self.offset}
        if self.symbol is not None:
            p["symbol"] = self.symbol
        else:
            p["addr"] = hex(self.addr or 0)
        if self.as_ is not None:
            p["as"] = self.as_
        return p

    def __str__(self) -> str:
        base = self.symbol if self.symbol is not None else hex(self.addr or 0)
        off = f"{'+' if self.offset >= 0 else '-'}{hex(abs(self.offset))}" if self.offset else ""
        return f"{base}{off}" + (f":{self.as_}" if self.as_ else "")


def _target(target: Expr | str | int) -> Expr:
    if isinstance(target, Expr):
        return target
    if isinstance(target, int):
        return Expr(addr=target)
    return Expr.parse(target)


# --------------------------------------------------------------------------
# Client


@dataclass
class Response:
    """One response frame with ``ok: true``."""

    id: int
    result: Any
    binary: bytes | None = None
    raw: dict = field(default_factory=dict)


@dataclass
class CapturePart:
    """One captured resource for one frame: metadata plus the raw bytes."""

    meta: dict
    data: bytes

    @property
    def resource(self) -> str:
        return str(self.meta.get("resource", "resource"))

    @property
    def frame(self) -> int:
        return int(self.meta.get("frame", 0))


def read_token(token_file: str | os.PathLike) -> str:
    """Read the token from ``[Dev] TokenFile`` (hex, surrounding whitespace ignored)."""
    with open(token_file, encoding="utf-8") as f:
        token = f.read().strip()
    if not token:
        raise ValueError(f"token file {os.fspath(token_file)!r} is empty")
    return token


_CLOSED = object()


class DevClient:
    """A connection to the dev server.

    A background thread reads frames: responses are routed to the waiting
    caller by id, events go to ``on_event`` if set, otherwise to an internal
    queue read with :meth:`next_event` / :meth:`events`. Calls are thread-safe.
    """

    def __init__(
        self,
        sock: socket.socket,
        *,
        timeout: float = 30.0,
        on_event: Callable[[dict], None] | None = None,
        event_queue_size: int = 100_000,
    ):
        self.sock = sock
        self.timeout = timeout
        self.on_event = on_event
        self.server_info: dict = {}
        self.dropped_events = 0
        self._ids = itertools.count(1)
        self._send_lock = threading.Lock()
        self._pending: dict[int, queue.Queue] = {}
        self._pending_lock = threading.Lock()
        self._events: queue.Queue = queue.Queue(event_queue_size)
        self._closed_reason: BaseException | None = None
        sock.settimeout(None)
        self._reader = FrameReader(sock.recv)
        self._thread = threading.Thread(target=self._read_loop, name="devclient-reader", daemon=True)
        self._thread.start()

    # -- construction -------------------------------------------------------

    @classmethod
    def connect(
        cls,
        host: str = "127.0.0.1",
        port: int = DEFAULT_PORT,
        *,
        token: str | None = None,
        token_file: str | os.PathLike | None = None,
        timeout: float = 30.0,
        connect_timeout: float = 5.0,
        on_event: Callable[[dict], None] | None = None,
    ) -> DevClient:
        """Connect and authenticate. ``host`` may be ``host:port``.

        Raises :class:`AuthError` if the token is refused.
        """
        if token is None:
            if token_file is None:
                raise ValueError("token or token_file is required")
            token = read_token(token_file)
        host, port = split_address(host, port)
        sock = socket.create_connection((host, port), timeout=connect_timeout)
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        client = cls(sock, timeout=timeout, on_event=on_event)
        try:
            client.auth(token)
        except BaseException:
            client.close()
            raise
        return client

    def auth(self, token: str) -> dict:
        """Send ``auth`` (must be the first request); stores and returns the result."""
        try:
            info = self.call("auth", {"token": token})
        except DevError as e:
            raise AuthError(e.code, e.message, e.data) from None
        except ConnectionClosed as e:
            raise AuthError("unauthorized", f"server closed the connection ({e})") from None
        self.server_info = info if isinstance(info, dict) else {"result": info}
        return self.server_info

    def close(self) -> None:
        """Close the socket; pending calls fail with :class:`ConnectionClosed`."""
        try:
            self.sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.sock.close()
        self._thread.join(timeout=2)

    def __enter__(self) -> DevClient:
        return self

    def __exit__(self, *exc) -> None:
        self.close()

    @property
    def closed(self) -> bool:
        return self._closed_reason is not None

    # -- reader -------------------------------------------------------------

    def _read_loop(self) -> None:
        reason: BaseException
        try:
            while True:
                frame = self._reader.read_frame()
                if frame is None:
                    reason = ConnectionClosed("connection closed by server")
                    break
                self._dispatch(frame)
        except (OSError, ProtocolError) as e:
            reason = e if isinstance(e, ProtocolError) else ConnectionClosed(str(e))
        self._closed_reason = reason
        with self._pending_lock:
            waiters = list(self._pending.values())
        for q in waiters:
            q.put(_CLOSED)
        self._put_event(_CLOSED)

    def _dispatch(self, frame: Frame) -> None:
        msg = frame.msg
        if "event" in msg:
            if frame.binary is not None:
                msg = dict(msg, _binary=frame.binary)
            if self.on_event is not None:
                try:
                    self.on_event(msg)
                except Exception:  # a faulty callback must not kill the reader
                    traceback.print_exc()
            else:
                self._put_event(msg)
            return
        rid = msg.get("id")
        with self._pending_lock:
            q = self._pending.get(rid) if isinstance(rid, int) else None
        if q is not None:
            q.put(frame)
        # A response nobody waits for (its caller timed out) is dropped.

    def _put_event(self, item: Any) -> None:
        try:
            self._events.put_nowait(item)
        except queue.Full:
            if item is _CLOSED:
                return
            self.dropped_events += 1

    # -- requests -----------------------------------------------------------

    def send_request(self, method: str, params: Any = None) -> tuple[int, queue.Queue]:
        """Send one request; returns its id and the queue its frames arrive on.

        Low level: prefer :meth:`call` / :meth:`call_parts`.
        """
        if self._closed_reason is not None:
            raise ConnectionClosed(str(self._closed_reason))
        rid = next(self._ids)
        q: queue.Queue = queue.Queue()
        with self._pending_lock:
            self._pending[rid] = q
        msg = {"id": rid, "method": method, "params": {} if params is None else params}
        data = encode_frame(msg)
        try:
            with self._send_lock:
                self.sock.sendall(data)
        except OSError as e:
            self._forget(rid)
            raise ConnectionClosed(str(e)) from e
        return rid, q

    def _forget(self, rid: int) -> None:
        with self._pending_lock:
            self._pending.pop(rid, None)

    def _next_part(self, rid: int, q: queue.Queue, method: str, timeout: float | None) -> Response:
        try:
            item = q.get(timeout=self.timeout if timeout is None else timeout)
        except queue.Empty:
            raise TimeoutError(f"no response to {method!r} (id {rid})") from None
        if item is _CLOSED:
            raise ConnectionClosed(str(self._closed_reason))
        msg = item.msg
        if msg.get("ok") is not True:
            err = msg.get("error") or {}
            if not isinstance(err, dict):
                err = {"message": str(err)}
            raise DevError(str(err.get("code", "internal")), str(err.get("message", "")), err.get("data"))
        return Response(rid, msg.get("result"), item.binary, msg)

    def call_parts(
        self, method: str, params: Any = None, *, timeout: float | None = None
    ) -> Iterator[Response]:
        """Yield every response part (``more: true`` until the last one).

        ``timeout`` bounds the wait for each part (default: ``self.timeout``).
        """
        rid, q = self.send_request(method, params)
        try:
            while True:
                resp = self._next_part(rid, q, method, timeout)
                yield resp
                if not resp.raw.get("more"):
                    return
        finally:
            self._forget(rid)

    def call_raw(self, method: str, params: Any = None, *, timeout: float | None = None) -> Response:
        """Single-response call returning the full :class:`Response` (incl. binary)."""
        parts = list(self.call_parts(method, params, timeout=timeout))
        if len(parts) != 1:
            raise ProtocolError(f"{method!r} answered with {len(parts)} parts, expected 1")
        return parts[0]

    def call(self, method: str, params: Any = None, *, timeout: float | None = None) -> Any:
        """Call a method and return its ``result``; raises :class:`DevError`."""
        return self.call_raw(method, params, timeout=timeout).result

    # -- events -------------------------------------------------------------

    def next_event(self, timeout: float | None = None) -> dict | None:
        """Next queued event, or ``None`` after ``timeout`` seconds.

        Raises :class:`ConnectionClosed` once the connection is gone and the
        queue is drained. Not used when ``on_event`` is set.
        """
        try:
            item = self._events.get(timeout=timeout)
        except queue.Empty:
            return None
        if item is _CLOSED:
            self._events.put_nowait(_CLOSED)  # keep reporting closure
            raise ConnectionClosed(str(self._closed_reason))
        return item

    def events(self, kind: str | None = None, timeout: float | None = None) -> Iterator[dict]:
        """Iterate events (optionally only ``event == kind``) until none arrives
        within ``timeout`` or the connection closes."""
        while True:
            try:
                ev = self.next_event(timeout)
            except ConnectionClosed:
                return
            if ev is None:
                return
            if kind is None or ev.get("event") == kind:
                yield ev

    # -- methods ------------------------------------------------------------

    def hello(self) -> dict:
        return self.call("hello")

    def state(self) -> dict:
        return self.call("state")

    def settings_get(self) -> dict:
        return self.call("settings.get")

    def settings_set(self, section: str, key: str, value: Any) -> dict:
        """Set one setting live; the result says whether it needs a restart."""
        return self.call("settings.set", {"section": section, "key": key, "value": value})

    def settings_set_many(self, changes: list[tuple[str, str, Any]] | list[dict]) -> dict:
        """Set several settings in one request (``(section, key, value)`` tuples or dicts)."""
        items = [c if isinstance(c, dict) else {"section": c[0], "key": c[1], "value": c[2]} for c in changes]
        return self.call("settings.set", items)

    def experiment_set(self, key: str, value: Any) -> dict:
        return self.call("experiment.set", {"key": key, "value": value})

    def symbols(self) -> dict:
        return self.call("symbols")

    def mem_read(
        self,
        target: Expr | str | int,
        *,
        offset: int = 0,
        length: int | None = None,
        as_: str | None = None,
        count: int | None = None,
    ) -> bytes | list:
        """Read memory. Without a type returns bytes; with ``as_`` (or a typed
        expression) returns the list of decoded values. ``count`` is a number
        of elements and overrides ``length`` (bytes)."""
        expr = _target(target)
        params = expr.params()
        params["offset"] = expr.offset + offset
        typ = as_ or expr.as_
        if typ == "bytes":
            typ = None
        if typ is None:
            params.pop("as", None)
        else:
            params["as"] = typ
        if count is not None:
            length = count * type_size(typ or "bytes")
        if length is not None:
            params["len"] = length
        elif typ is None:
            raise ValueError("a raw read needs length")
        resp = self.call_raw("mem.read", params)
        if resp.binary is not None:
            return resp.binary if typ is None else decode_values(resp.binary, typ)
        result = resp.result or {}
        if typ is None:
            raise ProtocolError("raw mem.read returned no binary payload")
        return result.get("values", [])

    def mem_write(
        self,
        target: Expr | str | int,
        value: Any,
        *,
        as_: str | None = None,
        offset: int = 0,
        force: bool = False,
    ) -> dict:
        """Write ``value`` typed as ``as_``. ``bytes`` values travel as hex."""
        expr = _target(target)
        typ = as_ or expr.as_
        if typ is None:
            raise ValueError("mem_write needs a type")
        encode_values(value, typ)  # validate locally before touching the game
        params = expr.params()
        params["offset"] = expr.offset + offset
        params["as"] = typ
        params["value"] = value.hex() if isinstance(value, (bytes, bytearray)) else value
        if force:
            params["force"] = True
        return self.call("mem.write", params)

    def watch(
        self,
        name: str,
        exprs: list[Expr | str],
        *,
        every_frames: int = 1,
        point: str | None = None,
    ) -> Any:
        """Start a watch; events arrive as ``{"event": "watch", "name", "frame", "values"}``."""
        items = []
        for e in exprs:
            ex = _target(e)
            if ex.as_ is None:
                raise ValueError(f"watch expression {ex} needs a :type")
            items.append(ex.params())
        params: dict[str, Any] = {"name": name, "expr": items, "every_frames": every_frames}
        if point is not None:
            params["point"] = point
        return self.call("watch", params)

    def unwatch(self, name: str) -> Any:
        return self.call("unwatch", {"name": name})

    def log_subscribe(self, level: str = "info", targets: list[str] | None = None) -> Any:
        """Stream log records as ``{"event": "log", …}`` events."""
        return self.call("log.subscribe", {"level": level, "targets": targets or []})

    def log_tail(self, lines: int = 100) -> list[str]:
        result = self.call("log.tail", {"lines": lines})
        return list(result.get("lines", [])) if isinstance(result, dict) else list(result or [])

    def capture(
        self,
        resources: list[str],
        *,
        frames: int = 1,
        point: str | None = None,
        timeout: float | None = None,
    ) -> list[CapturePart]:
        """Capture resources; one part per resource and frame."""
        params: dict[str, Any] = {"resources": list(resources), "frames": frames}
        if point is not None:
            params["point"] = point
        parts = []
        for resp in self.call_parts("capture", params, timeout=timeout):
            if resp.binary is None:
                continue  # e.g. a summary part without pixels
            meta = resp.result if isinstance(resp.result, dict) else {}
            parts.append(CapturePart(meta, resp.binary))
        return parts

    def probe(self, kind: str, *, timeout: float | None = None, **params: Any) -> Any:
        """Run ``probe.<kind>`` (jitter, reset, hdr, mv, depth, wait, modules)."""
        return self.call(f"probe.{kind}", params, timeout=timeout)

    def trace(self, frames: int = 1, *, timeout: float | None = None) -> Any:
        return self.call("trace", {"frames": frames}, timeout=timeout)


def split_address(host: str, port: int = DEFAULT_PORT) -> tuple[str, int]:
    """Split ``host[:port]`` (``[v6]:port`` too); the port defaults to ``port``."""
    if host.startswith("["):
        end = host.find("]")
        if end < 0:
            raise ValueError(f"bad address {host!r}")
        rest = host[end + 1 :]
        return host[1:end], int(rest[1:]) if rest.startswith(":") else port
    if host.count(":") == 1:
        h, p = host.split(":")
        return h, int(p)
    return host, port
