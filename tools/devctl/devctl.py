#!/usr/bin/env python3
"""Command-line client for the mod's dev server (``docs/DEV-PROTOCOL.md``).

    devctl --host 192.168.1.20 --token-file dev.token state
    devctl … set Experimental JitterScale Render
    devctl … capture colour depth mv --frames 1 --out ./cap --convert
    devctl … probe jitter --frames 64
    devctl … watch cfg+0x200:f32 --every 1
    devctl … log --level debug --targets render,fg
    devctl … repl

Results print as indented JSON (``--json``: one line). Streams (``watch``,
``log``) print one human line per event (``--json``: the event as JSON) until
``--count`` events, ``--duration`` seconds, or Ctrl-C. Exit status: 0 ok,
1 the server returned an error, 2 connection/usage problem.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import threading
import time
from pathlib import Path
from typing import Any

from devclient import (
    DEFAULT_PORT,
    POINTS,
    TYPES,
    AuthError,
    ConnectionClosed,
    DevClient,
    DevError,
    Expr,
    ProtocolError,
    type_size,
)

RESOURCES = ["colour", "depth", "mv", "mv_raw", "pass_depth", "dlss_out", "hudless", "backbuffer", "game_mv", "game_depth"]
PROBES = ["jitter", "reset", "hdr", "mv", "depth", "wait", "modules"]
LEVELS = ["error", "warn", "info", "debug", "trace"]


class UsageError(Exception):
    """Bad command-line input detected after parsing."""


def _out(args: argparse.Namespace, value: Any) -> None:
    if args.json:
        print(json.dumps(value, separators=(",", ":"), ensure_ascii=False), flush=True)
    else:
        print(json.dumps(value, indent=2, ensure_ascii=False), flush=True)


def parse_value(text: str, as_: str) -> Any:
    """Parse a command-line value for ``mem write``: number, mat4 (16 numbers
    separated by commas/spaces) or hex bytes."""
    if as_ == "bytes":
        return bytes.fromhex(text.replace(" ", "")).hex()
    if as_ == "mat4":
        nums = [float(t) for t in text.replace(",", " ").split()]
        if len(nums) != 16:
            raise UsageError(f"mat4 needs 16 numbers, got {len(nums)}")
        return nums
    if as_.startswith("f"):
        return float(text)
    return int(text, 0)


def format_value(v: Any) -> str:
    if isinstance(v, float):
        return f"{v:.9g}"
    if isinstance(v, list):
        return "[" + ", ".join(format_value(x) for x in v) + "]"
    return str(v)


def _stream(client: DevClient, args: argparse.Namespace, kind: str, show, match=None) -> None:
    """Print events of ``kind`` until --count / --duration / Ctrl-C."""
    deadline = time.monotonic() + args.duration if args.duration else None
    seen = 0
    try:
        while args.count is None or seen < args.count:
            wait = None if deadline is None else deadline - time.monotonic()
            if wait is not None and wait <= 0:
                break
            ev = client.next_event(timeout=0.5 if wait is None else min(wait, 0.5))
            if ev is None or ev.get("event") != kind or (match is not None and not match(ev)):
                continue
            seen += 1
            if args.json:
                print(
                    json.dumps(
                        {k: v for k, v in ev.items() if k != "_binary"},
                        separators=(",", ":"),
                        ensure_ascii=False,
                    ),
                    flush=True,
                )
            else:
                print(show(ev), flush=True)
    except KeyboardInterrupt:
        pass


# -- commands ----------------------------------------------------------------


def cmd_simple(client: DevClient, args: argparse.Namespace) -> None:
    _out(args, client.call(args.method))


def cmd_settings(client: DevClient, args: argparse.Namespace) -> None:
    result = client.settings_get()
    if args.section:
        if args.section not in result:
            raise UsageError(f"no section {args.section!r} (have {', '.join(result)})")
        result = result[args.section]
    _out(args, result)


def cmd_set(client: DevClient, args: argparse.Namespace) -> None:
    if len(args.triples) % 3:
        raise UsageError("set takes SECTION KEY VALUE triples")
    t = args.triples
    changes = [(t[i], t[i + 1], t[i + 2]) for i in range(0, len(t), 3)]
    if len(changes) == 1:
        _out(args, client.settings_set(*changes[0]))
    else:
        _out(args, client.settings_set_many(changes))


def cmd_experiment(client: DevClient, args: argparse.Namespace) -> None:
    _out(args, client.experiment_set(args.key, args.value))


def cmd_mem_read(client: DevClient, args: argparse.Namespace) -> None:
    expr = Expr.parse(args.expr)
    as_ = args.as_ or expr.as_
    if args.out and as_ not in (None, "bytes"):
        raise UsageError("--out saves raw bytes; drop the type")
    if as_ in (None, "bytes"):
        length = args.len if args.len is not None else args.count
        if length is None:
            raise UsageError("a raw read needs --len")
        data = client.mem_read(Expr(expr.symbol, expr.addr, expr.offset), length=length)
        if args.out:
            Path(args.out).write_bytes(data)
            _out(args, {"saved": args.out, "len": len(data)})
        elif args.json:
            _out(args, {"hex": data.hex()})
        else:
            print(hexdump(data, _base(expr)), flush=True)
        return
    count = args.count if args.count is not None else (args.len // type_size(as_) if args.len else 1)
    values = client.mem_read(expr, as_=as_, count=count)
    if args.json:
        _out(args, {"expr": str(Expr(expr.symbol, expr.addr, expr.offset, as_)), "values": values})
    else:
        size = type_size(as_)
        for i, v in enumerate(values):
            e = Expr(expr.symbol, expr.addr, expr.offset + i * size, as_)
            print(f"{e} = {format_value(v)}", flush=True)


def _base(expr: Expr) -> int:
    return (expr.addr or 0) + expr.offset if expr.symbol is None else expr.offset


def hexdump(data: bytes, base: int = 0) -> str:
    """Classic 16-bytes-per-line hex dump with ASCII column."""
    lines = []
    for i in range(0, len(data), 16):
        chunk = data[i : i + 16]
        hexpart = " ".join(f"{b:02x}" for b in chunk)
        text = "".join(chr(b) if 32 <= b < 127 else "." for b in chunk)
        lines.append(f"{base + i:#014x}  {hexpart:<47}  {text}")
    return "\n".join(lines)


def cmd_mem_write(client: DevClient, args: argparse.Namespace) -> None:
    expr = Expr.parse(args.expr)
    as_ = args.as_ or expr.as_
    if as_ is None:
        raise UsageError("mem write needs a type (EXPR:type or --as)")
    value = parse_value(args.value, as_)
    if as_ == "bytes":
        value = bytes.fromhex(value)
    _out(args, client.mem_write(expr, value, as_=as_, force=args.force))


def cmd_watch(client: DevClient, args: argparse.Namespace) -> None:
    exprs = [Expr.parse(e, args.as_) for e in args.exprs]
    name = args.name or "devctl-%d" % os.getpid()
    client.watch(name, exprs, every_frames=args.every, point=args.point)

    def show(ev: dict) -> str:
        vals = ev.get("values", [])
        cols = " ".join(f"{e}={format_value(v)}" for e, v in zip(exprs, vals))
        return f"frame={ev.get('frame')} {cols}"

    try:
        _stream(client, args, "watch", show, match=lambda ev: ev.get("name") == name)
    finally:
        if not client.closed:
            client.unwatch(name)


def cmd_unwatch(client: DevClient, args: argparse.Namespace) -> None:
    _out(args, client.unwatch(args.name))


def cmd_log(client: DevClient, args: argparse.Namespace) -> None:
    targets = [t for t in (args.targets or "").split(",") if t]
    client.log_subscribe(args.level, targets)

    def show(ev: dict) -> str:
        if "line" in ev:
            return str(ev["line"])
        fields = "".join(f" {k}={v}" for k, v in (ev.get("fields") or {}).items())
        level = str(ev.get("level", "?"))[:1].upper()
        return f"{ev.get('time', '')} {level} [{ev.get('target')}] {ev.get('message', '')}{fields}"

    _stream(client, args, "log", show)


def cmd_tail(client: DevClient, args: argparse.Namespace) -> None:
    lines = client.log_tail(args.lines)
    if args.json:
        _out(args, lines)
    else:
        for line in lines:
            print(line, flush=True)


def cmd_capture(client: DevClient, args: argparse.Namespace) -> None:
    from capture import save_part

    parts = client.capture(args.resources, frames=args.frames, point=args.point, timeout=args.timeout)
    out = Path(args.out)
    files: list[str] = []
    for part in parts:
        try:
            paths = save_part(part.meta, part.data, out, convert=args.convert)
        except ValueError as e:  # malformed size/pitch from the server: keep the rest
            print(f"warning: {part.resource} frame {part.frame}: {e}", file=sys.stderr)
            paths = save_part(part.meta, part.data, out, convert=False)
        files += [str(p) for p in paths]
    _out(args, {"out": str(out), "parts": len(parts), "files": files})


def cmd_probe(client: DevClient, args: argparse.Namespace) -> None:
    params: dict[str, Any] = {}
    if args.kind == "wait":
        params["seconds"] = args.seconds
    elif args.kind != "modules":
        params["frames"] = args.frames
    _out(args, client.probe(args.kind, timeout=args.timeout, **params))


def cmd_trace(client: DevClient, args: argparse.Namespace) -> None:
    _out(args, client.trace(args.frames, timeout=args.timeout))


def cmd_call(client: DevClient, args: argparse.Namespace) -> None:
    params = json.loads(args.params) if args.params else {}
    for resp in client.call_parts(args.method, params, timeout=args.timeout):
        value: dict[str, Any] = {"result": resp.result}
        if resp.binary is not None:
            value["binary"] = len(resp.binary)
        _out(args, value)


def cmd_repl(client: DevClient, args: argparse.Namespace) -> None:
    """JSON lines in, JSON lines out. Input: a request object (``id`` is
    assigned), or ``method`` optionally followed by a JSON params object."""
    lock = threading.Lock()

    def emit(obj: dict) -> None:
        with lock:
            print(json.dumps(obj, separators=(",", ":"), ensure_ascii=False), flush=True)

    client.on_event = lambda ev: emit({k: v for k, v in ev.items() if k != "_binary"})
    interactive = sys.stdin.isatty()
    while True:
        if interactive:
            print("devctl> ", end="", file=sys.stderr, flush=True)
        line = sys.stdin.readline()
        if not line:
            break
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if line in ("quit", "exit"):
            break
        try:
            if line.startswith("{"):
                req = json.loads(line)
                method, params = req["method"], req.get("params", {})
            else:
                method, _, rest = line.partition(" ")
                params = json.loads(rest) if rest.strip() else {}
        except (ValueError, KeyError, TypeError) as e:
            emit({"ok": False, "error": {"code": "bad_input", "message": str(e)}})
            continue
        try:
            for resp in client.call_parts(method, params, timeout=args.timeout):
                out = dict(resp.raw)
                if resp.binary is not None:
                    out["binary"] = len(resp.binary)
                emit(out)
        except DevError as e:
            emit({"ok": False, "error": {"code": e.code, "message": e.message}})
        except TimeoutError as e:
            emit({"ok": False, "error": {"code": "timeout", "message": str(e)}})


# -- argument parsing ----------------------------------------------------------


class _SubParser(argparse.ArgumentParser):
    """Subcommand parser that also accepts ``--json`` after the command."""

    def __init__(self, *a: Any, **kw: Any):
        super().__init__(*a, **kw)
        self.add_argument(
            "--json",
            action="store_true",
            default=argparse.SUPPRESS,
            help="one-line JSON output (JSON lines for streams)",
        )


def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(
        prog="devctl",
        description=__doc__.split("\n\n")[0],
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    env = os.environ.get
    ap.add_argument(
        "--host",
        default=env("DEVCTL_HOST", "127.0.0.1"),
        help="server host or host:port ($DEVCTL_HOST, default 127.0.0.1)",
    )
    ap.add_argument(
        "--port",
        type=int,
        default=int(env("DEVCTL_PORT", DEFAULT_PORT)),
        help=f"server port ($DEVCTL_PORT, default {DEFAULT_PORT})",
    )
    ap.add_argument("--token", default=env("DEVCTL_TOKEN"), help="token value ($DEVCTL_TOKEN)")
    ap.add_argument(
        "--token-file",
        default=env("DEVCTL_TOKEN_FILE", "dev.token"),
        help="file holding the token ($DEVCTL_TOKEN_FILE, default dev.token)",
    )
    ap.add_argument(
        "--timeout",
        type=float,
        help="seconds to wait for a response (default: from the request, see default_timeout)",
    )
    ap.add_argument("--json", action="store_true", help="one-line JSON output (JSON lines for streams)")
    sub = ap.add_subparsers(dest="command", required=True, metavar="COMMAND", parser_class=_SubParser)

    for name, method, help_ in [
        ("hello", "hello", "mod version, build, gate, subsystems"),
        ("state", "state", "state snapshot"),
        ("symbols", "symbols", "address book and struct field table"),
    ]:
        p = sub.add_parser(name, help=help_)
        p.set_defaults(func=cmd_simple, method=method)

    p = sub.add_parser("settings", help="effective settings (settings.get)")
    p.add_argument("section", nargs="?", help="only this section")
    p.set_defaults(func=cmd_settings)

    p = sub.add_parser("set", help="settings.set: SECTION KEY VALUE [SECTION KEY VALUE ...]")
    p.add_argument("triples", nargs="+", metavar="SECTION KEY VALUE")
    p.set_defaults(func=cmd_set)

    p = sub.add_parser("experiment", help="experiment.set: KEY VALUE in [Experimental]")
    p.add_argument("key")
    p.add_argument("value")
    p.set_defaults(func=cmd_experiment)

    types = [*TYPES, "bytes"]
    mem = sub.add_parser("mem", help="read/write game memory").add_subparsers(dest="mem_cmd", required=True)
    p = mem.add_parser("read", help="mem.read EXPR (symbol|0xaddr[+off][:type])")
    p.add_argument("expr")
    p.add_argument("--as", dest="as_", choices=types, help="element type (overrides :type)")
    g = p.add_mutually_exclusive_group()
    g.add_argument("--len", type=lambda s: int(s, 0), help="bytes to read")
    g.add_argument("--count", type=lambda s: int(s, 0), help="elements to read")
    p.add_argument("--out", help="save raw bytes to this file")
    p.set_defaults(func=cmd_mem_read)
    p = mem.add_parser("write", help="mem.write EXPR VALUE")
    p.add_argument("expr")
    p.add_argument("value", help="number, 16 comma-separated numbers for mat4, hex for bytes")
    p.add_argument("--as", dest="as_", choices=types)
    p.add_argument("--force", action="store_true", help="allow writes outside the engine structs")
    p.set_defaults(func=cmd_mem_write)

    def stream_opts(p: argparse.ArgumentParser) -> None:
        p.add_argument("--count", type=int, help="stop after this many events")
        p.add_argument("--duration", type=float, help="stop after this many seconds")

    p = sub.add_parser("watch", help="stream values every N frames: EXPR:type ...")
    p.add_argument("exprs", nargs="+", metavar="EXPR")
    p.add_argument("--as", dest="as_", choices=list(TYPES), help="type for expressions without :type")
    p.add_argument("--every", type=int, default=1, help="every N frames (default 1)")
    p.add_argument("--point", choices=POINTS)
    p.add_argument("--name", help="watch name (default devctl-<pid>)")
    stream_opts(p)
    p.set_defaults(func=cmd_watch)

    p = sub.add_parser("unwatch", help="stop a watch by name")
    p.add_argument("name")
    p.set_defaults(func=cmd_unwatch)

    p = sub.add_parser("log", help="stream the log (log.subscribe)")
    p.add_argument("--level", default="info", choices=LEVELS)
    p.add_argument("--targets", help="comma-separated targets, e.g. render,fg (default all)")
    stream_opts(p)
    p.set_defaults(func=cmd_log)

    p = sub.add_parser("tail", help="last lines of the log ring (log.tail)")
    p.add_argument("--lines", type=int, default=100)
    p.set_defaults(func=cmd_tail)

    p = sub.add_parser("capture", help="capture resources to raw + JSON (+ PNG/PFM)")
    p.add_argument("resources", nargs="+", choices=RESOURCES, metavar="RESOURCE", help=", ".join(RESOURCES))
    p.add_argument("--frames", type=int, default=1)
    p.add_argument("--point", choices=POINTS)
    p.add_argument("--out", default="capture", help="output directory (default ./capture)")
    p.add_argument("--convert", action="store_true", help="also write PNG (8-bit) or PFM (float)")
    p.set_defaults(func=cmd_capture)

    p = sub.add_parser("probe", help="run a probe: " + ", ".join(PROBES))
    p.add_argument("kind", choices=PROBES)
    p.add_argument("--frames", type=int, default=16)
    p.add_argument("--seconds", type=float, default=5.0, help="for probe wait")
    p.set_defaults(func=cmd_probe)

    p = sub.add_parser("trace", help="per-frame timeline of the mod's events")
    p.add_argument("--frames", type=int, default=4)
    p.set_defaults(func=cmd_trace)

    p = sub.add_parser("call", help="raw request: METHOD [PARAMS_JSON]")
    p.add_argument("method")
    p.add_argument("params", nargs="?")
    p.set_defaults(func=cmd_call)

    p = sub.add_parser("repl", help="interactive JSON lines")
    p.set_defaults(func=cmd_repl)
    return ap


FRAME_COMMANDS = {"capture", "probe", "trace"}


def default_timeout(args: argparse.Namespace) -> float:
    """How long to wait when ``--timeout`` is not given: longer than the
    server's own wait for the request. ``probe wait``: seconds + 10; a
    frame-count request: 5 + 0.1 s per frame (10 fps) + 10, at least 60."""
    seconds: Any = None
    frames: Any = None
    if args.command == "probe" and args.kind == "wait":
        seconds = args.seconds
    elif args.command in FRAME_COMMANDS and not (args.command == "probe" and args.kind == "modules"):
        frames = args.frames
    elif args.command == "call" and args.params:
        try:
            params = json.loads(args.params)
        except ValueError:
            params = None
        if isinstance(params, dict):
            seconds, frames = params.get("seconds"), params.get("frames")
    if isinstance(seconds, (int, float)) and not isinstance(seconds, bool):
        return float(seconds) + 10.0
    if isinstance(frames, int) and not isinstance(frames, bool):
        return max(60.0, 5.0 + 0.1 * frames + 10.0)
    return 60.0


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    if args.timeout is None:
        args.timeout = default_timeout(args)
    try:
        client = DevClient.connect(
            args.host,
            args.port,
            token=args.token,
            token_file=None if args.token else args.token_file,
            timeout=args.timeout,
        )
    except AuthError as e:
        print(f"devctl: authentication failed: {e}", file=sys.stderr)
        return 2
    except (OSError, ValueError, ConnectionClosed, ProtocolError, TimeoutError) as e:
        print(f"devctl: cannot connect: {e}", file=sys.stderr)
        return 2
    try:
        args.func(client, args)
    except DevError as e:
        print(f"devctl: {e.code}: {e.message}", file=sys.stderr)
        return 1
    except UsageError as e:
        print(f"devctl: {e}", file=sys.stderr)
        return 2
    except (ConnectionClosed, ProtocolError, TimeoutError, ValueError) as e:
        print(f"devctl: {type(e).__name__}: {e}", file=sys.stderr)
        return 2
    finally:
        client.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
