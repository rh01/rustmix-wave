"""KANJIDIC2 (EDRDG, CC BY-SA 4.0) → RMXLEX1.

SKIP codes and other third-party query codes are not copied. The default build
keeps characters that have a grade or a jlpt field. `--full` keeps every
character.
"""

from __future__ import annotations

import re
from pathlib import Path

from ..format import (
    FIELD_DEF_EN,
    FIELD_HEADWORD,
    FIELD_KANJI_INFO,
    FIELD_READING,
    FLAG_CASE_FOLDED,
    Entry,
    Field,
    Lexicon,
)
from ..normalize import normalize_key
from .base import BuildOutput, Source

_ENTITY = re.compile(r"&([A-Za-z0-9_-]+);")


def _texts(block: str, tag: str) -> list[str]:
    return [item.strip() for item in re.findall(rf"<{tag}[^>]*>(.*?)</{tag}>", block, flags=re.S)]


def _attr_texts(block: str, tag: str, attr: str, value: str) -> list[str]:
    pattern = rf'<{tag}[^>]*{attr}="{value}"[^>]*>(.*?)</{tag}>'
    return [item.strip() for item in re.findall(pattern, block, flags=re.S)]


def iter_characters(path: Path):
    import gzip

    opener = gzip.open if path.suffix == ".gz" else open
    with opener(path, "rt", encoding="utf-8") as handle:
        collecting = False
        buf: list[str] = []
        for line in handle:
            if not collecting:
                if "<character>" in line:
                    collecting = True
                    buf = [line]
                continue
            buf.append(line)
            if "</character>" in line:
                raw = _ENTITY.sub(r"\1", "".join(buf))
                collecting = False
                buf = []
                yield raw


class KanjidicSource(Source):
    name = "kanjidic"

    def build(self, args: dict[str, object]) -> BuildOutput:
        path = Path(str(args["in"]))
        full = bool(args.get("full"))
        entries: list[Entry] = []
        keys: list[tuple[bytes, int]] = []
        warnings: list[str] = []
        for block in iter_characters(path):
            # Drop query_code so SKIP cannot leak into a sloppy field copy.
            block = re.sub(r"<query_code>.*?</query_code>", "", block, flags=re.S)
            literals = _texts(block, "literal")
            if not literals:
                continue
            literal = literals[0]
            grades = _texts(block, "grade")
            jlpt = _texts(block, "jlpt")
            if not full and not grades and not jlpt:
                continue
            strokes = _texts(block, "stroke_count")
            on = _attr_texts(block, "reading", "r_type", "ja_on")
            kun = _attr_texts(block, "reading", "r_type", "ja_kun")
            meanings = re.findall(r"<meaning>(.*?)</meaning>", block, flags=re.S)
            info = "on={};kun={};strokes={};grade={}".format(
                ",".join(on),
                ",".join(kun),
                strokes[0] if strokes else "",
                grades[0] if grades else "",
            )
            if jlpt:
                info += f";jlpt={jlpt[0]}"
            fields = [Field(FIELD_HEADWORD, literal), Field(FIELD_KANJI_INFO, info)]
            for reading in on + kun:
                fields.append(Field(FIELD_READING, reading))
            for meaning in meanings:
                fields.append(Field(FIELD_DEF_EN, meaning.strip()))
            entry_id = len(entries)
            entries.append(Entry(fields=fields))
            key = normalize_key(literal).encode("utf-8")
            if key:
                keys.append((key, entry_id))
        lexicon = Lexicon(
            entries=entries,
            keys=keys,
            src_lang="ja",
            dst_lang="en",
            header_flags=FLAG_CASE_FOLDED,
            warnings=warnings,
        )
        meta = {
            "id": "KANJI",
            "title": "KANJIDIC2",
            "src_lang": "ja",
            "dst_lang": "en",
            "entries": str(len(entries)),
            "source_url": "http://www.edrdg.org/kanjidic/kanjidic2.html",
            "source_date": str(args.get("source_date") or "unspecified"),
            "license": "CC BY-SA 4.0",
            "attribution": "KANJIDIC2 / Electronic Dictionary Research and Development Group (EDRDG). SKIP codes omitted.",
            "redistributable": "no" if args.get("personal") else "yes",
            "builder_version": "rmxlex-1",
        }
        return BuildOutput(lexicon=lexicon, meta=meta, dict_id="KANJI")
