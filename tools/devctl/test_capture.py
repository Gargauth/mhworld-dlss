"""Decoding of every capture format, PNG/PFM writers, and saving parts."""

import json
import math
import struct
import tempfile
import unittest
import zlib
from pathlib import Path

import capture
from capture import (
    decode,
    lookup_format,
    read_pfm,
    read_png_rgb,
    save_part,
    write_pfm,
    write_png,
)


def r11g11b10(r: int, g: int, b: int) -> bytes:
    return struct.pack("<I", r | (g << 11) | (b << 22))


ONE11, HALF11, ONE10 = 15 << 6, 14 << 6, 15 << 5


class FormatTest(unittest.TestCase):
    def test_lookup(self):
        self.assertEqual(lookup_format("DXGI_FORMAT_R11G11B10_FLOAT").dxgi, 26)
        self.assertEqual(lookup_format("r32_uint").name, "R32_UINT")
        self.assertEqual(lookup_format(87).name, "B8G8R8A8_UNORM")
        self.assertEqual(lookup_format("24").name, "R10G10B10A2_UNORM")
        self.assertEqual(lookup_format("R32_TYPELESS").name, "R32_FLOAT")
        self.assertIsNone(lookup_format("BC7_UNORM"))
        self.assertIsNone(lookup_format(None))
        self.assertIsNone(lookup_format(True))


class DecodeTest(unittest.TestCase):
    use_numpy = False

    def dec(self, fmt, data, w, h=1, pitch=None):
        return decode(data, lookup_format(fmt), w, h, pitch, use_numpy=self.use_numpy)

    def floats(self, img):
        return [float(v) for v in img.pixels]

    def test_r11g11b10(self):
        data = (
            r11g11b10(ONE11, HALF11, ONE10)
            + r11g11b10(0, 1, 31 << 5)
            + r11g11b10(31 << 6, (15 << 6) | 32, (16 << 5) | 16)
        )
        img = self.dec("R11G11B10_FLOAT", data, 3)
        v = self.floats(img)
        self.assertEqual(img.channels, 3)
        self.assertEqual(v[0:3], [1.0, 0.5, 1.0])
        self.assertEqual(v[3], 0.0)
        self.assertAlmostEqual(v[4], 2.0**-14 / 64)  # smallest 11-bit denormal
        self.assertTrue(math.isinf(v[5]))
        self.assertTrue(math.isinf(v[6]))
        self.assertEqual(v[7], 1.5)
        self.assertEqual(v[8], 3.0)

    def test_r11g11b10_nan(self):
        self.assertTrue(
            math.isnan(self.floats(self.dec("R11G11B10_FLOAT", r11g11b10((31 << 6) | 1, 0, 0), 1))[0])
        )

    def test_half_and_float_formats(self):
        img = self.dec("R16G16_FLOAT", struct.pack("<4e", 0.5, -1.0, 2.0, 65504.0), 2)
        self.assertEqual((img.channels, self.floats(img)), (2, [0.5, -1.0, 2.0, 65504.0]))
        img = self.dec("R32G32_FLOAT", struct.pack("<2f", 0.25, -3.0), 1)
        self.assertEqual(self.floats(img), [0.25, -3.0])
        img = self.dec("R32_FLOAT", struct.pack("<2f", 0.125, 1.0), 2)
        self.assertEqual((img.channels, self.floats(img)), (1, [0.125, 1.0]))
        img = self.dec("R16G16B16A16_FLOAT", struct.pack("<4e", 1, 2, 3, 4), 1)
        self.assertEqual((img.channels, self.floats(img)), (4, [1, 2, 3, 4]))

    def test_packed_half_motion_vectors(self):
        word = (struct.unpack("<H", struct.pack("<e", 0.75))[0] << 16) | struct.unpack(
            "<H", struct.pack("<e", -0.25)
        )[0]
        img = self.dec("R32_UINT", struct.pack("<I", word), 1)
        self.assertEqual((img.channels, self.floats(img)), (2, [0.75, -0.25]))  # x = high, y = low

    def test_unorm8_formats(self):
        img = self.dec("B8G8R8A8_UNORM", bytes([1, 2, 3, 4, 5, 6, 7, 8]), 2)
        self.assertEqual((img.channels, img.pixels), (3, bytes([3, 2, 1, 7, 6, 5])))
        img = self.dec("R8G8B8A8_UNORM_SRGB", bytes([1, 2, 3, 4]), 1)
        self.assertEqual(img.pixels, bytes([1, 2, 3]))
        img = self.dec("R10G10B10A2_UNORM", struct.pack("<I", 1023 | (512 << 10) | (0 << 20) | (3 << 30)), 1)
        self.assertEqual(img.pixels, bytes([255, 128, 0]))

    def test_row_pitch(self):
        row0 = struct.pack("<2f", 1, 2) + b"\xee" * 8
        row1 = struct.pack("<2f", 3, 4) + b"\xee" * 8
        img = self.dec("R32_FLOAT", row0 + row1, 2, 2, 16)
        self.assertEqual(self.floats(img), [1, 2, 3, 4])
        img = self.dec("R32_FLOAT", row0 + row1[:8], 2, 2, 16)  # last row unpadded
        self.assertEqual(self.floats(img), [1, 2, 3, 4])
        with self.assertRaises(ValueError):
            self.dec("R32_FLOAT", row0, 2, 2, 16)
        with self.assertRaises(ValueError):
            self.dec("R32_FLOAT", row0 + row1, 2, 2, 4)


