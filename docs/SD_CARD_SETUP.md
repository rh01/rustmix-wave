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
      AUDIO.IDX
      AUDIO.PAK
      AUDIO.TXT
    LISTS/
      *.WLS
  VOCAB/
    PROGRESS.BIN
    SETTINGS.TXT
  WEREAD.TXT
  WEREAD/
    SESS.TXT
    <8HEX>/
      META.TXT
      TOC.TXT
      CHxxxx.TXT
      PROG.TXT
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

Two first-class paths, both kept on the Wi-Fi firmware line (not the BLE build):

1. **SD `/RUSTMIX/WIFI.TXT`** — copy or edit this file, insert the card, and boot. If it exists and the station associates, the device stays in STA mode. If the file exists but is invalid, the device reports `WIFI.TXT is invalid` and does not fall back to NVS.
2. **SoftAP web setup** — when `WIFI.TXT` is missing, station join fails, or you choose **Settings → Network → Configure Wi-Fi**, the device opens an open access point named `Rustmix-Setup`. Join it from a phone and open `http://192.168.4.1`. The page lists nearby SSIDs (refreshable), accepts a password (8–63 characters, or a 64-character hex PSK) or a manual SSID, then saves. The AP stops after 10 minutes idle and after 10 minutes total; e-paper then shows how to open **Settings → Network → Configure Wi-Fi** again.

SoftAP save writes **NVS and `WIFI.TXT`** (when the SD card is mounted) and switches to STA. After join, **Start Wi-Fi Transfer** is unchanged: LAN portal on the home network. Starting transfer tears down SoftAP first and only continues when the station is associated.

Copy or edit `/RUSTMIX/WIFI.TXT`:

```text
ssid=YOUR_NETWORK
password=YOUR_PASSWORD
timezone=America/New_York
ntp_server=pool.ntp.org
```

Do not commit real credentials. `WIFI.TXT` remains valid after a SoftAP save; you can still edit it on a computer as a manual path.

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

## Pronunciation audio

Optional per-dictionary clips live beside `DICT.LEX`:

```text
/RUSTMIX/LEXICON/<ID>/AUDIO.IDX
/RUSTMIX/LEXICON/<ID>/AUDIO.PAK
/RUSTMIX/LEXICON/<ID>/AUDIO.TXT
```

`AUDIO.IDX` is an `RMXAUD1` table sorted by entry id. `AUDIO.PAK` holds `RMXADP1` IMA ADPCM clips, mono, 16 kHz. The firmware binary-searches the index and decodes one short chunk at a time. A missing index, pack, or entry is silent: the card shows no audio mark and does not crash. Do not commit generated audio.

IMA ADPCM is about 8 KB per second. PCM16 at the same rate is 32 KB per second, about four times larger. An MP3 at 32 kbps would be smaller, and the Waveshare C examples decode MP3, but this firmware has no MP3 decoder. Rough SD sizes, including clip headers and the index, for words that average under a second:

```text
5,000 English words at 0.9 s   about 36 MB
JLPT N5, about 800 words       about 6 MB
JLPT N1–N5, about 8,000 words  about 55 MB
5,000 Chinese words at 0.8 s   about 34 MB
```

The word list's `dict_id` must match the lexicon directory (`ECDICT`, `JMDICT`, `CEDICT`). Voices with a non-commercial, research-only, or unknown dataset license are refused. Piper `en_US-lessac-*`, Piper `ja_JP-hi_fi_captain-medium` (CC BY-NC-SA 4.0), and Piper `zh_CN-huayan-medium` (unknown dataset license, Lessac finetune) are not used.

English uses Piper and the public-domain LJ Speech voice, trained from scratch. Download the model and its `.onnx.json` once and keep them outside the repo:

```bash
python3 -B tools/lexicon/build_lexicon.py audio \
  --wordlist build/lexicon-sd/RUSTMIX/LEXICON/LISTS/YOUR.WLS \
  --lexicon build/lexicon-sd/RUSTMIX/LEXICON/ECDICT/DICT.LEX \
  --voice en_US-ljspeech-medium \
  --model "$HOME/voices/en_US-ljspeech-medium.onnx" \
  --out build/lexicon-sd
```

Japanese and Chinese use MeloTTS (MIT). Install that package, then:

```bash
python3 -B tools/lexicon/build_lexicon.py audio \
  --wordlist build/lexicon-sd/RUSTMIX/LEXICON/LISTS/JLPTN5.WLS \
  --lexicon build/lexicon-sd/RUSTMIX/LEXICON/JMDICT/DICT.LEX \
  --voice melo-jp \
  --out build/lexicon-sd

python3 -B tools/lexicon/build_lexicon.py audio \
  --wordlist build/lexicon-sd/RUSTMIX/LEXICON/LISTS/YOUR.WLS \
  --lexicon build/lexicon-sd/RUSTMIX/LEXICON/CEDICT/DICT.LEX \
  --voice melo-zh \
  --out build/lexicon-sd
```

`/RUSTMIX/VOCAB/SETTINGS.TXT` may set `auto_pronounce=on` to play a clip when a vocabulary card or lexicon entry is shown. The default is off. BOOT short on those screens plays the current word when a clip exists. Volume is the existing audio setting. The amplifier stays off while idle.

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
