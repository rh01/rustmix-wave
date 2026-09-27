# Known issues and deferred work

## Weather provider reliability

Open-Meteo requests can fail transiently with transport, TLS, timeout, or HTTP service errors. The device already applies bounded retries, delayed backoff, and last-known-good in-memory retention. A cold boot with no successful fetch may still end in a readable `Weather unavailable` state.

## MCU deep sleep

Auto sleep and the Power-key sleep image now stop Wi-Fi, power down the panel rail, and enter ESP32-S3 deep sleep. Wake is a button on GPIO0, GPIO4, GPIO5, or GPIO6 (`ext1`, active low). The sleep image prints those keys. The AXP2101 Power key is an I2C status bit and is not wired to an RTC GPIO, so it cannot wake the SoC. GPIO45 is not an RTC IO, so a programmed alarm uses a timer wake; deep sleep is refused when an alarm is pending and the RTC reading is missing. PSRAM contents do not survive deep sleep; the open TXT/EPUB offset and WeRead chapter are written to the SD card first, and the route is stored in NVS plus RTC noinit memory. Light sleep between page turns retains RAM and PSRAM, and the same buttons wake it through `gpio_wakeup_enable`. The panel rail stays powered during reading and sleeps after 60 seconds idle. Current figures in the power notes are estimates until they are measured on a board.

## EPUB scope

Reader supports bounded reflowable text extraction, TOC navigation, bookmarks, and resume. CSS layout, images, hyperlinks, footnotes, fixed-layout EPUB, DRM, ZIP64, and SD-backed EPUB anchor caches remain deferred.

## Calendar scope

Calendar personal events and U.S. holidays are active. U.S. holiday rows remain read-only. Calendar reminders do not automatically create RTC alarms. Non-U.S. calendar packs are intentionally excluded from the native Calendar route.

## Dictionary scope

Dictionary exact and prefix lookup is active through the complete X4 pack. Saved words, search history, and Reader word-selection lookup remain deferred.

## WeRead

WeRead chapter text uses the documented web reader endpoints. Those responses are obfuscated, not a DRM envelope, but the signing inputs and cookie lifetime can change on the server without notice. Host tests cover the published vectors; a live QR scan, TLS session, and chapter download still need the device. Offline download uses one long-lived worker with a PSRAM stack. That worker does not touch the SD card; the main task writes each 8 KiB chunk, because FATFS is not safe on an external-memory stack. An SD write failure counts toward the chapter retry limit and is shown as "SD card full" or another write error. Skipped chapter indexes are stored in `SKIP.TXT`. A response that ends before its Content-Length, or a chunked response that never sends the terminal chunk, is a failed chapter and is not stored. Cancelling closes the HTTP client instead of waiting out the request timeout. `WereadUi::needs_radio` stays true while chapters are still being saved, including the gap between them, and is cleared when the download is left, cancelled, or finished. The power policy keeps the station up, the SD host at 10 MHz, and auto deep sleep blocked only for that interval. The worker logs its own stack high-water mark. The chapter is decoded when it is opened, not while the following chapters download. A chapter that still fails is retried, then skipped, and the download continues. A ~2 MiB SD TTF is parsed into PSRAM outlines at boot. Download peak use is the worker stack plus a few chunks, and the TLS I/O buffers stay in internal RAM. The `sd-font-loaded` log line records free PSRAM and the largest free block after that face is resident.

Phone-number one-time-password login is not implemented. SELECT cancels an in-flight WeRead request; the HTTPS read itself still ends within the 20 second timeout. Renewal does not extend `wr_skey` unless that cookie value actually changes. Covers and inline images accept JPEG and PNG only, and only from WeRead or Tencent image hosts. Covers stay off until the shelf row is toggled. A downloaded chapter is stored raw until it is opened, then rewritten as plain text. Inline bitmaps are not stored. Progress upload sends a character offset derived from the local page, and it needs the QR session. The official API key cannot fetch chapter text or update progress. Chapter fetches require an SNTP-synced clock because the RTC is stored as UTC+8 wall time. The official skill payload uses version 1.0.4.

## Merged factory-image release artifact

The supported release artifact is the ESP-IDF ELF flashed through `espflash flash`.
Raw-address flashing with `espflash write-bin` is intentionally unsupported. A
merged factory image remains deferred until the bootloader, partition-table, and
application offsets have been validated on physical hardware.
