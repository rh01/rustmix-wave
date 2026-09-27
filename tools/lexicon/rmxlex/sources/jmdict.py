"""JMdict_e (EDRDG, CC BY-SA 4.0) → RMXLEX1.

Common subset: ke_pri/re_pri contains news1, ichi1, spec1, spec2, or gai1.
`--full` keeps every entry. SKIP-like fields are not present in JMdict.
"""

from __future__ import annotations

import re
from pathlib import Path

from ..format import (
    ENTRY_COMMON,
    FIELD_DEF_EN,
    FIELD_HEADWORD,
    FIELD_POS,
    FIELD_READING,
    FLAG_CASE_FOLDED,
    FLAG_KANA_KEYS,
    Entry,
    Field,
    Lexicon,
)
from ..normalize import normalize_key
from .base import BuildOutput, Source

COMMON_PRI = {"news1", "ichi1", "spec1", "spec2", "gai1"}
_ENTITY = re.compile(r"&([A-Za-z0-9_-]+);")


def _texts(block: str, tag: str) -> list[str]:
    return [item.strip() for item in re.findall(rf"<{tag}>(.*?)</{tag}>", block, flags=re.S)]


def iter_entries(path: Path):
    import gzip

    opener = gzip.open if path.suffix == ".gz" else open
    with opener(path, "rt", encoding="utf-8") as handle:
        collecting = False
        buf: list[str] = []
        for line in handle:
            if not collecting:
                if "<entry>" in line:
                    collecting = True
                    buf = [line]
                continue
            buf.append(line)
            if "</entry>" in line:
                raw = _ENTITY.sub(r"\1", "".join(buf))
                collecting = False
                buf = []
                yield raw


class JmdictSource(Source):
    name = "jmdict"

    def build(self, args: dict[str, object]) -> BuildOutput:
        path = Path(str(args["in"]))
        full = bool(args.get("full"))
        entries: list[Entry] = []
        keys: list[tuple[bytes, int]] = []
        warnings: list[str] = []
        for block in iter_entries(path):
            kebs = _texts(block, "keb")
            rebs = _texts(block, "reb")
            pris = set(_texts(block, "ke_pri") + _texts(block, "re_pri"))
            common = bool(pris & COMMON_PRI)
            if not full and not common:
                continue
            senses = re.findall(r"<sense>(.*?)</sense>", block, flags=re.S)
            fields: list[Field] = []
            head = kebs[0] if kebs else (rebs[0] if rebs else "")
            if not head:
                continue
            fields.append(Field(FIELD_HEADWORD, head))
            for reading in rebs:
                fields.append(Field(FIELD_READING, reading))
            for sense in senses:
                pos = ", ".join(_texts(sense, "pos"))
                if pos:
                    fields.append(Field(FIELD_POS, pos))
                glosses = _texts(sense, "gloss")
                if glosses:
                    fields.append(Field(FIELD_DEF_EN, "; ".join(glosses)))
            entry_id = len(entries)
            entries.append(Entry(flags=ENTRY_COMMON if common else 0, fields=fields))
            seen: set[bytes] = set()
            for surface in kebs + rebs:
                key = normalize_key(surface).encode("utf-8")
                if not key or key in seen:
                    continue
                seen.add(key)
                keys.append((key, entry_id))
        lexicon = Lexicon(
            entries=entries,
            keys=keys,
            src_lang="ja",
            dst_lang="en",
            header_flags=FLAG_CASE_FOLDED | FLAG_KANA_KEYS,
            warnings=warnings,
        )
        meta = {
            "id": "JMDICT",
            "title": "JMdict English",
            "src_lang": "ja",
            "dst_lang": "en",
            "entries": str(len(entries)),
            "source_url": "http://www.edrdg.org/jmdict/j_jmdict.html",
            "source_date": str(args.get("source_date") or "unspecified"),
            "license": "CC BY-SA 4.0",
            "attribution": (
                "JMdict / Electronic Dictionary Research and Development Group (EDRDG). "
                "JLPT lists, when bundled: jamsinclair/open-anki-jlpt-decks (MIT); "
                "source lists Jonathan Waller / Tanos.co.uk (CC BY)."
            ),
            "redistributable": "no" if args.get("personal") else "yes",
            "builder_version": "rmxlex-1",
        }
        return BuildOutput(lexicon=lexicon, meta=meta, dict_id="JMDICT")
