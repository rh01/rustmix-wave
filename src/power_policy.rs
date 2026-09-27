//! Host-testable power policy for the ESP32-S3 e-paper reader.
//!
//! The firmware loop owns clocks, radios, and sleep entry. This module decides
//! when those transitions are allowed and how long the main task may block.
//! Blocking is what lets tickless idle enter automatic light sleep. Deep sleep
//! is a separate, explicit transition after the idle timeout.

use crate::rtc::RtcDateTime;

/// Lowest DFS frequency that keeps octal PSRAM clocked at 80 MHz.
pub const CPU_FREQ_MIN_MHZ: i32 = 80;
/// ESP32-S3 application CPU ceiling used while a refresh or network job runs.
pub const CPU_FREQ_MAX_MHZ: i32 = 240;
/// Wi-Fi stays up this long after NTP, weather, WeRead, or a portal job ends.
pub const RADIO_IDLE_TIMEOUT_SECS: u64 = 20;
/// Give SNTP this long, then treat the radio as idle even if sync has not finished.
pub const RADIO_NTP_HOLD_SECS: u64 = 45;
/// Default auto deep-sleep delay while the UI is idle.
pub const DEFAULT_AUTO_DEEP_SLEEP_MINUTES: u32 = 10;
/// Choices offered on the power debug screen.
pub const AUTO_DEEP_SLEEP_CHOICES_MIN: [u32; 4] = [5, 10, 30, 60];
/// SDMMC clock while the card is in use. Matches the storage mount.
pub const SD_ACTIVE_CLOCK_KHZ: u32 = 10_000;
/// Identification-rate clock used between filesystem transactions.
pub const SD_IDLE_CLOCK_KHZ: u32 = 400;
/// Longest single tickless wait. The power key is polled on I2C, so idle waits
/// are also capped by that poll.
pub const MAX_BLOCK_MS: u64 = 5_000;
/// Power-key poll while the user was active in the last few seconds.
pub const POWER_KEY_ACTIVE_POLL_MS: u64 = 100;
/// Power-key poll once the UI has been idle. Light sleep fills this gap.
pub const POWER_KEY_IDLE_POLL_MS: u64 = 1_000;
/// User is treated as idle for the slower power-key poll after this gap.
pub const POWER_KEY_IDLE_AFTER_MS: u64 = 3_000;

/// RTC-noinit / NVS record magic (`PKW1`).
pub const RESUME_MAGIC: u32 = 0x5257_4B31;

/// Why the next e-paper frame should or should not use a full GC waveform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshCause {
    /// Ordinary navigation or a page turn while the controller RAM is still valid.
    UserNavigation,
    /// The SSD1677 was powered off, so both RAM planes were lost.
    PanelRailWasCut,
    /// Leaving the sleep image.
    SleepImageExit,
    /// First frame after an MCU wake that must clear ghosts.
    McuWake,
}

impl RefreshCause {
    #[must_use]
    pub const fn wants_global_refresh(self) -> bool {
        matches!(self, Self::SleepImageExit | Self::McuWake)
    }
}

/// How `refresh_screen` must drive the SSD1677 for one frame.
///
/// A partial that writes only `0x24` is valid while the controller still holds
/// the previous image in `0x26`. After a rail cut that plane is empty, and a
/// bare partial leaves unchanged pixels blank.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelTransport {
    /// `0x24` = new frame, `0xFF`. The old plane is still in the controller.
    PartialLive,
    /// `0x26` = previous frame, `0x24` = new frame, `0xFF`.
    RestoreOldPlaneThenPartial,
    /// `0x24` and `0x26` = new frame, `0xF7`.
    GlobalBase,
}

impl PanelTransport {
    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::PartialLive => "partial-live",
            Self::RestoreOldPlaneThenPartial => "partial-restore-old-plane",
            Self::GlobalBase => "global-base",
        }
    }

    #[must_use]
    pub const fn writes_previous_frame_to_old_plane(self) -> bool {
        matches!(self, Self::RestoreOldPlaneThenPartial)
    }
}

/// What the next refresh must assume after [`crate::epaper::Epaper397::sleep`].
///
/// Command `0x10` invalidates SSD1677 RAM before ALDO3 is cut. A later pin or
/// rail error still means both planes are gone, so the retained frame is
/// dropped and the next refresh is `show_base`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelSleepFollowUp {
    /// `0x10` never left the bus. Controller RAM still holds the last frame.
    KeepControllerRam,
    /// `0x10` was accepted. Drop the retained frame and run `show_base` next.
    ForceShowBase,
}

