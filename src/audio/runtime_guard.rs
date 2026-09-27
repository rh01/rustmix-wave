//! Host-testable guards for the shared playback write and the amplifier pin.
//!
//! Pronunciation, voice-note playback, and alarm chimes share one I2S write.
//! That write waits [`I2S_TX_TIMEOUT_MS`] and, on `ESP_ERR_TIMEOUT`, stops
//! playback so the NS4150B enable returns low. GPIO39 is claimed and driven
//! low before codec or I2S setup. A failed init returns that pin in an
//! [`AmpEnableHold`] instead of dropping the driver.

/// Bounded I2S TX wait shared by every playback write.
pub const I2S_TX_TIMEOUT_MS: u64 = 50;

/// `ESP_ERR_TIMEOUT` from ESP-IDF `esp_err.h` (`ESP_FAIL` family, `0x107`).
pub const ESP_ERR_TIMEOUT_CODE: i32 = 0x107;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum I2sTxStatus {
    Wrote,
    /// `i2s_channel_write` returned `ESP_ERR_TIMEOUT`.
    Timeout,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackWriteAction {
    Continue,
    /// Call `stop_playback` so the amplifier enable is driven low.
    StopPlayback,
    ReportError,
}

#[derive(Debug)]
pub enum PlaybackWriteError<E> {
    Timeout,
    Stop(E),
    Failed,
}

/// Classify an I2S write result. `None` means the HAL write returned `Ok`.
#[must_use]
pub fn i2s_tx_status_from_code(code: Option<i32>) -> I2sTxStatus {
    match code {
        None => I2sTxStatus::Wrote,
        Some(ESP_ERR_TIMEOUT_CODE) => I2sTxStatus::Timeout,
        Some(_) => I2sTxStatus::Failed,
    }
}

#[must_use]
pub fn playback_write_action(status: I2sTxStatus) -> PlaybackWriteAction {
    match status {
        I2sTxStatus::Wrote => PlaybackWriteAction::Continue,
        I2sTxStatus::Timeout => PlaybackWriteAction::StopPlayback,
        I2sTxStatus::Failed => PlaybackWriteAction::ReportError,
    }
}

/// Apply the shared write decision. Only a timeout runs `stop_playback`.
pub fn apply_playback_write<E>(
    status: I2sTxStatus,
    stop_playback: impl FnOnce() -> Result<(), E>,
) -> Result<(), PlaybackWriteError<E>> {
    match playback_write_action(status) {
        PlaybackWriteAction::Continue => Ok(()),
        PlaybackWriteAction::StopPlayback => match stop_playback() {
            Ok(()) => Err(PlaybackWriteError::Timeout),
            Err(error) => Err(PlaybackWriteError::Stop(error)),
        },
        PlaybackWriteAction::ReportError => Err(PlaybackWriteError::Failed),
    }
}

/// Amplifier-enable line. High turns the NS4150B on.
pub trait AmpEnableLine {
    type Error;

    fn drive_low(&mut self) -> Result<(), Self::Error>;

    #[must_use]
    fn is_low(&self) -> bool;
}

/// Owns the amplifier-enable driver after audio init fails.
///
/// Dropping the `PinDriver` releases GPIO39, so the NS4150B enable is no
/// longer held low. This value has to stay alive for the rest of runtime.
#[must_use = "dropping AmpEnableHold releases the NS4150B enable pin"]
#[derive(Debug)]
pub struct AmpEnableHold<P> {
    pin: P,
}

impl<P> AmpEnableHold<P> {
    #[must_use]
    pub fn new(pin: P) -> Self {
        Self { pin }
    }

    #[must_use]
    pub fn pin(&self) -> &P {
        &self.pin
    }
}

impl<P: AmpEnableLine> AmpEnableHold<P> {
    #[must_use]
    pub fn is_driven_low(&self) -> bool {
        self.pin.is_low()
    }
}

#[derive(Debug)]
pub enum AudioStartup<P, R, E> {
    Ready(R),
    Failed { hold: AmpEnableHold<P>, error: E },
}

/// Drive `pin` low, then run I2S and codec setup.
///
/// The closure runs only after the pin is low. I2S setup failure and a codec
/// NACK both return that same pin in an [`AmpEnableHold`], driven low again.
pub fn claim_amp_then_start<P, R, E>(
    mut pin: P,
    map_drive_error: impl FnOnce(P::Error) -> E,
    start: impl FnOnce(P) -> Result<R, (P, E)>,
) -> AudioStartup<P, R, E>
where
    P: AmpEnableLine,
{
    if let Err(error) = pin.drive_low() {
        return AudioStartup::Failed {
            hold: AmpEnableHold::new(pin),
            error: map_drive_error(error),
        };
    }
    match start(pin) {
        Ok(runtime) => AudioStartup::Ready(runtime),
        Err((mut pin, error)) => {
            // Best effort. The driver is still owned below even if this fails.
            let _ = pin.drive_low();
            AudioStartup::Failed {
                hold: AmpEnableHold::new(pin),
                error,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_playback_write, claim_amp_then_start, i2s_tx_status_from_code, AmpEnableLine,
        AudioStartup, PlaybackWriteError, ESP_ERR_TIMEOUT_CODE, I2S_TX_TIMEOUT_MS,
    };

    #[derive(Debug)]
    struct FakeAmp {
        high: bool,
    }

    impl AmpEnableLine for FakeAmp {
        type Error = &'static str;

        fn drive_low(&mut self) -> Result<(), Self::Error> {
            self.high = false;
            Ok(())
        }

        fn is_low(&self) -> bool {
            !self.high
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum BringupFault {
        I2s,
        CodecNack,
    }

    #[test]
    fn i2s_tx_timeout_is_50ms_and_stops_playback() {
        assert_eq!(I2S_TX_TIMEOUT_MS, 50);
        assert_eq!(ESP_ERR_TIMEOUT_CODE, 0x107);

        let status = i2s_tx_status_from_code(Some(ESP_ERR_TIMEOUT_CODE));
        let mut amp_high = true;
        let mut stopped = false;
        let result = apply_playback_write(status, || {
            stopped = true;
            amp_high = false;
            Ok::<(), &str>(())
        });

        assert!(matches!(result, Err(PlaybackWriteError::Timeout)));
        assert!(stopped);
        assert!(!amp_high);
    }

    #[test]
    fn other_i2s_write_errors_do_not_drop_the_amplifier_in_the_shared_write() {
        let status = i2s_tx_status_from_code(Some(0x105));
        let mut stopped = false;
        let result = apply_playback_write(status, || {
            stopped = true;
            Ok::<(), &str>(())
        });
        assert!(matches!(result, Err(PlaybackWriteError::Failed)));
        assert!(!stopped);
    }

    #[test]
    fn codec_nack_keeps_amp_enable_low_in_the_hold() {
        let outcome: AudioStartup<FakeAmp, (), BringupFault> = claim_amp_then_start(
            FakeAmp { high: true },
            |_| panic!("drive-low failed"),
            |mut pin| {
                assert!(pin.is_low(), "GPIO39 is low before codec setup");
                pin.high = true;
                Err((pin, BringupFault::CodecNack))
            },
        );
        match outcome {
            AudioStartup::Failed { hold, error } => {
                assert_eq!(error, BringupFault::CodecNack);
                assert!(hold.is_driven_low());
                assert!(hold.pin().is_low());
            }
            AudioStartup::Ready(_) => panic!("codec NACK must keep the amp hold"),
        }
    }

    #[test]
    fn i2s_failure_keeps_amp_enable_low_in_the_hold() {
        let outcome: AudioStartup<FakeAmp, (), BringupFault> = claim_amp_then_start(
            FakeAmp { high: true },
            |_| panic!("drive-low failed"),
            |pin| {
                assert!(pin.is_low(), "GPIO39 is low before I2S setup");
                Err((pin, BringupFault::I2s))
            },
        );
        match outcome {
            AudioStartup::Failed { hold, error } => {
                assert_eq!(error, BringupFault::I2s);
                assert!(hold.is_driven_low());
                assert!(hold.pin().is_low());
            }
            AudioStartup::Ready(_) => panic!("I2S failure must keep the amp hold"),
        }
    }

    #[test]
    fn firmware_i2s_writes_share_the_50ms_timeout() {
        let src = include_str!("espidf.rs");
        assert!(src.contains("TickType::new_millis(I2S_TX_TIMEOUT_MS)"));
        assert!(src.contains("apply_playback_write"));
        assert!(src.contains("PlaybackWriteError::Timeout"));
        assert!(src.contains("self.stop_playback()"));
        assert!(!src.contains(", BLOCK"));
        assert!(!src.contains("write_all(&stereo"));
        assert_eq!(src.matches("write_all(").count(), 1);
    }

    #[test]
    fn firmware_claims_gpio39_before_i2s_and_keeps_the_hold() {
        let src = include_str!("../main.rs");
        let claim = src
            .find("PinDriver::output(peripherals.pins.gpio39)")
            .expect("GPIO39 claim");
        let i2s = src
            .find("I2sDriver::<I2sBiDir>::new_std_bidir")
            .expect("I2S setup");
        let codec = src.find("AudioRuntime::initialize").expect("codec init");
        assert!(claim < i2s, "GPIO39 is claimed before I2S setup");
        assert!(i2s < codec, "I2S setup runs before codec init");
        assert!(src.contains("claim_amp_then_start"));
        assert!(src.contains("AmpEnableHold"));
        assert!(src.contains("amp_enable_hold"));
        assert!(!src.contains("let _ = amp_enable_hold"));
    }
}
