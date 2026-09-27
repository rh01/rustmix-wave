"""Build RMXAUD1 pronunciation packs for one word list.

Clips are IMA ADPCM mono at 16 kHz (`RMXADP1` blobs inside `AUDIO.PAK`).
The index is sorted by lexicon entry id so the firmware can binary-search it.

Size, per second of speech, before the 24-byte clip header:

- PCM16 16 kHz: 32,000 bytes (about 160 MB for 5,000 one-second words)
- IMA ADPCM: 8,000 bytes (about 40 MB for the same list)
- MP3 at 32 kbps would be smaller still, but this firmware has no MP3 decoder

A typical headword is under a second, so a JLPT N5 list is on the order of
5 MB and a 5,000-word English or Chinese list is on the order of 30–40 MB.
Generated audio is not committed.

Voices whose dataset licenses are non-commercial, research-only, or unknown
are refused. The selected voices are recorded in tools/lexicon/CREDITS.txt.
"""

from __future__ import annotations

import struct
import subprocess
import sys
import tempfile
import wave
import zlib
from dataclasses import dataclass
from pathlib import Path

from rmxlex.format import (
    FIELD_HEADWORD,
    FIELD_READING,
    FormatError,
    parse_lexicon,
    parse_wordlist,
)

INDEX_MAGIC = b"RMXAUD1\x00"
CLIP_MAGIC = b"RMXADP1\x00"
SAMPLE_RATE = 16_000
CLIP_HEADER_LEN = 24

STEP_TABLE = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45,
    50, 55, 60, 66, 73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230,
    253, 279, 307, 337, 371, 408, 449, 494, 544, 598, 658, 724, 796, 876, 963,
    1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272, 2499, 2749, 3024, 3327,
    3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493, 10442,
    11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794,
    32767,
]
INDEX_TABLE = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8]

# Official Piper voices that must not be used for this pack.
REJECTED_VOICES = {
    "en_US-lessac-medium": "Blizzard 2013 Lessac data is research-only and excludes speech products",
    "en_US-lessac-low": "Blizzard 2013 Lessac data is research-only and excludes speech products",
    "en_US-lessac-high": "Blizzard 2013 Lessac data is research-only and excludes speech products",
    "en_US-amy-medium": "finetuned from the research-only Lessac voice",
    "en_US-amy-low": "finetuned from the research-only Lessac voice",
    "ja_JP-hi_fi_captain-medium": "Hi-Fi-CAPTAIN is CC BY-NC-SA 4.0",
    "zh_CN-huayan-medium": "HuaYan dataset license is unknown and the voice is a Lessac finetune",
}


@dataclass(frozen=True)
class Voice:
    voice_id: str
    lang: str
    engine: str
    license: str
    attribution: str
    url: str
    note: str


VOICES = {
    "en_US-ljspeech-medium": Voice(
        voice_id="en_US-ljspeech-medium",
        lang="en",
        engine="piper",
        license="Public domain dataset; Piper engine MIT",
        attribution="Keith Ito, LJ Speech; Piper voice en_US-ljspeech-medium (trained from scratch)",
        url="https://keithito.com/LJ-Speech-Dataset/",
        note="rhasspy/piper-voices en/en_US/ljspeech/medium MODEL_CARD",
    ),
    "melo-jp": Voice(
        voice_id="melo-jp",
        lang="ja",
        engine="melo",
        license="MIT",
        attribution="MyShell.ai MeloTTS Japanese (JP)",
        url="https://github.com/myshell-ai/MeloTTS",
        note="https://huggingface.co/myshell-ai/MeloTTS-Japanese",
    ),
    "melo-zh": Voice(
        voice_id="melo-zh",
        lang="zh",
        engine="melo",
        license="MIT",
        attribution="MyShell.ai MeloTTS Chinese (ZH)",
        url="https://github.com/myshell-ai/MeloTTS",
        note="https://huggingface.co/myshell-ai/MeloTTS-Chinese",
    ),
}

MELO_LANGUAGE = {"melo-jp": ("JP", "JP"), "melo-zh": ("ZH", "ZH")}


def resolve_voice(voice_id: str) -> Voice:
    if voice_id in REJECTED_VOICES:
        raise SystemExit(f"refusing voice {voice_id}: {REJECTED_VOICES[voice_id]}")
    voice = VOICES.get(voice_id)
    if voice is None:
        known = ", ".join(sorted(VOICES))
        raise SystemExit(f"unknown voice {voice_id}; choose one of: {known}")
    return voice


def speak_text(entry, lang: str) -> str:
    head = _field(entry, FIELD_HEADWORD)
    reading = _field(entry, FIELD_READING)
    if lang == "ja" and reading:
        text = reading
    else:
        text = head or reading
    return " ".join(text.split())[:120]


