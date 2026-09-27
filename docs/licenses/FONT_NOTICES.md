# Generated bitmap-font notices

RustMix Wave embeds generated 1-bpp printable-ASCII bitmap atlases derived from:

- Inter Medium and Inter SemiBold
- Atkinson Hyperlegible Regular and Atkinson Hyperlegible Bold

Compact, Standard and Large raster strikes are generated for the physical e-paper panel. The repository contains only generated Rust arrays in `src/app/typography/assets.rs`; raw font files are intentionally not distributed.

The source families are distributed under the SIL Open Font License 1.1. The license text is retained in `docs/licenses/OFL-1.1.txt`.


RustMix Wave v0.16.2 also embeds generated printable-ASCII Reader-page raster strikes derived from DejaVu Serif. The repository contains only generated Rust arrays in `src/app/reader_serif_assets.rs`; the raw DejaVu font file is intentionally not distributed. DejaVu changes are public domain and the base Bitstream Vera permission notice is retained in `docs/licenses/DEJAVU-SERIF-NOTICE.txt`.


RustMix Wave v0.17.2 adds Reader-only generated printable-ASCII raster strikes derived from Atkinson Hyperlegible Next Medium and Literata Medium. The generated Rust arrays live in `src/app/reader_atkinson_next_assets.rs` and `src/app/reader_literata_assets.rs`. The existing persisted Reader preference keys `atkinson-hyperlegible` and `serif` remain unchanged; `literata` is a new explicit key. Raw font files are intentionally not distributed. Both new source families are used under the SIL Open Font License 1.1 retained in `docs/licenses/OFL-1.1.txt`.

RustMix Wave also embeds a GNU Unifont GB2312 16x16 1-bpp subset in `src/fonts/unifont_gb2312.bin` as the CJK fallback when no SD TTF/OTF face is installed. The committed bitmap covers GB2312 plus the punctuation and fullwidth ranges packed by `scripts/generate-unifont-gb2312.py`. It does not include IPA or JIS X 0208. Regenerate with `--with-ipa` (about 14 KiB) and, only after checking app-partition headroom, `--with-jis0208` (about 100 KiB). Hiragana, katakana, and Japanese kanji outside GB2312 need that regeneration or an SD face such as Noto Sans JP at or below 2 MiB. Half-width Unifont cells advance by half the pixel size. GNU Unifont is distributed under the SIL Open Font License 1.1. The license text is retained in `docs/licenses/OFL-1.1.txt`. The firmware does not ship raw `.ttf` / `.otf` files; optional CJK quality faces are installed on the SD card under `/fonts` or `/RUSTMIX/FONTS`.

Recommended SD CJK families (OFL, install a subset under 2 MiB):

- Noto Sans SC
- Source Han Sans SC

