"""Normalization must match tests/fixtures/lexicon/normalize_vectors.tsv."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT))

from rmxlex.normalize import normalize_key  # noqa: E402


class NormalizeTests(unittest.TestCase):
    def test_shared_vectors(self) -> None:
        path = REPO / "tests" / "fixtures" / "lexicon" / "normalize_vectors.tsv"
        lines = [line for line in path.read_text(encoding="utf-8").splitlines() if line]
        self.assertGreaterEqual(len(lines), 40)
        for line in lines:
            source, expected = line.split("\t")
            self.assertEqual(normalize_key(source), expected, source)

    def test_nfkc_fullwidth_digits(self) -> None:
        self.assertEqual(normalize_key("１２３"), "123")


if __name__ == "__main__":
    unittest.main()
