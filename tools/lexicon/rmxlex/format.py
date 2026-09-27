"""RMXLEX1 / RMXWLS1 writer and reader.

Integers are little-endian. Strings are length-prefixed UTF-8 with no NUL
terminator. Key pages are fixed 4096-byte blocks; records do not cross pages.
The writer is deterministic: stable key order, no timestamps.
"""

from __future__ import annotations

import struct
import zlib
from dataclasses import dataclass, field

MAGIC_LEX = b"RMXLEX1\x00"
MAGIC_WLS = b"RMXWLS1\x00"
PAGE_SIZE = 4096
HEADER_LEN = 64
MAX_KEY_LEN = 64
MAX_ENTRY_BYTES = 8 * 1024
MAX_FIELDS = 32
MAX_PAGE_INDEX = 256 * 1024

FIELD_HEADWORD = 1
FIELD_READING = 2
FIELD_PHONETIC = 3
FIELD_POS = 4
FIELD_DEF_ZH = 5
FIELD_DEF_EN = 6
FIELD_EXAMPLE = 7
FIELD_FORMS = 8
FIELD_KANJI_INFO = 9
FIELD_NOTE = 10

FLAG_CASE_FOLDED = 1 << 0
FLAG_KANA_KEYS = 1 << 1
FLAG_PINYIN_KEYS = 1 << 2

ENTRY_COMMON = 1 << 0
ENTRY_OXFORD = 1 << 1

TAG_ZK = 1 << 0
TAG_GK = 1 << 1
TAG_CET4 = 1 << 2
TAG_CET6 = 1 << 3
TAG_KY = 1 << 4
TAG_TOEFL = 1 << 5
TAG_IELTS = 1 << 6
TAG_GRE = 1 << 7
TAG_JLPT_N5 = 1 << 8
TAG_JLPT_N4 = 1 << 9
TAG_JLPT_N3 = 1 << 10
TAG_JLPT_N2 = 1 << 11
TAG_JLPT_N1 = 1 << 12

TAG_BY_NAME = {
    "zk": TAG_ZK,
    "gk": TAG_GK,
    "cet4": TAG_CET4,
    "cet6": TAG_CET6,
    "ky": TAG_KY,
    "toefl": TAG_TOEFL,
    "ielts": TAG_IELTS,
    "gre": TAG_GRE,
    "n5": TAG_JLPT_N5,
    "jlptn5": TAG_JLPT_N5,
    "n4": TAG_JLPT_N4,
    "jlptn4": TAG_JLPT_N4,
    "n3": TAG_JLPT_N3,
    "jlptn3": TAG_JLPT_N3,
    "n2": TAG_JLPT_N2,
    "jlptn2": TAG_JLPT_N2,
    "n1": TAG_JLPT_N1,
    "jlptn1": TAG_JLPT_N1,
}


class FormatError(ValueError):
    """Binary dictionary or word list failed validation."""


@dataclass
class Field:
    field_id: int
    text: str


@dataclass
class Entry:
    tags: int = 0
    freq_rank: int = 0
    flags: int = 0
    fields: list[Field] = field(default_factory=list)


@dataclass
class Lexicon:
    entries: list[Entry]
    keys: list[tuple[bytes, int]]
    src_lang: str
    dst_lang: str
    header_flags: int
    warnings: list[str] = field(default_factory=list)


@dataclass
class WordList:
    dict_id: str
    title: str
    entry_ids: list[int]
    name: str = ""


def lang4(code: str) -> bytes:
    raw = code.encode("ascii", errors="strict")[:4]
    return raw + b"\x00" * (4 - len(raw))


def _utf8_limit(text: str, limit: int) -> bytes:
    raw = text.encode("utf-8")
    if len(raw) <= limit:
        return raw
    trimmed = raw[:limit]
    while trimmed and (trimmed[-1] & 0xC0) == 0x80:
        trimmed = trimmed[:-1]
    return trimmed


