"""Dictionary source plugins.

`zhwikt` (kaikki.org Chinese Wiktionary) is reserved and intentionally not
registered. Add a `Source` subclass and register it here when that stage starts.
"""

from __future__ import annotations

from .base import BuildOutput, Source
from .cedict import CedictSource
from .ecdict import EcdictSource
from .jlpt import JlptSource
from .jmdict import JmdictSource
from .kanjidic import KanjidicSource

SOURCES: dict[str, Source] = {
    "ecdict": EcdictSource(),
    "jmdict": JmdictSource(),
    "cedict": CedictSource(),
    "kanjidic": KanjidicSource(),
    "jlpt": JlptSource(),
}
