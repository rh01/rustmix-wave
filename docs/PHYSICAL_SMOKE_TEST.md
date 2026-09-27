# Consolidated physical smoke test

For screen names, navigation controls, and reference images, see [`USER_GUIDE.md`](USER_GUIDE.md).

Run this checklist after a release build or any cross-cutting runtime change.

## Build and boot

1. Run `./scripts/validate.sh`.
2. Run `cargo +esp build --release`.
3. Flash with `./scripts/flash.sh monitor`.
4. Confirm boot reaches the Home screen without panic or reset loops.
5. Confirm the displayed version is `1.0.0` and the repository-cleanup readiness marker appears.

## Power key and display refresh

1. Press Power briefly and confirm the display-maintenance menu opens.
2. Select `Clear ghosting now` and confirm a clean global refresh returns to the underlying screen.
3. Press Power briefly, select Cancel, and confirm no sleep transition.
4. Hold Power and confirm random sleep-image mode starts and network services suspend.
5. Wait for the wake quiet guard and press Power to restore the prior route.

## Reader

1. Open one TXT book and one EPUB or `.EPU` book.
2. Confirm staged loading, page navigation, Reader Options, preferences, TOC behavior, and bookmark add/remove.
3. In Reading Preferences confirm font size steps 16/20/24/32/48/72 and CJK Unifont / SD CJK faces.
4. Open a Chinese TXT or EPUB if available and confirm glyphs are not `?`.
5. Reboot and confirm Continue Reading restores the prior book and page.
6. Confirm `/RUSTMIX/READER/POSITS.TXT` and `CACHE/<8HEX>.CCH` exist.

## Dictionary

1. Open `Tools > Dictionary`.
2. Confirm `CAB`, `BARN`, and `CALENDAR` exact lookup.
3. Confirm `AAR*` prefix lookup and result cycling.
4. Press BOOT briefly and confirm `NAV H` / `NAV V` switches without moving the selected key.
5. Hold BOOT and confirm hierarchical Back.

## Lexicon and vocabulary

These checks need a release build on the device. Host tests do not flash firmware or measure the app partition.

1. Run `cargo +esp build --release` and compare the app ELF size with the app partition. If headroom is tight, do not rebuild Unifont with `--with-jis0208`.
2. Copy a generated `/RUSTMIX/LEXICON` tree to the card.
3. Open `Tools > Lexicon`, search an English headword, and confirm the result appears in under 300 ms.
4. Open `SRC` and confirm ECDICT, JMdict, KANJIDIC2, and CC-CEDICT credits are visible.
5. Open `Productivity > Vocabulary` with the RTC set, flip one card, rate it Good, and power off. Confirm `PROGRESS.BIN` still contains the review after boot.
6. Clear or unset the RTC and confirm the trainer shows `时钟未设置` instead of scheduling.
7. Flip several cards and confirm ghosting stays within the normal partial-refresh policy.
8. With `AUDIO.IDX` installed, open an entry and press BOOT briefly. Confirm the word plays and the amplifier is quiet again after the clip. Confirm a word with no clip shows no audio mark and does not reset the device.
9. Set `auto_pronounce=on` in `/RUSTMIX/VOCAB/SETTINGS.TXT`, show a card that has a clip, and confirm it plays without a button press.
10. On the bench, confirm GPIO13, GPIO14, GPIO47, GPIO48, GPIO21, and GPIO39 still match the board, that volume 60 is intelligible on the 8 ohm speaker, and that an idle amplifier pin stays low with no hiss.

## Calendar

1. Open `Productivity > Calendar`.
2. Confirm U.S. event markers and daily agenda rendering.
3. Create, edit, and delete one personal event.
4. Confirm U.S. holiday rows remain read-only.
5. Confirm agenda summary, pagination, first row, and footer do not overlap.
6. Confirm `EVENTS.TMP` is absent after successful write and `EVENTS.BAK` is retained.

## Voice Notes

1. Record a note, pause, resume, and save.
2. Confirm a new `VOICE###.WAV` file persists after reboot.
3. Confirm gain selection persists, metadata is readable, playback works, and delete confirmation works.
4. Confirm LAN export displays a path and protected sidecars are not exposed.

## Network, alarms, and settings

1. Confirm Wi-Fi connection and SNTP status, or SoftAP setup when `WIFI.TXT` is missing.
2. From a phone, join `Rustmix-Setup`, open `http://192.168.4.1`, scan SSIDs, save, and confirm STA join plus `WIFI.TXT` write-back.
3. Start the explicit Wi-Fi transfer portal after STA join, access it with the displayed code, then stop it.
4. Confirm Settings → Network → Configure Wi-Fi can reopen SoftAP while keeping `WIFI.TXT` as a manual path.
5. Leave SoftAP idle (and, separately, keep the phone page open) and confirm both the 10-minute idle timeout and the 10-minute total timeout stop HTTP and the AP radio, then show Settings → Network → Configure Wi-Fi on e-paper.
6. Confirm an invalid `WIFI.TXT` logs `WIFI.TXT is invalid` and does not join the NVS network. Stop setup with no station config and confirm the AP radio is off. Confirm Start Wi-Fi Transfer does not treat `192.168.4.1` as connected.
7. Confirm an alarm can sound, snooze, and dismiss.
8. Confirm alarm behavior is not hidden by the Power-key display menu.
9. Confirm Display settings persist after reboot.

## Games and sensors

1. Open Sudoku and verify rotary movement, BOOT-short axis toggle, edit, and commit.
2. Open one motion game and verify debounced IMU movement.
3. Open Environment and Motion diagnostic screens.
4. Run the audio test chime.

## Text-editor layout alignment

1. Open Voice Notes, select a saved WAV, and choose **Edit friendly title**.
2. Confirm the header reads **VOICE NOTE TITLE / EDIT FRIENDLY TITLE**.
3. Confirm the shared grid keyboard is visible and defaults to `NAV H`.
4. Press BOOT briefly and confirm `NAV V` appears without moving the selected key.
5. Use `SAVE` to persist a friendly title and confirm the internal `VOICE###.WAV` filename remains unchanged.
6. Reopen the title editor, hold BOOT, and confirm the edit is cancelled without saving.
7. Open Calendar, create or edit a personal event, and confirm the status strip shows a compact `YYYY-MM-DD` date plus `NAV H` or `NAV V` without overlap.
8. Confirm the Calendar editor footer is fully visible.
