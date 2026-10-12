"""Framing, typed values, expressions and DevClient against the mock server."""

import json
import socket
import struct
import threading
import time
import unittest

from devclient import (
    AuthError,
    ConnectionClosed,
    DevClient,
    DevError,
    Expr,
    FrameReader,
    ProtocolError,
    decode_values,
    encode_frame,
    encode_values,
    split_address,
)
from mock_game import STRUCTS
from mock_server import MockServer


def reader_over(data: bytes, chunk: int = 1 << 16) -> FrameReader:
    pos = 0

    def recv(n: int) -> bytes:
        nonlocal pos
        out = data[pos : pos + min(n, chunk)]
        pos += len(out)
        return out

    return FrameReader(recv)


class FramingTest(unittest.TestCase):
    def test_round_trip_any_chunking(self):
        payload = b'\n{"id":9}\n\x00\xff'  # binary that looks like framing
        stream = (
            encode_frame({"event": "log", "line": "héllo"})
            + encode_frame({"id": 1, "ok": True, "result": {}}, payload)
            + encode_frame({"id": 2, "ok": True, "result": 3}, b"")
        )
        for chunk in (1, 2, 3, 7, 1 << 16):
            r = reader_over(stream, chunk)
            f1, f2, f3 = r.read_frame(), r.read_frame(), r.read_frame()
            self.assertEqual(f1.msg["line"], "héllo")
            self.assertIsNone(f1.binary)
            self.assertEqual(f2.binary, payload)
            self.assertEqual(f2.msg["binary"], len(payload))
            self.assertEqual(f3.binary, b"")
            self.assertIsNone(r.read_frame())

    def test_encode_sets_binary_length_and_single_line(self):
        data = encode_frame({"id": 1, "s": "a\nb"}, b"xyz")
        line, rest = data.split(b"\n", 1)
        self.assertEqual(json.loads(line), {"id": 1, "s": "a\nb", "binary": 3})
        self.assertEqual(rest, b"xyz")

    def test_truncated_streams(self):
        with self.assertRaisesRegex(ProtocolError, "inside a JSON line"):
            reader_over(b'{"id":1').read_frame()
        with self.assertRaisesRegex(ProtocolError, "binary payload"):
            reader_over(b'{"id":1,"binary":4}\nab').read_frame()

    def test_malformed_frames(self):
        for line in (
            b"not json\n",
            b"[1,2]\n",
            b'{"binary":-1}\n',
            b'{"binary":true}\n',
            b'{"binary":"4"}\n',
            b"\xff\xfe\n",
        ):
            with self.subTest(line=line), self.assertRaises(ProtocolError):
                reader_over(line).read_frame()


class ValuesTest(unittest.TestCase):
    def test_scalar_round_trip(self):
        cases = {
            "u8": [0, 255],
            "u16": [65535],
            "u32": [1, 2**32 - 1],
            "u64": [2**64 - 1],
            "f32": [0.5, -2.0],
            "f64": [1e300],
        }
        for as_, vals in cases.items():
            with self.subTest(as_=as_):
                self.assertEqual(decode_values(encode_values(vals, as_), as_), vals)
        self.assertEqual(encode_values(7, "u16"), b"\x07\x00")

    def test_mat4(self):
        m = [float(i) for i in range(16)]
        data = encode_values(m, "mat4")
        self.assertEqual(len(data), 64)
        self.assertEqual(decode_values(data + data, "mat4"), [m, m])
        with self.assertRaises(ValueError):
            encode_values([1.0] * 15, "mat4")

    def test_bytes_and_errors(self):
        self.assertEqual(encode_values("0aff", "bytes"), b"\x0a\xff")
        self.assertEqual(decode_values(b"\x01\x02", "bytes"), [1, 2])
        with self.assertRaises(ValueError):
            decode_values(b"\x00" * 3, "u32")
        with self.assertRaises(ValueError):
            encode_values(256, "u8")
        with self.assertRaises(ValueError):
            decode_values(b"", "i128")


