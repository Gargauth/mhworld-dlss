"""Every devctl subcommand end to end against the mock server."""

import contextlib
import io
import json
import socket
import struct
import sys
import tempfile
import unittest
from pathlib import Path

import devctl
from capture import read_pfm, read_png_rgb
from mock_server import MockServer


class DefaultTimeoutTest(unittest.TestCase):
    def timeout(self, *argv: str) -> float:
        args = devctl.build_parser().parse_args(list(argv))
        return devctl.default_timeout(args)

    def test_waits_longer_than_the_server(self):
        self.assertEqual(self.timeout("probe", "wait", "--seconds", "60"), 70.0)
        self.assertEqual(self.timeout("probe", "jitter", "--frames", "1024"), 5 + 102.4 + 10)
        self.assertEqual(self.timeout("probe", "jitter", "--frames", "16"), 60.0)
        self.assertEqual(self.timeout("trace", "--frames", "1000"), 115.0)
        self.assertEqual(self.timeout("capture", "colour"), 60.0)
        self.assertEqual(self.timeout("probe", "modules"), 60.0)
        self.assertEqual(self.timeout("state"), 60.0)
        self.assertEqual(self.timeout("call", "probe.wait", '{"seconds": 55}'), 65.0)
        self.assertEqual(self.timeout("call", "probe.reset", '{"frames": 800}'), 95.0)

    def test_an_explicit_timeout_wins(self):
        args = devctl.build_parser().parse_args(["--timeout", "3", "probe", "wait", "--seconds", "60"])
        self.assertEqual(args.timeout, 3.0)