#[must_use]
pub const fn panel_sleep_follow_up(deep_sleep_command_sent: bool) -> PanelSleepFollowUp {
    if deep_sleep_command_sent {
        PanelSleepFollowUp::ForceShowBase
    } else {
        PanelSleepFollowUp::KeepControllerRam
    }
}

/// Whether the PSRAM copy of the last frame is still safe to reuse.
///
/// A failed display command can leave the waveform half-applied, so the copy
/// is dropped. A failed PSRAM clone cannot support a later old-plane restore,
/// so the next frame is a global base instead of aborting.
#[must_use]
pub const fn keep_retained_frame(display_succeeded: bool, clone_succeeded: bool) -> bool {
    display_succeeded && clone_succeeded
}

/// Decide the panel command sequence. `refresh_screen` must use this and must
/// not send a live partial after the rail was cut.
#[must_use]
pub const fn plan_panel_transport(
    cause: RefreshCause,
    coordinator_wants_global: bool,
    has_previous_frame: bool,
) -> PanelTransport {
    if cause.wants_global_refresh() || coordinator_wants_global {
        PanelTransport::GlobalBase
    } else if matches!(cause, RefreshCause::PanelRailWasCut) {
        if has_previous_frame {
            PanelTransport::RestoreOldPlaneThenPartial
        } else {
            PanelTransport::GlobalBase
        }
    } else {
        PanelTransport::PartialLive
    }
}

/// What to do with a pending alarm at deep-sleep entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AlarmWakePlan {
    None,
    Timer {
        seconds: u64,
    },
    /// An alarm is scheduled but the clock is missing, so a timer cannot be armed.
    RefuseMissingClock,
}

#[must_use]
pub fn alarm_wake_plan(now: Option<RtcDateTime>, alarm_at: Option<RtcDateTime>) -> AlarmWakePlan {
    let Some(alarm_at) = alarm_at else {
        return AlarmWakePlan::None;
    };
    let Some(now) = now else {
        return AlarmWakePlan::RefuseMissingClock;
    };
    match seconds_until(now, alarm_at) {
        Some(seconds) => AlarmWakePlan::Timer { seconds },
        None => AlarmWakePlan::None,
    }
}

/// Coarse radio job that is allowed to hold the modem up.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RadioJob {
    None,
    Ntp,
    Weather,
    WeRead,
    SoftAp,
    TransferPortal,
}

impl RadioJob {
    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Ntp => "ntp",
            Self::Weather => "weather",
            Self::WeRead => "weread",
            Self::SoftAp => "softap",
            Self::TransferPortal => "portal",
        }
    }

    #[must_use]
    pub const fn holds_radio(self) -> bool {
        !matches!(self, Self::None)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McuPowerMode {
    /// CPU is awake. DFS may still drop the clock while tasks block briefly.
    Active,
    /// Tickless idle may light-sleep between page turns.
    LightSleepEligible,
    /// Idle timeout elapsed and nothing is holding the system awake.
    DeepSleepDue,
}

impl McuPowerMode {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::LightSleepEligible => "light-sleep",
            Self::DeepSleepDue => "deep-sleep-due",
        }
    }
}

/// Wake reason decoded from `esp_sleep_get_wakeup_cause`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McuWake {
    PowerOn,
    Button,
    Timer,
    Other(u32),
}

impl McuWake {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::PowerOn => "power-on",
            Self::Button => "button",
            Self::Timer => "timer",
            Self::Other(_) => "other",
        }
    }
}

/// Map an ESP-IDF `esp_sleep_wakeup_cause_t` discriminant.
#[must_use]
pub const fn classify_wake_cause(code: u32) -> McuWake {
    match code {
        0 => McuWake::PowerOn,
        3 | 7 => McuWake::Button,
        4 => McuWake::Timer,
        other => McuWake::Other(other),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SleepResume {
    pub route_code: u8,
    pub restore_reader: bool,
    pub auto_sleep_minutes: u32,
}

impl SleepResume {
    #[must_use]
    pub const fn encode(self) -> [u8; 12] {
        let minutes = self.auto_sleep_minutes.to_le_bytes();
        [
            0x31,
            0x4B,
            0x57,
            0x52,
            self.route_code,
            if self.restore_reader { 1 } else { 0 },
            minutes[0],
            minutes[1],
            minutes[2],
            minutes[3],
            0,
            0,
        ]
    }

    #[must_use]
    pub const fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 10
            || bytes[0] != 0x31
            || bytes[1] != 0x4B
            || bytes[2] != 0x57
            || bytes[3] != 0x52
        {
            return None;
        }
        let minutes = u32::from_le_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]);
        Some(Self {
            route_code: bytes[4],
            restore_reader: bytes[5] != 0,
            auto_sleep_minutes: clamp_auto_sleep_minutes(minutes),
        })
    }
}

