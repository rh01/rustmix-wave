"""Synthetic source fixtures. No upstream dumps are committed."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from rmxlex.format import (  # noqa: E402
    FIELD_DEF_EN,
    FIELD_DEF_ZH,
    FIELD_PHONETIC,
    TAG_CET4,
    TAG_CET6,
    TAG_ZK,
    build_lexicon,
    parse_lexicon,
)
from rmxlex.normalize import normalize_key  # noqa: E402
from rmxlex.sources.cedict import CedictSource, display_pinyin  # noqa: E402
from rmxlex.sources.ecdict import EcdictSource, is_core  # noqa: E402
from rmxlex.sources.jlpt import JLPT_ATTRIBUTION, JlptSource, match_entry  # noqa: E402
from rmxlex.sources.jmdict import JmdictSource  # noqa: E402
from rmxlex.sources.kanjidic import KanjidicSource  # noqa: E402

FIXTURES = Path(__file__).resolve().parent / "fixtures"


def _field_text(entry, field_id: int) -> list[str]:
    return [item.text for item in entry.fields if item.field_id == field_id]


class EcdictTests(unittest.TestCase):
    def test_core_filter_phonetic_and_tags(self) -> None:
        self.assertFalse(is_core("", "cet4", 1, 1, 1))
        self.assertTrue(is_core("苹果", "cet4", 0, 0, 0))
        self.assertTrue(is_core("香蕉", "", 30000, 0, 0))
        self.assertFalse(is_core("普通的", "", 0, 0, 0))
        self.assertFalse(is_core("普通的", "", 30001, 30001, 0))
        self.assertTrue(is_core("x", "", 0, 0, 2))
        built = EcdictSource().build({"in": FIXTURES / "ecdict_sample.csv", "full": False, "source_date": "fixture"})
        words = [_field_text(entry, 1)[0] for entry in built.lexicon.entries]
        self.assertEqual(words, ["apple", "banana", "apply", "bank"])
        apple = built.lexicon.entries[0]
        self.assertIn("ə", _field_text(apple, FIELD_PHONETIC)[0])
        self.assertNotIn("\u04d9", _field_text(apple, FIELD_PHONETIC)[0])
        self.assertEqual(_field_text(apple, FIELD_DEF_ZH), ["苹果", "果实"])
        self.assertEqual(apple.tags & TAG_CET4, TAG_CET4)
        apply = built.lexicon.entries[2]
        self.assertEqual(apply.tags & (TAG_CET4 | TAG_CET6), TAG_CET4 | TAG_CET6)
        bank = built.lexicon.entries[3]
        self.assertEqual(bank.tags & TAG_ZK, TAG_ZK)
        names = [item.name for item in built.wordlists]
        self.assertIn("CET4", names)
        self.assertIn("ZK", names)
        cet4 = next(item for item in built.wordlists if item.name == "CET4")
        self.assertEqual(cet4.entry_ids, [2, 0])
        self.assertEqual(built.meta["license"], "MIT")
        self.assertEqual(built.meta["redistributable"], "yes")

    def test_personal_flag(self) -> None:
        built = EcdictSource().build(
            {"in": FIXTURES / "ecdict_sample.csv", "full": True, "personal": True, "source_date": "fixture"}
        )
        self.assertEqual(built.meta["redistributable"], "no")
        words = [_field_text(entry, 1)[0] for entry in built.lexicon.entries]
        self.assertIn("plain", words)
        plain = next(entry for entry in built.lexicon.entries if _field_text(entry, 1)[0] == "plain")
        self.assertIn("ε", _field_text(plain, FIELD_PHONETIC)[0])


class JmdictTests(unittest.TestCase):
    def test_common_keys_and_glosses(self) -> None:
        built = JmdictSource().build({"in": FIXTURES / "jmdict_sample.xml", "full": False, "source_date": "fixture"})
        self.assertEqual(len(built.lexicon.entries), 2)
        parsed = parse_lexicon(build_lexicon(built.lexicon))
        keys = {key.decode(): entry_id for key, entry_id in parsed.keys}
        self.assertEqual(keys["猫"], 0)
        self.assertEqual(keys[normalize_key("ねこ")], 0)
        self.assertEqual(keys[normalize_key("こんにちは")], 1)
        self.assertNotIn("犬", keys)
        cat = built.lexicon.entries[0]
        self.assertEqual(cat.flags & 1, 1)
        gloss = _field_text(cat, FIELD_DEF_EN)
        self.assertEqual(gloss, ["cat; feline"])
        self.assertEqual(_field_text(cat, 4), ["noun (common) (futsuumeishi)"])
        hello = built.lexicon.entries[1]
        self.assertEqual(_field_text(hello, 4), ["interjection (kandoushi)"])
        self.assertIn("CC BY-SA 4.0", built.meta["license"])
        self.assertIn("EDRDG", built.meta["attribution"])
        self.assertIn(JLPT_ATTRIBUTION, built.meta["attribution"])

    def test_full_includes_uncommon(self) -> None:
        built = JmdictSource().build({"in": FIXTURES / "jmdict_sample.xml", "full": True, "source_date": "fixture"})
        self.assertEqual(len(built.lexicon.entries), 3)


class CedictTests(unittest.TestCase):
    def test_keys_and_tone_marks(self) -> None:
        self.assertEqual(display_pinyin("ni3 hao3"), "nǐ hǎo")
        self.assertEqual(display_pinyin("nv3"), "nǚ")
        self.assertEqual(display_pinyin("lüe4"), "lüè")
        built = CedictSource().build({"in": FIXTURES / "cedict_sample.txt", "source_date": "fixture"})
        parsed = parse_lexicon(build_lexicon(built.lexicon))
        by_key: dict[bytes, list[int]] = {}
        for key, entry_id in parsed.keys:
            by_key.setdefault(key, []).append(entry_id)
        self.assertEqual(by_key["你好".encode()], [0])
        self.assertEqual(by_key[b"nihao"], [0])
        self.assertEqual(by_key["银行".encode()], [1])
        self.assertEqual(by_key["銀行".encode()], [1])
        self.assertEqual(by_key[b"yinhang"], [1])
        self.assertEqual(by_key[b"nv"], [2])
        hello = built.lexicon.entries[0]
        self.assertIn("nǐ hǎo", _field_text(hello, 2))
        self.assertEqual(_field_text(hello, FIELD_DEF_EN), ["hello", "hi"])
        self.assertEqual(built.meta["license"], "CC BY-SA 4.0")


class KanjidicTests(unittest.TestCase):
    def test_grade_filter_and_no_skip(self) -> None:
        built = KanjidicSource().build({"in": FIXTURES / "kanjidic_sample.xml", "full": False, "source_date": "fixture"})
        self.assertEqual(len(built.lexicon.entries), 1)
        info = _field_text(built.lexicon.entries[0], 9)[0]
        self.assertIn("on=ドウ", info)
        self.assertIn("strokes=13", info)
        self.assertNotIn("skip", info.lower())
        self.assertNotIn("2-3-10", info)
        self.assertEqual(_field_text(built.lexicon.entries[0], FIELD_DEF_EN), ["work"])
        full = KanjidicSource().build({"in": FIXTURES / "kanjidic_sample.xml", "full": True, "source_date": "fixture"})
        self.assertEqual(len(full.lexicon.entries), 2)
        self.assertIn("EDRDG", built.meta["attribution"])


class JlptTests(unittest.TestCase):
    def test_match_and_unmatched_report(self) -> None:
        jmdict = JmdictSource().build({"in": FIXTURES / "jmdict_sample.xml", "full": True, "source_date": "fixture"})
        lex_path = FIXTURES / "jmdict_built.lex"
        lex_path.write_bytes(build_lexicon(jmdict.lexicon))
        try:
            built = JlptSource().build(
                {"in_dir": FIXTURES / "jlpt", "jmdict": lex_path, "source_date": "fixture"}
            )
        finally:
            lex_path.unlink(missing_ok=True)
        self.assertEqual(len(built.wordlists), 1)
        self.assertEqual(built.wordlists[0].name, "JLPTN5")
        self.assertEqual(len(built.wordlists[0].entry_ids), 2)
        self.assertIn("未収録", built.reports["unmatched.txt"])
        self.assertNotIn("猫", built.reports["unmatched.txt"].split("未収録")[0])

    def test_kanji_and_reading_must_agree(self) -> None:
        hashi = normalize_key("はし").encode()
        bridge = normalize_key("橋").encode()
        chopsticks = normalize_key("箸").encode()
        index = {bridge: [1], chopsticks: [2], hashi: [1, 2]}
        self.assertEqual(match_entry(index, "橋", "はし"), 1)
        self.assertIsNone(match_entry({bridge: [1], hashi: [2]}, "橋", "はし"))
        self.assertEqual(match_entry({hashi: [2]}, "", "はし"), 2)
        self.assertIn("Creative Commons BY", JLPT_ATTRIBUTION)
        self.assertIn("jamsinclair/open-anki-jlpt-decks (MIT)", JLPT_ATTRIBUTION)
        self.assertIn("https://www.tanos.co.uk/jlpt/sharing/", JLPT_ATTRIBUTION)


if __name__ == "__main__":
    unittest.main()
