#!/usr/bin/env python3
"""Generate crates/engine/src/sites.rs from docs/re/signatures.json.

The JSON is the authority for every game address the mod uses
(docs/re/ReSignatures.md). This script turns it into Rust tables:

* SITES   -- the gate sites as `mhw_core::sig::Site` (robust pattern,
             RVA in the reference build, the exact bytes compared, `wild`):
             the 16 of the 0.2 gate and TONEMAP_RELEASE
             (docs/re/ReVolumetrics.md);
* GLOBALS -- the globals read out of RIP-relative instructions inside
             those sites;
* ANCHORS -- every struct-offset anchor: a pattern, where in it the
             displacement of the field sits, and the field it must give.

The reference executable is required (`--exe`, or `MHW_EXE`; Steam build
15539686, the build the RE was done on): the JSON gives patterns and
displacement positions, the executable gives where each pattern matches.
Every row is self-checked against it before anything is written:

* each pattern is unique in `.text` and matches at the RVA used;
* each gate site's verify bytes are there (except `wild`);
* each global resolves to its RVA;
* each anchor's displacement reads the field offset.

The RVAs are only where the mod looks first; the gate itself does not
depend on the build (crates/engine/src/gate.rs). Anchor patterns are
loosened -- the displacement bytes turned into wildcards, so that a field
that moved in a later build is still found and its new offset read --
whenever the loosened pattern is still unique in `.text`. Otherwise the
strict pattern is kept and the anchor only confirms the field. Each field
records whether its object also has unanchored offsets
(crates/engine/src/layout.rs, `OBJECTS` below): such a field must read the
offset it has here, or the gate stands down (crates/engine/src/anchors.rs).

Usage: tools/gen_sites.py [--exe PATH] [--check]
  --check   do not write; fail if the checked-in file differs.
"""

import argparse
import json
import os
import re
import struct
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
JSON = ROOT / "docs/re/signatures.json"
OUT = ROOT / "crates/engine/src/sites.rs"
DEFAULT_EXE = ROOT.parent.parent / "MonsterHunterWorld.exe"


class Pe:
    """Just enough of a PE reader: sections and RVA reads."""

    def __init__(self, path):
        self.data = Path(path).read_bytes()
        d = self.data
        nt = struct.unpack_from("<I", d, 0x3C)[0]
        if d[nt : nt + 4] != b"PE\0\0":
            sys.exit(f"{path}: not a PE image")
        count = struct.unpack_from("<H", d, nt + 6)[0]
        optional = struct.unpack_from("<H", d, nt + 20)[0]
        table = nt + 24 + optional
        self.sections = []
        for i in range(count):
            e = table + i * 40
            name = d[e : e + 8].rstrip(b"\0").decode("ascii", "replace")
            vsize, rva, rsize, raw = struct.unpack_from("<IIII", d, e + 8)
            self.sections.append((name, rva, vsize, raw, rsize))

    def read(self, rva, n):
        for _, start, vsize, raw, rsize in self.sections:
            if start <= rva < start + max(vsize, rsize):
                off = rva - start
                return self.data[raw + off : raw + off + n]
        return b""

    def text(self):
        for name, rva, vsize, raw, rsize in self.sections:
            if name == ".text":
                return rva, self.data[raw : raw + min(vsize, rsize)]
        sys.exit("no .text section")


def parse_pattern(text):
    out = []
    for token in text.split():
        out.append(None if token in ("?", "??") else int(token, 16))
    return out


def pattern_text(pattern):
    return " ".join("??" if b is None else f"{b:02X}" for b in pattern)


def regex(pattern):
    body = b"".join(b"." if b is None else re.escape(bytes([b])) for b in pattern)
    return re.compile(b"(?=" + body + b")", re.DOTALL)


def hits(text, pattern, limit=3):
    base, code = text
    found = []
    for m in regex(pattern).finditer(code):
        found.append(base + m.start())
        if len(found) >= limit:
            break
    return found


def matches_at(pe, rva, pattern):
    got = pe.read(rva, len(pattern))
    return len(got) == len(pattern) and all(
        w is None or w == h for w, h in zip(pattern, got)
    )