#[must_use]
pub const fn clamp_auto_sleep_minutes(minutes: u32) -> u32 {
    if minutes < 1 {
        DEFAULT_AUTO_DEEP_SLEEP_MINUTES
    } else if minutes > 24 * 60 {
        24 * 60
    } else {
        minutes
    }
}

#[must_use]
pub fn cycle_auto_deep_sleep_minutes(current: u32) -> u32 {
    let current = clamp_auto_sleep_minutes(current);
    let mut index = 0;
    while index < AUTO_DEEP_SLEEP_CHOICES_MIN.len() {
        if AUTO_DEEP_SLEEP_CHOICES_MIN[index] == current {
            return AUTO_DEEP_SLEEP_CHOICES_MIN[(index + 1) % AUTO_DEEP_SLEEP_CHOICES_MIN.len()];
        }
        index += 1;
    }
    AUTO_DEEP_SLEEP_CHOICES_MIN[0]
}

/// Tracks how long the station has been unused after the last useful job.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RadioIdle {
    useful: bool,
    idle_since_ms: Option<u64>,
    timeout_ms: u64,
}

impl RadioIdle {
    #[must_use]
    pub const fn new(timeout_secs: u64) -> Self {
        Self {
            useful: false,
            idle_since_ms: None,
            timeout_ms: timeout_secs.saturating_mul(1_000),
        }
    }

    pub fn set_useful(&mut self, useful: bool, now_ms: u64) {
        if useful {
            self.useful = true;
            self.idle_since_ms = None;
            return;
        }
        if self.useful || self.idle_since_ms.is_none() {
            self.idle_since_ms = Some(now_ms);
        }
        self.useful = false;
    }

    pub fn on_radio_stopped(&mut self) {
        self.useful = false;
        self.idle_since_ms = None;
    }

    #[must_use]
    pub const fn is_useful(self) -> bool {
        self.useful
    }