def encode_entry(entry: Entry, warnings: list[str]) -> bytes:
    fields = list(entry.fields)
    if len(fields) > MAX_FIELDS:
        warnings.append(f"truncated fields {len(fields)} -> {MAX_FIELDS}")
        fields = fields[:MAX_FIELDS]
    encoded: list[tuple[int, bytes]] = []
    for item in fields:
        raw = item.text.encode("utf-8")
        if len(raw) > 65535:
            warnings.append("truncated field longer than 65535 bytes")
            raw = _utf8_limit(item.text, 65535)
        encoded.append((item.field_id & 0xFF, raw))

    def size_of(items: list[tuple[int, bytes]]) -> int:
        return 10 + sum(3 + len(raw) for _fid, raw in items)

    while encoded and size_of(encoded) > MAX_ENTRY_BYTES:
        warnings.append("truncated entry to 8 KiB")
        encoded.pop()
    if size_of(encoded) > MAX_ENTRY_BYTES:
        raise FormatError("entry exceeds 8 KiB even with no fields")

    buf = bytearray()
    buf += struct.pack("<IIBB", entry.tags & 0xFFFFFFFF, entry.freq_rank & 0xFFFFFFFF, entry.flags & 0xFF, len(encoded))
    for fid, raw in encoded:
        buf.append(fid)
        buf += struct.pack("<H", len(raw))
        buf += raw
    return bytes(buf)


def _pack_key_pages(keys: list[tuple[bytes, int]]) -> list[bytes]:
    pages: list[bytes] = []
    current: list[tuple[bytes, int]] = []
    used = 2
    for key, entry_id in keys:
        rec = 1 + len(key) + 4
        if rec > PAGE_SIZE - 2:
            raise FormatError("key record does not fit in a page")
        if used + rec > PAGE_SIZE:
            pages.append(_encode_page(current))
            current = []
            used = 2
        current.append((key, entry_id))
        used += rec
    if current:
        pages.append(_encode_page(current))
    return pages


def _encode_page(records: list[tuple[bytes, int]]) -> bytes:
    buf = bytearray(PAGE_SIZE)
    struct.pack_into("<H", buf, 0, len(records))
    offset = 2
    for key, entry_id in records:
        if offset + 1 + len(key) + 4 > PAGE_SIZE:
            raise FormatError("key record crosses a page")
        buf[offset] = len(key)
        offset += 1
        buf[offset : offset + len(key)] = key
        offset += len(key)
        struct.pack_into("<I", buf, offset, entry_id & 0xFFFFFFFF)
        offset += 4
    return bytes(buf)


def _align(value: int, boundary: int) -> int:
    return (value + boundary - 1) // boundary * boundary


