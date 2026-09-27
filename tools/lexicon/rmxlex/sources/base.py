"""Source plugin interface. Payloads are built on the PC and copied to SD."""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path

from ..format import Lexicon, WordList


@dataclass
class BuildOutput:
    lexicon: Lexicon | None = None
    wordlists: list[WordList] = field(default_factory=list)
    meta: dict[str, str] = field(default_factory=dict)
    dict_id: str = ""
    reports: dict[str, str] = field(default_factory=dict)


class Source:
    """One upstream dictionary or word-list family."""

    name = "base"

    def build(self, args: dict[str, object]) -> BuildOutput:
        raise NotImplementedError(f"{self.name} is not implemented")


def read_text_maybe_gzip(path: Path) -> str:
    raw = path.read_bytes()
    if path.suffix == ".gz" or raw[:2] == b"\x1f\x8b":
        import gzip

        raw = gzip.decompress(raw)
    return raw.decode("utf-8")


def open_maybe_gzip(path: Path):
    import gzip

    raw_head = path.open("rb").read(2)
    path.open("rb").close()
    if path.suffix == ".gz" or raw_head == b"\x1f\x8b":
        return gzip.open(path, "rt", encoding="utf-8")
    return path.open("r", encoding="utf-8")
