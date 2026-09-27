//! ESP-IDF playback and Voice Notes runtime for the ES8311 codec and bidirectional I2S0 channel.

use anyhow::{anyhow, Result};
use embedded_hal::{delay::DelayNs, i2c::I2c};
use es8311::{ClockConfig, Resolution};
use esp_idf_svc::hal::{
    delay::{TickType, NON_BLOCK},
    gpio::{Output, PinDriver},
    i2s::{I2sBiDir, I2sDriver},
};

use crate::voice_notes::{
    apply_pcm16_gain_in_place, expand_pcm16_mono_to_stereo, VoiceCaptureMetrics, VoiceMicGain,
};

use super::{
    apply_playback_write,
    board_codec::{BoardEs8311, CodecProfileSnapshot},
    i2s_tx_status_from_code,
    tone::{ChimeGenerator, ChimeMode, PCM_CHUNK_BYTES},
    AmpEnableLine, AudioPlaybackState, AudioSnapshot, AudioUiRequest, I2sTxStatus,
    PlaybackWriteError, AUDIO_MCLK_HZ, AUDIO_SAMPLE_RATE_HZ, AUDIO_VOLUME_STEP_PERCENT,
    DEFAULT_AUDIO_VOLUME_PERCENT, ES8311_I2C_ADDRESS_HIGH, ES8311_I2C_ADDRESS_LOW,
    ESP_ERR_TIMEOUT_CODE, I2S_TX_TIMEOUT_MS, MAX_AUDIO_VOLUME_PERCENT,
};

const _: () = assert!(ESP_ERR_TIMEOUT_CODE as i64 == esp_idf_svc::sys::ESP_ERR_TIMEOUT as i64);

impl AmpEnableLine for PinDriver<'_, Output> {
    type Error = esp_idf_svc::sys::EspError;

    fn drive_low(&mut self) -> Result<(), Self::Error> {
        self.set_low()
    }

    fn is_low(&self) -> bool {
        self.is_set_low()
    }
}

/// Codec or mute setup failed. The amplifier driver is returned so the caller
/// can keep GPIO39 driven low instead of dropping the pin.
pub struct AudioInitError<'d> {
    pub amplifier: PinDriver<'d, Output>,
    pub error: anyhow::Error,
}

/// Own the safe-start playback runtime. The amplifier is held low unless PCM
/// audio is actively being streamed.
pub struct AudioRuntime<'d, I2C> {
    bus: I2C,
    codec: BoardEs8311,
    profile: CodecProfileSnapshot,
    tx: I2sDriver<'d, I2sBiDir>,
    amplifier: PinDriver<'d, Output>,
    snapshot: AudioSnapshot,
    chime: ChimeGenerator,
    i2s_enabled: bool,
    codec_powered: bool,
}

