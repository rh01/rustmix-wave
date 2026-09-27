"""RMXLEX1 writer/reader and the committed MINI fixture."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT))

from rmxlex.format import (  # noqa: E402
    FIELD_DEF_EN,
    FIELD_DEF_ZH,
    FIELD_HEADWORD,
    FIELD_PHONETIC,
    FIELD_READING,
    FLAG_CASE_FOLDED,
    FLAG_KANA_KEYS,
    FLAG_PINYIN_KEYS,
    Entry,
    Field,
    FormatError,
    Lexicon,
    WordList,
    build_lexicon,
    build_wordlist,
    lookup_exact,
    lookup_prefix,
    parse_lexicon,
    parse_wordlist,
)
from rmxlex.normalize import normalize_key  # noqa: E402


def mini_lexicon() -> Lexicon:
    entries = [
        Entry(fields=[Field(FIELD_HEADWORD, "apple"), Field(FIELD_DEF_EN, "a fruit"), Field(FIELD_DEF_ZH, "苹果")]),
        Entry(fields=[Field(FIELD_HEADWORD, "apply"), Field(FIELD_DEF_EN, "to put into use")]),
        Entry(fields=[Field(FIELD_HEADWORD, "bank"), Field(FIELD_DEF_EN, "river edge")]),
        Entry(fields=[Field(FIELD_HEADWORD, "bank"), Field(FIELD_DEF_EN, "financial institution")]),
        Entry(
            flags=1,
            fields=[
                Field(FIELD_HEADWORD, "猫"),
                Field(FIELD_READING, "ねこ"),
                Field(FIELD_DEF_EN, "cat"),
            ],
        ),
        Entry(
            fields=[
                Field(FIELD_HEADWORD, "你好"),
                Field(FIELD_READING, "nǐ hǎo"),
                Field(FIELD_DEF_EN, "hello"),
            ],
        ),
    ]
    keys = [
        (normalize_key("apple").encode(), 0),
        (normalize_key("apply").encode(), 1),
        (normalize_key("bank").encode(), 2),
        (normalize_key("bank").encode(), 3),
        (normalize_key("ねこ").encode(), 4),
        (normalize_key("ネコ").encode(), 4),
        (normalize_key("猫").encode(), 4),
        (normalize_key("你好").encode(), 5),
        (normalize_key("nǐ hǎo").encode(), 5),
    ]
    return Lexicon(
        entries=entries,
        keys=keys,
        src_lang="mul",
        dst_lang="mul",
        header_flags=FLAG_CASE_FOLDED | FLAG_KANA_KEYS | FLAG_PINYIN_KEYS,
    )


def mini_wordlist() -> WordList:
    return WordList(dict_id="MINI", title="Mini deck", entry_ids=[0, 1, 4, 5], name="MINI")


class FormatTests(unittest.TestCase):
    def test_round_trip_and_lookups(self) -> None:
        blob = build_lexicon(mini_lexicon())
        parsed = parse_lexicon(blob)
        self.assertEqual(parsed.entry_count, 6)
        self.assertEqual(lookup_exact(parsed, b"apple"), [0])
        self.assertEqual(lookup_exact(parsed, normalize_key("ネコ").encode()), [4])
        self.assertEqual(lookup_exact(parsed, "猫".encode()), [4])
        self.assertEqual(lookup_exact(parsed, b"nihao"), [5])
        self.assertEqual(lookup_exact(parsed, b"bank"), [2, 3])
        prefix = lookup_prefix(parsed, b"app", 8)
        self.assertEqual([item[0] for item in prefix], [b"apple", b"apply"])
        self.assertEqual(lookup_prefix(parsed, b"app", 1), [(b"apple", 0)])

    def test_wordlist_round_trip(self) -> None:
        blob = build_wordlist(mini_wordlist())
        parsed = parse_wordlist(blob)
        self.assertEqual(parsed.dict_id, "MINI")
        self.assertEqual(parsed.title, "Mini deck")
        self.assertEqual(parsed.entry_ids, [0, 1, 4, 5])

    def test_committed_mini_fixture_is_deterministic(self) -> None:
        lex_path = REPO / "tests" / "fixtures" / "lexicon" / "MINI.LEX"
        wls_path = REPO / "tests" / "fixtures" / "lexicon" / "MINI.WLS"
        self.assertEqual(build_lexicon(mini_lexicon()), lex_path.read_bytes())
        self.assertEqual(build_wordlist(mini_wordlist()), wls_path.read_bytes())
        again = build_lexicon(mini_lexicon())
        self.assertEqual(again, build_lexicon(mini_lexicon()))

    def test_bad_magic_and_crc(self) -> None:
        blob = bytearray(build_lexicon(mini_lexicon()))
        broken = bytearray(blob)
        broken[0] = ord("X")
        with self.assertRaises(FormatError):
            parse_lexicon(bytes(broken))
        broken = bytearray(blob)
        broken[60] ^= 0xFF
        with self.assertRaises(FormatError):
            parse_lexicon(bytes(broken))

    def test_rejects_oversized_declared_page_index(self) -> None:
        blob = bytearray(build_lexicon(mini_lexicon()))
        blob[32:36] = (256 * 1024 + 1).to_bytes(4, "little")
        blob[60:64] = __import__("zlib").crc32(blob[:60]).to_bytes(4, "little")
        with self.assertRaises(FormatError):
            parse_lexicon(bytes(blob))

    def test_phonetic_constant_present(self) -> None:
        self.assertEqual(FIELD_PHONETIC, 3)


if __name__ == "__main__":
    unittest.main()
