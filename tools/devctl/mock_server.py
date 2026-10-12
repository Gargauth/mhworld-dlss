"""A fake dev server speaking ``docs/DEV-PROTOCOL.md`` with synthetic data.

Used by the tests, and runnable on its own so scripts built on
:class:`devclient.DevClient` can be developed without the game::

    python3 mock_server.py --listen 127.0.0.1:47800 --token-file dev.token

Simulated game: a frame counter advancing every ``frame_interval`` seconds,
five engine structs in a fake heap (writable), a read-only code page, the
settings sections of ARCHITECTURE.md with their validation, a log ring, and
captures with the real formats at small sizes (rows padded to 256 bytes like
a D3D12 readback). Every handled request is logged at debug level to target
``dev`` *before* its response is sent, so a log subscriber always sees an
event interleaved ahead of the response. Data and method handlers live in
:mod:`mock_game`; this module owns connections, auth, framing and the clock.
"""

from __future__ import annotations

import argparse
import collections
import json
import os
import secrets
import socket
import socketserver
import struct
import threading
import time
from typing import Any

from devclient import (
    POINTS,
    FrameReader,
    ProtocolError,
    decode_values,
    encode_frame,
    split_address,
    type_size,
)
from mock_game import (
    ALIASES,
    CODE,
    DISPLAY,
    LEVELS,
    METHODS,
    RENDER,
    SETTINGS,
    STRUCTS,
    MockError,
)


class Connection:
    """Per-client state: send lock, log subscription, watches."""

    def __init__(self, server: MockServer, sock: socket.socket):
        self.server = server
        self.sock = sock
        self.lock = threading.Lock()
        self.log_filter: tuple[int, set[str]] | None = None
        self.watches: dict[str, threading.Event] = {}
        self.alive = True

    def send(self, msg: dict, binary: bytes | None = None) -> None:
        data = encode_frame(msg, binary)
        with self.lock:
            if not self.alive:
                return
            try:
                self.sock.sendall(data)
            except OSError:
                self.alive = False

    def stop(self) -> None:
        self.alive = False
        for ev in self.watches.values():
            ev.set()