impl<'d, I2C> AudioRuntime<'d, I2C>
where
    I2C: I2c,
    I2C::Error: core::fmt::Debug,
{
    pub fn initialize<D>(
        mut bus: I2C,
        tx: I2sDriver<'d, I2sBiDir>,
        mut amplifier: PinDriver<'d, Output>,
        delay: &mut D,
    ) -> Result<Self, AudioInitError<'d>>
    where
        D: DelayNs,
    {
        if let Err(error) = amplifier.set_low() {
            return Err(AudioInitError {
                amplifier,
                error: anyhow!("failed to disable audio amplifier: {error:?}"),
            });
        }
        let clock = ClockConfig {
            mclk_inverted: false,
            sclk_inverted: false,
            mclk_from_mclk_pin: true,
            mclk_frequency: AUDIO_MCLK_HZ,
            sample_frequency: AUDIO_SAMPLE_RATE_HZ,
        };
        let mut last_error = None;
        let mut detected = None;
        for address in [ES8311_I2C_ADDRESS_LOW, ES8311_I2C_ADDRESS_HIGH] {
            let codec = BoardEs8311::new(address);
            match codec.init(
                &mut bus,
                &clock,
                Resolution::Bits16,
                Resolution::Bits16,
                delay,
            ) {
                Ok(profile) => {
                    detected = Some((codec, address, profile));
                    break;
                }
                Err(error) => last_error = Some(format!("{error:?}")),
            }
        }
        let Some((codec, codec_address, profile)) = detected else {
            return Err(AudioInitError {
                amplifier,
                error: anyhow!(
                    "ES8311 probe failed at 0x{ES8311_I2C_ADDRESS_LOW:02X} and 0x{ES8311_I2C_ADDRESS_HIGH:02X}: {}",
                    last_error.unwrap_or_else(|| "unknown codec error".into())
                ),
            });
        };
        if let Err(error) = codec.volume_set(&mut bus, DEFAULT_AUDIO_VOLUME_PERCENT, None) {
            return Err(AudioInitError {
                amplifier,
                error: anyhow!("failed to set ES8311 volume: {error:?}"),
            });
        }
        if let Err(error) = codec.mute(&mut bus, true) {
            return Err(AudioInitError {
                amplifier,
                error: anyhow!("failed to mute ES8311: {error:?}"),
            });
        }

        let mut runtime = Self {
            bus,
            codec,
            profile,
            tx,
            amplifier,
            snapshot: AudioSnapshot {
                available: true,
                codec_address: Some(codec_address),
                codec_ready: true,
                i2s_ready: true,
                amplifier_enabled: false,
                codec_powered: true,
                muted: true,
                volume_percent: DEFAULT_AUDIO_VOLUME_PERCENT,
                playback_state: AudioPlaybackState::Muted,
                error: None,
            },
            chime: ChimeGenerator::default(),
            i2s_enabled: true,
            codec_powered: true,
        };
        if let Err(error) = runtime.sleep_codec() {
            log::warn!("rustmix-wave=audio-codec status=power-down-failed error={error:#}");
        }
        Ok(runtime)
    }

    #[must_use]
    pub fn snapshot(&self) -> AudioSnapshot {
        self.snapshot.clone()
    }

    #[must_use]
    pub const fn profile(&self) -> CodecProfileSnapshot {
        self.profile
    }

    pub fn start_alarm_chime(&mut self) -> Result<()> {
        self.wake_codec()?;
        self.begin_playback(ChimeMode::AlarmRepeat)
    }

    pub fn apply_request(&mut self, request: AudioUiRequest) -> Result<&'static str> {
        match request {
            AudioUiRequest::PlayTestChime => {
                self.wake_codec()?;
                self.begin_playback(ChimeMode::TestOnce)?;
                Ok("test-tone-start")
            }
            AudioUiRequest::StopPlayback => {
                self.stop_playback()?;
                Ok("playback-stop")
            }
            AudioUiRequest::VolumeUp => {
                self.set_volume(
                    self.snapshot
                        .volume_percent
                        .saturating_add(AUDIO_VOLUME_STEP_PERCENT),
                )?;
                Ok("volume-up")
            }
            AudioUiRequest::VolumeDown => {
                self.set_volume(
                    self.snapshot
                        .volume_percent
                        .saturating_sub(AUDIO_VOLUME_STEP_PERCENT),
                )?;
                Ok("volume-down")
            }
            AudioUiRequest::ToggleMute => {
                self.set_muted(!self.snapshot.muted)?;
                Ok(if self.snapshot.muted {
                    "muted"
                } else {
                    "unmuted"
                })
            }
        }
    }

    /// Feed one bounded DMA chunk. Returns `true` when the visible diagnostics
    /// state changes, for example when a one-shot test chime completes.
    pub fn tick(&mut self) -> Result<bool> {
        if !self.chime.is_playing() {
            return Ok(false);
        }
        let mut bytes = [0_u8; PCM_CHUNK_BYTES];
        let completed_test = self.chime.fill_stereo_pcm(
            &mut bytes,
            self.snapshot.volume_percent,
            self.snapshot.muted,
        );
        self.write_playback_pcm(&bytes, "I2S TX write")?;
        if completed_test {
            self.stop_playback()?;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn begin_voice_note_playback(&mut self) -> Result<()> {
        self.wake_codec()?;
        self.chime.stop();
        self.codec
            .mute(&mut self.bus, false)
            .map_err(|error| anyhow!("failed to unmute ES8311 for voice note: {error:?}"))?;
        if let Err(error) = self.amplifier.set_high() {
            let _ = self.codec.mute(&mut self.bus, true);
            return Err(anyhow!("failed to enable audio amplifier: {error:?}"));
        }
        self.snapshot.amplifier_enabled = true;
        self.snapshot.muted = false;
        self.snapshot.playback_state = AudioPlaybackState::PlayingVoiceNote;
        self.snapshot.error = None;
        Ok(())
    }

    /// Unmute and enable the NS4150B for one word clip. The caller feeds PCM
    /// in short chunks and must call [`Self::finish_pronounce`] so the amp
    /// returns low when the clip, or a missing file, ends.
    pub fn begin_pronounce(&mut self) -> Result<()> {
        if self.snapshot.playback_state == AudioPlaybackState::RecordingVoiceNote {
            return Err(anyhow!("microphone capture owns the codec"));
        }
        self.wake_codec()?;
        self.chime.stop();
        self.codec
            .mute(&mut self.bus, false)
            .map_err(|error| anyhow!("failed to unmute ES8311 for pronunciation: {error:?}"))?;
        if let Err(error) = self.amplifier.set_high() {
            let _ = self.codec.mute(&mut self.bus, true);
            return Err(anyhow!("failed to enable audio amplifier: {error:?}"));
        }
        self.snapshot.amplifier_enabled = true;
        self.snapshot.muted = false;
        self.snapshot.playback_state = AudioPlaybackState::PlayingPronounce;
        self.snapshot.error = None;
        Ok(())
    }

    pub fn write_pronounce_pcm16_mono(&mut self, mono: &[u8], stereo: &mut [u8]) -> Result<()> {
        if self.snapshot.playback_state != AudioPlaybackState::PlayingPronounce {
            return Err(anyhow!("pronunciation playback is not active"));
        }
        let stereo_bytes = expand_pcm16_mono_to_stereo(mono, stereo)?;
        self.write_playback_pcm(&stereo[..stereo_bytes], "I2S pronunciation TX write")
    }

    pub fn finish_pronounce(&mut self) -> Result<()> {
        self.stop_playback()
    }

    pub fn write_voice_pcm16_mono(&mut self, mono: &[u8], stereo: &mut [u8]) -> Result<()> {
        if self.snapshot.playback_state != AudioPlaybackState::PlayingVoiceNote {
            return Err(anyhow!("voice-note playback is not active"));
        }
        let stereo_bytes = expand_pcm16_mono_to_stereo(mono, stereo)?;
        self.write_playback_pcm(&stereo[..stereo_bytes], "I2S voice-note TX write")
    }

    pub fn finish_voice_note_playback(&mut self) -> Result<()> {
        self.stop_playback()
    }

    pub fn begin_voice_recording(&mut self) -> Result<()> {
        self.wake_codec()?;
        self.chime.stop();
        self.amplifier
            .set_low()
            .map_err(|error| anyhow!("failed to disable audio amplifier: {error:?}"))?;
        self.codec
            .mute(&mut self.bus, true)
            .map_err(|error| anyhow!("failed to mute ES8311 DAC before recording: {error:?}"))?;
        self.snapshot.amplifier_enabled = false;
        self.snapshot.muted = true;
        self.snapshot.playback_state = AudioPlaybackState::RecordingVoiceNote;
        self.snapshot.error = None;
        Ok(())
    }

    pub fn finish_voice_recording(&mut self) -> Result<()> {
        self.snapshot.playback_state = AudioPlaybackState::Muted;
        self.snapshot.muted = true;
        self.snapshot.amplifier_enabled = false;
        self.sleep_codec()
    }

    pub fn read_voice_pcm_mono(
        &mut self,
        stereo: &mut [u8],
        mono: &mut [u8],
        gain: VoiceMicGain,
    ) -> Result<VoiceCaptureMetrics> {
        if stereo.len() < mono.len().saturating_mul(2) || mono.len() % 2 != 0 {
            return Err(anyhow!("invalid voice PCM buffers"));
        }
        let bytes = match self.tx.read(stereo, NON_BLOCK) {
            Ok(bytes) => bytes,
            Err(error) if error.code() == esp_idf_svc::sys::ESP_ERR_TIMEOUT => {
                return Ok(VoiceCaptureMetrics::default());
            }
            Err(error) => return Err(anyhow!("I2S RX read failed: {error:?}")),
        };
        let frames = (bytes / 4).min(mono.len() / 2);
        for frame in 0..frames {
            let source = frame * 4;
            let target = frame * 2;
            mono[target..target + 2].copy_from_slice(&stereo[source..source + 2]);
        }
        Ok(apply_pcm16_gain_in_place(&mut mono[..frames * 2], gain))
    }

    /// Drain one bounded I2S RX chunk while a voice-note recording is paused.
    /// This keeps stale microphone frames out of the resumed WAV stream without
    /// moving codec or DMA ownership away from the native main-loop runtime.
    pub fn discard_voice_pcm(&mut self, stereo: &mut [u8]) -> Result<usize> {
        match self.tx.read(stereo, NON_BLOCK) {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.code() == esp_idf_svc::sys::ESP_ERR_TIMEOUT => Ok(0),
            Err(error) => Err(anyhow!("I2S RX discard failed: {error:?}")),
        }
    }

    pub fn stop_playback(&mut self) -> Result<()> {
        self.chime.stop();
        self.amplifier
            .set_low()
            .map_err(|error| anyhow!("failed to disable audio amplifier: {error:?}"))?;
        self.codec
            .mute(&mut self.bus, true)
            .map_err(|error| anyhow!("failed to mute ES8311: {error:?}"))?;
        self.snapshot.amplifier_enabled = false;
        self.snapshot.muted = true;
        self.snapshot.playback_state = AudioPlaybackState::Muted;
        self.sleep_codec()
    }

    fn wake_codec(&mut self) -> Result<()> {
        self.set_i2s_enabled(true)?;
        if self.codec_powered {
            return Ok(());
        }
        self.codec
            .power_up(&mut self.bus)
            .map_err(|error| anyhow!("failed to power up ES8311: {error:?}"))?;
        self.codec_powered = true;
        self.snapshot.codec_powered = true;
        log::info!("rustmix-wave=audio-codec status=power-up");
        Ok(())
    }

    fn sleep_codec(&mut self) -> Result<()> {
        let _ = self.amplifier.set_low();
        if self.codec_powered {
            self.codec
                .power_down(&mut self.bus)
                .map_err(|error| anyhow!("failed to power down ES8311: {error:?}"))?;
            self.codec_powered = false;
            log::info!("rustmix-wave=audio-codec status=power-down amp=low");
        }
        self.snapshot.codec_powered = false;
        self.snapshot.amplifier_enabled = false;
        self.set_i2s_enabled(false)?;
        Ok(())
    }

    fn set_i2s_enabled(&mut self, enabled: bool) -> Result<()> {
        if self.i2s_enabled == enabled {
            return Ok(());
        }
        if enabled {
            self.tx
                .tx_enable()
                .map_err(|error| anyhow!("I2S TX enable failed: {error:?}"))?;
            self.tx
                .rx_enable()
                .map_err(|error| anyhow!("I2S RX enable failed: {error:?}"))?;
        } else {
            self.tx
                .tx_disable()
                .map_err(|error| anyhow!("I2S TX disable failed: {error:?}"))?;
            self.tx
                .rx_disable()
                .map_err(|error| anyhow!("I2S RX disable failed: {error:?}"))?;
        }
        self.i2s_enabled = enabled;
        Ok(())
    }

    pub fn record_failure(&mut self, error: impl Into<String>) {
        let _ = self.amplifier.set_low();
        let _ = self.codec.mute(&mut self.bus, true);
        self.chime.stop();
        let _ = self.sleep_codec();
        self.snapshot.amplifier_enabled = false;
        self.snapshot.muted = true;
        self.snapshot.playback_state = AudioPlaybackState::Error;
        self.snapshot.error = Some(error.into());
    }

    /// Pronunciation, voice-note playback, and alarm chimes share this write.
    /// The wait is [`I2S_TX_TIMEOUT_MS`]. A stalled bit clock returns
    /// `ESP_ERR_TIMEOUT`, and `stop_playback` drops the amplifier.
    fn write_playback_pcm(&mut self, pcm: &[u8], context: &'static str) -> Result<()> {
        let timeout = TickType::new_millis(I2S_TX_TIMEOUT_MS).ticks();
        let (status, failure) = match self.tx.write_all(pcm, timeout) {
            Ok(()) => (I2sTxStatus::Wrote, None),
            Err(error) => {
                let status = i2s_tx_status_from_code(Some(error.code()));
                let failure = if status == I2sTxStatus::Failed {
                    Some(format!("{error:?}"))
                } else {
                    None
                };
                (status, failure)
            }
        };
        match apply_playback_write(status, || self.stop_playback()) {
            Ok(()) => Ok(()),
            Err(PlaybackWriteError::Timeout) => {
                Err(anyhow!("{context} timed out after {I2S_TX_TIMEOUT_MS} ms"))
            }
            Err(PlaybackWriteError::Stop(error)) => Err(anyhow!(
                "{context} timed out after {I2S_TX_TIMEOUT_MS} ms and amplifier shutdown failed: {error:#}"
            )),
            Err(PlaybackWriteError::Failed) => Err(anyhow!(
                "{context} failed: {}",
                failure.unwrap_or_else(|| "unknown I2S error".into())
            )),
        }
    }

    fn begin_playback(&mut self, mode: ChimeMode) -> Result<()> {
        match mode {
            ChimeMode::TestOnce => self.chime.start_test_once(),
            ChimeMode::AlarmRepeat => self.chime.start_alarm_repeat(),
            ChimeMode::Idle => self.chime.stop(),
        }
        self.codec
            .mute(&mut self.bus, false)
            .map_err(|error| anyhow!("failed to unmute ES8311: {error:?}"))?;
        self.amplifier
            .set_high()
            .map_err(|error| anyhow!("failed to enable audio amplifier: {error:?}"))?;
        self.snapshot.muted = false;
        self.snapshot.amplifier_enabled = true;
        self.snapshot.playback_state = match mode {
            ChimeMode::TestOnce => AudioPlaybackState::PlayingTestTone,
            ChimeMode::AlarmRepeat => AudioPlaybackState::PlayingAlarm,
            ChimeMode::Idle => AudioPlaybackState::Ready,
        };
        self.snapshot.error = None;
        Ok(())
    }

    fn set_muted(&mut self, muted: bool) -> Result<()> {
        self.codec
            .mute(&mut self.bus, muted)
            .map_err(|error| anyhow!("failed to change ES8311 mute state: {error:?}"))?;
        let streamed = matches!(
            self.snapshot.playback_state,
            AudioPlaybackState::PlayingVoiceNote | AudioPlaybackState::PlayingPronounce
        );
        if muted || (!self.chime.is_playing() && !streamed) {
            self.amplifier
                .set_low()
                .map_err(|error| anyhow!("failed to disable audio amplifier: {error:?}"))?;
            self.snapshot.amplifier_enabled = false;
        } else {
            self.amplifier
                .set_high()
                .map_err(|error| anyhow!("failed to enable audio amplifier: {error:?}"))?;
            self.snapshot.amplifier_enabled = true;
        }
        self.snapshot.muted = muted;
        self.snapshot.playback_state = if self.chime.mode() == ChimeMode::AlarmRepeat {
            AudioPlaybackState::PlayingAlarm
        } else if self.chime.mode() == ChimeMode::TestOnce {
            AudioPlaybackState::PlayingTestTone
        } else if self.snapshot.playback_state == AudioPlaybackState::PlayingPronounce {
            AudioPlaybackState::PlayingPronounce
        } else if streamed {
            AudioPlaybackState::PlayingVoiceNote
        } else if muted {
            AudioPlaybackState::Muted
        } else {
            AudioPlaybackState::Ready
        };
        Ok(())
    }

    fn set_volume(&mut self, requested: u8) -> Result<()> {
        let volume = requested.min(MAX_AUDIO_VOLUME_PERCENT);
        self.codec
            .volume_set(&mut self.bus, volume, None)
            .map_err(|error| anyhow!("failed to set ES8311 volume: {error:?}"))?;
        self.snapshot.volume_percent = volume;
        Ok(())
    }
}