def build_audio_pack(
    wordlist_path: Path,
    lexicon_path: Path,
    voice_id: str,
    out_dir: Path,
    *,
    synth: bool = False,
    model: Path | None = None,
    piper_bin: str = "piper",
) -> dict[str, object]:
    voice = resolve_voice(voice_id)
    wordlist = parse_wordlist(wordlist_path.read_bytes())
    lexicon = parse_lexicon(lexicon_path.read_bytes())
    if not _fat_id(wordlist.dict_id):
        raise FormatError(f"dict id {wordlist.dict_id!r} is not a FAT 8.3 stem")
    phrases: list[tuple[int, str]] = []
    seen: set[int] = set()
    for entry_id in wordlist.entry_ids:
        if entry_id in seen or entry_id < 0 or entry_id >= len(lexicon.entries):
            continue
        seen.add(entry_id)
        text = speak_text(lexicon.entries[entry_id], voice.lang)
        if text:
            phrases.append((entry_id, text))
    phrases.sort(key=lambda item: item[0])
    if synth:
        clips = [(entry_id, synth_samples(text)) for entry_id, text in phrases]
    elif voice.engine == "piper":
        if model is None:
            raise SystemExit(f"--model is required for {voice.voice_id}")
        clips = _piper_clips(phrases, model, piper_bin)
    elif voice.engine == "melo":
        clips = _melo_clips(phrases, voice.voice_id)
    else:
        raise SystemExit(f"no engine for {voice.voice_id}")
    pak, records = _pack_clips(clips)
    index = encode_index(records)
    folder = out_dir / "RUSTMIX" / "LEXICON" / wordlist.dict_id
    folder.mkdir(parents=True, exist_ok=True)
    (folder / "AUDIO.PAK").write_bytes(pak)
    (folder / "AUDIO.IDX").write_bytes(index)
    sidecar = _sidecar(voice, len(records), len(pak), synth)
    (folder / "AUDIO.TXT").write_text(sidecar, encoding="utf-8")
    return {
        "dict_id": wordlist.dict_id,
        "clips": len(records),
        "pak_bytes": len(pak),
        "idx_bytes": len(index),
        "voice": voice.voice_id,
        "license": voice.license,
        "folder": str(folder),
    }