def disp(raw, size):
    return int.from_bytes(raw[:size], "little", signed=True)


def const_name(text):
    return re.sub(r"[^A-Z0-9]+", "_", text.upper()).strip("_")


# object in the JSON -> (constant prefix, doc, the `crate::layout` module
# holding unanchored offsets of the same object, if any).
OBJECTS = {
    "cfg": ("CFG", "sRender (render config, `0x1451c4480`)", "render_config"),
    "ctx": ("CAM", "sCamera (`0x1451c4400`)", "camera"),
    "settings": ("RS", "render settings (`0x1451c4368`)", "render_settings"),
    "SS_ENQUEUE argument": ("TAA", "uTemporalAAFilter2 (SS enqueue capture +8)", "taa"),
    "EVAL_SS argument": ("SS_DESC", "EvalSS descriptor", "descriptor"),
    "EVAL_ISP argument": ("ISP_DESC", "EvalISP descriptor", "descriptor"),
    "OPTIMAL argument": (
        "DLSS_CTX",
        "the game's DLSS object (`device+0x42700`)",
        "dlss_context",
    ),
    "tonemap": ("TONEMAP", "the active tonemap/luminance filter (`0x1451cbc08`)", None),
}


def fail(msg):
    sys.exit(f"gen_sites: {msg}")


def gate_sites(entries, pe, text):
    rows = []
    for e in entries:
        name = e["name"]
        rva = int(e["rva"], 16)
        pattern = parse_pattern(e["pattern"])
        found = hits(text, pattern)
        if found != [rva - e["offset"]]:
            fail(f"site {name}: pattern hits {[hex(h) for h in found]}, want {rva:#x}")
        verify = bytes.fromhex(e["gargauth_bytes"].replace(" ", ""))
        wild = e["gargauth_wild"] or [0, 0]
        got = pe.read(rva, len(verify))
        for i, (w, h) in enumerate(zip(verify, got)):
            if w != h and not (wild[0] <= i < wild[1]):
                fail(f"site {name}: verify byte {i} is {h:#x}, want {w:#x}")
        rows.append(
            dict(
                name=name,
                role=e["role"],
                pattern=pattern_text(pattern),
                to_target=e["offset"],
                rva=rva,
                verify=verify,
                wild=wild,
                disasm=e["disasm"][0],
            )
        )
    return rows


RESOLVE = re.compile(r"match\+(\d+):.*target = insn_end \+ disp32 @ match\+(\d+)")


def globals_(entries, sites, pe):
    rows = []
    by_name = {s["name"]: s for s in sites}
    for e in entries:
        site = by_name.get(e["via"]) or fail(f"global {e['name']}: no site {e['via']}")
        m = RESOLVE.search(e["resolve"]) or fail(f"global {e['name']}: resolve text")
        insn_at, disp_at = int(m.group(1)), int(m.group(2))
        insn_len = disp_at - insn_at + 4
        target = site["rva"] + insn_at + insn_len + disp(pe.read(site["rva"] + disp_at, 4), 4)
        if target != int(e["rva"], 16):
            fail(f"global {e['name']}: resolves to {target:#x}, want {e['rva']}")
        short = e["name"].split()[0]
        short = re.sub(r"^g_", "", short)
        short = re.sub(r"_const$", "", short)
        # The RE calls the object behind g_render_context sCamera.
        short = {"render_context": "camera"}.get(short, short)
        rows.append(
            dict(
                const=const_name(short),
                name=e["name"],
                purpose=e["purpose"],
                site=e["via"],
                insn_at=insn_at,
                disp_at=disp_at,
                insn_len=insn_len,
                rva=target,
            )
        )
    return rows


def anchor_size(pattern, at, field):
    four = list(field.to_bytes(4, "little"))
    if pattern[at : at + 4] == four:
        return 4
    if field < 0x80 and pattern[at] == field:
        return 1
    return None


