"""Pronunciation pack builder: index lookup, ADPCM, and missing clips."""

from __future__ import annotations

import struct
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from rmxlex.audio_pack import (  # noqa: E402
    build_audio_pack,
    decode_clip_samples,
    encode_clip,
    encode_index,
    estimate_sd_bytes,
    lookup_entry,
    parse_index,
    resolve_voice,
    speak_text,
)
from rmxlex.format import (  # noqa: E402
    FIELD_HEADWORD,
    FIELD_READING,
    Entry,
    Field,
    FormatError,
    Lexicon,
    WordList,
    build_lexicon,
    build_wordlist,
)


def _write_inputs(folder: Path) -> tuple[Path, Path]:
    lexicon = Lexicon(
        entries=[
            Entry(fields=[Field(FIELD_HEADWORD, "apple")]),
            Entry(fields=[Field(FIELD_HEADWORD, "猫"), Field(FIELD_READING, "ねこ")]),
            Entry(fields=[Field(FIELD_HEADWORD, "你好")]),
        ],
        keys=[(b"apple", 0), ("猫".encode(), 1), ("你好".encode(), 2)],
        src_lang="mul",
        dst_lang="mul",
        header_flags=0,
    )
    wordlist = WordList(dict_id="MINI", title="Mini", entry_ids=[2, 0, 0, 9], name="MINI")
    folder.mkdir(parents=True, exist_ok=True)
    lex_path = folder / "DICT.LEX"
    list_path = folder / "MINI.WLS"
    lex_path.write_bytes(build_lexicon(lexicon))
    list_path.write_bytes(build_wordlist(wordlist))
    return list_path, lex_path


class AudioPackTests(unittest.TestCase):
    def test_synth_index_lookup_and_missing_entry(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            list_path, lex_path = _write_inputs(root / "in")
            summary = build_audio_pack(
                list_path,
                lex_path,
                "en_US-ljspeech-medium",
                root / "out",
                synth=True,
            )
            self.assertEqual(summary["clips"], 2)
            folder = Path(str(summary["folder"]))
            records = parse_index((folder / "AUDIO.IDX").read_bytes())
            self.assertEqual([item[0] for item in records], [0, 2])
            self.assertIsNone(lookup_entry(records, 1))
            hit = lookup_entry(records, 0)
            self.assertIsNotNone(hit)
            pak = (folder / "AUDIO.PAK").read_bytes()
            assert hit is not None
            decoded = decode_clip_samples(pak[hit[1] : hit[1] + hit[2]])
            self.assertEqual(len(decoded), 160)
            sidecar = (folder / "AUDIO.TXT").read_text(encoding="utf-8")
            self.assertIn("Public domain", sidecar)
            self.assertIn("Keith Ito", sidecar)
            self.assertFalse((folder / "NOFILE.IDX").is_file())

    def test_japanese_speak_text_uses_reading(self) -> None:
        entry = Entry(fields=[Field(FIELD_HEADWORD, "猫"), Field(FIELD_READING, "ねこ")])
        self.assertEqual(speak_text(entry, "ja"), "ねこ")
        self.assertEqual(speak_text(entry, "zh"), "猫")
        self.assertEqual(speak_text(Entry(fields=[Field(FIELD_HEADWORD, "apple")]), "en"), "apple")

    def test_rejected_and_unknown_voices(self) -> None:
        with self.assertRaises(SystemExit):
            resolve_voice("ja_JP-hi_fi_captain-medium")
        with self.assertRaises(SystemExit):
            resolve_voice("zh_CN-huayan-medium")
        with self.assertRaises(SystemExit):
            resolve_voice("en_US-lessac-medium")
        self.assertEqual(resolve_voice("melo-jp").license, "MIT")
        self.assertEqual(resolve_voice("melo-zh").license, "MIT")

    def test_hostile_counts_and_truncated_files(self) -> None:
        header = bytearray(24)
        header[:8] = b"RMXADP1\x00"
        struct.pack_into("<HHII", header, 8, 1, 1, 16000, 0xFFFFFFFF)
        with self.assertRaises(FormatError):
            decode_clip_samples(bytes(header))
        short = bytearray(encode_clip([0, 1000, -1000, 0]))
        with self.assertRaises(FormatError):
            decode_clip_samples(bytes(short[:-1]))
        index = bytearray(24)
        index[:8] = b"RMXAUD1\x00"
        struct.pack_into("<HHII", index, 8, 1, 1, 16000, 0xFFFFFFFF)
        with self.assertRaises(FormatError) as raised:
            parse_index(bytes(index))
        self.assertIn("count", str(raised.exception))
        good = encode_index([(1, 0, len(short))])
        with self.assertRaises(FormatError):
            parse_index(good[:-1])
        with self.assertRaises(FormatError):
            parse_index(encode_index([(1, 0, 0xFFFFFFFF)]))

    def test_bad_clip_and_index(self) -> None:
        blob = bytearray(encode_clip([0, 1000, -1000, 0]))
        self.assertEqual(bytes(blob[24:]), bytes([0x70, 0x2F]))
        blob[0] = ord("X")
        with self.assertRaises(FormatError):
            decode_clip_samples(bytes(blob))
        index = bytearray(encode_index([(1, 0, 28), (4, 28, 28)]))
        index[-1] ^= 0xFF
        with self.assertRaises(FormatError):
            parse_index(bytes(index))

    def test_size_estimate_is_smaller_than_pcm(self) -> None:
        adpcm = estimate_sd_bytes(5000, 1.0)
        pcm = 5000 * 32000
        self.assertLess(adpcm, pcm // 3)
        self.assertGreater(adpcm, 5000 * 8000)

    def test_cli_synth_refuses_unknown_voice(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            list_path, lex_path = _write_inputs(root / "in")
            script = ROOT / "build_lexicon.py"
            ok = subprocess.run(
                [
                    sys.executable,
                    "-B",
                    str(script),
                    "audio",
                    "--wordlist",
                    str(list_path),
                    "--lexicon",
                    str(lex_path),
                    "--voice",
                    "melo-zh",
                    "--synth",
                    "--out",
                    str(root / "out"),
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(ok.returncode, 0, ok.stderr)
            self.assertIn("MIT", ok.stdout)
            bad = subprocess.run(
                [
                    sys.executable,
                    "-B",
                    str(script),
                    "audio",
                    "--wordlist",
                    str(list_path),
                    "--lexicon",
                    str(lex_path),
                    "--voice",
                    "zh_CN-huayan-medium",
                    "--synth",
                    "--out",
                    str(root / "out2"),
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(bad.returncode, 0)
            self.assertIn("refusing voice", bad.stderr + bad.stdout)


if __name__ == "__main__":
    unittest.main()
