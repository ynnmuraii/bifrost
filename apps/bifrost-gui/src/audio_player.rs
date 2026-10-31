//! Provisional presentation boundary for a future AudioStatus IPC adapter.
//!
//! Stream identities must be stable opaque core identities, never display names. The adapter
//! supplies negotiated media permissions and command support; local gain/mute are Listener-only.

/// Recognised media application; unknown applications use the system-audio glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppKind {
    #[default]
    System,
    YouTube,
}

/// Stream health, independent of playback and Listener mute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamState {
    #[default]
    Ready,
    Buffering,
    Degraded,
    Failed,
}

/// One Source stream or diagnosed failed Listener path, populated only from authoritative status.
#[derive(Clone, PartialEq, Eq)]
pub struct AudioPlayerCard {
    pub stream_id: String,
    pub source_name: String,
    pub title: Option<String>,
    pub subtitle: Option<String>,
    /// A media session can exist even when the application supplies no title.
    pub has_media_session: bool,
    pub app: AppKind,
    pub position_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub playing: bool,
    /// Permission AND session command support, not just application detection.
    pub can_prev: bool,
    pub can_play_pause: bool,
    pub can_next: bool,
    pub muted: bool,
    pub gain_percent: u8,
    pub state: StreamState,
    pub path: Option<crate::audio_path::AudioPathObservation>,
    /// Follow Focus background streams remain visible, with reduced emphasis.
    pub background: bool,
}

impl std::fmt::Debug for AudioPlayerCard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioPlayerCard")
            .field("stream_id", &self.stream_id)
            .field("metadata", &"[REDACTED]")
            .field("state", &self.state)
            .field("muted", &self.muted)
            .finish()
    }
}

/// Intent, not optimistic playback state. The IPC adapter must recheck permissions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioPlayerAction {
    Previous(String),
    PlayPause(String),
    Next(String),
    SetMuted { stream_id: String, muted: bool },
    SetGain { stream_id: String, percent: u8 },
    Stop(String),
    OpenSettings,
    HidePlayer,
}

/// GUI-only popup lifecycle. Positions intentionally are not persisted: gui.toml has no
/// existing native-window geometry convention.
#[derive(Debug, Clone, Default)]
pub struct AudioPlayer {
    pub cards: Vec<AudioPlayerCard>,
    pub open: bool,
    had_streams: bool,
}

impl AudioPlayer {
    /// Replace a complete authoritative snapshot. Only an empty-to-active transition auto-opens.
    pub fn replace_cards(&mut self, cards: Vec<AudioPlayerCard>) {
        let active = !cards.is_empty();
        if active && !self.had_streams {
            self.open = true;
        }
        self.had_streams = active;
        self.cards = cards;
    }
}
