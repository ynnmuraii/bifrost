//! Remote-audio presentation vocabulary and GUI-only popup lifecycle.
//!
//! Stream identities must be stable opaque core identities, never display names. The adapter
//! supplies negotiated media permissions and command support; local gain/mute are Listener-only.

/// Authoritative Listener policy and whether this frame may change it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlayerMode {
    pub current: Option<protocol::ipc::AudioMode>,
    pub enabled: bool,
}

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

/// Current authorized media identity and native seek range.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AudioSeekTarget {
    pub session_token: u64,
    pub media_revision: u64,
    pub min_ms: u64,
    pub max_ms: u64,
}

impl std::fmt::Debug for AudioSeekTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioSeekTarget")
            .field("session_token", &self.session_token)
            .field("media_revision", &self.media_revision)
            .field("range", &"[REDACTED]")
            .finish()
    }
}

/// One decoded cover thumbnail, keyed by the authoritative image hash. The pixels are decoded
/// off the render thread and bounded by the IPC contract (<= 192x192); the texture is uploaded
/// once per hash, not per frame or per media revision.
#[derive(Clone, PartialEq, Eq)]
pub struct Artwork {
    pub id: [u8; 32],
    pub rgba: std::sync::Arc<[u8]>,
    pub width: u32,
    pub height: u32,
}

impl std::fmt::Debug for Artwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Artwork")
            .field("pixels", &"[REDACTED]")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

/// One Source stream or diagnosed failed Listener path, populated only from authoritative status.
#[derive(Clone, PartialEq, Eq)]
pub struct AudioPlayerCard {
    pub stream_id: String,
    pub source_name: String,
    pub title: Option<String>,
    pub subtitle: Option<String>,
    /// The authoritative image hash: `None` until the core's cache holds a bounded image.
    pub artwork_id: Option<[u8; 32]>,
    /// Decoded pixels for `artwork_id`, when this GUI has them.
    pub artwork: Option<Artwork>,
    /// A media session can exist even when the application supplies no title.
    pub has_media_session: bool,
    pub app: AppKind,
    pub position_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub seek: Option<AudioSeekTarget>,
    pub playing: bool,
    /// Permission AND session command support, not just application detection.
    pub can_prev: bool,
    pub can_play_pause: bool,
    pub can_next: bool,
    pub muted: bool,
    pub gain_percent: u8,
    /// Latest local slider intent; never a claim about confirmed output.
    pub desired_gain_percent: Option<u8>,
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
#[derive(Clone, PartialEq, Eq)]
pub enum AudioPlayerAction {
    Previous(String),
    PlayPause(String),
    Next(String),
    Seek {
        stream_id: String,
        target: AudioSeekTarget,
        position_ms: u64,
    },
    SetMuted {
        stream_id: String,
        muted: bool,
    },
    SetGain {
        stream_id: String,
        percent: u8,
    },
    Stop(String),
    OpenSettings,
    /// Listener-wide mixing policy; independent of a card's media session.
    SetMode(protocol::ipc::AudioMode),
    HidePlayer,
    /// The expand button of the player header: a GUI geometry change, never a core command.
    ToggleExpanded,
}

impl std::fmt::Debug for AudioPlayerAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Previous(id) => f.debug_tuple("Previous").field(id).finish(),
            Self::PlayPause(id) => f.debug_tuple("PlayPause").field(id).finish(),
            Self::Next(id) => f.debug_tuple("Next").field(id).finish(),
            Self::Seek {
                stream_id, target, ..
            } => f
                .debug_struct("Seek")
                .field("stream_id", stream_id)
                .field("target", target)
                .field("position", &"[REDACTED]")
                .finish(),
            Self::SetMuted { stream_id, muted } => f
                .debug_struct("SetMuted")
                .field("stream_id", stream_id)
                .field("muted", muted)
                .finish(),
            Self::SetGain { stream_id, percent } => f
                .debug_struct("SetGain")
                .field("stream_id", stream_id)
                .field("percent", percent)
                .finish(),
            Self::Stop(id) => f.debug_tuple("Stop").field(id).finish(),
            Self::OpenSettings => f.write_str("OpenSettings"),
            Self::SetMode(mode) => f.debug_tuple("SetMode").field(mode).finish(),
            Self::HidePlayer => f.write_str("HidePlayer"),
            Self::ToggleExpanded => f.write_str("ToggleExpanded"),
        }
    }
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
    /// Auto-open only after a worker-observed active stream, never a pending or failed path.
    /// Rebuffering does not reopen a player the user already hid for this stream.
    pub fn replace_cards(&mut self, cards: Vec<AudioPlayerCard>, observed_active: bool) {
        if observed_active && !self.had_streams {
            self.open = true;
        }
        self.had_streams = !cards.is_empty() && (self.had_streams || observed_active);
        self.cards = cards;
    }
}
