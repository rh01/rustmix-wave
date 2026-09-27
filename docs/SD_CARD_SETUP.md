# SD-card setup

Use a FAT-formatted SD card. Rustmix Wave mounts it at `/sdcard` and expects the following product tree:

```text
/RUSTMIX/
  WIFI.TXT
  WEATHER.TXT
  ALARMS.TXT
  DISPLAY.TXT
  BOOKS/
  READER/
    CACHE/
  VOICE/
  SLEEP/
    *.BMP
  APPS/
    HGRID/
    SUDOKU/
    MINES/
    TILTMAZE/
    M2048/
    SOKOBAN/
    DICT/
      INDEX.TXT
      DATA/*.JSN
    CALENDAR/
      EVENTS.TXT
      US2026.TXT
  LEXICON/
    ECDICT/
      DICT.LEX
      META.TXT
    LISTS/
      *.WLS
  VOCAB/
    PROGRESS.BIN
    SETTINGS.TXT
```

## Install bundled examples

```bash
./scripts/install-sd-examples.sh /Volumes/YOUR_SD_CARD
```

Existing paths are preserved by default. Use `--force` only when deliberately replacing bundled example files:

```bash
./scripts/install-sd-examples.sh --force /Volumes/YOUR_SD_CARD
```

The generic installer preserves an existing Dictionary and Calendar tree. Use the dedicated installers for intentional complete-pack replacement.

## Wi-Fi

Copy or edit `/RUSTMIX/WIFI.TXT`:

```text
ssid=YOUR_NETWORK
password=YOUR_PASSWORD
timezone=America/New_York
ntp_server=pool.ntp.org
```

Do not commit real credentials.

## Weather

Optional `/RUSTMIX/WEATHER.TXT` example:

```text
provider=open-meteo
location=New York, NY
latitude=40.7128
longitude=-74.0060
timezone=America/New_York
refresh_minutes=30
```

## Alarms

Optional `/RUSTMIX/ALARMS.TXT` example:

```text
snooze_minutes=10
alarm=Workday,07:30,weekdays,on,recurring
alarm=Weekend,09:00,weekends,off,recurring
alarm=Appointment,16:45,2026-06-10,on,once
```

Calendar personal events remain separate from alarms.

## Display preferences

`/RUSTMIX/DISPLAY.TXT` supports:

```text
font_family=inter|atkinson-hyperlegible
font_size=compact|standard|large
```

## Sleep images

Files below `/RUSTMIX/SLEEP` must be uncompressed monochrome Windows BMP files:

```text
800 × 480
1-bpp
```

Install bundled samples:

```bash
./scripts/install-sleep-images.sh /Volumes/YOUR_SD_CARD
```

## Reader books and state

Copy TXT, EPUB, or FAT-friendly `.EPU` books into:

```text
/RUSTMIX/BOOKS
```

The device creates Reader state automatically:

```text
/RUSTMIX/READER/STATE.TXT
/RUSTMIX/READER/POSITS.TXT
/RUSTMIX/READER/RECENT.TXT
/RUSTMIX/READER/MARKS.TXT
/RUSTMIX/READER/PREFS.TXT
/RUSTMIX/READER/CACHE/<8HEX>.CCH
```

Reader writes use `.TMP` and `.BAK` siblings for recovery.

## CJK fonts

Chinese Reader pages, titles, and filenames need glyphs the built-in Latin strikes do not contain. Install open-licensed `.ttf` or `.otf` files (Noto Sans SC or Source Han Sans SC subsets recommended) in either:

```text
/fonts/*.ttf
/fonts/*.otf
/RUSTMIX/FONTS/*.ttf
/RUSTMIX/FONTS/*.otf
```

Keep each file at or below 2 MiB so it fits in PSRAM. A GB2312 or SC subset is enough for most books; full Noto Sans SC Regular is too large for the 8 MB PSRAM budget.

Without an SD face, firmware still renders GB2312 Chinese from the embedded GNU Unifont subset (blocky at large sizes, but usable).

Example, after shrinking a local OFL font:

```bash
mkdir -p /Volumes/YOUR_SD_CARD/fonts
cp NotoSansSC-subset.otf /Volumes/YOUR_SD_CARD/fonts/NOTOSC.OTF
```

The Wi-Fi transfer portal can also drop files into `/RUSTMIX/FONTS`.

## Voice Notes

The device creates:

```text
/RUSTMIX/VOICE/VOICE###.WAV
/RUSTMIX/VOICE/INDEX.TXT
/RUSTMIX/VOICE/META.TXT
/RUSTMIX/VOICE/SETTINGS.TXT
```

Do not hand-edit sidecars while the device is active.

## Complete Dictionary pack

Install from a local `rustmix-x4-firmware` checkout:

```bash
./scripts/install-dictionary-x4-pack.sh \
  --force \
  --x4-repo /Users/piyushdaiya/Documents/projects/rustmix-x4-firmware \
  /Volumes/YOUR_SD_CARD
```

Verify representative lookups:

```bash
./scripts/verify-dictionary-x4-pack.sh /Volumes/YOUR_SD_CARD
```

## Lexicon and vocabulary packs

Build the SD tree on a computer. The script downloads ECDICT, JMdict, KANJIDIC2, CC-CEDICT, and the JLPT CSV lists into a cache, then writes `RUSTMIX/LEXICON` and `RUSTMIX/LEXICON/LISTS`. It is not part of CI. Raw dumps stay in the cache and must not be committed.

```bash
./scripts/build-lexicon-pack.sh /path/to/output
```

Equivalent steps, with an explicit UTC source date:

```bash
mkdir -p build/lexicon-cache build/lexicon-sd
# Place or download ecdict.csv, JMdict_e.gz, kanjidic2.xml.gz, cedict.txt.gz,
# and build/lexicon-cache/jlpt/n1.csv through n5.csv. See scripts/build-lexicon-pack.sh
# for the source URLs.
export PYTHONDONTWRITEBYTECODE=1
python3 -B tools/lexicon/build_lexicon.py ecdict \
  --in build/lexicon-cache/ecdict.csv --out build/lexicon-sd --source-date "$(date -u +%F)"
python3 -B tools/lexicon/build_lexicon.py jmdict \
  --in build/lexicon-cache/JMdict_e.gz --out build/lexicon-sd --source-date "$(date -u +%F)"
python3 -B tools/lexicon/build_lexicon.py cedict \
  --in build/lexicon-cache/cedict.txt.gz --out build/lexicon-sd --source-date "$(date -u +%F)"
python3 -B tools/lexicon/build_lexicon.py kanjidic \
  --in build/lexicon-cache/kanjidic2.xml.gz --out build/lexicon-sd --source-date "$(date -u +%F)"
python3 -B tools/lexicon/build_lexicon.py jlpt \
  --in-dir build/lexicon-cache/jlpt \
  --jmdict build/lexicon-sd/RUSTMIX/LEXICON/JMDICT/DICT.LEX \
  --out build/lexicon-sd
python3 -B tools/lexicon/build_lexicon.py verify \
  build/lexicon-sd/RUSTMIX/LEXICON/ECDICT/DICT.LEX
```

Copy `build/lexicon-sd/RUSTMIX/LEXICON` to `/RUSTMIX/LEXICON` on the card, or upload that directory with the Wi-Fi transfer portal. Create `/RUSTMIX/VOCAB` on the device by opening Vocabulary; the trainer writes `PROGRESS.BIN` through `PROGRESS.TMP`. Licenses and attribution are in each `META.TXT` and in `tools/lexicon/CREDITS.txt`. The Lexicon sources screen shows the same credits on device. ECDICT is MIT. JMdict, KANJIDIC2, and CC-CEDICT are CC BY-SA 4.0 and require that attribution. JLPT vocabulary lists: Jonathan Waller, tanos.co.uk (Creative Commons BY, https://www.tanos.co.uk/jlpt/sharing/); CSV packaging: jamsinclair/open-anki-jlpt-decks (MIT).

Optional IPA glyphs and Japanese kana are not in the committed Unifont bitmap. See `docs/licenses/FONT_NOTICES.md`.

## U.S.-only Calendar pack

Install from a local X4 checkout:

```bash
./scripts/install-calendar-x4-pack.sh \
  --force \
  --x4-repo /Users/piyushdaiya/Documents/projects/rustmix-x4-firmware \
  /Volumes/YOUR_SD_CARD
```

The installer includes `EVENTS.TXT` and `US2026.TXT`, and explicitly excludes `HINDU26.TXT`.

Calendar personal-event writes use:

```text
EVENTS.TMP -> EVENTS.TXT
EVENTS.BAK retained for rollback
```