def estimate_sd_bytes(words: int, seconds: float = 0.9) -> int:
    """IMA ADPCM payload plus one index record per word. Headers add a little more."""
    payload = int(words * seconds * (SAMPLE_RATE // 2))
    index = 24 + words * 12
    headers = words * CLIP_HEADER_LEN
    return payload + index + headers


def encode_clip(samples: list[int]) -> bytes:
    if not samples:
        raise FormatError("empty clip")
    predictor = 0
    step_index = 0
    adpcm = bytearray()
    index = 0
    while index < len(samples):
        low = _encode_nibble(samples[index], predictor, step_index)
        predictor, step_index = low[1], low[2]
        if index + 1 < len(samples):
            high = _encode_nibble(samples[index + 1], predictor, step_index)
            predictor, step_index = high[1], high[2]
            high_nibble = high[0]
        else:
            high_nibble = 0
        adpcm.append(low[0] | (high_nibble << 4))
        index += 2
    header = struct.pack(
        "<8sHHIIHB",
        CLIP_MAGIC,
        1,
        1,
        SAMPLE_RATE,
        len(samples),
        0,
        0,
    )
    # struct above is 8+2+2+4+4+2+1 = 23; pad the reserved byte.
    if len(header) != 23:
        raise FormatError("clip header layout")
    return header + b"\x00" + bytes(adpcm)


def decode_clip_samples(blob: bytes) -> list[int]:
    if len(blob) < CLIP_HEADER_LEN or blob[:8] != CLIP_MAGIC:
        raise FormatError("bad pronunciation clip")
    version, channels, rate, count, _predictor, step_index = struct.unpack_from(
        "<HHIIHB", blob, 8
    )
    if version != 1 or channels != 1 or rate != SAMPLE_RATE or step_index > 88:
        raise FormatError("unsupported pronunciation clip")
    expected = (count + 1) // 2
    if count <= 0 or len(blob) - CLIP_HEADER_LEN != expected:
        raise FormatError("pronunciation clip truncated")
    predictor = int(struct.unpack_from("<h", blob, 20)[0])
    step = int(blob[22])
    samples: list[int] = []
    for byte in blob[CLIP_HEADER_LEN:]:
        predictor, step = _decode_nibble(byte & 0x0F, predictor, step)
        samples.append(predictor)
        if len(samples) >= count:
            break
        predictor, step = _decode_nibble(byte >> 4, predictor, step)
        samples.append(predictor)
    if len(samples) != count:
        raise FormatError("pronunciation clip decode")
    return samples


def encode_index(records: list[tuple[int, int, int]]) -> bytes:
    body = bytearray(INDEX_MAGIC)
    body += struct.pack("<HHII", 1, 1, SAMPLE_RATE, len(records))
    previous = -1
    for entry_id, offset, length in records:
        if entry_id <= previous:
            raise FormatError("pronunciation index is not sorted")
        previous = entry_id
        body += struct.pack("<III", entry_id, offset, length)
    body += struct.pack("<I", zlib.crc32(body) & 0xFFFFFFFF)
    return bytes(body)


def parse_index(blob: bytes) -> list[tuple[int, int, int]]:
    if len(blob) < 24 or blob[:8] != INDEX_MAGIC:
        raise FormatError("bad pronunciation index")
    version, codec, rate, count = struct.unpack_from("<HHII", blob, 8)
    if version != 1 or codec != 1 or rate != SAMPLE_RATE:
        raise FormatError("unsupported pronunciation index")
    body_end = len(blob) - 4
    if body_end != 20 + count * 12:
        raise FormatError("pronunciation index length")
    expect = zlib.crc32(blob[:body_end]) & 0xFFFFFFFF
    actual = struct.unpack_from("<I", blob, body_end)[0]
    if expect != actual:
        raise FormatError("pronunciation index crc")
    records = []
    offset = 20
    previous = -1
    for _ in range(count):
        entry_id, pak_off, length = struct.unpack_from("<III", blob, offset)
        if entry_id <= previous:
            raise FormatError("pronunciation index is not sorted")
        previous = entry_id
        records.append((entry_id, pak_off, length))
        offset += 12
    return records


def lookup_entry(records: list[tuple[int, int, int]], entry_id: int) -> tuple[int, int, int] | None:
    lo = 0
    hi = len(records)
    while lo < hi:
        mid = (lo + hi) // 2
        found = records[mid][0]
        if found == entry_id:
            return records[mid]
        if found < entry_id:
            lo = mid + 1
        else:
            hi = mid
    return None


def synth_samples(text: str) -> list[int]:
    seed = zlib.crc32(text.encode("utf-8")) & 0xFFFFFFFF
    step = 400 + (seed % 800)
    samples = []
    acc = 0
    for _ in range(160):
        acc = (acc + step) % 2000
        samples.append(acc - 1000)
    return samples


def _pack_clips(clips: list[tuple[int, list[int]]]) -> tuple[bytes, list[tuple[int, int, int]]]:
    pak = bytearray()
    records = []
    for entry_id, samples in clips:
        blob = encode_clip(samples)
        records.append((entry_id, len(pak), len(blob)))
        pak += blob
    return bytes(pak), records


def _piper_clips(
    phrases: list[tuple[int, str]], model: Path, piper_bin: str
) -> list[tuple[int, list[int]]]:
    if not phrases:
        return []
    with tempfile.TemporaryDirectory(prefix="rmx-piper-") as tmp:
        folder = Path(tmp)
        lines = "\n".join(text for _entry_id, text in phrases) + "\n"
        (folder / "lines.txt").write_text(lines, encoding="utf-8")
        out_dir = folder / "wav"
        out_dir.mkdir()
        completed = subprocess.run(
            [piper_bin, "--model", str(model), "--output_dir", str(out_dir)],
            input=lines.encode("utf-8"),
            check=False,
        )
        clips = []
        if completed.returncode == 0 and (out_dir / "0.wav").is_file():
            for index, (entry_id, _text) in enumerate(phrases):
                clips.append((entry_id, _wav_to_16k(out_dir / f"{index}.wav")))
            return clips
        for index, (entry_id, text) in enumerate(phrases):
            wav_path = folder / f"{index}.wav"
            one = subprocess.run(
                [piper_bin, "--model", str(model), "--output_file", str(wav_path)],
                input=(text + "\n").encode("utf-8"),
                check=False,
            )
            if one.returncode != 0 or not wav_path.is_file():
                raise SystemExit(
                    f"piper failed for entry {entry_id} (exit {one.returncode}); "
                    "install the MIT rhasspy/piper binary and the public-domain "
                    "en_US-ljspeech-medium model"
                )
            clips.append((entry_id, _wav_to_16k(wav_path)))
        return clips


def _melo_clips(phrases: list[tuple[int, str]], voice_id: str) -> list[tuple[int, list[int]]]:
    language, speaker = MELO_LANGUAGE[voice_id]
    worker = r"""
import sys
from melo.api import TTS
lang, speaker = sys.stdin.readline().rstrip("\n").split("\t")
model = TTS(language=lang, device="cpu")
sid = model.hps.data.spk2id[speaker]
for line in sys.stdin:
    line = line.rstrip("\n")
    if not line:
        continue
    path, text = line.split("\t", 1)
    model.tts_to_file(text, sid, path, speed=1.0)
    sys.stdout.write(path + "\n")
    sys.stdout.flush()
"""
    with tempfile.TemporaryDirectory(prefix="rmx-melo-") as tmp:
        folder = Path(tmp)
        proc = subprocess.Popen(
            [sys.executable, "-c", worker],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
        )
        assert proc.stdin and proc.stdout
        proc.stdin.write(f"{language}\t{speaker}\n")
        clips = []
        for index, (entry_id, text) in enumerate(phrases):
            wav_path = folder / f"{index}.wav"
            proc.stdin.write(f"{wav_path}\t{text}\n")
            proc.stdin.flush()
            echoed = proc.stdout.readline().strip()
            if echoed != str(wav_path) or not wav_path.is_file():
                proc.kill()
                raise SystemExit(
                    "MeloTTS failed; install the MIT package from "
                    "https://github.com/myshell-ai/MeloTTS"
                )
            clips.append((entry_id, _wav_to_16k(wav_path)))
        proc.stdin.close()
        proc.wait(timeout=30)
        return clips


def _wav_to_16k(path: Path) -> list[int]:
    with wave.open(str(path), "rb") as handle:
        channels = handle.getnchannels()
        rate = handle.getframerate()
        width = handle.getsampwidth()
        frames = handle.readframes(handle.getnframes())
    if width != 2:
        raise FormatError(f"{path} is not 16-bit PCM")
    count = len(frames) // 2
    samples = list(struct.unpack("<" + "h" * count, frames))
    if channels == 2:
        samples = [(samples[i] + samples[i + 1]) // 2 for i in range(0, len(samples), 2)]
    elif channels != 1:
        raise FormatError(f"{path} has {channels} channels")
    return _resample(samples, rate, SAMPLE_RATE)


def _resample(samples: list[int], src_rate: int, dst_rate: int) -> list[int]:
    if src_rate == dst_rate or not samples:
        return samples
    out_len = max(1, int(len(samples) * dst_rate / src_rate))
    out = []
    last = len(samples) - 1
    for index in range(out_len):
        pos = index * src_rate / dst_rate
        left = int(pos)
        frac = pos - left
        a = samples[min(left, last)]
        b = samples[min(left + 1, last)]
        out.append(int(a + (b - a) * frac))
    return out


def _sidecar(voice: Voice, clips: int, pak_bytes: int, synth: bool) -> str:
    mode = "synth" if synth else voice.engine
    return (
        f"voice={voice.voice_id}\n"
        f"lang={voice.lang}\n"
        f"engine={mode}\n"
        f"license={voice.license}\n"
        f"attribution={voice.attribution}\n"
        f"url={voice.url}\n"
        f"note={voice.note}\n"
        f"clips={clips}\n"
        f"pak_bytes={pak_bytes}\n"
        f"codec=ima-adpcm-16khz-mono\n"
    )


def _field(entry, field_id: int) -> str:
    if hasattr(entry, "fields"):
        pairs = ((field.field_id, field.text) for field in entry.fields)
    else:
        pairs = ((fid, raw.decode("utf-8", "replace")) for fid, raw in entry[3])
    for fid, text in pairs:
        if fid == field_id:
            return text.strip()
    return ""


def _fat_id(dict_id: str) -> bool:
    return bool(dict_id) and len(dict_id) <= 8 and dict_id.isascii() and dict_id.isalnum() and dict_id.isupper()


def _decode_nibble(nibble: int, predictor: int, step_index: int) -> tuple[int, int]:
    nibble &= 0x0F
    step = STEP_TABLE[step_index]
    diff = step >> 3
    if nibble & 4:
        diff += step
    if nibble & 2:
        diff += step >> 1
    if nibble & 1:
        diff += step >> 2
    if nibble & 8:
        predictor -= diff
    else:
        predictor += diff
    predictor = max(-32768, min(32767, predictor))
    step_index = max(0, min(88, step_index + INDEX_TABLE[nibble]))
    return predictor, step_index


def _encode_nibble(sample: int, predictor: int, step_index: int) -> tuple[int, int, int]:
    step = STEP_TABLE[step_index]
    diff = sample - predictor
    nibble = 0
    if diff < 0:
        nibble = 8
        diff = -diff
    if diff >= step:
        nibble |= 4
        diff -= step
    if diff >= step >> 1:
        nibble |= 2
        diff -= step >> 1
    if diff >= step >> 2:
        nibble |= 1
    predictor, step_index = _decode_nibble(nibble, predictor, step_index)
    return nibble, predictor, step_index