def anchors(entries, text, pe):
    fields = []
    rows = []
    unanchored = []
    for e in entries:
        if not e.get("pattern"):
            unanchored.append(e["name"])
            continue
        obj, obj_doc, layout = OBJECTS.get(e["object"]) or fail(
            f"{e['name']}: object {e['object']}"
        )
        field = int(e["field_offset"], 16)
        const = f"{obj}_{field:X}"
        if any(f["const"] == const for f in fields):
            fail(f"duplicate field constant {const}")
        index = len(fields)
        fields.append(
            dict(
                const=const,
                name=e["name"],
                object=obj_doc,
                layout=layout,
                offset=field,
                reflection=(e.get("reflection_name") or "").split(" (")[0],
            )
        )
        candidates = [(e["pattern"], e["offset"], e.get("disp_size"), e.get("anchor", "?"))]
        for key in ("gate_anchor", "reflection_anchor", "function_anchor", "accessor_anchor"):
            a = e.get(key)
            if isinstance(a, dict) and a.get("pattern"):
                candidates.append(
                    (a["pattern"], a["disp_at"], a.get("disp_size"), key.replace("_", " "))
                )
        seen = set()
        for text_pattern, at, size, via in candidates:
            if (text_pattern, at) in seen:
                continue
            seen.add((text_pattern, at))
            strict = parse_pattern(text_pattern)
            size = size or anchor_size(strict, at, field)
            if size not in (1, 4) or anchor_size(strict, at, field) != size:
                fail(f"{e['name']}: displacement @{at} in the pattern is not {field:#x}")
            found = hits(text, strict)
            if len(found) != 1:
                fail(f"{e['name']} ({via}): strict pattern hits {[hex(h) for h in found]}")
            rva = found[0]
            loose = list(strict)
            for i in range(at, at + size):
                loose[i] = None
            loose_hits = hits(text, loose)
            pattern = loose if loose_hits == [rva] else strict
            if disp(pe.read(rva + at, size), size) != field:
                fail(f"{e['name']}: displacement at {rva + at:#x} is not {field:#x}")
            if not matches_at(pe, rva, pattern):
                fail(f"{e['name']}: pattern does not match at {rva:#x}")
            rows.append(
                dict(
                    field=const,
                    index=index,
                    pattern=pattern_text(pattern),
                    loose=pattern is loose,
                    disp_at=at,
                    disp_size=size,
                    rva=rva,
                    via=via,
                )
            )
    return fields, rows, unanchored


def rust_bytes(raw):
    return ", ".join(f"0x{b:02x}" for b in raw)


