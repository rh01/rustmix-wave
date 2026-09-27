# Board contract

Target board: Waveshare ESP32-S3 e-paper 3.97-inch development board.

## Display

```text
Controller     SSD1677
Native frame   800 × 480 monochrome
Logical UI     480 × 800 portrait
SCLK           GPIO11
MOSI           GPIO12
CS             GPIO10
DC             GPIO9
RST            GPIO46
BUSY           GPIO3
```

## User inputs

```text
Rotary wheel / Select   Primary UI navigation
BOOT                    GPIO0, short contextual action, long hierarchical Back
Power key               AXP2101 PEK short / long interrupts
```

Power-key product behavior:

```text
Short Power press   Open display-maintenance menu
Long Power press    Enter random sleep-image mode
Deep-sleep wake     UP, SELECT, DOWN, or BOOT (GPIO4, GPIO5, GPIO6, GPIO0)
Power key           Cannot wake deep sleep; AXP2101 PEK is I2C-only, not an RTC GPIO
```

## Storage

```text
SD mount       /sdcard
Product root   /sdcard/RUSTMIX
Filesystem     FAT; generated writable names must remain FAT 8.3-safe
```

## Audio and sensors

```text
Audio codec        ES8311, I2C 0x18
I2S MCLK           GPIO13
I2S BCLK           GPIO14
I2S WS             GPIO47
I2S DOUT           GPIO48, ESP32-S3 to ES8311
I2S DIN            GPIO21, ES8311 to ESP32-S3
Amplifier          NS4150B enable GPIO39, high while playing, low when idle
RTC alarm input    GPIO45 active low
Environment        SHTC3
IMU                QMI8658
```

Those audio pins are the constants in Waveshare's ESP-IDF `03_Music` and `08_ESP32-S3_e-Paper-3.97` file `components/es8311_bsp/es8311_bsp.h` (`I2S_MCLK_PIN`, `I2S_BCK_PIN`, `I2S_WS_PIN`, `I2S_DATA_POUT`, `I2S_DATA_PIN`, `I2S_PA_PIN`). The same header uses 16 kHz and MCLK ×384. Shutdown in that example drives the amplifier pin low. This firmware keeps the Rust ES8311 driver already used for Voice Notes; it does not add a second `esp_codec_dev` owner.

Hardware handles remain native-owned in `src/main.rs` and its focused runtime adapters. Lua apps do not receive raw peripheral access.
