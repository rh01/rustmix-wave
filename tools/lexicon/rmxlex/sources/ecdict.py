"""ECDICT (MIT) CSV → RMXLEX1 plus exam word lists derived from the tag field."""

from __future__ import annotations

import csv
import io
from pathlib import Path

from ..format import (
    ENTRY_OXFORD,
    FIELD_DEF_EN,
    FIELD_DEF_ZH,
    FIELD_FORMS,
    FIELD_HEADWORD,
    FIELD_PHONETIC,
    FIELD_POS,
    FLAG_CASE_FOLDED,
    TAG_BY_NAME,
    Entry,
    Field,
    Lexicon,
    WordList,
)
from ..normalize import normalize_key
from .base import BuildOutput, Source, read_text_maybe_gzip

EXAM_LISTS = (
    ("zk", "ZK", "中考"),
    ("gk", "GK", "高考"),
    ("cet4", "CET4", "大学英语四级"),
    ("cet6", "CET6", "大学英语六级"),
    ("ky", "KY", "考研"),
    ("toefl", "TOEFL", "TOEFL"),
    ("ielts", "IELTS", "IELTS"),
    ("gre", "GRE", "GRE"),
)


def _int(value: str) -> int:
    value = (value or "").strip()
    if not value:
        return 0
    try:
        return int(value)
    except ValueError:
        return 0


def fix_phonetic(value: str) -> str:
    return value.replace("\u04d9", "\u0259").replace("\u0454", "\u03b5")


def split_senses(value: str) -> list[str]:
    text = (value or "").replace("\\n", "\n")
    return [line.strip() for line in text.split("\n") if line.strip()]


def tag_mask(tag: str) -> int:
    mask = 0
    for part in tag.replace(",", " ").split():
        mask |= TAG_BY_NAME.get(part.strip().lower(), 0)
    return mask


def is_core(translation: str, tag: str, frq: int, bnc: int, collins: int) -> bool:
    if not translation.strip():
        return False
    if tag.strip():
        return True
    if 1 <= frq <= 30000 or 1 <= bnc <= 30000:
        return True
    return collins > 0


class EcdictSource(Source):
    name = "ecdict"

    def build(self, args: dict[str, object]) -> BuildOutput:
        path = Path(str(args["in"]))
        full = bool(args.get("full"))
        text = read_text_maybe_gzip(path)
        reader = csv.DictReader(io.StringIO(text))
        entries: list[Entry] = []
        keys: list[tuple[bytes, int]] = []
        warnings: list[str] = []
        exam_rows: dict[str, list[tuple[int, int, str]]] = {name: [] for name, _file, _title in EXAM_LISTS}

        for row in reader:
            word = (row.get("word") or "").strip()
            if not word:
                continue
            translation = row.get("translation") or ""
            tag = row.get("tag") or ""
            frq = _int(row.get("frq") or "")
            bnc = _int(row.get("bnc") or "")
            collins = _int(row.get("collins") or "")
            if not full and not is_core(translation, tag, frq, bnc, collins):
                continue
            entry_id = len(entries)
            flags = ENTRY_OXFORD if _int(row.get("oxford") or "") > 0 else 0
            fields = [Field(FIELD_HEADWORD, word)]
            phonetic = fix_phonetic(row.get("phonetic") or "")
            if phonetic:
                fields.append(Field(FIELD_PHONETIC, phonetic))
            pos = (row.get("pos") or "").strip()
            if pos:
                fields.append(Field(FIELD_POS, pos))
            for sense in split_senses(translation):
                fields.append(Field(FIELD_DEF_ZH, sense))
            for sense in split_senses(row.get("definition") or ""):
                fields.append(Field(FIELD_DEF_EN, sense))
            exchange = row.get("exchange") or ""
            if exchange:
                fields.append(Field(FIELD_FORMS, exchange))
            mask = tag_mask(tag)
            entries.append(Entry(tags=mask, freq_rank=frq if frq > 0 else 0, flags=flags, fields=fields))
            key = normalize_key(word)
            if key:
                keys.append((key.encode("utf-8"), entry_id))
            for name, _file, _title in EXAM_LISTS:
                if mask & TAG_BY_NAME[name]:
                    exam_rows[name].append((frq if frq > 0 else 10**9, entry_id, word))

        wordlists = []
        for name, filename, title in EXAM_LISTS:
            rows = sorted(exam_rows[name], key=lambda item: (item[0], item[2], item[1]))
            if not rows:
                continue
            wordlists.append(
                WordList(
                    dict_id="ECDICT",
                    title=title,
                    entry_ids=[item[1] for item in rows],
                    name=filename,
                )
            )

        lexicon = Lexicon(
            entries=entries,
            keys=keys,
            src_lang="en",
            dst_lang="zh",
            header_flags=FLAG_CASE_FOLDED,
            warnings=warnings,
        )
        meta = {
            "id": "ECDICT",
            "title": "ECDICT",
            "src_lang": "en",
            "dst_lang": "zh",
            "entries": str(len(entries)),
            "source_url": "https://github.com/skywind3000/ECDICT",
            "source_date": str(args.get("source_date") or "unspecified"),
            "license": "MIT",
            "attribution": "skywind3000/ECDICT",
            "redistributable": "no" if args.get("personal") else "yes",
            "builder_version": "rmxlex-1",
        }
        return BuildOutput(lexicon=lexicon, wordlists=wordlists, meta=meta, dict_id="ECDICT")