def emit(sites, glob, fields, rows, unanchored):
    o = []
    w = o.append
    w("//! The game's code sites, globals and struct-offset anchors.")
    w("//!")
    w("//! GENERATED by `tools/gen_sites.py` from `docs/re/signatures.json`, checked")
    w("//! against the reference executable (Steam build 15539686, where every")
    w("//! `known_rva` and `known` value comes from). Do not edit; regenerate.")
    w("//!")
    w("//! Unanchored offsets (no instruction found that names them; see")
    w("//! `docs/re/ReSignatures.md` §2.5): " + ", ".join(f"`{u}`" for u in unanchored) + ".")
    w("")
    w("use mhw_core::sig::Site;")
    w("")
    w("use crate::anchors::{FieldInfo, GlobalRef, OffsetAnchor};")
    w("")
    w("/// Indices into [`SITES`].")
    w("pub mod site {")
    for i, s in enumerate(sites):
        w(f"    /// `{s['name']}` ({s['role']}).")
        w(f"    pub const {s['name']}: usize = {i};")
    w("    /// Number of sites.")
    w(f"    pub const COUNT: usize = {len(sites)};")
    w("}")
    w("")
    w("/// Every gate site: all must resolve, or nothing is installed.")
    w("pub static SITES: [Site; site::COUNT] = [")
    for s in sites:
        w("    // " + s["disasm"])
        w("    Site {")
        w(f"        name: \"{s['name']}\",")
        w(f"        pattern: \"{s['pattern']}\",")
        w(f"        to_target: {s['to_target']},")
        w(f"        known_rva: {s['rva']:#x},")
        w(f"        verify: &[{rust_bytes(s['verify'])}],")
        w(f"        wild: {s['wild'][0]}..{s['wild'][1]},")
        w("    },")
    w("];")
    w("")
    w("/// Indices into [`GLOBALS`].")
    w("pub mod global {")
    for i, g in enumerate(glob):
        w(f"    /// `{g['name']}`: {g['purpose']}.")
        w(f"    pub const {g['const']}: usize = {i};")
    w("    /// Number of globals.")
    w(f"    pub const COUNT: usize = {len(glob)};")
    w("}")
    w("")
    w("/// Globals read out of RIP-relative instructions in the sites.")
    w("pub static GLOBALS: [GlobalRef; global::COUNT] = [")
    for g in glob:
        w("    GlobalRef {")
        w(f"        name: \"{g['const']}\",")
        w(f"        site: site::{g['site']},")
        w(f"        insn_at: {g['insn_at']},")
        w(f"        disp_at: {g['disp_at']},")
        w(f"        insn_len: {g['insn_len']},")
        w("    },")
    w("];")
    w("")
    w("/// Indices into [`FIELDS`]: one per anchored struct offset.")
    w("pub mod field {")
    for i, f in enumerate(fields):
        named = f" `{f['reflection']}`" if f["reflection"] else ""
        fixed = f" Must not move (`layout::{f['layout']}`)." if f["layout"] else ""
        w(f"    /// {f['object']} `+{f['offset']:#x}`{named}.{fixed}")
        w(f"    pub const {f['const']}: usize = {i};")
    w("    /// Number of fields.")
    w(f"    pub const COUNT: usize = {len(fields)};")
    w("}")
    w("")
    w("/// The anchored struct offsets, with the value in the reference build.")
    w("pub static FIELDS: [FieldInfo; field::COUNT] = [")
    for f in fields:
        w(
            f"    FieldInfo {{ name: \"{f['name']}\", known: {f['offset']:#x}, "
            f"layout: {'true' if f['layout'] else 'false'}, "
            f"reflection: \"{f['reflection']}\" }},"
        )
    w("];")
    w("")
    w("/// Where each field's offset is read: a pattern whose match holds the")
    w("/// field's displacement at `disp_at`. A field may have several anchors;")
    w("/// they must agree.")
    w(f"pub static ANCHORS: [OffsetAnchor; {len(rows)}] = [")
    for r in rows:
        w(f"    // {r['via']}{', displacement wildcarded' if r['loose'] else ''}")
        w("    OffsetAnchor {")
        w(f"        field: field::{r['field']},")
        w(f"        pattern: \"{r['pattern']}\",")
        w(f"        disp_at: {r['disp_at']},")
        w(f"        disp_size: {r['disp_size']},")
        w(f"        known_rva: {r['rva']:#x},")
        w("    },")
    w("];")
    return "\n".join(o) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--exe", default=os.environ.get("MHW_EXE", str(DEFAULT_EXE)))
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    if not Path(args.exe).is_file():
        fail(f"the reference executable is needed: --exe PATH or MHW_EXE ({args.exe})")
    pe = Pe(args.exe)
    text = pe.text()
    entries = json.loads(JSON.read_text())
    kind = lambda k: [e for e in entries if e["kind"] == k]
    sites = gate_sites(kind("gate_site"), pe, text)
    glob = globals_(kind("global"), sites, pe)
    fields, rows, unanchored = anchors(kind("struct_offset"), text, pe)
    out = emit(sites, glob, fields, rows, unanchored)
    if args.check:
        if OUT.read_text() != out:
            fail(f"{OUT.relative_to(ROOT)} is out of date; run tools/gen_sites.py")
        print("sites.rs is up to date")
        return
    OUT.write_text(out)
    loose = sum(r["loose"] for r in rows)
    print(
        f"wrote {OUT.relative_to(ROOT)}: {len(sites)} sites, {len(glob)} globals, "
        f"{len(fields)} fields, {len(rows)} anchors ({loose} with the displacement wildcarded)"
    )


if __name__ == "__main__":
    main()
