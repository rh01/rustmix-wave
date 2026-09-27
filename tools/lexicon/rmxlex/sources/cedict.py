"""CC-CEDICT (CC BY-SA 4.0) → RMXLEX1.

Keys are simplified, traditional when it differs, and toneless unspaced pinyin
(`ni3 hao3` → `nihao`). Display pinyin uses tone marks (`nǐ hǎo`).
"""

from __future__ import annotations

import re
from pathlib import Path

from ..format import (
    FIELD_DEF_EN,
    FIELD_HEADWORD,
    FIELD_READING,
    FLAG_CASE_FOLDED,
    FLAG_PINYIN_KEYS,
    Entry,
    Field,
    Lexicon,
)
from ..normalize import normalize_key
from .base import BuildOutput, Source, read_text_maybe_gzip

_LINE = re.compile(r"^(\S+)\s+(\S+)\s+\[(.+?)\]\s+/(.*)/$")
_TONE_MARKS = {
    "a": "āáǎà",
    "e": "ēéěè",
    "i": "īíǐì",
    "o": "ōóǒò",
    "u": "ūúǔù",
    "ü": "ǖǘǚǜ",
}


def tone_syllable(syllable: str) -> str:
    text = syllable.replace("u:", "ü").replace("v", "ü")
    if not text:
        return text
    tone = 0
    if text[-1].isdigit():
        tone = int(text[-1])
        text = text[:-1]
    if tone < 1 or tone > 4:
        return text
    index = -1
    for pos, ch in enumerate(text):
        if ch in "ae":
            index = pos
            break
    if index < 0 and "ou" in text:
        index = text.index("ou")
    if index < 0:
        for pos in range(len(text) - 1, -1, -1):
            if text[pos] in "iouü":
                index = pos
                break
    if index < 0:
        return text
    vowel = text[index]
    marked = _TONE_MARKS[vowel][tone - 1]
    return text[:index] + marked + text[index + 1 :]


def display_pinyin(raw: str) -> str:
    return " ".join(tone_syllable(part) for part in raw.split())


def toneless_key(raw: str) -> str:
    parts = []
    for part in raw.split():
        text = part.replace("u:", "v").replace("ü", "v").replace("v", "v")
        if text and text[-1].isdigit():
            text = text[:-1]
        text = text.replace("u:", "v").replace("ü", "v")
        parts.append(text)
    return normalize_key("".join(parts))


class CedictSource(Source):
    name = "cedict"

    def build(self, args: dict[str, object]) -> BuildOutput:
        path = Path(str(args["in"]))
        text = read_text_maybe_gzip(path)
        entries: list[Entry] = []
        keys: list[tuple[bytes, int]] = []
        warnings: list[str] = []
        for line in text.splitlines():
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            match = _LINE.match(line)
            if not match:
                warnings.append(f"skipped cedict line: {line[:40]}")
                continue
            traditional, simplified, pinyin, glosses = match.groups()
            fields = [Field(FIELD_HEADWORD, simplified), Field(FIELD_READING, display_pinyin(pinyin))]
            if traditional != simplified:
                fields.append(Field(FIELD_READING, traditional))
            for gloss in glosses.split("/"):
                gloss = gloss.strip()
                if gloss:
                    fields.append(Field(FIELD_DEF_EN, gloss))
            entry_id = len(entries)
            entries.append(Entry(fields=fields))
            seen: set[bytes] = set()
            candidates = [normalize_key(simplified)]
            if traditional != simplified:
                candidates.append(normalize_key(traditional))
            candidates.append(toneless_key(pinyin))
            for key in candidates:
                raw = key.encode("utf-8")
                if not raw or raw in seen:
                    continue
                seen.add(raw)
                keys.append((raw, entry_id))
        lexicon = Lexicon(
            entries=entries,
            keys=keys,
            src_lang="zh",
            dst_lang="en",
            header_flags=FLAG_CASE_FOLDED | FLAG_PINYIN_KEYS,
            warnings=warnings,
        )
        meta = {
            "id": "CEDICT",
            "title": "CC-CEDICT",
            "src_lang": "zh",
            "dst_lang": "en",
            "entries": str(len(entries)),
            "source_url": "https://www.mdbg.net/chinese/dictionary?page=cc-cedict",
            "source_date": str(args.get("source_date") or "unspecified"),
            "license": "CC BY-SA 4.0",
            "attribution": "CC-CEDICT / MDBG",
            "redistributable": "no" if args.get("personal") else "yes",
            "builder_version": "rmxlex-1",
        }
        return BuildOutput(lexicon=lexicon, meta=meta, dict_id="CEDICT")
