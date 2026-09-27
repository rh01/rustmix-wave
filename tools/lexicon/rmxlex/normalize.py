"""Key normalization shared with `src/lexicon/normalize.rs`.

Python applies Unicode NFKC first. The firmware query path does not link a
normalization crate; it folds the same subset the writer has already applied:
fullwidth ASCII, ASCII case, katakana, stripped separators, and pinyin tone
letters. Test vectors in `tests/fixtures/lexicon/normalize_vectors.tsv` must
agree byte for byte.
"""

from __future__ import annotations

import unicodedata

# Pinyin tone letters and ü map to unaccented letters. ü becomes v so CEDICT
# `nu:3` / `nü` keys match the toneless `nv` form.
_TONE = {
    "ā": "a",
    "á": "a",
    "ǎ": "a",
    "à": "a",
    "ē": "e",
    "é": "e",
    "ě": "e",
    "è": "e",
    "ī": "i",
    "í": "i",
    "ǐ": "i",
    "ì": "i",
    "ō": "o",
    "ó": "o",
    "ǒ": "o",
    "ò": "o",
    "ū": "u",
    "ú": "u",
    "ǔ": "u",
    "ù": "u",
    "ǖ": "v",
    "ǘ": "v",
    "ǚ": "v",
    "ǜ": "v",
    "ü": "v",
    "Ā": "a",
    "Á": "a",
    "Ǎ": "a",
    "À": "a",
    "Ē": "e",
    "É": "e",
    "Ě": "e",
    "È": "e",
    "Ī": "i",
    "Í": "i",
    "Ǐ": "i",
    "Ì": "i",
    "Ō": "o",
    "Ó": "o",
    "Ǒ": "o",
    "Ò": "o",
    "Ū": "u",
    "Ú": "u",
    "Ǔ": "u",
    "Ù": "u",
    "Ǖ": "v",
    "Ǘ": "v",
    "Ǚ": "v",
    "Ǜ": "v",
    "Ü": "v",
}

_STRIP = set(" -'·.")


def normalize_key(text: str) -> str:
    """Return the lookup key. Empty input stays empty."""
    folded = unicodedata.normalize("NFKC", text)
    out: list[str] = []
    for ch in folded:
        code = ord(ch)
        if 0x30A1 <= code <= 0x30F6:
            ch = chr(code - 0x60)
        mapped = _TONE.get(ch)
        if mapped is not None:
            ch = mapped
        elif "A" <= ch <= "Z":
            ch = ch.lower()
        if ch in _STRIP:
            continue
        out.append(ch)
    return "".join(out)