@unittest.skipIf(capture.np is None, "numpy not installed")
class DecodeNumpyTest(DecodeTest):
    use_numpy = True


class WriterTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)

    def test_png(self):
        rgb = bytes(range(18))
        path = self.dir / "a.png"
        write_png(path, 3, 2, rgb)
        blob = path.read_bytes()
        self.assertTrue(blob.startswith(b"\x89PNG\r\n\x1a\n"))
        length = struct.unpack(">I", blob[8:12])[0]
        self.assertEqual(
            struct.unpack(">I", blob[16 + length : 20 + length])[0], zlib.crc32(blob[12 : 16 + length])
        )
        self.assertEqual(read_png_rgb(path), (3, 2, rgb))
        with self.assertRaises(ValueError):
            write_png(path, 3, 3, rgb)

    def test_pfm_single_channel_bottom_up(self):
        path = self.dir / "d.pfm"
        write_pfm(path, 2, 2, 1, [1.0, 2.0, 3.0, 4.0])
        blob = path.read_bytes()
        self.assertTrue(blob.startswith(b"Pf\n2 2\n-1.0\n"))
        self.assertEqual(struct.unpack("<4f", blob[-16:]), (3.0, 4.0, 1.0, 2.0))
        self.assertEqual(read_pfm(path), (2, 2, 1, [1.0, 2.0, 3.0, 4.0]))

    def test_pfm_two_and_four_channels(self):
        path = self.dir / "mv.pfm"
        write_pfm(path, 1, 1, 2, [0.5, -0.5])
        self.assertEqual(read_pfm(path), (1, 1, 3, [0.5, -0.5, 0.0]))
        write_pfm(path, 1, 1, 4, [1.0, 2.0, 3.0, 4.0])
        self.assertEqual(read_pfm(path), (1, 1, 3, [1.0, 2.0, 3.0]))
        write_pfm(path, 2, 1, 3, [1, 2, 3, 4, 5, 6])
        self.assertEqual(read_pfm(path)[3], [1, 2, 3, 4, 5, 6])

    def test_save_part(self):
        meta = {
            "resource": "mv_raw",
            "frame": 42,
            "format": "R32_UINT",
            "width": 1,
            "height": 1,
            "row_pitch": 256,
            "sidecar": {"jitter": [0.1, 0.2]},
        }
        data = struct.pack("<2e", -0.25, 0.75).ljust(256, b"\0")
        paths = save_part(meta, data, self.dir / "cap", convert=True)
        self.assertEqual(
            [p.name for p in paths], ["mv_raw_000042.raw", "mv_raw_000042.pfm", "mv_raw_000042.json"]
        )
        self.assertEqual(paths[0].read_bytes(), data)
        side = json.loads(paths[2].read_text())
        self.assertEqual(
            (side["converted"], side["bytes"], side["sidecar"]),
            ("mv_raw_000042.pfm", 256, {"jitter": [0.1, 0.2]}),
        )
        self.assertEqual(read_pfm(paths[1])[3], [0.75, -0.25, 0.0])

    def test_save_part_unconvertible_and_png(self):
        paths = save_part(
            {"resource": "x/y", "format": "BC7_UNORM", "width": 1, "height": 1},
            b"\0" * 16,
            self.dir,
            convert=True,
        )
        self.assertEqual(len(paths), 2)
        self.assertIsNone(json.loads(paths[1].read_text())["converted"])
        self.assertEqual(paths[0].name, "x_y_000000.raw")
        paths = save_part(
            {"resource": "backbuffer", "frame": 1, "format": 87, "width": 1, "height": 1},
            bytes([10, 20, 30, 0]),
            self.dir,
            convert=True,
        )
        self.assertEqual(read_png_rgb(paths[1]), (1, 1, bytes([30, 20, 10])))
        paths = save_part(
            {"resource": "depth", "format": "R32_FLOAT", "width": 1, "height": 1},
            b"\0" * 4,
            self.dir / "plain",
        )
        self.assertEqual([p.suffix for p in paths], [".raw", ".json"])
        self.assertNotIn("converted", json.loads(paths[1].read_text()))


if __name__ == "__main__":
    unittest.main()
