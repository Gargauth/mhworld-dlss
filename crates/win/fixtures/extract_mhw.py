#!/usr/bin/env python3
"""Regenerate the PE fixtures in this directory from MonsterHunterWorld.exe.

Usage: extract_mhw.py <path to MonsterHunterWorld.exe (Steam build 15539686)>

Writes `mhw-imports.bin`: the parts of the executable the import parser
reads, in *mapped* layout (offset = RVA), as a sequence of chunks
`[u32 rva][u32 len][len bytes]`, little-endian. Covered: the headers, the
regular import table (KERNEL32 only: descriptor, name table, hint/name
strings) and the delay-import table (all descriptors and DLL names, d3d12's
name table, IAT and hint/name strings). Nothing else of the game is copied.
"""
import struct
import sys
from pathlib import Path


def main(exe: Path) -> None:
    data = exe.read_bytes()
    nt = struct.unpack_from("<I", data, 0x3C)[0]
    count = struct.unpack_from("<H", data, nt + 6)[0]
    optional_size = struct.unpack_from("<H", data, nt + 20)[0]
    table = nt + 24 + optional_size
    sections = [struct.unpack_from("<8sIIII", data, table + 40 * i) for i in range(count)]
    directory = nt + 24 + 112

    def offset(rva: int) -> int:
        for _name, vsize, va, rsize, raw in sections:
            if va <= rva < va + max(vsize, rsize):
                return rva - va + raw
        raise ValueError(hex(rva))

    def u32(rva: int) -> int:
        return struct.unpack_from("<I", data, offset(rva))[0]

    def u64(rva: int) -> int:
        return struct.unpack_from("<Q", data, offset(rva))[0]

    def c_str_len(rva: int) -> int:
        start = offset(rva)
        return data.index(b"\0", start) - start + 1

    chunks = [(0, struct.unpack_from("<I", data, nt + 24 + 60)[0])]  # SizeOfHeaders

    def thunks(names: int, iat: int) -> None:
        count = 0
        while u64(names + 8 * count):
            thunk = u64(names + 8 * count)
            if not thunk >> 63:
                chunks.append((thunk + 2, c_str_len(thunk + 2)))
            count += 1
        chunks.append((names, 8 * (count + 1)))
        chunks.append((iat, 8 * (count + 1)))

    imports = struct.unpack_from("<I", data, directory + 8)[0]
    index = 0
    while u32(imports + 20 * index + 12):
        name = u32(imports + 20 * index + 12)
        chunks.append((name, c_str_len(name)))
        thunks(u32(imports + 20 * index), u32(imports + 20 * index + 16))
        index += 1
    chunks.append((imports, 20 * (index + 1)))

    delay = struct.unpack_from("<I", data, directory + 8 * 13)[0]
    index = 0
    while u32(delay + 32 * index + 4):
        name = u32(delay + 32 * index + 4)
        chunks.append((name, c_str_len(name)))
        dll = data[offset(name) : offset(name) + c_str_len(name) - 1]
        if dll.lower() == b"d3d12.dll":
            thunks(u32(delay + 32 * index + 16), u32(delay + 32 * index + 12))
        index += 1
    chunks.append((delay, 32 * (index + 1)))

    out = bytearray()
    for rva, length in sorted(set(chunks)):
        out += struct.pack("<II", rva, length)
        out += data[offset(rva) : offset(rva) + length] if rva else data[:length]
    target = Path(__file__).with_name("mhw-imports.bin")
    target.write_bytes(bytes(out))
    print(f"{target}: {len(out)} bytes in {len(set(chunks))} chunks")


if __name__ == "__main__":
    main(Path(sys.argv[1]))