class ExprTest(unittest.TestCase):
    def test_parse(self):
        e = Expr.parse("cfg+0x200:f32")
        self.assertEqual((e.symbol, e.addr, e.offset, e.as_), ("cfg", None, 0x200, "f32"))
        self.assertEqual(e.params(), {"symbol": "cfg", "offset": 0x200, "as": "f32"})
        e = Expr.parse("0x1451c4480 + 8 - 0x4:u32")
        self.assertEqual((e.addr, e.offset), (0x1451C4480, 4))
        self.assertEqual(e.params()["addr"], "0x1451c4480")
        self.assertEqual(Expr.parse("Camera", "mat4").as_, "mat4")
        self.assertEqual(str(Expr.parse("taa+0x1a8:u64")), "taa+0x1a8:u64")

    def test_rejects(self):
        for bad in ("", "cfg+", "cfg:i128", "+4", "cfg*2"):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                Expr.parse(bad)

    def test_split_address(self):
        self.assertEqual(split_address("10.0.0.2"), ("10.0.0.2", 47800))
        self.assertEqual(split_address("10.0.0.2:5"), ("10.0.0.2", 5))
        self.assertEqual(split_address("[::1]:6"), ("::1", 6))
        self.assertEqual(split_address("::1", 7), ("::1", 7))


class MockCase(unittest.TestCase):
    def setUp(self):
        self.server = MockServer(token="secret").start()
        self.addCleanup(self.server.stop)
        host, self.port = self.server.address
        self.client = DevClient.connect(host, self.port, token="secret", timeout=5)
        self.addCleanup(self.client.close)


class AuthTest(unittest.TestCase):
    def test_success_reports_server_info(self):
        with MockServer(token="secret") as s, DevClient.connect(*s.address, token="secret") as c:
            self.assertEqual(c.server_info["build"], "dev")
            self.assertIn("features", c.server_info)

    def test_token_file(self):
        import os
        import tempfile

        with tempfile.TemporaryDirectory() as d, MockServer(token="abc") as s:
            path = os.path.join(d, "dev.token")
            with open(path, "w") as f:
                f.write("abc\n")
            with DevClient.connect(f"{s.address[0]}:{s.address[1]}", token_file=path) as c:
                self.assertTrue(c.hello()["gate"])

    def test_wrong_token(self):
        with MockServer(token="secret") as s:
            with self.assertRaises(AuthError) as cm:
                DevClient.connect(*s.address, token="nope")
            self.assertEqual(cm.exception.code, "unauthorized")

    def test_server_closes_without_reply(self):
        with MockServer(token="secret", reply_on_auth_failure=False) as s:
            with self.assertRaises(AuthError):
                DevClient.connect(*s.address, token="nope")

    def test_other_request_first_closes(self):
        with MockServer(token="secret") as s, socket.create_connection(s.address, timeout=5) as sock:
            sock.sendall(encode_frame({"id": 1, "method": "state", "params": {}}))
            r = FrameReader(sock.recv)
            first = r.read_frame()
            self.assertEqual(first.msg["error"]["code"], "unauthorized")
            self.assertIsNone(r.read_frame())  # connection closed


class InterleaveTest(unittest.TestCase):
    """Scripted server over a socketpair: events around and between responses."""

    def test_events_between_responses_and_binary(self):
        a, b = socket.socketpair()
        self.addCleanup(b.close)
        client = DevClient(a, timeout=5)
        self.addCleanup(client.close)

        def server():
            r = FrameReader(b.recv)
            req = r.read_frame().msg
            b.sendall(
                encode_frame({"event": "log", "line": "one"})
                + encode_frame({"event": "watch", "name": "w", "frame": 1, "values": [1]})
            )
            payload = b'{"event":"fake"}\n' * 3
            b.sendall(
                encode_frame({"id": req["id"], "ok": True, "result": {"n": 1}}, payload)
                + encode_frame({"event": "log", "line": "two"})
            )

        t = threading.Thread(target=server)
        t.start()
        resp = client.call_raw("mem.read", {"symbol": "cfg", "len": 51})
        t.join()
        self.assertEqual(resp.result, {"n": 1})
        self.assertEqual(resp.binary, b'{"event":"fake"}\n' * 3)
        lines = [client.next_event(1).get("line", "watch") for _ in range(3)]
        self.assertEqual(lines, ["one", "watch", "two"])

    def test_timeout_then_late_response_is_dropped(self):
        a, b = socket.socketpair()
        self.addCleanup(b.close)
        client = DevClient(a, timeout=0.1)
        self.addCleanup(client.close)
        r = FrameReader(b.recv)
        with self.assertRaises(TimeoutError):
            client.call("state")
        late = r.read_frame().msg
        b.sendall(encode_frame({"id": late["id"], "ok": True, "result": "late"}))
        t = threading.Thread(
            target=lambda: b.sendall(
                encode_frame({"id": r.read_frame().msg["id"], "ok": True, "result": "fresh"})
            )
        )
        t.start()
        self.assertEqual(client.call("state"), "fresh")
        t.join()

    def test_close_fails_pending_and_events(self):
        a, b = socket.socketpair()
        client = DevClient(a, timeout=5)
        self.addCleanup(client.close)
        threading.Timer(0.05, b.close).start()
        with self.assertRaises(ConnectionClosed):
            client.call("state")
        with self.assertRaises(ConnectionClosed):
            client.next_event(1)
        with self.assertRaises(ConnectionClosed):
            client.call("state")

    def test_error_response(self):
        a, b = socket.socketpair()
        self.addCleanup(b.close)
        client = DevClient(a, timeout=5)
        self.addCleanup(client.close)

        def server():
            req = FrameReader(b.recv).read_frame().msg
            b.sendall(
                encode_frame(
                    {
                        "id": req["id"],
                        "ok": False,
                        "error": {"code": "not_ready", "message": "fg not started"},
                    }
                )
            )

        threading.Thread(target=server).start()
        with self.assertRaises(DevError) as cm:
            client.call("probe.reset")
        self.assertEqual((cm.exception.code, cm.exception.message), ("not_ready", "fg not started"))