def build_lexicon(lexicon: Lexicon) -> bytes:
    warnings = lexicon.warnings
    entry_blobs = [encode_entry(entry, warnings) for entry in lexicon.entries]
    keys: list[tuple[bytes, int]] = []
    for key, entry_id in lexicon.keys:
        if entry_id < 0 or entry_id >= len(lexicon.entries):
            raise FormatError(f"key entry_id {entry_id} out of range")
        raw = key if isinstance(key, bytes) else key.encode("utf-8")
        if len(raw) > MAX_KEY_LEN:
            warnings.append(f"truncated key longer than {MAX_KEY_LEN} bytes")
            raw = _utf8_limit(raw.decode("utf-8", errors="ignore"), MAX_KEY_LEN)
        if not raw:
            warnings.append("dropped empty key")
            continue
        keys.append((raw, entry_id))
    keys.sort(key=lambda item: (item[0], item[1]))
    deduped: list[tuple[bytes, int]] = []
    for item in keys:
        if deduped and deduped[-1] == item:
            continue
        deduped.append(item)
    keys = deduped

    pages = _pack_key_pages(keys)
    page_count = len(pages)
    first_keys = []
    for page in pages:
        count = struct.unpack_from("<H", page, 0)[0]
        if count == 0:
            raise FormatError("empty key page")
        key_len = page[2]
        first_keys.append(page[3 : 3 + key_len])
    pidx_body = _encode_page_index(first_keys, 0)
    pidx_off = HEADER_LEN
    pidx_len = len(pidx_body)
    if pidx_len > MAX_PAGE_INDEX:
        raise FormatError("page index exceeds 256 KiB")
    keys_off = _align(pidx_off + pidx_len, PAGE_SIZE)
    pidx_body = _encode_page_index(first_keys, keys_off)
    if len(pidx_body) != pidx_len:
        raise FormatError("page index length changed after offset fill")

    offsets: list[int] = []
    cursor = 0
    entry_blob = bytearray()
    for blob in entry_blobs:
        offsets.append(cursor)
        entry_blob += blob
        cursor += len(blob)

    etab_off = keys_off + page_count * PAGE_SIZE
    ent_off = etab_off + len(lexicon.entries) * 4
    ent_len = len(entry_blob)
    total = ent_off + ent_len
    buf = bytearray(total)
    buf[pidx_off : pidx_off + pidx_len] = pidx_body
    for index, page in enumerate(pages):
        start = keys_off + index * PAGE_SIZE
        buf[start : start + PAGE_SIZE] = page
    for index, rel in enumerate(offsets):
        struct.pack_into("<I", buf, etab_off + index * 4, rel)
    buf[ent_off : ent_off + ent_len] = entry_blob

    buf[0:8] = MAGIC_LEX
    struct.pack_into("<H", buf, 8, 1)
    struct.pack_into("<H", buf, 10, lexicon.header_flags & 0xFFFF)
    struct.pack_into("<I", buf, 12, len(lexicon.entries))
    struct.pack_into("<I", buf, 16, len(keys))
    struct.pack_into("<I", buf, 20, PAGE_SIZE)
    struct.pack_into("<I", buf, 24, page_count)
    struct.pack_into("<I", buf, 28, pidx_off)
    struct.pack_into("<I", buf, 32, pidx_len)
    struct.pack_into("<I", buf, 36, keys_off)
    struct.pack_into("<I", buf, 40, etab_off)
    struct.pack_into("<I", buf, 44, ent_off)
    struct.pack_into("<I", buf, 48, ent_len)
    buf[52:56] = lang4(lexicon.src_lang)
    buf[56:60] = lang4(lexicon.dst_lang)
    struct.pack_into("<I", buf, 60, zlib.crc32(buf[:60]) & 0xFFFFFFFF)
    return bytes(buf)


def _encode_page_index(first_keys: list[bytes], keys_off: int) -> bytes:
    out = bytearray()
    for index, key in enumerate(first_keys):
        if len(key) > MAX_KEY_LEN:
            raise FormatError("page index key longer than 64 bytes")
        out.append(len(key))
        out += key
        out += struct.pack("<I", keys_off + index * PAGE_SIZE)
    return bytes(out)


def build_wordlist(wordlist: WordList) -> bytes:
    dict_id = wordlist.dict_id.encode("utf-8")
    title = wordlist.title.encode("utf-8")
    if not dict_id or len(dict_id) > 255:
        raise FormatError("dict id length out of range")
    if len(title) > 255:
        raise FormatError("title longer than 255 bytes")
    buf = bytearray()
    buf += MAGIC_WLS
    buf += struct.pack("<HH", 1, 0)
    buf.append(len(dict_id))
    buf += dict_id
    buf.append(len(title))
    buf += title
    buf += struct.pack("<I", len(wordlist.entry_ids))
    for entry_id in wordlist.entry_ids:
        buf += struct.pack("<I", entry_id & 0xFFFFFFFF)
    buf += struct.pack("<I", zlib.crc32(buf) & 0xFFFFFFFF)
    return bytes(buf)