class CliTest(unittest.TestCase):
    def setUp(self):
        self.server = MockServer(token="tok").start()
        self.addCleanup(self.server.stop)
        host, port = self.server.address
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name)
        token_file = self.tmp / "dev.token"
        token_file.write_text("tok\n")
        self.base = ["--host", f"{host}:{port}", "--token-file", str(token_file), "--timeout", "10"]

    def run_cli(self, *argv, stdin=""):
        out, err = io.StringIO(), io.StringIO()
        old_stdin = sys.stdin
        sys.stdin = io.StringIO(stdin)
        try:
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                code = devctl.main([*self.base, *argv])
        finally:
            sys.stdin = old_stdin
        return code, out.getvalue(), err.getvalue()

    def ok(self, *argv, **kw):
        code, out, err = self.run_cli(*argv, **kw)
        self.assertEqual(code, 0, err)
        return out

    def ok_json(self, *argv):
        return json.loads(self.ok(*argv))

    def test_hello_state_symbols(self):
        self.assertEqual(self.ok_json("hello")["gate"], "open")
        self.assertIn("render_size", self.ok_json("state"))
        self.assertIn("Camera", self.ok_json("symbols")["symbols"])
        self.assertEqual(len(self.ok("--json", "state").splitlines()), 1)
        self.assertEqual(len(self.ok("state", "--json").splitlines()), 1)

    def test_settings_and_set(self):
        self.assertIn("Experimental", self.ok_json("settings"))
        self.assertEqual(self.ok_json("settings", "Upscaling")["Mode"], "Quality")
        self.assertEqual(self.run_cli("settings", "Nope")[0], 2)
        r = self.ok_json("set", "Experimental", "JitterScale", "Render")
        self.assertEqual(r["changes"][0]["value"], "Render")
        r = self.ok_json("set", "Upscaling", "Mode", "Custom", "Upscaling", "RenderScale", "0.75")
        self.assertEqual([c["key"] for c in r["changes"]], ["Mode", "RenderScale"])
        self.assertEqual(self.ok_json("settings", "Upscaling")["RenderScale"], 0.75)
        self.assertEqual(self.run_cli("set", "Upscaling", "Mode")[0], 2)
        code, _, err = self.run_cli("set", "Upscaling", "Mode", "Ultra")
        self.assertEqual(code, 1)
        self.assertIn("bad_request", err)

    def test_experiment(self):
        self.assertEqual(self.ok_json("experiment", "HudLess", "true")["changes"][0]["value"], True)
        self.assertEqual(self.ok_json("settings", "Experimental")["HudLess"], True)

    def test_mem_read(self):
        self.assertEqual(self.ok("mem", "read", "cfg+0x200:f32").strip(), "cfg+0x200:f32 = 0.5")
        out = self.ok("mem", "read", "cfg+0x100", "--as", "u32", "--count", "2").splitlines()
        self.assertEqual(out, ["cfg+0x100:u32 = 64", "cfg+0x104:u32 = 36"])
        self.assertEqual(self.ok_json("--json", "mem", "read", "Camera:mat4")["values"][0][5], 1.0)
        dump = self.ok("mem", "read", "cfg+0x100", "--len", "8")
        self.assertIn("40 00 00 00 24 00 00 00", dump)
        self.assertEqual(self.ok_json("--json", "mem", "read", "cfg+0x100", "--len", "4")["hex"], "40000000")
        raw = self.tmp / "m.bin"
        self.ok("mem", "read", "cfg+0x100", "--len", "8", "--out", str(raw))
        self.assertEqual(raw.read_bytes(), struct.pack("<2I", 64, 36))
        self.assertEqual(self.run_cli("mem", "read", "cfg")[0], 2)  # raw read without --len
        self.assertEqual(self.run_cli("mem", "read", "0x10:u32")[0], 1)  # unmapped -> refused

    def test_mem_write(self):
        self.assertEqual(self.ok_json("mem", "write", "cfg+0x300:f32", "1.25")["written"], 4)
        self.assertIn("= 1.25", self.ok("mem", "read", "cfg+0x300:f32"))
        mat = ",".join(str(float(i)) for i in range(16))
        self.assertEqual(self.ok_json("mem", "write", "cam+0x80", mat, "--as", "mat4")["written"], 64)
        self.assertEqual(self.ok_json("--json", "mem", "read", "cam+0x80:mat4")["values"][0][15], 15.0)
        self.ok("mem", "write", "taa+0x30", "01 00", "--as", "bytes")
        self.ok("mem", "write", "dev+0x10da:u8", "0x1")
        self.assertIn("= 1", self.ok("mem", "read", "dev+0x10da:u8"))
        code, _, err = self.run_cli("mem", "write", "GameCode:u8", "0x90")
        self.assertEqual(code, 1)
        self.assertIn("refused", err)
        self.ok("mem", "write", "GameCode:u8", "0x90", "--force")
        self.assertEqual(self.run_cli("mem", "write", "cfg", "1")[0], 2)  # no type
        self.assertEqual(self.run_cli("mem", "write", "cam:mat4", "1,2")[0], 2)

    def test_watch(self):
        lines = self.ok(
            "watch",
            "cfg+0x10:u64",
            "cfg+0x200",
            "--as",
            "f32",
            "--count",
            "3",
            "--every",
            "2",
            "--point",
            "camera",
        ).splitlines()
        self.assertEqual(len(lines), 3)
        self.assertRegex(lines[0], r"^frame=\d+ cfg\+0x10:u64=\d+ cfg\+0x200:f32=0.5$")
        events = [
            json.loads(x)
            for x in self.ok("--json", "watch", "cfg+0x10:u64", "--name", "w", "--count", "2").splitlines()
        ]
        self.assertEqual([e["name"] for e in events], ["w", "w"])
        timed = self.ok("watch", "cfg+0x10:u64", "--duration", "0.1").splitlines()
        self.assertTrue(timed)
        for line in timed:
            self.assertRegex(line, r"^frame=\d+ cfg\+0x10:u64=\d+$")
        self.assertEqual(self.run_cli("watch", "cfg+0x10", "--count", "1")[0], 2)  # no type

    def test_unwatch(self):
        code, _, err = self.run_cli("unwatch", "missing")
        self.assertEqual(code, 1)
        self.assertIn("no watch", err)

    def test_log_and_tail(self):
        lines = self.ok("log", "--level", "debug", "--targets", "dev,perf", "--count", "2").splitlines()
        self.assertEqual(len(lines), 2)
        # a perf tick may land between subscribing and the subscribe record
        self.assertRegex("\n".join(lines), r"(?m)^\S+ D \[dev\] request method=log.subscribe$")
        ev = json.loads(self.ok("--json", "log", "--level", "trace", "--targets", "perf", "--count", "1"))
        self.assertEqual((ev["event"], ev["target"]), ("log", "perf"))
        self.assertEqual(self.run_cli("log", "--targets", "bogus", "--count", "1")[0], 1)
        self.assertIn("[boot] mod loaded", self.ok("tail", "--lines", "100"))
        self.assertEqual(len(json.loads(self.ok("--json", "tail", "--lines", "3"))), 3)

    def test_capture(self):
        out = self.tmp / "cap"
        r = self.ok_json(
            "capture",
            "colour",
            "depth",
            "mv",
            "mv_raw",
            "dlss_out",
            "hudless",
            "game_mv",
            "game_depth",
            "--frames",
            "2",
            "--point",
            "slot",
            "--out",
            str(out),
            "--convert",
        )
        self.assertEqual(r["parts"], 16)
        self.assertEqual(len(r["files"]), 48)
        names = sorted(p.name for p in out.iterdir())
        self.assertEqual(sum(n.endswith(".png") for n in names), 2)  # hudless x2 frames
        self.assertEqual(sum(n.endswith(".pfm") for n in names), 14)
        pfm = next(out.glob("mv_raw_*.pfm"))
        w, h, c, vals = read_pfm(pfm)
        self.assertEqual((w, h, c), (64, 36, 3))
        i = (1 * w + 1) * 3
        self.assertEqual(vals[i : i + 3], [1 / 64, -1 / 64, 0.0])  # mock: x = x/64, y = -y/64
        w, h, _ = read_png_rgb(next(out.glob("hudless_*.png")))
        self.assertEqual((w, h), (128, 72))
        side = json.loads(next(out.glob("colour_*.json")).read_text())
        self.assertEqual((side["format"], side["point"]), ("R11G11B10_FLOAT", "slot"))
        self.assertIn("camera", side["sidecar"])
        plain = self.ok_json("capture", "depth", "--out", str(self.tmp / "plain"))
        self.assertEqual([Path(f).suffix for f in plain["files"]], [".raw", ".json"])
        # Captures record at the upscale slot, which has no back buffer.
        self.assertEqual(self.run_cli("capture", "backbuffer")[0], 1)
        self.assertEqual(self.run_cli("capture", "depth", "--point", "frame")[0], 1)

    def test_probes_and_trace(self):
        self.assertEqual(self.ok_json("probe", "jitter", "--frames", "8")["verdict"], "display")
        for kind in ("reset", "hdr", "mv", "depth"):
            self.assertEqual(len(self.ok_json("probe", kind, "--frames", "2")["frames"]), 2)
        self.assertIn("latch", self.ok_json("probe", "wait", "--seconds", "1"))
        self.assertIn("device_created_ms", self.ok_json("probe", "modules"))
        self.assertEqual(len(self.ok_json("trace", "--frames", "3")["frames"]), 3)
        self.assertEqual(self.run_cli("probe", "jitter", "--frames", "0")[0], 1)

    def test_call(self):
        self.assertEqual(
            self.ok_json("call", "mem.read", '{"symbol":"cfg","offset":512,"as":"f32"}')["result"]["values"],
            [0.5],
        )
        parts = [
            json.loads(x)
            for x in self.ok("--json", "call", "capture", '{"resources":["depth","mv"]}').splitlines()
        ]
        self.assertEqual([p["binary"] for p in parts], [256 * 36] * 2)
        self.assertEqual(self.run_cli("call", "nope")[0], 1)

    def test_repl(self):
        script = "\n".join(
            [
                "# comment",
                "hello",
                '{"method": "log.subscribe", "params": {"level": "debug", "targets": ["dev"]}}',
                'mem.read {"symbol": "cfg", "len": 4}',
                "nope",
                "{bad json",
                "quit",
                "state",
            ]
        )
        lines = [json.loads(x) for x in self.ok("repl", stdin=script).splitlines()]
        responses = [x for x in lines if "event" not in x]
        events = [x for x in lines if "event" in x]
        self.assertEqual(responses[0]["result"]["gate"], "open")
        self.assertTrue(responses[1]["ok"])
        self.assertEqual(responses[2]["binary"], 4)
        self.assertEqual(responses[3]["error"]["code"], "unknown_method")
        self.assertEqual(responses[4]["error"]["code"], "bad_input")
        self.assertEqual(len(responses), 5)  # nothing after quit
        self.assertTrue(any(e.get("fields", {}).get("method") == "mem.read" for e in events))

    def test_connection_problems(self):
        code, _, err = self.run_cli("--token", "wrong", "state")
        self.assertEqual(code, 2)
        self.assertIn("authentication failed", err)
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            free = s.getsockname()[1]
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = devctl.main(["--host", "127.0.0.1", "--port", str(free), "--token", "x", "state"])
        self.assertEqual(code, 2)
        self.assertIn("cannot connect", err.getvalue())
        code, _, err = self.run_cli("--token-file", str(self.tmp / "missing"), "state")
        self.assertEqual(code, 2)


if __name__ == "__main__":
    unittest.main()