    #[must_use]
    pub fn should_stop(self, now_ms: u64, radio_on: bool) -> bool {
        if !radio_on || self.useful {
            return false;
        }
        self.idle_since_ms
            .is_some_and(|started| now_ms.saturating_sub(started) >= self.timeout_ms)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaitInput {
    pub voice_pump: bool,
    pub reader_background: bool,
    pub weread_busy: bool,
    pub imu_due_ms: Option<u64>,
    pub alarm_due_ms: u64,
    pub power_key_due_ms: u64,
    pub radio_stop_due_ms: Option<u64>,
    pub deep_sleep_due_ms: Option<u64>,
    pub status_due_ms: Option<u64>,
    pub weread_due_ms: Option<u64>,
    pub auto_turn_due_ms: Option<u64>,
}

#[must_use]
pub fn next_block_ms(input: WaitInput) -> u64 {
    let mut limit = MAX_BLOCK_MS;
    if input.voice_pump {
        limit = limit.min(10);
    }
    if input.reader_background {
        limit = limit.min(250);
    }
    if input.weread_busy {
        limit = limit.min(100);
    }
    limit = limit.min(input.alarm_due_ms.max(1));
    limit = limit.min(input.power_key_due_ms.max(1));
    if let Some(due) = input.imu_due_ms {
        limit = limit.min(due.max(1));
    }
    if let Some(due) = input.radio_stop_due_ms {
        limit = limit.min(due.max(1));
    }
    if let Some(due) = input.deep_sleep_due_ms {
        limit = limit.min(due.max(1));
    }
    if let Some(due) = input.status_due_ms {
        limit = limit.min(due.max(1));
    }
    if let Some(due) = input.weread_due_ms {
        limit = limit.min(due.max(1));
    }
    if let Some(due) = input.auto_turn_due_ms {
        limit = limit.min(due.max(1));
    }
    limit.clamp(1, MAX_BLOCK_MS)
}

#[must_use]
pub const fn power_key_poll_ms(idle_for_ms: u64) -> u64 {
    if idle_for_ms >= POWER_KEY_IDLE_AFTER_MS {
        POWER_KEY_IDLE_POLL_MS
    } else {
        POWER_KEY_ACTIVE_POLL_MS
    }
}

#[must_use]
pub const fn sd_clock_khz(idle: bool) -> u32 {
    if idle {
        SD_IDLE_CLOCK_KHZ
    } else {
        SD_ACTIVE_CLOCK_KHZ
    }
}

/// The SD host may drop to the identification clock only while the next wait
/// will not touch the card.
///
/// `weread_needs_radio` stays true for a whole offline download, including the
/// pause between chapters and the main-task FAT write. The in-flight HTTPS
/// flag is false during that pause, so it is not enough to keep the 10 MHz clock.
#[must_use]
pub const fn sd_host_can_idle(
    mounted: bool,
    voice_busy: bool,
    reader_busy: bool,
    weread_needs_radio: bool,
) -> bool {
    mounted && !voice_busy && !reader_busy && !weread_needs_radio
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PowerView {
    pub mcu: McuPowerMode,
    pub radio_job: RadioJob,
    pub radio_on: bool,
    pub panel_rail_on: bool,
    pub audio_codec_on: bool,
    pub amplifier_on: bool,
    pub sd_idle: bool,
    pub auto_sleep_minutes: u32,
    pub idle_seconds: u32,
    pub light_sleep_configured: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PowerDebugSnapshot {
    pub cpu_min_mhz: i32,
    pub cpu_max_mhz: i32,
    pub dfs: bool,
    pub light_sleep: bool,
    pub mcu: &'static str,
    pub radio: &'static str,
    pub panel: &'static str,
    pub audio: &'static str,
    pub sd: &'static str,
    pub bluetooth: &'static str,
    pub auto_sleep_minutes: u32,
    pub idle_seconds: u32,
    pub estimate: &'static str,
}

impl Default for PowerDebugSnapshot {
    fn default() -> Self {
        Self::from_view(PowerView {
            mcu: McuPowerMode::Active,
            radio_job: RadioJob::None,
            radio_on: false,
            panel_rail_on: false,
            audio_codec_on: false,
            amplifier_on: false,
            sd_idle: true,
            auto_sleep_minutes: DEFAULT_AUTO_DEEP_SLEEP_MINUTES,
            idle_seconds: 0,
            light_sleep_configured: true,
        })
    }
}

impl PowerDebugSnapshot {
    #[must_use]
    pub const fn from_view(view: PowerView) -> Self {
        let radio = if view.radio_on {
            view.radio_job.marker()
        } else {
            "off"
        };
        Self {
            cpu_min_mhz: CPU_FREQ_MIN_MHZ,
            cpu_max_mhz: CPU_FREQ_MAX_MHZ,
            dfs: true,
            light_sleep: view.light_sleep_configured,
            mcu: view.mcu.label(),
            radio,
            panel: if view.panel_rail_on {
                "rail-on"
            } else {
                "deep-sleep"
            },
            audio: if view.amplifier_on {
                "amp-on"
            } else if view.audio_codec_on {
                "codec-on"
            } else {
                "codec-off"
            },
            sd: if view.sd_idle { "idle" } else { "active" },
            bluetooth: "off",
            auto_sleep_minutes: view.auto_sleep_minutes,
            idle_seconds: view.idle_seconds,
            estimate: estimate_label(view),
        }
    }
}

const fn estimate_label(view: PowerView) -> &'static str {
    if view.amplifier_on {
        return "40-120mA";
    }
    if view.radio_on && matches!(view.radio_job, RadioJob::SoftAp | RadioJob::TransferPortal) {
        return "90-180mA";
    }
    if view.radio_on {
        return "70-140mA";
    }
    if view.panel_rail_on {
        return "20-50mA";
    }
    match view.mcu {
        McuPowerMode::DeepSleepDue => "0.1-0.5mA",
        McuPowerMode::LightSleepEligible => "2-8mA",
        McuPowerMode::Active => "15-40mA",
    }
}

#[must_use]
pub fn mcu_mode(
    idle_ms: u64,
    deep_sleep_after_ms: u64,
    blocked: bool,
    reading_gap: bool,
) -> McuPowerMode {
    if !blocked && idle_ms >= deep_sleep_after_ms {
        McuPowerMode::DeepSleepDue
    } else if reading_gap {
        McuPowerMode::LightSleepEligible
    } else if idle_ms >= POWER_KEY_IDLE_AFTER_MS && !blocked {
        McuPowerMode::LightSleepEligible
    } else {
        McuPowerMode::Active
    }
}

#[must_use]
pub fn deep_sleep_blocked(
    voice: bool,
    weread: bool,
    portal: bool,
    alarm_ringing: bool,
    reader_busy: bool,
    auto_page_turn: bool,
) -> bool {
    voice || weread || portal || alarm_ringing || reader_busy || auto_page_turn
}

/// Whole seconds from `now` until `later`, if `later` is strictly in the future.
#[must_use]
pub fn seconds_until(now: RtcDateTime, later: RtcDateTime) -> Option<u64> {
    let now_secs = civil_seconds(now)?;
    let later_secs = civil_seconds(later)?;
    let delta = later_secs.saturating_sub(now_secs);
    if delta == 0 {
        None
    } else {
        Some(delta)
    }
}

fn civil_seconds(time: RtcDateTime) -> Option<u64> {
    if time.month < 1 || time.month > 12 || time.day < 1 || time.day > 31 {
        return None;
    }
    if time.hour > 23 || time.minute > 59 || time.second > 59 {
        return None;
    }
    let days = days_from_civil(
        i32::from(time.year),
        u32::from(time.month),
        u32::from(time.day),
    )?;
    let seconds = days
        .saturating_mul(86_400)
        .saturating_add(i64::from(time.hour) * 3_600)
        .saturating_add(i64::from(time.minute) * 60)
        .saturating_add(i64::from(time.second));
    u64::try_from(seconds).ok()
}

/// Howard Hinnant's `days_from_civil`, days since 1970-01-01.
fn days_from_civil(year: i32, month: u32, day: u32) -> Option<i64> {
    if !(1970..=2099).contains(&year) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u32;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(i64::from(era) * 146_097 + i64::from(doe) - 719_468)
}

#[must_use]
pub fn format_battery(voltage_mv: Option<u16>, percent: Option<u8>) -> String {
    match (voltage_mv, percent) {
        (Some(mv), Some(percent)) => format!("{:.2} V  {percent}%", f32::from(mv) / 1000.0),
        (Some(mv), None) => format!("{:.2} V", f32::from(mv) / 1000.0),
        (None, Some(percent)) => format!("{percent}%"),
        (None, None) => "no battery ADC".into(),
    }
}

/// Order-of-magnitude battery current before and after this change.
/// These are planning estimates from ESP32-S3, SSD1677, and AXP2101 public
/// figures, not measurements from this board.
pub struct CurrentDrawEstimate {
    pub state: &'static str,
    pub before_ma: &'static str,
    pub after_ma: &'static str,
    pub basis: &'static str,
}

pub const CURRENT_DRAW_ESTIMATES: &[CurrentDrawEstimate] = &[
    CurrentDrawEstimate {
        state: "Boot / page refresh pulse",
        before_ma: "40-180",
        after_ma: "40-180",
        basis: "SSD1677 waveform plus CPU; unchanged during the refresh itself",
    },
    CurrentDrawEstimate {
        state: "Idle UI, Wi-Fi associated, panel rail on, 20 ms poll",
        before_ma: "70-150",
        after_ma: "2-8",
        basis: "Radio off after the idle timeout, panel in deep sleep, tickless light sleep, DFS at 80 MHz",
    },
    CurrentDrawEstimate {
        state: "Reading, between page turns",
        before_ma: "70-140",
        after_ma: "8-25",
        basis: "Rail stays on so the old RAM plane survives. CPU light-sleeps. The controller sleeps after 60 s idle",
    },
    CurrentDrawEstimate {
        state: "WeRead / NTP / weather job",
        before_ma: "80-160",
        after_ma: "80-160",
        basis: "Station stays up only while the job runs",
    },
    CurrentDrawEstimate {
        state: "After WeRead, sync, or portal job",
        before_ma: "70-150",
        after_ma: "2-8",
        basis: "Radio stops 20 s after the job; SoftAP and the transfer portal already had their own teardown",
    },
    CurrentDrawEstimate {
        state: "SoftAP or transfer portal while open",
        before_ma: "90-180",
        after_ma: "90-180",
        basis: "The AP or HTTP server is the job; the radio drops when that server stops",
    },
    CurrentDrawEstimate {
        state: "Sleep image, MCU left running",
        before_ma: "15-40",
        after_ma: "0.1-0.5",
        basis: "EXT1 button wake and an alarm timer replace the awake poll loop. PSRAM is not retained",
    },
    CurrentDrawEstimate {
        state: "Audio playing",
        before_ma: "40-120",
        after_ma: "40-120",
        basis: "Codec, I2S, and the NS4150B stay on only while samples are streaming",
    },
    CurrentDrawEstimate {
        state: "Audio idle",
        before_ma: "8-20 added",
        after_ma: "~0 added",
        basis: "ES8311 register 0x01 power-down, amplifier low, I2S channels disabled",
    },
    CurrentDrawEstimate {
        state: "SD card between reads",
        before_ma: "5-20",
        after_ma: "0.2-2",
        basis: "Host clock drops from 10 MHz to 400 kHz while no file transaction is running",
    },
];

#[cfg(target_os = "espidf")]
pub fn configure_dynamic_frequency_and_light_sleep() -> Result<(), i32> {
    let config = esp_idf_svc::sys::esp_pm_config_t {
        max_freq_mhz: CPU_FREQ_MAX_MHZ,
        min_freq_mhz: CPU_FREQ_MIN_MHZ,
        light_sleep_enable: true,
    };
    let status = unsafe {
        esp_idf_svc::sys::esp_pm_configure(
            (&config as *const esp_idf_svc::sys::esp_pm_config_t).cast(),
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(status)
    }
}

#[cfg(target_os = "espidf")]
const NVS_NAMESPACE: &str = "rw_pwr";

#[cfg(target_os = "espidf")]
pub fn save_resume(resume: SleepResume) -> bool {
    use esp_idf_svc::sys::{
        nvs_close, nvs_commit, nvs_handle_t, nvs_open, nvs_open_mode_t_NVS_READWRITE, nvs_set_blob,
        ESP_OK,
    };
    use std::ffi::CString;

    let bytes = resume.encode();
    unsafe {
        let mut handle: nvs_handle_t = 0;
        let ns = CString::new(NVS_NAMESPACE).unwrap();
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READWRITE, &mut handle) != ESP_OK {
            log::warn!("rustmix-wave=power-nvs status=open-failed");
            return false;
        }
        let key = CString::new("resume").unwrap();
        let set_status = nvs_set_blob(handle, key.as_ptr(), bytes.as_ptr().cast(), bytes.len());
        if set_status != ESP_OK {
            log::warn!("rustmix-wave=power-nvs status=set-blob-failed code={set_status}");
            nvs_close(handle);
            return false;
        }
        let commit_status = nvs_commit(handle);
        nvs_close(handle);
        if commit_status != ESP_OK {
            log::warn!("rustmix-wave=power-nvs status=commit-failed code={commit_status}");
            return false;
        }
        true
    }
}

#[cfg(target_os = "espidf")]
pub fn load_resume() -> Option<SleepResume> {
    use esp_idf_svc::sys::{
        nvs_close, nvs_get_blob, nvs_handle_t, nvs_open, nvs_open_mode_t_NVS_READONLY, ESP_OK,
    };
    use std::ffi::CString;

    unsafe {
        let mut handle: nvs_handle_t = 0;
        let ns = CString::new(NVS_NAMESPACE).unwrap();
        if nvs_open(ns.as_ptr(), nvs_open_mode_t_NVS_READONLY, &mut handle) != ESP_OK {
            return None;
        }
        let key = CString::new("resume").unwrap();
        let mut bytes = [0_u8; 12];
        let mut len = bytes.len();
        let status = nvs_get_blob(handle, key.as_ptr(), bytes.as_mut_ptr().cast(), &mut len);
        nvs_close(handle);
        if status != ESP_OK {
            return None;
        }
        SleepResume::decode(&bytes[..len])
    }
}

#[cfg(target_os = "espidf")]
#[repr(C)]
struct RtcResumeRaw {
    bytes: [u8; 12],
}

#[cfg(target_os = "espidf")]
#[used]
#[link_section = ".rtc_noinit"]
static mut RTC_RESUME: RtcResumeRaw = RtcResumeRaw { bytes: [0; 12] };

#[cfg(target_os = "espidf")]
pub fn store_rtc_resume(resume: SleepResume) {
    unsafe {
        core::ptr::addr_of_mut!(RTC_RESUME.bytes).write(resume.encode());
    }
}

#[cfg(target_os = "espidf")]
pub fn load_rtc_resume() -> Option<SleepResume> {
    unsafe {
        let bytes = core::ptr::addr_of!(RTC_RESUME.bytes).read();
        SleepResume::decode(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        alarm_wake_plan, clamp_auto_sleep_minutes, classify_wake_cause,
        cycle_auto_deep_sleep_minutes, days_from_civil, deep_sleep_blocked, format_battery,
        keep_retained_frame, mcu_mode, next_block_ms, panel_sleep_follow_up, plan_panel_transport,
        power_key_poll_ms, sd_clock_khz, sd_host_can_idle, seconds_until, AlarmWakePlan,
        McuPowerMode, McuWake, PanelSleepFollowUp, PanelTransport, PowerDebugSnapshot, RadioIdle,
        RadioJob, RefreshCause, SleepResume, WaitInput, CURRENT_DRAW_ESTIMATES,
        DEFAULT_AUTO_DEEP_SLEEP_MINUTES, RADIO_IDLE_TIMEOUT_SECS, SD_ACTIVE_CLOCK_KHZ,
        SD_IDLE_CLOCK_KHZ,
    };
    use crate::rtc::RtcDateTime;

    fn sample_time(hour: u8, minute: u8, second: u8) -> RtcDateTime {
        RtcDateTime {
            year: 2026,
            month: 9,
            day: 27,
            weekday: 0,
            hour,
            minute,
            second,
        }
    }

    #[test]
    fn refresh_screen_honors_a_rail_cut_by_restoring_the_old_plane() {
        assert_eq!(
            plan_panel_transport(RefreshCause::UserNavigation, false, true),
            PanelTransport::PartialLive
        );
        assert_eq!(
            plan_panel_transport(RefreshCause::PanelRailWasCut, false, true),
            PanelTransport::RestoreOldPlaneThenPartial
        );
        assert!(PanelTransport::RestoreOldPlaneThenPartial.writes_previous_frame_to_old_plane());
        assert!(!PanelTransport::PartialLive.writes_previous_frame_to_old_plane());
        assert_eq!(
            plan_panel_transport(RefreshCause::PanelRailWasCut, false, false),
            PanelTransport::GlobalBase
        );
        assert_eq!(
            plan_panel_transport(RefreshCause::PanelRailWasCut, true, true),
            PanelTransport::GlobalBase
        );
        assert_eq!(
            plan_panel_transport(RefreshCause::McuWake, false, true),
            PanelTransport::GlobalBase
        );
        assert_eq!(
            plan_panel_transport(RefreshCause::SleepImageExit, false, true),
            PanelTransport::GlobalBase
        );
        assert!(!RefreshCause::PanelRailWasCut.wants_global_refresh());
        assert!(RefreshCause::McuWake.wants_global_refresh());
    }

    #[test]
    fn sleep_after_deep_sleep_command_forces_show_base() {
        assert_eq!(
            panel_sleep_follow_up(false),
            PanelSleepFollowUp::KeepControllerRam
        );
        assert_eq!(
            panel_sleep_follow_up(true),
            PanelSleepFollowUp::ForceShowBase
        );
        assert_eq!(
            plan_panel_transport(RefreshCause::PanelRailWasCut, false, false),
            PanelTransport::GlobalBase
        );
        assert!(keep_retained_frame(true, true));
        assert!(!keep_retained_frame(false, true));
        assert!(!keep_retained_frame(true, false));
    }

    #[test]
    fn missing_clock_refuses_deep_sleep_when_an_alarm_is_pending() {
        let now = sample_time(8, 0, 0);
        let later = sample_time(8, 30, 0);
        assert_eq!(alarm_wake_plan(None, None), AlarmWakePlan::None);
        assert_eq!(
            alarm_wake_plan(None, Some(later)),
            AlarmWakePlan::RefuseMissingClock
        );
        assert_eq!(
            alarm_wake_plan(Some(now), Some(later)),
            AlarmWakePlan::Timer { seconds: 1_800 }
        );
        assert_eq!(alarm_wake_plan(Some(later), Some(now)), AlarmWakePlan::None);
    }

    #[test]
    fn radio_stops_only_after_the_idle_timeout() {
        let mut idle = RadioIdle::new(RADIO_IDLE_TIMEOUT_SECS);
        idle.set_useful(true, 0);
        assert!(!idle.should_stop(1_000, true));
        idle.set_useful(false, 1_000);
        assert!(!idle.should_stop(1_000 + 19_000, true));
        assert!(idle.should_stop(1_000 + 20_000, true));
        assert!(!idle.should_stop(1_000 + 20_000, false));
        idle.on_radio_stopped();
        idle.set_useful(true, 50_000);
        assert!(idle.is_useful());
        assert_eq!(RadioJob::WeRead.marker(), "weread");
        assert!(RadioJob::TransferPortal.holds_radio());
        assert!(!RadioJob::None.holds_radio());
    }

    #[test]
    fn reading_gap_is_light_sleep_and_long_idle_is_deep_sleep() {
        assert_eq!(
            mcu_mode(1_000, 600_000, false, true),
            McuPowerMode::LightSleepEligible
        );
        assert_eq!(
            mcu_mode(600_000, 600_000, false, true),
            McuPowerMode::DeepSleepDue
        );
        assert_eq!(
            mcu_mode(600_000, 600_000, true, true),
            McuPowerMode::LightSleepEligible
        );
        assert!(deep_sleep_blocked(false, true, false, false, false, false));
        assert!(!deep_sleep_blocked(
            false, false, false, false, false, false
        ));
        assert!(deep_sleep_blocked(false, false, false, false, false, true));
    }

    #[test]
    fn weread_download_holds_the_radio_the_sd_clock_and_deep_sleep() {
        assert!(RadioJob::WeRead.holds_radio());
        assert!(deep_sleep_blocked(false, true, false, false, false, false));
        assert!(!sd_host_can_idle(true, false, false, true));
        assert_eq!(sd_clock_khz(false), SD_ACTIVE_CLOCK_KHZ);
        assert!(sd_host_can_idle(true, false, false, false));
        assert!(!sd_host_can_idle(false, false, false, false));
        assert!(!sd_host_can_idle(true, true, false, false));
        assert!(!sd_host_can_idle(true, false, true, false));
        assert_eq!(sd_clock_khz(true), SD_IDLE_CLOCK_KHZ);
    }

    #[test]
    fn wait_shrinks_for_audio_and_grows_when_idle() {
        let idle = WaitInput {
            voice_pump: false,
            reader_background: false,
            weread_busy: false,
            imu_due_ms: None,
            alarm_due_ms: 1_000,
            power_key_due_ms: 1_000,
            radio_stop_due_ms: Some(20_000),
            deep_sleep_due_ms: Some(600_000),
            status_due_ms: Some(30_000),
            weread_due_ms: None,
            auto_turn_due_ms: None,
        };
        let turning = WaitInput {
            auto_turn_due_ms: Some(40),
            ..idle
        };
        assert_eq!(next_block_ms(turning), 40);
        assert_eq!(next_block_ms(idle), 1_000);
        let pumping = WaitInput {
            voice_pump: true,
            ..idle
        };
        assert_eq!(next_block_ms(pumping), 10);
        assert_eq!(power_key_poll_ms(0), 100);
        assert_eq!(power_key_poll_ms(3_000), 1_000);
        assert_eq!(sd_clock_khz(true), SD_IDLE_CLOCK_KHZ);
        assert_eq!(sd_clock_khz(false), 10_000);
    }

    #[test]
    fn resume_record_roundtrips_and_wake_codes_match_idf() {
        let resume = SleepResume {
            route_code: 19,
            restore_reader: true,
            auto_sleep_minutes: 30,
        };
        assert_eq!(SleepResume::decode(&resume.encode()), Some(resume));
        assert_eq!(SleepResume::decode(&[0, 1, 2, 3]), None);
        assert_eq!(clamp_auto_sleep_minutes(0), DEFAULT_AUTO_DEEP_SLEEP_MINUTES);
        assert_eq!(cycle_auto_deep_sleep_minutes(10), 30);
        assert_eq!(cycle_auto_deep_sleep_minutes(60), 5);
        assert_eq!(classify_wake_cause(0), McuWake::PowerOn);
        assert_eq!(classify_wake_cause(3), McuWake::Button);
        assert_eq!(classify_wake_cause(4), McuWake::Timer);
    }

    #[test]
    fn alarm_delta_uses_civil_time() {
        assert_eq!(days_from_civil(1970, 1, 1), Some(0));
        let now = sample_time(8, 0, 0);
        let later = sample_time(8, 0, 30);
        assert_eq!(seconds_until(now, later), Some(30));
        assert_eq!(seconds_until(later, now), None);
    }

    #[test]
    fn battery_text_and_estimate_table_cover_the_sleep_states() {
        assert_eq!(format_battery(Some(3920), Some(72)), "3.92 V  72%");
        assert_eq!(format_battery(None, None), "no battery ADC");
        let labels: Vec<_> = CURRENT_DRAW_ESTIMATES.iter().map(|row| row.state).collect();
        assert!(labels.iter().any(|label| label.contains("between page")));
        assert!(labels.iter().any(|label| label.contains("Sleep image")));
        assert!(labels.iter().any(|label| label.contains("WeRead")));
        let snap = PowerDebugSnapshot::default();
        assert_eq!(snap.cpu_min_mhz, 80);
        assert_eq!(snap.cpu_max_mhz, 240);
        assert_eq!(snap.bluetooth, "off");
        assert!(snap.estimate.contains("mA"));
    }
}
