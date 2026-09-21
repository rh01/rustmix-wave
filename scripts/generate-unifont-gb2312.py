#!/usr/bin/env python3
"""Generate a compact Unifont GB2312 16x16 bitmap fallback for firmware.

Source: GNU Unifont .hex (SIL Open Font License 1.1).
Output: little-endian packed BMP glyphs for GB2312 plus CJK punctuation.
"""

from __future__ import annotations

import argparse
import gzip
import struct
from pathlib import Path


MAGIC = b"UGB1"


def gb2312_codepoints() -> set[int]:
    codepoints: set[int] = set()
    for lead in range(0xA1, 0xF8):
        for trail in range(0xA1, 0xFF):
            raw = bytes([lead, trail])
            try:
                text = raw.decode("gb2312")
            except UnicodeDecodeError:
                continue
            for character in text:
                code = ord(character)
                if code >= 0x80:
                    codepoints.add(code)
    # Book punctuation and fullwidth forms commonly seen in TXT/EPUB.
    codepoints.update(range(0x2010, 0x2028))
    codepoints.update(range(0x3000, 0x3040))
    codepoints.update(range(0xFE10, 0xFE1A))
    codepoints.update(range(0xFE30, 0xFE50))
    codepoints.update(range(0xFF00, 0xFFEF))
    return codepoints


def parse_unifont_hex(path: Path) -> dict[int, bytes]:
    glyphs: dict[int, bytes] = {}
    opener = gzip.open if path.suffix == ".gz" else open
    with opener(path, "rt", encoding="ascii", errors="ignore") as handle:
        for line in handle:
            line = line.strip()
            if not line or ":" not in line:
                continue
            code_hex, bitmap_hex = line.split(":", 1)
            try:
                code = int(code_hex, 16)
            except ValueError:
                continue
            bitmap_hex = bitmap_hex.strip()
            if len(bitmap_hex) not in (32, 64):
                continue
            raw = bytes.fromhex(bitmap_hex)
            glyphs[code] = normalize_bitmap(raw)
    return glyphs


def normalize_bitmap(raw: bytes) -> bytes:
    """Store every glyph as 16x16 1-bpp, 32 bytes, row-major MSB-first."""
    if len(raw) == 32:
        return raw
    if len(raw) != 16:
        raise ValueError(f"unexpected Unifont bitmap length {len(raw)}")
    # 8-pixel-wide Unifont glyphs: place pixels in the left half of 16x16.
    wide = bytearray(32)
    for row in range(16):
        wide[row * 2] = raw[row]
        wide[row * 2 + 1] = 0
    return bytes(wide)


def write_pack(path: Path, selected: list[tuple[int, bytes]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("wb") as handle:
        handle.write(MAGIC)
        handle.write(struct.pack("<I", len(selected)))
        for code, _bitmap in selected:
            handle.write(struct.pack("<H", code))
        for _code, bitmap in selected:
            handle.write(bitmap)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--hex", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()

    wanted = gb2312_codepoints()
    glyphs = parse_unifont_hex(args.hex)
    selected = []
    missing = 0
    for code in sorted(wanted):
        if code > 0xFFFF:
            continue
        bitmap = glyphs.get(code)
        if bitmap is None:
            missing += 1
            continue
        selected.append((code, bitmap))

    write_pack(args.out, selected)
    print(
        f"unifont-gb2312 glyphs={len(selected)} missing={missing} "
        f"bytes={args.out.stat().st_size} path={args.out}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
