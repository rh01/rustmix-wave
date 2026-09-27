"""Dictionary source plugins registered with the lexicon builder."""

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
