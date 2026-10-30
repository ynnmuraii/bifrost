//! Small control-plane contract for native audio adapters, independent of media workers.
//!
//! Construct callbacks and all their storage before starting a stream. Native real-time callbacks
//! MUST NOT allocate, block, acquire locks, log, access the network, or initialize lazy resources.
//! They transfer interleaved f32 PCM only. Actual native rates/channels, not the requested format,
//! are authoritative: media workers must resample off the real-time thread.
//! Borrowed callbacks let native workers retain their packet buffers without forcing a PCM
//! allocation, copy, lock or ring implementation into the trait. Callers may transfer samples
//! into their own preallocated SPSC ring; callback construction happens off the audio thread.
//!
//! Device/default notifications invalidate routing decisions; an open stream stays pinned to its
//! resolved endpoint. Callers stop and recreate default-selected streams after a default change.
//! Enumeration never opens capture devices or changes system defaults. No method grants consent.

use std::fmt;

pub mod dry;

pub const TARGET_SAMPLE_RATE: u32 = 48_000;
pub const EVENT_CAPACITY: usize = 64;
pub const MAX_DEVICE_ID_BYTES: usize = 512;
pub const MAX_DEVICE_LABEL_BYTES: usize = 256;
/// Hard negotiated/native format ceilings: at most 960 * 32 f32 samples per capture block.
pub const MAX_CHANNELS: u16 = 32;
pub const MAX_SAMPLE_RATE: u32 = 384_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendError {
    Unsupported,
    DeviceGone,
    ServerUnavailable,
    PermissionDenied,
    InvalidArgument,
    Busy,
    StaleRevision,
    OwnershipLost,
    Stopped,
    Timeout,
    Cancelled,
    Conflict,
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for BackendError {}

/// Stable backend endpoint identity, never a localized label or transient enumeration index.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(String);

impl DeviceId {
    pub fn new(value: impl Into<String>) -> Result<Self, BackendError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_DEVICE_ID_BYTES || value.contains('\0') {
            return Err(BackendError::InvalidArgument);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Capture,
    Render,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

impl AudioFormat {
    pub fn validate(self) -> Result<Self, BackendError> {
        if self.sample_rate == 0
            || self.sample_rate > MAX_SAMPLE_RATE
            || self.channels == 0
            || self.channels > MAX_CHANNELS
        {
            return Err(BackendError::InvalidArgument);
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub id: DeviceId,
    pub label: String,
    pub direction: Direction,
    pub format: AudioFormat,
}

impl Device {
    pub fn validate(&self) -> Result<(), BackendError> {
        self.format.validate()?;
        if self.label.len() > MAX_DEVICE_LABEL_BYTES || self.label.contains('\0') {
            return Err(BackendError::InvalidArgument);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultRole {
    Multimedia,
    Communications,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DefaultEndpoints {
    pub render: Option<DeviceId>,
    pub capture: Option<DeviceId>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Defaults {
    pub multimedia: DefaultEndpoints,
    pub communications: DefaultEndpoints,
}

impl Defaults {
    pub fn for_role(&self, role: DefaultRole) -> &DefaultEndpoints {
        match role {
            DefaultRole::Multimedia => &self.multimedia,
            DefaultRole::Communications => &self.communications,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceSelection {
    Explicit(DeviceId),
    FollowDefault(DefaultRole),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedbackGuarantee {
    /// All render streams in this audio-role process tree are excluded, across endpoints.
    /// This does not prevent acoustic or other applications' monitoring feedback.
    OwnProcessTreeExcluded,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureSpec {
    /// Not endpoint-filtered. Native adapters target the current audio role and descendants.
    /// The excluded root is deliberately std::process::id() of the calling audio role,
    /// which owns every Bifrost render stream. No caller-selected PID is accepted.
    /// Caller must supply f32 stereo 48 kHz; activation failure is fatal, with no fallback.
    SystemExcludingOwnProcessTree { format: AudioFormat },
    EndpointLoopback {
        endpoint: DeviceSelection,
        format: AudioFormat,
    },
    Monitor {
        sink: DeviceSelection,
        format: AudioFormat,
    },
    Microphone {
        endpoint: DeviceSelection,
        format: AudioFormat,
    },
}

/// Borrowed interleaved PCM; frames are actual native frames, never assumed to be 480.
/// Timestamp is a worker-local monotonic nanosecond position of the first sample, not wall time
/// or a cross-PC synchronized clock. Native workers split oversized packets into bounded blocks.
#[derive(Clone, Copy)]
pub struct CaptureBlock<'a> {
    pub samples: &'a [f32],
    pub frames: usize,
    pub format: AudioFormat,
    pub timestamp_ns: u64,
    /// False means timestamp_ns is not usable; never infer validity from a zero value.
    pub timestamp_valid: bool,
    /// First sample's position in native frames, when the capture API supplies it.
    pub device_position: Option<u64>,
    pub discontinuity: bool,
}

impl fmt::Debug for CaptureBlock<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaptureBlock")
            .field("frames", &self.frames)
            .field("format", &self.format)
            .field("timestamp_ns", &self.timestamp_ns)
            .field("timestamp_valid", &self.timestamp_valid)
            .field("device_position", &self.device_position)
            .field("discontinuity", &self.discontinuity)
            .finish_non_exhaustive()
    }
}

pub const MAX_CAPTURE_BLOCK_FRAMES: usize = 960;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderRequest {
    pub device: DeviceSelection,
    pub requested: AudioFormat,
}

pub type CaptureCallback = Box<dyn for<'a> FnMut(CaptureBlock<'a>) + Send + 'static>;
pub type RenderCallback = Box<dyn FnMut(&mut [f32]) + Send + 'static>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamStatus {
    Running,
    Stopped,
    DeviceInvalidated,
    Failed(BackendError),
}

/// A control-plane snapshot of the native worker, not a request to query from RT code.
///
/// Position is in device-clock ticks and frequency is ticks/second. Native WASAPI
/// adapters retain IAudioClock units and its correlated QPC time; Linux adapters
/// convert Pulse timing_info indices to native frames and report sample_rate as
/// frequency. Host time is a correlated local monotonic clock in 100ns units.
/// None means unavailable/invalid; Some(0) is a valid starting position/time.
/// Dry reports deterministic frame position/sample_rate/time, with zero hardware
/// xrun counters because explicit pumping never loses a callback to scheduling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamSnapshot {
    pub status: StreamStatus,
    pub device_position: Option<u64>,
    pub clock_frequency: Option<u64>,
    pub host_time_100ns: Option<u64>,
    pub underrun_frames: u64,
    pub dropped_capture_frames: u64,
}

/// Owned stream. Implementations also stop on Drop. After stop returns (including an error),
/// no callback may be running or called again, and callback captures must have been released.
/// Stop is idempotent; it cancels native work and joins workers on this control thread.
pub trait Stream: Send {
    fn format(&self) -> AudioFormat;
    /// None for process-tree capture, which is not tied to an endpoint.
    fn device(&self) -> Option<&DeviceId>;
    fn feedback_guarantee(&self) -> FeedbackGuarantee;
    fn snapshot(&self) -> StreamSnapshot;
    fn stop(&mut self) -> Result<(), BackendError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MuteSnapshot {
    pub endpoint: DeviceId,
    /// Endpoint incarnation, not a metadata revision. Never reuse during a worker lifetime.
    pub generation: u64,
    pub muted: bool,
}

/// Endpoint-wide mute lease, not an application-local gain. Preserve the original state, never
/// clear a pre-existing mute, and restore only changes still owned by this lease on restore/Drop.
/// Native adapters must invalidate ownership on external changes, including same-value writes
/// when observable. If ownership cannot be proven, fail closed instead of undoing another owner.
pub trait MuteGuard: Send {
    fn original_muted(&self) -> bool;
    fn snapshot(&self) -> Result<MuteSnapshot, BackendError>;
    fn set_capture_mute(
        &mut self,
        expected_generation: u64,
        muted: bool,
    ) -> Result<(), BackendError>;
    /// Releases ownership, even on error. Subsequent writes must fail with Stopped.
    fn restore(&mut self) -> Result<(), BackendError>;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub system_capture: bool,
    pub exclude_own_process_tree: bool,
    pub endpoint_mute: bool,
    pub media: bool,
    /// True only if reliable global press AND release events are available.
    pub global_ptt: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    Available,
    Unavailable(BackendError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseObservation {
    Native,
    /// Explicit bounded key-state observation, not a synthetic key-up or sticky toggle.
    Polling {
        interval_ms: u16,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapabilityProbe {
    pub os_build: Option<u32>,
    pub process_exclude: Availability,
    pub endpoint_mute: Availability,
    pub media: Availability,
    pub ptt: Availability,
    pub ptt_release: Option<ReleaseObservation>,
}

/// Opaque local session identity, stable across metadata revisions, never an application ID.
/// Unique within a backend-worker generation; never carry tokens across worker replacement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MediaSessionToken(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaCommandRequest {
    pub request_id: u64,
    pub session: MediaSessionToken,
    pub expected_revision: u64,
    pub command: MediaCommand,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackState {
    Playing,
    Paused,
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaCommand {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MediaCommands {
    pub play: bool,
    pub pause: bool,
    pub toggle: bool,
    pub next: bool,
    pub previous: bool,
}

impl MediaCommands {
    pub fn contains(self, command: MediaCommand) -> bool {
        match command {
            MediaCommand::Play => self.play,
            MediaCommand::Pause => self.pause,
            MediaCommand::Toggle => self.toggle,
            MediaCommand::Next => self.next,
            MediaCommand::Previous => self.previous,
        }
    }
}

/// Bounded private user content. Never persist or log fields; Debug intentionally redacts them.
/// Share only inside an active, consented system-audio session. Token identifies the session;
/// revision must advance on each metadata/state change. Commands check both at execution time.
#[derive(Clone, PartialEq, Eq)]
pub struct MediaInfo {
    app: String,
    title: String,
    subtitle: String,
    pub session: MediaSessionToken,
    pub revision: u64,
    /// None when the native session does not expose a timeline; never invent a zero position.
    pub position_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub state: PlaybackState,
    pub commands: MediaCommands,
}

impl MediaInfo {
    pub fn new(
        app: String,
        title: String,
        subtitle: String,
        session: MediaSessionToken,
        revision: u64,
        state: PlaybackState,
        commands: MediaCommands,
    ) -> Result<Self, BackendError> {
        if app.len() > 128 || title.len() > 256 || subtitle.len() > 256 {
            return Err(BackendError::InvalidArgument);
        }
        Ok(Self {
            app,
            title,
            subtitle,
            session,
            revision,
            position_ms: None,
            duration_ms: None,
            state,
            commands,
        })
    }

    pub fn app(&self) -> &str {
        &self.app
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn subtitle(&self) -> &str {
        &self.subtitle
    }
}

impl fmt::Debug for MediaInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MediaInfo")
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

/// USB HID Keyboard/Keypad usage-page 0x07 key plus independent modifier flags.
/// The conservative binding vocabulary accepts non-modifier usage IDs 0x04..=0x73 (A..F24).
/// Never pass Windows VK, XKB, scan-code or character values. Adapters translate HID usages and
/// reject unmappable keys, conflicts or unavailable release observation rather than stealing keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PttHotkey {
    pub key_code: u32,
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

impl PttHotkey {
    pub fn validate(self) -> Result<Self, BackendError> {
        if !(0x04..=0x73).contains(&self.key_code) {
            return Err(BackendError::InvalidArgument);
        }
        Ok(self)
    }
}

pub const MEDIA_COMMAND_TIMEOUT_MS: u32 = 1000;

/// Native PTT leases expire after 250ms; renew at intervals no greater than half.
pub const PTT_LEASE_NS: u64 = 250_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceEvent {
    Added(DeviceId),
    Removed(DeviceId),
    DefaultChanged {
        direction: Direction,
        role: DefaultRole,
    },
    FormatChanged(DeviceId),
    Invalidated(DeviceId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Device(DeviceEvent),
    /// Includes external same-value writes when observable, never only our requested state.
    MuteChanged(MuteSnapshot),
    MediaChanged {
        session: MediaSessionToken,
        info: Option<MediaInfo>,
    },
    MediaCommandCompleted {
        request_id: u64,
        result: Result<(), BackendError>,
    },
    /// Absolute host-monotonic times in nanoseconds (QPC-derived on Windows,
    /// CLOCK_MONOTONIC on Linux; Dry uses explicitly advanced logical time).
    /// Consumers MUST check expiry at use time, including after queue/control stalls.
    PttPressed {
        observed_at_ns: u64,
        expires_at_ns: u64,
    },
    /// Renew only an already-held, unexpired press; this event can never reopen
    /// a released/expired gate. Native-release backends must renew too, at <=lease/2.
    PttRenewed {
        expires_at_ns: u64,
    },
    PttReleased,
    /// Queue overflow: refresh device/default/mute/media snapshots and revoke all PTT grants.
    ResyncRequired,
}

/// Control-thread interface; associated owned handles allow platform-specific lifetimes without
/// exposing media internals or conditional compilation. No platform adapter depends on audio-media.
/// Poll queues are bounded; overflow must be explicit, never an invisible lost release event.
pub trait Backend {
    type Stream: Stream;
    type Mute: MuteGuard;

    fn capabilities(&self) -> Capabilities;
    /// Explicit opt-in runtime probe; may open native devices. Never invoke implicitly at
    /// construction, enumeration, cached capability reads or while audio is disabled.
    fn probe_capabilities(&mut self) -> CapabilityProbe;
    fn devices(&self) -> Result<Vec<Device>, BackendError>;
    fn defaults(&self) -> Result<Defaults, BackendError>;
    fn start_capture(
        &mut self,
        request: CaptureSpec,
        callback: CaptureCallback,
    ) -> Result<Self::Stream, BackendError>;
    fn start_render(
        &mut self,
        request: RenderRequest,
        callback: RenderCallback,
    ) -> Result<Self::Stream, BackendError>;
    fn capture_mute(&self, selection: &DeviceSelection) -> Result<MuteSnapshot, BackendError>;
    fn acquire_mute(&mut self, selection: &DeviceSelection) -> Result<Self::Mute, BackendError>;
    /// Empty is a valid successful result, not an unavailable media subsystem.
    fn media_sessions(&self) -> Result<Vec<MediaInfo>, BackendError>;
    /// Bounded asynchronous admission. Every admitted request yields exactly one completion;
    /// Busy means not admitted. Native workers finish within MEDIA_COMMAND_TIMEOUT_MS after
    /// admission or report Timeout and cancel remaining work. Timeout is an unknown native outcome,
    /// not proof that an already dispatched OS command had no effect; never retry automatically.
    /// Session disappearance cancels its pending commands with DeviceGone; worker loss cancels all
    /// pending commands with ServerUnavailable. No late successful completion may follow either.
    /// Caller separately enforces remote-control permission. Dry completes only on explicit pump
    /// or scripted timeout, with no hidden wall clock.
    fn media_command(&mut self, request: MediaCommandRequest) -> Result<(), BackendError>;
    /// Explicitly cancel every pending request, retaining one Cancelled completion per request.
    /// Completed requests remain available for polling; no cancelled request may execute later.
    fn cancel_media_commands(&mut self);
    /// None unregisters and revokes a held press. Unsupported global release semantics must
    /// return Unsupported, not emulate release with a toggle or timer.
    /// Reject a chord matching reserved switch/emergency/game-guard hotkeys with
    /// Conflict, without changing an existing binding or held-press state.
    fn configure_ptt(
        &mut self,
        hotkey: Option<PttHotkey>,
        reserved: &[PttHotkey],
    ) -> Result<(), BackendError>;
    fn poll_event(&mut self) -> Option<Event>;
}
