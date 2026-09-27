#!/usr/bin/env python3
"""Build RMXLEX1 dictionaries and RMXWLS1 word lists for the SD card.

Uses the Python 3.10+ standard library only. Raw dumps are read from disk or
downloaded by scripts/build-lexicon-pack.sh; this tool does not vendor them.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

# Allow `python3 tools/lexicon/build_lexicon.py` without installing the package.
sys.path.insert(0, str(Path(__file__).resolve().parent))

from rmxlex.audio_pack import build_audio_pack  # noqa: E402
from rmxlex.format import build_lexicon, build_wordlist, parse_lexicon, write_meta  # noqa: E402
from rmxlex.sources import SOURCES  # noqa: E402


def _emit(output: Path, dict_id: str, built) -> None:
    root = output / "RUSTMIX" / "LEXICON"
    if built.lexicon is not None and dict_id:
        folder = root / dict_id
        folder.mkdir(parents=True, exist_ok=True)
        blob = build_lexicon(built.lexicon)
        (folder / "DICT.LEX").write_bytes(blob)
        if built.meta:
            (folder / "META.TXT").write_text(write_meta(built.meta), encoding="utf-8")
        for warning in built.lexicon.warnings:
            print(f"warning: {warning}", file=sys.stderr)
        print(f"wrote {folder / 'DICT.LEX'} entries={built.meta.get('entries', '?')}")
    if built.wordlists:
        lists = root / "LISTS"
        lists.mkdir(parents=True, exist_ok=True)
        for wordlist in built.wordlists:
            name = wordlist.name or "LIST"
            if len(name) > 8:
                raise SystemExit(f"word list name {name} exceeds 8 characters")
            path = lists / f"{name}.WLS"
            path.write_bytes(build_wordlist(wordlist))
            print(f"wrote {path} count={len(wordlist.entry_ids)}")
    for name, text in built.reports.items():
        report = output / name
        report.write_text(text, encoding="utf-8")
        print(f"wrote {report}")


def _common(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--source-date", default="unspecified")
    parser.add_argument(
        "--personal",
        action="store_true",
        help="Mark META redistributable=no. Not part of the recommended SD pack.",
    )
    parser.add_argument("--full", action="store_true")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Build Rustmix lexicon packs")
    sub = parser.add_subparsers(dest="command", required=True)

    for name in ("ecdict", "jmdict", "cedict", "kanjidic"):
        cmd = sub.add_parser(name)
        cmd.add_argument("--in", dest="infile", required=True, type=Path)
        _common(cmd)

    jlpt = sub.add_parser("jlpt")
    jlpt.add_argument("--in-dir", required=True, type=Path)
    jlpt.add_argument("--jmdict", required=True, type=Path)
    jlpt.add_argument("--out", required=True, type=Path)
    jlpt.add_argument("--source-date", default="unspecified")
    jlpt.add_argument("--personal", action="store_true")

    verify = sub.add_parser("verify")
    verify.add_argument("lexicon", type=Path)

    audio = sub.add_parser("audio")
    audio.add_argument("--wordlist", required=True, type=Path)
    audio.add_argument("--lexicon", required=True, type=Path)
    audio.add_argument("--voice", required=True)
    audio.add_argument("--model", type=Path, help="Piper .onnx model for en_US-ljspeech-medium")
    audio.add_argument("--piper", default="piper", help="Piper executable")
    audio.add_argument("--out", required=True, type=Path)
    audio.add_argument(
        "--synth",
        action="store_true",
        help="Write deterministic tones instead of calling Piper or MeloTTS",
    )

    args = parser.parse_args(argv)
    if args.command == "audio":
        summary = build_audio_pack(
            args.wordlist,
            args.lexicon,
            args.voice,
            args.out,
            synth=args.synth,
            model=args.model,
            piper_bin=args.piper,
        )
        print(
            f"wrote {summary['folder']} clips={summary['clips']} "
            f"pak_bytes={summary['pak_bytes']} voice={summary['voice']} "
            f"license={summary['license']}"
        )
        return 0

    if args.command == "verify":
        parsed = parse_lexicon(args.lexicon.read_bytes())
        print(
            f"ok entries={parsed.entry_count} keys={parsed.key_count} "
            f"pages={parsed.page_count} {parsed.src_lang}->{parsed.dst_lang}"
        )
        return 0

    if args.command == "jlpt":
        built = SOURCES["jlpt"].build(
            {
                "in_dir": args.in_dir,
                "jmdict": args.jmdict,
                "source_date": args.source_date,
                "personal": args.personal,
            }
        )
        _emit(args.out, "", built)
        return 0

    built = SOURCES[args.command].build(
        {
            "in": args.infile,
            "full": args.full,
            "source_date": args.source_date,
            "personal": args.personal,
        }
    )
    _emit(args.out, built.dict_id, built)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