class MockServer:
    """Threaded fake dev server. ``port=0`` picks a free port (see ``address``)."""

    def __init__(
        self,
        token: str = "mock-token",
        host: str = "127.0.0.1",
        port: int = 0,
        *,
        frame_interval: float = 0.002,
        reply_on_auth_failure: bool = True,
    ):
        self.token = token
        self.frame_interval = frame_interval
        self.reply_on_auth_failure = reply_on_auth_failure
        self.started = time.monotonic()
        self.settings = {s: {k: d for k, (d, _, _) in keys.items()} for s, keys in SETTINGS.items()}
        self.memory = {name: bytearray(size) for name, (_, size) in STRUCTS.items()}
        self.memory[CODE[0]] = bytearray(b"\xcc" * CODE[2])
        self.ring: collections.deque[dict] = collections.deque(maxlen=1000)
        self.next_seq = 0
        self.connections: set[Connection] = set()
        self.lock = threading.Lock()
        self._init_memory()
        for msg in ("mod loaded", "settings parsed", "gate open", "dev server listening"):
            self.log("info", "boot", msg)

        outer = self

        class Handler(socketserver.BaseRequestHandler):
            def handle(self) -> None:
                outer._serve(self.request)

        class Server(socketserver.ThreadingTCPServer):
            daemon_threads = True
            allow_reuse_address = True

        self._tcp = Server((host, port), Handler)
        self._thread: threading.Thread | None = None
        self._stop = threading.Event()

    # -- lifecycle ----------------------------------------------------------

    @property
    def address(self) -> tuple[str, int]:
        host, port = self._tcp.server_address[:2]
        return str(host), int(port)

    def start(self) -> MockServer:
        self._thread = threading.Thread(
            target=self._tcp.serve_forever, args=(0.05,), name="mock-dev", daemon=True
        )
        self._thread.start()
        threading.Thread(target=self._tick, name="mock-tick", daemon=True).start()
        return self

    def _tick(self) -> None:
        """A debug ``perf`` record every 30 frames, like the real per-frame stats."""
        while not self._stop.wait(30 * self.frame_interval):
            self.log("debug", "perf", "frame stats", frame=self.frame, gpu_ms=4.2)

    def stop(self) -> None:
        self._stop.set()
        self._tcp.shutdown()
        self._tcp.server_close()
        with self.lock:
            conns = list(self.connections)
        for c in conns:
            c.stop()
            try:
                c.sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass

    def __enter__(self) -> MockServer:
        return self.start()

    def __exit__(self, *exc: Any) -> None:
        self.stop()

    # -- simulated game -----------------------------------------------------

    @property
    def frame(self) -> int:
        return 1000 + int((time.monotonic() - self.started) / self.frame_interval)

    def _init_memory(self) -> None:
        cfg = self.memory["RenderConfig"]
        struct.pack_into("<II", cfg, 0x100, *RENDER)
        struct.pack_into("<f", cfg, 0x200, RENDER[0] / DISPLAY[0])
        cam = self.memory["Camera"]
        struct.pack_into("<16f", cam, 0x0, *[1.0 if i % 5 == 0 else 0.0 for i in range(16)])
        struct.pack_into("<16f", cam, 0x40, 1.0, 0, 0, 0, 0, 1.78, 0, 0, 0, 0, 0, 1, 0, 0, 0.1, 0)
        struct.pack_into("<I", self.memory["RenderSettings"], 0x20, 32)
        struct.pack_into("<H", self.memory["TaaFilter"], 0x30, 1)

    def sync_memory(self) -> None:
        """Refresh the live fields (the frame counter) before a read."""
        struct.pack_into("<Q", self.memory["RenderConfig"], 0x10, self.frame)

    def log(self, level: str, target: str, message: str, **fields: Any) -> None:
        """Append a record to the ring and stream it to matching subscribers."""
        now = time.time()
        stamp = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(now)) + f".{int(now * 1000) % 1000:03d}"
        tail = "".join(f" {k}={v}" for k, v in fields.items())
        line = f"{stamp} {level[0].upper()} [{target}] {message}{tail}"
        rec = {
            "event": "log",
            "time": stamp,
            "level": level,
            "target": target,
            "message": message,
            # The server parses fields back out of the formatted line: strings.
            "fields": {k: str(v) for k, v in fields.items()},
            "line": line,
        }
        lvl = LEVELS.index(level)
        with self.lock:
            rec["seq"] = self.next_seq
            self.next_seq += 1
            self.ring.append(rec)
            conns = list(self.connections)
        for c in conns:
            f = c.log_filter
            if f is not None and lvl <= f[0] and (not f[1] or target in f[1]):
                c.send(rec)

    # -- connection loop ----------------------------------------------------

    def _serve(self, sock: socket.socket) -> None:
        conn = Connection(self, sock)
        reader = FrameReader(sock.recv)
        try:
            if not self._authenticate(conn, reader):
                return
            with self.lock:
                self.connections.add(conn)
            while conn.alive:
                try:
                    frame = reader.read_frame()
                except ProtocolError as e:
                    conn.send({"id": None, "ok": False, "error": {"code": "bad_request", "message": str(e)}})
                    return
                if frame is None:
                    return
                self._handle(conn, frame.msg)
        except OSError:
            pass
        finally:
            conn.stop()
            with self.lock:
                self.connections.discard(conn)
            try:
                sock.close()
            except OSError:
                pass

    def _authenticate(self, conn: Connection, reader: FrameReader) -> bool:
        try:
            frame = reader.read_frame()
        except ProtocolError:
            return False
        if frame is None:
            return False
        msg = frame.msg
        params = msg.get("params") if isinstance(msg.get("params"), dict) else {}
        if msg.get("method") == "auth" and secrets.compare_digest(str(params.get("token", "")), self.token):
            self.log("info", "dev", "client authenticated")
            conn.send(
                {
                    "id": msg.get("id"),
                    "ok": True,
                    "result": {
                        "build": "dev",
                        "game_build": "timestamp 0x66d13cb6, image size 0x55d3000",
                        "features": ["dev"],
                    },
                }
            )
            return True
        self.log("warn", "dev", "authentication failed")
        if self.reply_on_auth_failure:
            conn.send(
                {
                    "id": msg.get("id"),
                    "ok": False,
                    "error": {"code": "unauthorized", "message": "auth required"},
                }
            )
        return False

    def _handle(self, conn: Connection, msg: dict) -> None:
        rid = msg.get("id")
        method = msg.get("method")
        params = msg.get("params", {})
        try:
            if not isinstance(rid, int) or not isinstance(method, str):
                raise MockError("bad_request", "request needs an integer id and a method")
            handler = METHODS.get(method)
            if handler is None:
                raise MockError("unknown_method", f"no method {method!r}")
            if params is None:
                params = {}
            if method != "settings.set" and not isinstance(params, dict):
                raise MockError("bad_request", "params must be an object")
            point = params.get("point", "frame") if isinstance(params, dict) else "frame"
            if point not in POINTS:
                raise MockError("bad_request", f"unknown point {point!r}")
            out = handler(self, conn, params)
        except MockError as e:
            self.log("debug", "dev", "request failed", method=method, code=e.code)
            conn.send({"id": rid, "ok": False, "error": {"code": e.code, "message": e.message}})
            return
        except (KeyError, TypeError, ValueError) as e:
            conn.send({"id": rid, "ok": False, "error": {"code": "bad_request", "message": str(e)}})
            return
        self.log("debug", "dev", "request", method=method)
        parts = out if isinstance(out, list) else [out]
        for i, part in enumerate(parts):
            result, binary = part if isinstance(part, tuple) else (part, None)
            resp: dict[str, Any] = {"id": rid, "ok": True, "result": result}
            if i + 1 < len(parts):
                resp["more"] = True
            conn.send(resp, binary)

    # -- memory -------------------------------------------------------------

    def _locate(self, p: dict) -> tuple[str, int, int]:
        """Resolve ``{addr|symbol, offset}`` to (region, region offset, absolute address)."""
        offset = int(p.get("offset", 0) or 0)
        if "symbol" in p:
            name = ALIASES.get(p["symbol"], p["symbol"])
            if name in STRUCTS:
                base = STRUCTS[name][0]
            elif name == CODE[0]:
                base = CODE[1]
            else:
                raise MockError("bad_request", f"unknown symbol {p['symbol']!r}")
        elif "addr" in p:
            a = p["addr"]
            base = int(a, 0) if isinstance(a, str) else int(a)
        else:
            raise MockError("bad_request", "addr or symbol required")
        addr = base + offset
        for name, (start, size) in [*STRUCTS.items(), (CODE[0], (CODE[1], CODE[2]))]:
            if start <= addr < start + size:
                return name, addr - start, addr
        raise MockError("refused", f"{addr:#x} is not mapped")

    def span(self, p: dict, length: int) -> tuple[str, int, int]:
        """Like ``_locate`` but also checks that ``length`` bytes fit in the region."""
        region, off, addr = self._locate(p)
        if length < 0 or off + length > len(self.memory[region]):
            raise MockError("refused", f"{length} bytes at {addr:#x} cross the end of {region}")
        return region, off, addr

    def read_typed(self, p: dict, as_: str, count: int = 1) -> list:
        """Read ``count`` elements of ``as_`` at ``{addr|symbol, offset}``."""
        size = type_size(as_)
        region, off, _ = self.span(p, size * count)
        self.sync_memory()
        return decode_values(bytes(self.memory[region][off : off + size * count]), as_)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Fake dev server with synthetic data.")
    ap.add_argument("--listen", default="127.0.0.1:47800", help="host:port (default %(default)s)")
    ap.add_argument("--token", help="token to accept (default: read or create --token-file)")
    ap.add_argument("--token-file", default="dev.token", help="created with a random token if missing")
    ap.add_argument("--frame-ms", type=float, default=16.6, help="simulated frame time")
    args = ap.parse_args(argv)
    token = args.token
    if token is None:
        if os.path.exists(args.token_file):
            with open(args.token_file, encoding="utf-8") as f:
                token = f.read().strip()
        else:
            token = secrets.token_hex(32)
            with open(args.token_file, "w", encoding="utf-8") as f:
                f.write(token + "\n")
    host, port = split_address(args.listen)
    server = MockServer(token, host, port, frame_interval=args.frame_ms / 1000).start()
    print(json.dumps({"listening": "%s:%d" % server.address, "token_file": args.token_file}), flush=True)
    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        server.stop()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
