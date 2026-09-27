//! Hierarchical screen router for the portrait product UI shell.

/// Product screens exposed by the RustMix Wave shell.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ScreenRoute {
    #[default]
    Home,
    Reader,
    Productivity,
    Games,
    Tools,
    Settings,
    ContinueReading,
    Library,
    Bookmarks,
    ReaderBookmarks,
    ReaderLoading,
    ReaderPage,
    ReaderOptions,
    ReaderPreferences,
    ReaderToc,
    WeRead,
    WeReadLogin,
    WeReadBook,
    WeReadToc,
    WeReadRead,
    WeReadNotes,
    WeReadDownload,
    Calendar,
    CalendarAgenda,
    CalendarEventDetails,
    CalendarEventEditor,
    CalendarDeleteConfirmation,
    VoiceNotes,
    VoiceNoteDetails,
    VoiceNoteRecording,
    Vocab,
    VocabSession,
    VocabStats,
    GamesTbd,
    LuaApps,
    LuaGame,
    LuaGameError,
    Files,
    Dictionary,
    Lexicon,
    LexiconEntry,
    LexiconSources,
    UnitConverter,
    Alarms,
    Audio,
    AudioDetails,
    Clock,
    ClockDetails,
    Display,
    PowerKeyMenu,
    DeviceInfo,
    DeviceInfoBoard,
    DeviceInfoRuntime,
    Environment,
    EnvironmentDetails,
    Motion,
    MotionEvents,
    MotionDetails,
    Network,
    NetworkDetails,
    WifiTransfer,
    WifiSetup,
    Weather,
    WeatherDetails,
    /// Battery, radio, and sleep estimate. Reached from Device Info.
    DeviceInfoPower,
    /// Shown when a Wi-Fi feature is opened without a station. BOOT cancels.
    WifiPrompt,
}

