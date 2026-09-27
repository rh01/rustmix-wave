//! Polling button adapters for the active-low onboard keys and GPIO0 BOOT back action.

use core::fmt::Debug;

use anyhow::{anyhow, Result};
use embedded_hal::{delay::DelayNs, digital::InputPin};

const DEBOUNCE_MS: u32 = 25;
/// Hold duration required for GPIO0 BOOT to navigate one hierarchy level back.
pub const BOOT_BACK_LONG_PRESS_MS: u32 = 900;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ButtonEvent {
    Up,
    Select,
    Down,
}

/// Debounced active-low key adapter. The firmware arms a falling-edge interrupt
/// so the main loop can block; `poll` still confirms the press and release.
pub struct Buttons<UP, SELECT, DOWN> {
    up: UP,
    select: SELECT,
    down: DOWN,
}

impl<UP, SELECT, DOWN> Buttons<UP, SELECT, DOWN>
where
    UP: InputPin,
    UP::Error: Debug,
    SELECT: InputPin,
    SELECT::Error: Debug,
    DOWN: InputPin,
    DOWN::Error: Debug,
{
    #[must_use]
    pub fn new(up: UP, select: SELECT, down: DOWN) -> Self {
        Self { up, select, down }
    }

    /// Return one debounced press. Keys are active low on the Waveshare board.
    pub fn poll<D: DelayNs>(&mut self, delay: &mut D) -> Result<Option<ButtonEvent>> {
        if self.is_pressed(ButtonEvent::Up)? {
            return self.confirm(delay, ButtonEvent::Up);
        }
        if self.is_pressed(ButtonEvent::Select)? {
            return self.confirm(delay, ButtonEvent::Select);
        }
        if self.is_pressed(ButtonEvent::Down)? {
            return self.confirm(delay, ButtonEvent::Down);
        }
        Ok(None)
    }

    fn confirm<D: DelayNs>(
        &mut self,
        delay: &mut D,
        event: ButtonEvent,
    ) -> Result<Option<ButtonEvent>> {
        delay.delay_ms(DEBOUNCE_MS);
        if !self.is_pressed(event)? {
            return Ok(None);
        }

        // Do not generate repeated UI events while the panel is refreshing.
        while self.is_pressed(event)? {
            delay.delay_ms(10);
        }
        Ok(Some(event))
    }

    /// True when any reader key is held down. Layout batches use this so a
    /// press is noticed before the next page of a long chapter is built.
    pub fn any_pressed(&mut self) -> Result<bool> {
        Ok(self.is_pressed(ButtonEvent::Up)?
            || self.is_pressed(ButtonEvent::Select)?
            || self.is_pressed(ButtonEvent::Down)?)
    }

    pub fn up_mut(&mut self) -> &mut UP {
        &mut self.up
    }

    pub fn select_mut(&mut self) -> &mut SELECT {
        &mut self.select
    }

    pub fn down_mut(&mut self) -> &mut DOWN {
        &mut self.down
    }

    fn is_pressed(&mut self, event: ButtonEvent) -> Result<bool> {
        match event {
            ButtonEvent::Up => self
                .up
                .is_low()
                .map_err(|error| anyhow!("GPIO4 UP read failed: {error:?}")),
            ButtonEvent::Select => self
                .select
                .is_low()
                .map_err(|error| anyhow!("GPIO5 SELECT read failed: {error:?}")),
            ButtonEvent::Down => self
                .down
                .is_low()
                .map_err(|error| anyhow!("GPIO6 DOWN read failed: {error:?}")),
        }
    }
}

/// Dedicated active-low GPIO0 BOOT-button adapter.
///
/// Long presses remain hierarchy-level Back. Short presses are surfaced so
/// route-specific features such as Sudoku axis selection can use BOOT without
/// changing Back behavior elsewhere.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootButtonEvent {
    ShortPress,
    LongPress,
}

pub struct LongPressBackButton<BACK> {
    back: BACK,
}

impl<BACK> LongPressBackButton<BACK>
where
    BACK: InputPin,
    BACK::Error: Debug,
{
    #[must_use]
    pub fn new(back: BACK) -> Self {
        Self { back }
    }

    /// Return one BOOT release classified as short or long.
    pub fn poll<D: DelayNs>(&mut self, delay: &mut D) -> Result<Option<BootButtonEvent>> {
        if !self.is_pressed()? {
            return Ok(None);
        }
        delay.delay_ms(DEBOUNCE_MS);
        if !self.is_pressed()? {
            return Ok(None);
        }

        let mut held_ms = DEBOUNCE_MS;
        while self.is_pressed()? {
            if held_ms >= BOOT_BACK_LONG_PRESS_MS {
                while self.is_pressed()? {
                    delay.delay_ms(10);
                }
                return Ok(Some(BootButtonEvent::LongPress));
            }
            delay.delay_ms(10);
            held_ms = held_ms.saturating_add(10);
        }
        Ok(Some(BootButtonEvent::ShortPress))
    }

    pub fn pin_mut(&mut self) -> &mut BACK {
        &mut self.back
    }

    fn is_pressed(&mut self) -> Result<bool> {
        self.back
            .is_low()
            .map_err(|error| anyhow!("GPIO0 BOOT read failed: {error:?}"))
    }
}
