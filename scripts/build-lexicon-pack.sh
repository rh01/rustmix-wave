#!/usr/bin/env bash
# Download permitted dictionary sources and build /RUSTMIX/LEXICON for an SD card.
# Not run in CI. Raw dumps stay in the cache directory and are not committed.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CACHE="${LEXICON_CACHE:-"$ROOT/build/lexicon-cache"}"
OUT="${1:-"$ROOT/build/lexicon-sd"}"
mkdir -p "$CACHE" "$OUT"

download() {
  local url="$1"
  local dest="$2"
  if [[ -f "$dest" ]]; then
    printf 'cache=%s\n' "$dest"
    return
  fi
  curl -L --fail --retry 3 --output "$dest" "$url"
}

download "https://raw.githubusercontent.com/skywind3000/ECDICT/master/ecdict.csv" "$CACHE/ecdict.csv"
download "http://ftp.edrdg.org/pub/Nihongo/JMdict_e.gz" "$CACHE/JMdict_e.gz"
download "http://www.edrdg.org/kanjidic/kanjidic2.xml.gz" "$CACHE/kanjidic2.xml.gz"
download "https://www.mdbg.net/chinese/export/cedict/cedict_1_0_ts_utf-8_mdbg.txt.gz" "$CACHE/cedict.txt.gz"
mkdir -p "$CACHE/jlpt"
for level in 1 2 3 4 5; do
  download "https://raw.githubusercontent.com/jamsinclair/open-anki-jlpt-decks/main/src/n${level}.csv" "$CACHE/jlpt/n${level}.csv"
done

export PYTHONDONTWRITEBYTECODE=1
python3 -B "$ROOT/tools/lexicon/build_lexicon.py" ecdict --in "$CACHE/ecdict.csv" --out "$OUT" --source-date "$(date -u +%F)"
python3 -B "$ROOT/tools/lexicon/build_lexicon.py" jmdict --in "$CACHE/JMdict_e.gz" --out "$OUT" --source-date "$(date -u +%F)"
python3 -B "$ROOT/tools/lexicon/build_lexicon.py" cedict --in "$CACHE/cedict.txt.gz" --out "$OUT" --source-date "$(date -u +%F)"
python3 -B "$ROOT/tools/lexicon/build_lexicon.py" kanjidic --in "$CACHE/kanjidic2.xml.gz" --out "$OUT" --source-date "$(date -u +%F)"
python3 -B "$ROOT/tools/lexicon/build_lexicon.py" jlpt --in-dir "$CACHE/jlpt" --jmdict "$OUT/RUSTMIX/LEXICON/JMDICT/DICT.LEX" --out "$OUT"
python3 -B "$ROOT/tools/lexicon/build_lexicon.py" verify "$OUT/RUSTMIX/LEXICON/ECDICT/DICT.LEX"
printf 'lexicon-pack=%s\n' "$OUT"