def write_meta(fields: dict[str, str]) -> str:
    lines = [f"{key}={value}" for key, value in fields.items()]
    text = "\n".join(lines) + "\n"
    if len(text.encode("utf-8")) > 4096:
        raise FormatError("META.TXT exceeds 4 KiB")
    return text


@dataclass
class ParsedLexicon:
    entry_count: int
    key_count: int
    page_count: int
    src_lang: str
    dst_lang: str
    header_flags: int
    keys: list[tuple[bytes, int]]
    entries: list[tuple[int, int, int, list[tuple[int, bytes]]]]


def parse_lexicon(data: bytes) -> ParsedLexicon:
    if len(data) < HEADER_LEN:
        raise FormatError("truncated header")
    if data[:8] != MAGIC_LEX:
        raise FormatError("bad magic")
    version = struct.unpack_from("<H", data, 8)[0]
    if version != 1:
        raise FormatError("unsupported version")
    expected = zlib.crc32(data[:60]) & 0xFFFFFFFF
    actual = struct.unpack_from("<I", data, 60)[0]
    if expected != actual:
        raise FormatError("bad header crc")
    flags, entry_count, key_count, page_size, page_count = struct.unpack_from("<HIIII", data, 10)
    pidx_off, pidx_len, keys_off, etab_off, ent_off, ent_len = struct.unpack_from("<IIIIII", data, 28)
    if page_size != PAGE_SIZE:
        raise FormatError("unexpected page size")
    if pidx_len > MAX_PAGE_INDEX:
        raise FormatError("page index exceeds 256 KiB")
    _need(data, pidx_off, pidx_len)
    _need(data, keys_off, page_count * PAGE_SIZE)
    _need(data, etab_off, entry_count * 4)
    _need(data, ent_off, ent_len)
    src = data[52:56].split(b"\x00", 1)[0].decode("ascii", errors="replace")
    dst = data[56:60].split(b"\x00", 1)[0].decode("ascii", errors="replace")

    pages_meta = _parse_page_index(data[pidx_off : pidx_off + pidx_len], page_count, keys_off)
    keys: list[tuple[bytes, int]] = []
    for page_offset, first_key in pages_meta:
        page = data[page_offset : page_offset + PAGE_SIZE]
        records = _parse_key_page(page)
        if not records or records[0][0] != first_key:
            raise FormatError("page index first key mismatch")
        keys.extend(records)
    if len(keys) != key_count:
        raise FormatError("key count mismatch")
    previous: bytes | None = None
    for key, _entry_id in keys:
        if previous is not None and key < previous:
            raise FormatError("keys are not ordered")
        previous = key

    offsets = list(struct.unpack_from("<" + "I" * entry_count, data, etab_off)) if entry_count else []
    entries = []
    for index, rel in enumerate(offsets):
        end = offsets[index + 1] if index + 1 < len(offsets) else ent_len
        if rel > end or end > ent_len:
            raise FormatError("entry offset out of range")
        blob = data[ent_off + rel : ent_off + end]
        if len(blob) > MAX_ENTRY_BYTES:
            raise FormatError("entry exceeds 8 KiB")
        entries.append(_parse_entry(blob))
    if len(entries) != entry_count:
        raise FormatError("entry count mismatch")
    return ParsedLexicon(entry_count, key_count, page_count, src, dst, flags, keys, entries)


def _need(data: bytes, offset: int, length: int) -> None:
    if offset < 0 or length < 0 or offset + length > len(data):
        raise FormatError("offset out of range")


def _parse_page_index(blob: bytes, page_count: int, keys_off: int) -> list[tuple[int, bytes]]:
    pages = []
    offset = 0
    for index in range(page_count):
        if offset >= len(blob):
            raise FormatError("truncated page index")
        key_len = blob[offset]
        offset += 1
        if key_len == 0 or key_len > MAX_KEY_LEN or offset + key_len + 4 > len(blob):
            raise FormatError("bad page index key")
        key = blob[offset : offset + key_len]
        offset += key_len
        page_offset = struct.unpack_from("<I", blob, offset)[0]
        offset += 4
        if page_offset != keys_off + index * PAGE_SIZE:
            raise FormatError("page offset mismatch")
        pages.append((page_offset, key))
    if offset != len(blob):
        raise FormatError("page index trailing bytes")
    return pages


