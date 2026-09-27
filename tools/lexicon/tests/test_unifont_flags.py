"""Generator flags for optional IPA and JIS X 0208 Unifont coverage."""

import importlib.util
import unittest
from pathlib import Path


def load_generator():
    path = Path(__file__).resolve().parents[3] / "scripts" / "generate-unifont-gb2312.py"
    spec = importlib.util.spec_from_file_location("generate_unifont_gb2312", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class UnifontFlagTests(unittest.TestCase):
    def test_ipa_includes_schwa_and_jis_includes_hiragana(self):
        generator = load_generator()
        ipa = generator.ipa_codepoints()
        self.assertIn(ord("ə"), ipa)
        self.assertNotIn(ord("中"), ipa)
        jis = generator.jis0208_codepoints()
        self.assertIn(ord("あ"), jis)
        self.assertIn(ord("ア"), jis)
        self.assertTrue(generator.gb2312_codepoints())


if __name__ == "__main__":
    unittest.main()