class MethodsTest(MockCase):
    def test_hello_state_symbols_settings(self):
        self.assertIn("version", self.client.hello())
        st = self.client.state()
        for key in ("frame", "settings", "render_size", "display_size", "scale", "dlss", "fg", "counters"):
            self.assertIn(key, st)
        syms = self.client.symbols()
        self.assertEqual(int(syms["symbols"]["Camera"], 16), STRUCTS["Camera"][0])
        self.assertIn("Experimental", self.client.settings_get())

    def test_settings_set_and_experiment(self):
        r = self.client.settings_set("Upscaling", "Mode", "performance")
        self.assertEqual(r["changes"][0]["value"], "Performance")
        self.assertFalse(r["restart_required"])
        r = self.client.settings_set_many(
            [
                ("FrameGeneration", "Enabled", True),
                {"section": "Upscaling", "key": "Sharpness", "value": "0.25"},
            ]
        )
        self.assertTrue(r["restart_required"])
        self.client.experiment_set("JitterScale", "Render")
        s = self.client.settings_get()
        self.assertEqual(s["Experimental"]["JitterScale"], "Render")
        self.assertEqual(s["Upscaling"]["Sharpness"], 0.25)

    def test_settings_invalid_is_atomic(self):
        with self.assertRaises(DevError) as cm:
            self.client.settings_set_many([("Upscaling", "Mode", "DLAA"), ("Upscaling", "Preset", "Z")])
        self.assertEqual(cm.exception.code, "bad_request")
        self.assertEqual(self.client.settings_get()["Upscaling"]["Mode"], "Quality")

    def test_settings_out_of_range_numbers_are_clamped_with_a_note(self):
        r = self.client.settings_set_many([("Upscaling", "Mode", "Custom"), ("Upscaling", "RenderScale", 2)])
        self.assertEqual(r["changes"][1]["value"], 1.0)
        self.assertIn("clamped", r["changes"][1]["note"])
        r = self.client.settings_set("Upscaling", "RenderScale", "0.75")
        self.assertEqual(r["changes"][0].get("note"), None)
        r = self.client.settings_set("Upscaling", "Mode", "DLAA")
        r = self.client.settings_set("Upscaling", "RenderScale", "0.8")
        self.assertEqual(r["changes"][0]["note"], "only used with Mode = Custom")

    def test_mem_typed_and_binary(self):
        self.assertEqual(self.client.mem_read("cfg+0x200:f32"), [0.5])
        self.client.mem_write("cfg+0x204", [1, 2, 3], as_="u32")
        self.assertEqual(self.client.mem_read("cfg+0x204", as_="u32", count=3), [1, 2, 3])
        raw = self.client.mem_read("cfg+0x204", length=12)
        self.assertEqual(raw, struct.pack("<3I", 1, 2, 3))
        self.client.mem_write("cam+0x80", b"\x01\x02", as_="bytes")
        self.assertEqual(self.client.mem_read("cam+0x80", length=2), b"\x01\x02")
        mat = self.client.mem_read("Camera:mat4")[0]
        self.assertEqual(mat[0], 1.0)
        addr = STRUCTS["RenderConfig"][0] + 0x200
        self.assertEqual(self.client.mem_read(addr, as_="f32"), [0.5])

    def test_mem_write_policy(self):
        with self.assertRaises(DevError) as cm:
            self.client.mem_write("GameCode", 0x90, as_="u8")
        self.assertEqual(cm.exception.code, "refused")
        self.client.mem_write("GameCode", 0x90, as_="u8", force=True)
        self.assertEqual(self.client.mem_read("GameCode:u8"), [0x90])
        with self.assertRaises(DevError) as cm:
            self.client.mem_read("0x10:u32")
        self.assertEqual(cm.exception.code, "refused")
        with self.assertRaises(ValueError):
            self.client.mem_write("cfg", 1)  # no type

    def test_watch_streams_until_unwatch(self):
        self.client.watch("f", ["cfg+0x10:u64", "cfg+0x200:f32"], every_frames=2, point="camera")
        evs = []
        for ev in self.client.events("watch", timeout=2):
            evs.append(ev)
            if len(evs) == 3:
                break
        self.assertEqual([e["name"] for e in evs], ["f"] * 3)
        self.assertEqual(evs[0]["values"][1], 0.5)
        self.assertLess(evs[0]["frame"], evs[2]["frame"])
        self.client.unwatch("f")
        time.sleep(0.05)
        while self.client.next_event(0.01):
            pass
        self.assertIsNone(self.client.next_event(0.1))
        with self.assertRaises(ValueError):
            self.client.watch("g", ["cfg+0x10"])

    def test_log_events_interleave_with_responses(self):
        self.client.log_subscribe("debug", ["dev"])
        self.assertEqual(self.client.next_event(2)["fields"]["method"], "log.subscribe")
        self.assertEqual(self.client.hello()["gate"], "open")
        ev = self.client.next_event(2)
        self.assertEqual((ev["event"], ev["target"], ev["fields"]["method"]), ("log", "dev", "hello"))
        self.assertTrue(any("authenticated" in line for line in self.client.log_tail(50)))
        self.assertEqual(len(self.client.log_tail(2)), 2)

    def test_log_order_on_the_wire(self):
        with socket.create_connection(self.server.address, timeout=5) as sock:
            r = FrameReader(sock.recv)
            sock.sendall(encode_frame({"id": 1, "method": "auth", "params": {"token": "secret"}}))
            self.assertTrue(r.read_frame().msg["ok"])
            sock.sendall(
                encode_frame(
                    {"id": 2, "method": "log.subscribe", "params": {"level": "debug", "targets": ["dev"]}}
                )
            )
            sock.sendall(encode_frame({"id": 3, "method": "state", "params": {}}))
            kinds = [
                ("event" if "event" in f.msg else f.msg["id"]) for f in (r.read_frame() for _ in range(4))
            ]
            self.assertEqual(kinds, ["event", 2, "event", 3])

    def test_capture_parts(self):
        parts = self.client.capture(["colour", "mv_raw"], frames=2, point="slot")
        self.assertEqual([p.resource for p in parts], ["colour", "mv_raw"] * 2)
        self.assertEqual([p.meta["index"] for p in parts], [0, 1, 2, 3])
        self.assertEqual(parts[0].frame + 1, parts[2].frame)
        for p in parts:
            self.assertEqual(len(p.data), p.meta["row_pitch"] * p.meta["height"])
            self.assertIn("jitter", p.meta["sidecar"])
        with self.assertRaises(DevError):
            self.client.capture(["nope"])

    def test_probes_and_trace(self):
        self.assertEqual(self.client.probe("jitter", frames=4)["verdict"], "display")
        for kind in ("reset", "hdr", "mv", "depth"):
            self.assertEqual(len(self.client.probe(kind, frames=3)["frames"]), 3)
        self.assertIn("timeouts", self.client.probe("wait", seconds=0.5))
        self.assertTrue(self.client.probe("modules")["modules"])
        self.assertEqual(len(self.client.trace(2)["frames"]), 2)

    def test_errors(self):
        with self.assertRaises(DevError) as cm:
            self.client.call("nope")
        self.assertEqual(cm.exception.code, "unknown_method")
        with self.assertRaises(DevError) as cm:
            self.client.probe("jitter", frames=0)
        self.assertEqual(cm.exception.code, "bad_request")
        with self.assertRaises(DevError) as cm:
            self.client.call("state", {"point": "nowhere"})
        self.assertEqual(cm.exception.code, "bad_request")

    def test_concurrent_calls(self):
        results = []

        def work():
            for _ in range(20):
                results.append(self.client.mem_read("cfg+0x200:f32")[0])

        threads = [threading.Thread(target=work) for _ in range(4)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        self.assertEqual(results, [0.5] * 80)


if __name__ == "__main__":
    unittest.main()