def _parse_key_page(page: bytes) -> list[tuple[bytes, int]]:
    if len(page) != PAGE_SIZE:
        raise FormatError("short key page")
    count = struct.unpack_from("<H", page, 0)[0]
    offset = 2
    records = []
    for _ in range(count):
        if offset >= PAGE_SIZE:
            raise FormatError("record crosses page")
        key_len = page[offset]
        offset += 1
        if key_len == 0 or key_len > MAX_KEY_LEN:
            raise FormatError("key_len out of range")
        rec_end = offset + key_len + 4
        if rec_end > PAGE_SIZE:
            raise FormatError("record crosses page")
        key = bytes(page[offset : offset + key_len])
        offset += key_len
        entry_id = struct.unpack_from("<I", page, offset)[0]
        offset += 4
        records.append((key, entry_id))
    return records


def _parse_entry(blob: bytes) -> tuple[int, int, int, list[tuple[int, bytes]]]:
    if len(blob) < 10:
        raise FormatError("truncated entry")
    tags, freq, flags, field_count = struct.unpack_from("<IIBB", blob, 0)
    if field_count > MAX_FIELDS:
        raise FormatError("too many fields")
    offset = 10
    fields = []
    for _ in range(field_count):
        if offset + 3 > len(blob):
            raise FormatError("truncated field")
        fid = blob[offset]
        length = struct.unpack_from("<H", blob, offset + 1)[0]
        offset += 3
        if offset + length > len(blob):
            raise FormatError("field exceeds entry")
        fields.append((fid, bytes(blob[offset : offset + length])))
        offset += length
    if offset != len(blob):
        raise FormatError("entry trailing bytes")
    return tags, freq, flags, fields


def parse_wordlist(data: bytes) -> WordList:
    if len(data) < 8 + 2 + 2 + 1 + 1 + 4 + 4:
        raise FormatError("truncated word list")
    if data[:8] != MAGIC_WLS:
        raise FormatError("bad word list magic")
    version, _reserved = struct.unpack_from("<HH", data, 8)
    if version != 1:
        raise FormatError("unsupported word list version")
    offset = 12
    dict_len = data[offset]
    offset += 1
    if offset + dict_len > len(data) - 4:
        raise FormatError("bad dict id")
    dict_id = data[offset : offset + dict_len].decode("utf-8")
    offset += dict_len
    title_len = data[offset]
    offset += 1
    if offset + title_len > len(data) - 4:
        raise FormatError("bad title")
    title = data[offset : offset + title_len].decode("utf-8")
    offset += title_len
    count = struct.unpack_from("<I", data, offset)[0]
    offset += 4
    end = offset + count * 4
    if end + 4 != len(data):
        raise FormatError("word list length mismatch")
    ids = list(struct.unpack_from("<" + "I" * count, data, offset)) if count else []
    expected = zlib.crc32(data[:end]) & 0xFFFFFFFF
    actual = struct.unpack_from("<I", data, end)[0]
    if expected != actual:
        raise FormatError("bad word list crc")
    return WordList(dict_id, title, ids)


def lookup_exact(parsed: ParsedLexicon, key: bytes) -> list[int]:
    return [entry_id for item, entry_id in parsed.keys if item == key]


def lookup_prefix(parsed: ParsedLexicon, prefix: bytes, limit: int) -> list[tuple[bytes, int]]:
    found = []
    for key, entry_id in parsed.keys:
        if key.startswith(prefix):
            found.append((key, entry_id))
            if len(found) >= limit:
                break
        elif key > prefix and found:
            break
    return found