impl ScreenRoute {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Reader => "Reader",
            Self::Productivity => "Productivity",
            Self::Games => "Games",
            Self::Tools => "Tools",
            Self::Settings => "Settings",
            Self::ContinueReading => "Continue Reading",
            Self::Library => "Library",
            Self::Bookmarks => "Bookmarks",
            Self::ReaderBookmarks => "Reader Bookmarks",
            Self::ReaderLoading => "Opening Book",
            Self::ReaderPage => "Reader Page",
            Self::ReaderOptions => "Reader Options",
            Self::ReaderPreferences => "Reading Preferences",
            Self::ReaderToc => "Table of Contents",
            Self::WeRead => "WeRead",
            Self::WeReadLogin => "WeRead Sign In",
            Self::WeReadBook => "WeRead Book",
            Self::WeReadToc => "WeRead Contents",
            Self::WeReadRead => "WeRead Chapter",
            Self::WeReadNotes => "WeRead Notes",
            Self::WeReadDownload => "WeRead Download",
            Self::Calendar => "Calendar",
            Self::CalendarAgenda => "Daily Agenda",
            Self::CalendarEventDetails => "Calendar Event",
            Self::CalendarEventEditor => "Edit Calendar Event",
            Self::CalendarDeleteConfirmation => "Delete Calendar Event",
            Self::VoiceNotes => "Voice Notes",
            Self::VoiceNoteDetails => "Voice Note",
            Self::VoiceNoteRecording => "Record Voice Note",
            Self::Vocab => "Vocabulary",
            Self::VocabSession => "Vocabulary Session",
            Self::VocabStats => "Vocabulary Stats",
            Self::GamesTbd => "TBD",
            Self::LuaApps => "SD Lua Apps",
            Self::LuaGame => "Lua App",
            Self::LuaGameError => "Lua App Error",
            Self::Files => "File Browser",
            Self::Dictionary => "Dictionary",
            Self::Lexicon => "Lexicon",
            Self::LexiconEntry => "Lexicon Entry",
            Self::LexiconSources => "Lexicon Sources",
            Self::UnitConverter => "Unit Converter",
            Self::Alarms => "Alarms",
            Self::Audio => "Audio",
            Self::AudioDetails => "Audio details",
            Self::Clock => "Clock",
            Self::ClockDetails => "RTC details",
            Self::Display => "Display",
            Self::PowerKeyMenu => "Power Key Menu",
            Self::DeviceInfo => "Device Info",
            Self::DeviceInfoBoard => "Board services",
            Self::DeviceInfoRuntime => "Runtime services",
            Self::Environment => "Environment",
            Self::EnvironmentDetails => "Sensor details",
            Self::Motion => "Motion",
            Self::MotionEvents => "Motion events",
            Self::MotionDetails => "Motion details",
            Self::Network => "Network",
            Self::NetworkDetails => "Provisioning details",
            Self::WifiTransfer => "Wi-Fi Transfer",
            Self::WifiSetup => "Configure Wi-Fi",
            Self::Weather => "Weather",
            Self::WeatherDetails => "Weather details",
            Self::DeviceInfoPower => "Power",
            Self::WifiPrompt => "Wi-Fi Needed",
        }
    }

    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Reader => "reader",
            Self::Productivity => "productivity",
            Self::Games => "games",
            Self::Tools => "tools",
            Self::Settings => "settings",
            Self::ContinueReading => "continue-reading",
            Self::Library => "library",
            Self::Bookmarks => "bookmarks",
            Self::ReaderBookmarks => "reader-bookmarks",
            Self::ReaderLoading => "reader-loading",
            Self::ReaderPage => "reader-page",
            Self::ReaderOptions => "reader-options",
            Self::ReaderPreferences => "reader-preferences",
            Self::ReaderToc => "reader-toc",
            Self::WeRead => "weread",
            Self::WeReadLogin => "weread-login",
            Self::WeReadBook => "weread-book",
            Self::WeReadToc => "weread-toc",
            Self::WeReadRead => "weread-read",
            Self::WeReadNotes => "weread-notes",
            Self::WeReadDownload => "weread-download",
            Self::Calendar => "calendar",
            Self::CalendarAgenda => "calendar-agenda",
            Self::CalendarEventDetails => "calendar-event-details",
            Self::CalendarEventEditor => "calendar-event-editor",
            Self::CalendarDeleteConfirmation => "calendar-delete-confirmation",
            Self::VoiceNotes => "voice-notes",
            Self::VoiceNoteDetails => "voice-note-details",
            Self::VoiceNoteRecording => "voice-note-recording",
            Self::Vocab => "vocab",
            Self::VocabSession => "vocab-session",
            Self::VocabStats => "vocab-stats",
            Self::GamesTbd => "games-tbd",
            Self::LuaApps => "lua-apps",
            Self::LuaGame => "lua-game",
            Self::LuaGameError => "lua-game-error",
            Self::Files => "file-browser",
            Self::Dictionary => "dictionary",
            Self::Lexicon => "lexicon",
            Self::LexiconEntry => "lexicon-entry",
            Self::LexiconSources => "lexicon-sources",
            Self::UnitConverter => "unit-converter",
            Self::Alarms => "alarms",
            Self::Audio => "audio",
            Self::AudioDetails => "audio-details",
            Self::Clock => "clock",
            Self::ClockDetails => "rtc-details",
            Self::Display => "display",
            Self::PowerKeyMenu => "power-key-menu",
            Self::DeviceInfo => "device-info",
            Self::DeviceInfoBoard => "device-info-board",
            Self::DeviceInfoRuntime => "device-info-runtime",
            Self::Environment => "environment",
            Self::EnvironmentDetails => "environment-details",
            Self::Motion => "motion",
            Self::MotionEvents => "motion-events",
            Self::MotionDetails => "motion-details",
            Self::Network => "network",
            Self::NetworkDetails => "network-details",
            Self::WifiTransfer => "wifi-transfer",
            Self::WifiSetup => "wifi-setup",
            Self::Weather => "weather",
            Self::WeatherDetails => "weather-details",
            Self::DeviceInfoPower => "device-info-power",
            Self::WifiPrompt => "wifi-prompt",
        }
    }

    #[must_use]
    pub const fn is_category(self) -> bool {
        matches!(
            self,
            Self::Reader | Self::Productivity | Self::Games | Self::Tools | Self::Settings
        )
    }

    #[must_use]
    pub const fn is_placeholder(self) -> bool {
        matches!(self, Self::GamesTbd)
    }

    #[must_use]
    pub const fn parent(self) -> Option<Self> {
        match self {
            Self::Home => None,
            Self::Reader | Self::Productivity | Self::Games | Self::Tools | Self::Settings => {
                Some(Self::Home)
            }
            Self::ContinueReading | Self::Library | Self::Bookmarks | Self::WeRead => {
                Some(Self::Reader)
            }
            Self::WeReadLogin | Self::WeReadBook => Some(Self::WeRead),
            Self::WeReadToc | Self::WeReadRead | Self::WeReadNotes | Self::WeReadDownload => {
                Some(Self::WeReadBook)
            }
            Self::ReaderBookmarks => Some(Self::ReaderOptions),
            Self::ReaderLoading | Self::ReaderPage => Some(Self::Library),
            Self::ReaderOptions => Some(Self::ReaderPage),
            Self::ReaderPreferences => Some(Self::ReaderOptions),
            Self::ReaderToc => Some(Self::ReaderOptions),
            Self::Calendar | Self::VoiceNotes | Self::Vocab => Some(Self::Productivity),
            Self::CalendarAgenda => Some(Self::Calendar),
            Self::CalendarEventDetails => Some(Self::CalendarAgenda),
            Self::CalendarEventEditor => Some(Self::CalendarAgenda),
            Self::CalendarDeleteConfirmation => Some(Self::CalendarEventDetails),
            Self::VoiceNoteDetails | Self::VoiceNoteRecording => Some(Self::VoiceNotes),
            Self::VocabSession | Self::VocabStats => Some(Self::Vocab),
            Self::GamesTbd | Self::LuaApps => Some(Self::Games),
            Self::LuaGame | Self::LuaGameError => Some(Self::LuaApps),
            Self::Files | Self::Dictionary | Self::Lexicon | Self::UnitConverter => {
                Some(Self::Tools)
            }
            Self::LexiconEntry | Self::LexiconSources => Some(Self::Lexicon),
            Self::PowerKeyMenu => Some(Self::Home),
            Self::Alarms
            | Self::Audio
            | Self::Clock
            | Self::Display
            | Self::DeviceInfo
            | Self::Environment
            | Self::Motion
            | Self::Network
            | Self::Weather => Some(Self::Settings),
            Self::AudioDetails => Some(Self::Audio),
            Self::ClockDetails => Some(Self::Clock),
            Self::DeviceInfoBoard => Some(Self::DeviceInfo),
            Self::DeviceInfoRuntime => Some(Self::DeviceInfoBoard),
            Self::DeviceInfoPower => Some(Self::DeviceInfoRuntime),
            Self::EnvironmentDetails => Some(Self::Environment),
            Self::MotionEvents => Some(Self::Motion),
            Self::MotionDetails => Some(Self::MotionEvents),
            Self::NetworkDetails | Self::WifiTransfer | Self::WifiSetup => Some(Self::Network),
            Self::WeatherDetails => Some(Self::Weather),
            Self::WifiPrompt => Some(Self::Home),
        }
    }

    #[must_use]
    pub const fn is_weread(self) -> bool {
        matches!(
            self,
            Self::WeRead
                | Self::WeReadLogin
                | Self::WeReadBook
                | Self::WeReadToc
                | Self::WeReadRead
                | Self::WeReadNotes
                | Self::WeReadDownload
        )
    }

    #[must_use]
    pub const fn uses_live_status(self) -> bool {
        matches!(
            self,
            Self::Clock
                | Self::ClockDetails
                | Self::Environment
                | Self::EnvironmentDetails
                | Self::Motion
                | Self::MotionDetails
                | Self::Network
                | Self::NetworkDetails
                | Self::WifiTransfer
                | Self::WifiSetup
                | Self::Alarms
                | Self::Calendar
                | Self::CalendarAgenda
                | Self::ReaderLoading
                | Self::VoiceNoteRecording
                | Self::DeviceInfoPower
        )
    }

    /// Stable code stored in RTC memory and NVS before deep sleep.
    #[must_use]
    pub const fn sleep_code(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_sleep_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Self::Home,
            1 => Self::Reader,
            2 => Self::Productivity,
            3 => Self::Games,
            4 => Self::Tools,
            5 => Self::Settings,
            6 => Self::ContinueReading,
            7 => Self::Library,
            8 => Self::Bookmarks,
            9 => Self::ReaderBookmarks,
            10 => Self::ReaderLoading,
            11 => Self::ReaderPage,
            12 => Self::ReaderOptions,
            13 => Self::ReaderPreferences,
            14 => Self::ReaderToc,
            15 => Self::WeRead,
            16 => Self::WeReadLogin,
            17 => Self::WeReadBook,
            18 => Self::WeReadToc,
            19 => Self::WeReadRead,
            20 => Self::WeReadNotes,
            21 => Self::WeReadDownload,
            22 => Self::Calendar,
            23 => Self::CalendarAgenda,
            24 => Self::CalendarEventDetails,
            25 => Self::CalendarEventEditor,
            26 => Self::CalendarDeleteConfirmation,
            27 => Self::VoiceNotes,
            28 => Self::VoiceNoteDetails,
            29 => Self::VoiceNoteRecording,
            30 => Self::Vocab,
            31 => Self::VocabSession,
            32 => Self::VocabStats,
            33 => Self::GamesTbd,
            34 => Self::LuaApps,
            35 => Self::LuaGame,
            36 => Self::LuaGameError,
            37 => Self::Files,
            38 => Self::Dictionary,
            39 => Self::Lexicon,
            40 => Self::LexiconEntry,
            41 => Self::LexiconSources,
            42 => Self::UnitConverter,
            43 => Self::Alarms,
            44 => Self::Audio,
            45 => Self::AudioDetails,
            46 => Self::Clock,
            47 => Self::ClockDetails,
            48 => Self::Display,
            49 => Self::PowerKeyMenu,
            50 => Self::DeviceInfo,
            51 => Self::DeviceInfoBoard,
            52 => Self::DeviceInfoRuntime,
            53 => Self::Environment,
            54 => Self::EnvironmentDetails,
            55 => Self::Motion,
            56 => Self::MotionEvents,
            57 => Self::MotionDetails,
            58 => Self::Network,
            59 => Self::NetworkDetails,
            60 => Self::WifiTransfer,
            61 => Self::WifiSetup,
            62 => Self::Weather,
            63 => Self::WeatherDetails,
            64 => Self::DeviceInfoPower,
            65 => Self::WifiPrompt,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScreenRouter {
    current: ScreenRoute,
}

impl ScreenRouter {
    #[must_use]
    pub const fn current(self) -> ScreenRoute {
        self.current
    }

    pub fn navigate_to(&mut self, route: ScreenRoute) {
        self.current = route;
    }

    pub fn back(&mut self) {
        self.current = self.current.parent().unwrap_or(ScreenRoute::Home);
    }

    pub fn back_home(&mut self) {
        self.current = ScreenRoute::Home;
    }
}

#[cfg(test)]
mod tests {
    use super::{ScreenRoute, ScreenRouter};

    #[test]
    fn router_exposes_static_parent_hierarchy() {
        assert_eq!(ScreenRoute::Files.parent(), Some(ScreenRoute::Tools));
        assert_eq!(ScreenRoute::Display.parent(), Some(ScreenRoute::Settings));
        assert_eq!(ScreenRoute::PowerKeyMenu.parent(), Some(ScreenRoute::Home));
        assert_eq!(
            ScreenRoute::Calendar.parent(),
            Some(ScreenRoute::Productivity)
        );
        assert_eq!(
            ScreenRoute::CalendarAgenda.parent(),
            Some(ScreenRoute::Calendar)
        );
        assert_eq!(
            ScreenRoute::CalendarEventDetails.parent(),
            Some(ScreenRoute::CalendarAgenda)
        );
        assert_eq!(
            ScreenRoute::CalendarEventEditor.parent(),
            Some(ScreenRoute::CalendarAgenda)
        );
        assert_eq!(
            ScreenRoute::CalendarDeleteConfirmation.parent(),
            Some(ScreenRoute::CalendarEventDetails)
        );
        assert_eq!(
            ScreenRoute::UnitConverter.parent(),
            Some(ScreenRoute::Tools)
        );
        assert!(!ScreenRoute::UnitConverter.is_placeholder());
        assert!(!ScreenRoute::Dictionary.is_placeholder());
        assert_eq!(ScreenRoute::AudioDetails.parent(), Some(ScreenRoute::Audio));
        assert_eq!(
            ScreenRoute::DeviceInfoRuntime.parent(),
            Some(ScreenRoute::DeviceInfoBoard)
        );
        assert_eq!(ScreenRoute::Reader.parent(), Some(ScreenRoute::Home));
        assert_eq!(ScreenRoute::LuaApps.parent(), Some(ScreenRoute::Games));
        assert_eq!(ScreenRoute::LuaGame.parent(), Some(ScreenRoute::LuaApps));
        assert_eq!(
            ScreenRoute::LuaGameError.parent(),
            Some(ScreenRoute::LuaApps)
        );
        assert_eq!(
            ScreenRoute::WifiTransfer.parent(),
            Some(ScreenRoute::Network)
        );
        assert_eq!(ScreenRoute::WifiSetup.parent(), Some(ScreenRoute::Network));
        assert_eq!(ScreenRoute::Home.parent(), None);
        assert_eq!(ScreenRoute::Lexicon.parent(), Some(ScreenRoute::Tools));
        assert_eq!(
            ScreenRoute::LexiconEntry.parent(),
            Some(ScreenRoute::Lexicon)
        );
        assert_eq!(
            ScreenRoute::LexiconSources.parent(),
            Some(ScreenRoute::Lexicon)
        );
        assert_eq!(ScreenRoute::WeRead.parent(), Some(ScreenRoute::Reader));
        assert_eq!(ScreenRoute::WeReadLogin.parent(), Some(ScreenRoute::WeRead));
        assert_eq!(
            ScreenRoute::WeReadRead.parent(),
            Some(ScreenRoute::WeReadBook)
        );
        assert!(ScreenRoute::WeReadRead.is_weread());
        assert_eq!(
            ScreenRoute::DeviceInfoPower.parent(),
            Some(ScreenRoute::DeviceInfoRuntime)
        );
        assert_eq!(
            ScreenRoute::from_sleep_code(ScreenRoute::ReaderPage.sleep_code()),
            Some(ScreenRoute::ReaderPage)
        );
        assert_eq!(
            ScreenRoute::from_sleep_code(ScreenRoute::WeReadRead.sleep_code()),
            Some(ScreenRoute::WeReadRead)
        );
        assert_eq!(
            ScreenRoute::from_sleep_code(ScreenRoute::DeviceInfoPower.sleep_code()),
            Some(ScreenRoute::DeviceInfoPower)
        );
        assert_eq!(ScreenRoute::WeReadRead.sleep_code(), 19);
        assert_eq!(ScreenRoute::Vocab.parent(), Some(ScreenRoute::Productivity));
        assert_eq!(ScreenRoute::VocabSession.parent(), Some(ScreenRoute::Vocab));
        assert_eq!(ScreenRoute::VocabStats.parent(), Some(ScreenRoute::Vocab));
    }

    #[test]
    fn back_returns_details_to_overview_then_category_then_home() {
        let mut router = ScreenRouter::default();
        router.navigate_to(ScreenRoute::Settings);
        router.navigate_to(ScreenRoute::Audio);
        router.navigate_to(ScreenRoute::AudioDetails);
        router.back();
        assert_eq!(router.current(), ScreenRoute::Audio);
        router.back();
        assert_eq!(router.current(), ScreenRoute::Settings);
        router.back();
        assert_eq!(router.current(), ScreenRoute::Home);
    }
}
