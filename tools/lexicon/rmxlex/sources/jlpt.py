"""JLPT word lists (open-anki-jlpt-decks MIT; Tanos CC BY) mapped onto JMdict ids."""

from __future__ import annotations

import csv
from pathlib import Path

from ..format import WordList, parse_lexicon
from ..normalize import normalize_key
from .base import BuildOutput, Source


LEVELS = ("n5", "n4", "n3", "n2", "n1")
TITLES = {
    "n5": "JLPT N5",
    "n4": "JLPT N4",
    "n3": "JLPT N3",
    "n2": "JLPT N2",
    "n1": "JLPT N1",
}


def _index_keys(path: Path) -> dict[bytes, list[int]]:
    parsed = parse_lexicon(path.read_bytes())
    index: dict[bytes, list[int]] = {}
    for key, entry_id in parsed.keys:
        index.setdefault(key, []).append(entry_id)
    return index


def match_entry(index: dict[bytes, list[int]], expression: str, reading: str) -> int | None:
    expr_key = normalize_key(expression).encode("utf-8")
    read_key = normalize_key(reading).encode("utf-8")
    expr_ids = index.get(expr_key, [])
    read_ids = index.get(read_key, [])
    if expr_ids and read_ids:
        both = [entry_id for entry_id in expr_ids if entry_id in set(read_ids)]
        if both:
            return both[0]
    if read_ids:
        return read_ids[0]
    if expr_ids:
        return expr_ids[0]
    return None


class JlptSource(Source):
    name = "jlpt"

    def build(self, args: dict[str, object]) -> BuildOutput:
        in_dir = Path(str(args["in_dir"]))
        jmdict = Path(str(args["jmdict"]))
        index = _index_keys(jmdict)
        wordlists: list[WordList] = []
        unmatched: list[str] = []
        for level in LEVELS:
            path = in_dir / f"{level}.csv"
            if not path.is_file():
                unmatched.append(f"# missing {path.name}")
                continue
            ids: list[int] = []
            with path.open("r", encoding="utf-8", newline="") as handle:
                reader = csv.DictReader(handle)
                for row_number, row in enumerate(reader, start=2):
                    expression = (row.get("expression") or "").strip()
                    reading = (row.get("reading") or "").strip()
                    entry_id = match_entry(index, expression, reading)
                    if entry_id is None:
                        unmatched.append(f"{level}\t{row_number}\t{expression}\t{reading}")
                        continue
                    ids.append(entry_id)
            wordlists.append(
                WordList(
                    dict_id="JMDICT",
                    title=TITLES[level],
                    entry_ids=ids,
                    name=f"JLPT{level.upper()}",
                )
            )
        report = "\n".join(unmatched) + ("\n" if unmatched else "")
        return BuildOutput(
            wordlists=wordlists,
            dict_id="JMDICT",
            reports={"unmatched.txt": report},
        )
