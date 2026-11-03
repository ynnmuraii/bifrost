//! Authoritative audio IPC adapter. Pending intents never change playback or privacy state.
use crate::audio_player::{
    AppKind, Artwork, AudioPlayerAction, AudioPlayerCard, AudioSeekTarget, StreamState,
};
use crate::view_model::{Modal, Request, ViewModel};
use audio_model::status as model;
use protocol::ipc::{self, gui_frame::Msg};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use ui_text::{text, Key, Lang};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioRequest {
    Snapshot,
    Devices,
    Mutation,
    /// An urgent existing control must not own an outstanding system switch's ACK.
    AuxiliaryMutation,
    /// One cover fetch: an unavailable cache is an honest "no image", not an audio failure.
    Artwork,
}

/// The current-scope tuple a cover is requested and cached by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArtRequest {
    session_id: [u8; 16],
    token: u64,
    id: [u8; 32],
}

#[derive(Debug, Clone, PartialEq)]
pub enum AudioAction {
    EditConfig(Box<ipc::AudioConfig>),
    ApplyConfig,
    Refresh,
    Share(bool),
    Receive(bool),
    Retarget([u8; 32]),
    ListDevices(ipc::AudioDeviceKind),
    StopSystemAudio,
    SetMicrophoneMute(bool),
    SetPtt(bool),
    Player(AudioPlayerAction),
    OpenSettings,
}

type ArtDecodeJob = ([u8; 32], Vec<u8>);
type ArtDecodeResult = ([u8; 32], Option<Artwork>);

/// The off-thread cover decode channel, shared by clones of the view.
#[derive(Clone, Default)]
struct ArtWorker {
    inner: Arc<ArtWorkerInner>,
}

#[derive(Default)]
struct ArtWorkerInner {
    /// A bounded job queue: a hostile hash rotation cannot grow threads or memory without end.
    jobs: std::sync::Mutex<Option<mpsc::SyncSender<ArtDecodeJob>>>,
    results: std::sync::Mutex<Option<mpsc::Receiver<ArtDecodeResult>>>,
    /// Hashes handed to the one worker and not yet answered.
    inflight: std::sync::Mutex<Vec<[u8; 32]>>,
    wake: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl std::fmt::Debug for ArtWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ArtWorker(..)")
    }
}

/// The authoritative audio state plus the decoded-cover cache.
#[derive(Debug, Clone)]
pub struct AudioView {
    pub status: Option<ipc::AudioStatus>,
    pub draft: ipc::AudioConfig,
    pub devices: Vec<ipc::AudioDevices>,
    pub pending: bool,
    pub available: bool,
    pub error: Option<String>,
    pending_id: Option<u64>,
    pending_revision: Option<u64>,
    pending_since: Option<Instant>,
    pending_snapshot_id: Option<u64>,
    pending_target: Option<[u8; 32]>,
    pending_mode: Option<i32>,
    pub retargeting: bool,
    /// Pending switch intent, not an indication of active capture or playback.
    pub system_intent: Option<(ipc::AudioRole, bool)>,
    device_requests: Vec<(u64, i32)>,
    pub dirty: bool,
    /// Ownership of a GUI hold button, separate from authoritative/global PTT state.
    pub gui_ptt_held: bool,
    /// Decoded covers, keyed by the authoritative hash; bounded by the cards on screen.
    pub artwork: Vec<Artwork>,
    /// Tuples already asked for once; pruned when the scope/hash leaves the status.
    art_requested: Vec<ArtRequest>,
    /// Local request ids for tuples, so a reply without a usable frame id can still retire it.
    art_inflight: Vec<(u64, ArtRequest)>,
    art_worker: ArtWorker,
}

impl Default for AudioView {
    fn default() -> Self {
        Self {
            status: None,
            draft: default_config(),
            devices: Vec::new(),
            pending: false,
            available: false,
            error: None,
            pending_id: None,
            pending_revision: None,
            pending_since: None,
            pending_snapshot_id: None,
            pending_target: None,
            pending_mode: None,
            retargeting: false,
            system_intent: None,
            device_requests: Vec::new(),
            dirty: false,
            gui_ptt_held: false,
            artwork: Vec::new(),
            art_requested: Vec::new(),
            art_inflight: Vec::new(),
            art_worker: ArtWorker::default(),
        }
    }
}

pub fn default_config() -> ipc::AudioConfig {
    ipc::AudioConfig {
        enabled: false,
        role: ipc::AudioRole::Source as i32,
        group_id_override: Some(false),
        mode: ipc::AudioMode::MixAll as i32,
        profile: ipc::AudioProfile::Ethernet as i32,
        udp_port: 47903,
        output_device: "default".into(),
        capture_device: "default_monitor".into(),
        focus_debounce_ms: 250,
        focus_ramp_ms: 150,
        background_gain_db: -18,
        microphone: Some(ipc::AudioMicrophoneConfig {
            capture_device: "default".into(),
            muted: true,
            ..Default::default()
        }),
        ..Default::default()
    }
}

impl AudioView {
    pub fn player_mode(&self, connected: bool) -> crate::audio_player::PlayerMode {
        let status = self.status.as_ref();
        let config = status.and_then(|status| status.config.as_ref());
        let current = config
            .and_then(|config| ipc::AudioMode::try_from(config.mode).ok())
            .filter(|mode| matches!(mode, ipc::AudioMode::MixAll | ipc::AudioMode::FollowFocus));
        crate::audio_player::PlayerMode {
            current,
            enabled: connected
                && self.available
                && !self.pending
                && current.is_some()
                && status.is_some_and(|status| status.role == ipc::AudioRole::Listener as i32)
                && config.is_some_and(|config| config.role == ipc::AudioRole::Listener as i32),
        }
    }
    fn settle(&mut self) {
        self.pending = false;
        self.pending_id = None;
        self.pending_revision = None;
        self.pending_since = None;
        self.pending_snapshot_id = None;
        self.pending_target = None;
        self.pending_mode = None;
        self.retargeting = false;
        self.system_intent = None;
    }
    fn fail(&mut self, lang: Lang, key: Key) {
        let preserve_draft = self.pending_mode.is_some();
        self.settle();
        self.error = Some(text(lang, key).into());
        if preserve_draft {
            return;
        }
        if let Some(config) = self.status.as_ref().and_then(|s| s.config.as_ref()) {
            self.draft = config.clone();
        }
        self.dirty = false;
    }
    /// The decoded cover for `id`, if this window already holds it.
    pub fn artwork_for(&self, id: [u8; 32]) -> Option<Artwork> {
        self.artwork.iter().find(|art| art.id == id).cloned()
    }

    /// The decoded pixels are read by the GUI thread; the decode itself never runs there.
    pub fn set_art_wake(&mut self, wake: Arc<dyn Fn() + Send + Sync>) {
        *self.art_worker.inner.wake.lock().expect("art wake") = Some(wake);
    }

    /// Keeps the cache and the asked-for tuples at exactly the status's current scope, and
    /// returns the tuples that still need one request. Position revisions never re-request.
    fn plan_artwork(&mut self, wanted: &[ArtRequest]) -> Vec<ArtRequest> {
        self.art_requested
            .retain(|request| wanted.contains(request));
        self.artwork
            .retain(|art| wanted.iter().any(|request| request.id == art.id));
        let mut fresh = Vec::new();
        for request in wanted {
            if self.artwork.iter().any(|art| art.id == request.id)
                || self.art_requested.contains(request)
            {
                continue;
            }
            self.art_requested.push(*request);
            fresh.push(*request);
        }
        fresh
    }

    fn note_art_request(&mut self, id: u64, request: ArtRequest) {
        self.art_inflight.push((id, request));
    }

    /// Retires the tuple a reply answers even when the frame carried no usable request id, so
    /// a best-effort cover never stays inflight until the generic audio deadline.
    pub(crate) fn take_art_inflight(&mut self, artwork: &ipc::AudioArtwork) -> Option<u64> {
        let id = <[u8; 32]>::try_from(artwork.artwork_id.as_slice()).ok()?;
        let session_id = <[u8; 16]>::try_from(artwork.session_id.as_slice()).ok()?;
        let token = artwork.session_token;
        let index = self.art_inflight.iter().position(|(_, request)| {
            request.id == id && request.token == token && request.session_id == session_id
        })?;
        Some(self.art_inflight.remove(index).0)
    }

    fn prune_art_inflight(&mut self) {
        self.art_inflight
            .retain(|(_, request)| self.art_requested.contains(request));
    }

    /// A reply is accepted only for the tuple currently asked for. One bounded worker decodes
    /// it off the render thread; duplicate inflight hashes are dropped, not queued twice.
    pub(crate) fn accept_artwork(&mut self, artwork: ipc::AudioArtwork) {
        if artwork.mime != "image/jpeg"
            || artwork.data.is_empty()
            || artwork.data.len() > protocol::limits::MAX_AUDIO_ARTWORK_BYTES
            || artwork.width == 0
            || artwork.height == 0
            || artwork.width > protocol::limits::MAX_AUDIO_ARTWORK_EDGE
            || artwork.height > protocol::limits::MAX_AUDIO_ARTWORK_EDGE
        {
            return;
        }
        let Ok(id) = <[u8; 32]>::try_from(artwork.artwork_id.as_slice()) else {
            return;
        };
        let Ok(session_id) = <[u8; 16]>::try_from(artwork.session_id.as_slice()) else {
            return;
        };
        let request = ArtRequest {
            session_id,
            token: artwork.session_token,
            id,
        };
        if !self.art_requested.contains(&request) || self.artwork.iter().any(|art| art.id == id) {
            return;
        }
        {
            let inflight = self.art_worker.inner.inflight.lock().expect("art inflight");
            if inflight.contains(&id) {
                return;
            }
        }
        let wake = self.art_worker.inner.wake.lock().expect("art wake").clone();
        let mut spawn_failed = false;
        let mut enqueued = false;
        {
            let mut jobs = self.art_worker.inner.jobs.lock().expect("art jobs");
            let mut results = self.art_worker.inner.results.lock().expect("art results");
            if jobs.is_none() {
                let (job_tx, job_rx) = mpsc::sync_channel::<ArtDecodeJob>(2);
                let (result_tx, result_rx) = mpsc::channel::<ArtDecodeResult>();
                let worker_wake = wake.clone();
                match std::thread::Builder::new()
                    .name("bifrost-gui-artwork".to_owned())
                    .spawn(move || {
                        while let Ok((id, data)) = job_rx.recv() {
                            let decoded = decode_artwork(&id, &data);
                            let sent = result_tx.send((id, decoded)).is_ok();
                            if sent {
                                if let Some(wake) = &worker_wake {
                                    wake();
                                }
                            }
                        }
                    }) {
                    Ok(_) => {
                        *jobs = Some(job_tx);
                        *results = Some(result_rx);
                    }
                    Err(_) => spawn_failed = true,
                }
            }
            if !spawn_failed {
                if let Some(job_tx) = jobs.as_ref().cloned() {
                    self.art_worker
                        .inner
                        .inflight
                        .lock()
                        .expect("art inflight")
                        .push(id);
                    enqueued = job_tx.try_send((id, artwork.data)).is_ok();
                }
            }
        }
        if spawn_failed {
            // No decoder: forget this tuple so a later status may ask once more instead of
            // waiting on a worker that does not exist.
            self.art_requested.retain(|held| held != &request);
            self.prune_art_inflight();
            return;
        }
        if !enqueued {
            // Saturated: drop this cover honestly (no image, no retry loop).
            self.art_worker
                .inner
                .inflight
                .lock()
                .expect("art inflight")
                .retain(|held| *held != id);
        }
    }

    /// Takes every decoded cover whose tuple is still current and reports whether a frame is
    /// needed. A stale or failed result is dropped, never shown for another track or scope.
    pub fn drain_artwork(&mut self) -> bool {
        let guard = self.art_worker.inner.results.lock().expect("art results");
        let Some(results) = guard.as_ref() else {
            return false;
        };
        let mut changed = false;
        while let Ok((id, decoded)) = results.try_recv() {
            self.art_worker
                .inner
                .inflight
                .lock()
                .expect("art inflight")
                .retain(|held| *held != id);
            let Some(art) = decoded else {
                continue;
            };
            let current = self.art_requested.iter().any(|request| request.id == id);
            if current && !self.artwork.iter().any(|held| held.id == id) {
                if self.artwork.len() >= 4 {
                    self.artwork.remove(0);
                }
                self.artwork.push(art);
                changed = true;
            }
        }
        changed
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.pending_since.map(|t| t + Duration::from_secs(10))
    }
    pub fn sharing_requested(&self, role: ipc::AudioRole) -> bool {
        if let Some((pending_role, enabled)) = self.system_intent {
            return pending_role == role && enabled;
        }
        self.status.as_ref().is_some_and(|status| {
            if status.role != role as i32 {
                return false;
            }
            if let Some(requested) = status.system_sharing_requested {
                return requested;
            }
            // Legacy policy has no requested-intent observation. Only an actually running,
            // bilaterally authorized pipeline can display ON; never infer it from enabled.
            status.sessions.iter().any(|session| {
                session.local_role == role as i32
                    && session.state == ipc::AudioSessionState::Running as i32
                    && session.consent.as_ref().is_some_and(|consent| {
                        let (Some(local), Some(remote)) = (&consent.local, &consent.remote) else {
                            return false;
                        };
                        if role == ipc::AudioRole::Listener {
                            local.allow_system_receive && remote.allow_system_send
                        } else {
                            local.allow_system_send
                                && remote.allow_system_receive
                                && session.sending == Some(true)
                                && session.capture_active == Some(true)
                        }
                    })
            })
        })
    }
    pub fn error_message(&self, lang: Lang) -> Option<&str> {
        self.error.as_deref().or_else(|| {
            self.status
                .as_ref()
                .filter(|status| !self.pending && !status.system_sharing_error.is_empty())
                .map(|status| text(lang, system_error_key(&status.system_sharing_error)))
        })
    }
}

pub fn error_key(code: &str) -> Key {
    match code {
        "AUDIO_CONSENT_REQUIRED" => Key::AudioConsentRequired,
        "AUDIO_ROLE_CONFLICT" => Key::AudioRoleConflict,
        "AUDIO_UNSUPPORTED_BACKEND" => Key::AudioUnsupportedBackend,
        "AUDIO_NO_UDP_PATH" => Key::AudioNoUdpPath,
        "AUDIO_KEY_EXPIRED" => Key::AudioKeyExpired,
        "AUDIO_DEVICE_UNAVAILABLE" => Key::AudioDeviceUnavailable,
        "AUDIO_VIRTUAL_MIC_MISSING" => Key::AudioVirtualMicMissing,
        "AUDIO_CONFIG_WRITE" => Key::AudioConfigWrite,
        "AUDIO_REVISION_CONFLICT" => Key::AudioRevisionConflict,
        "AUDIO_STALE_MEDIA" => Key::AudioStaleMedia,
        "AUDIO_UNSUPPORTED_COMMAND" => Key::AudioUnsupportedCommand,
        "AUDIO_INVALID_CONFIG" => Key::AudioInvalidConfig,
        _ => Key::AudioUnavailable,
    }
}

fn system_error_key(code: &str) -> Key {
    if code == "AUDIO_CONSENT_REQUIRED" {
        Key::AudioSystemConsentRequired
    } else {
        error_key(code)
    }
}

/// The one current-scope tuple per consented stream that carries an authoritative image hash.
/// Empty when the core's bounded cache is not complete; the id itself is the readiness signal.
fn wanted_artworks(status: &model::AudioStatus) -> Vec<ArtRequest> {
    let mut wanted: Vec<ArtRequest> = Vec::new();
    for session in &status.sessions {
        let Some(media) = session.media_info.as_ref() else {
            continue;
        };
        let Some(id) = media.artwork_id else {
            continue;
        };
        if media.session_token == 0 || wanted.iter().any(|request| request.id == id) {
            continue;
        }
        wanted.push(ArtRequest {
            session_id: session.session_id,
            token: media.session_token,
            id,
        });
    }
    wanted
}

/// Decodes one normalized JPEG thumbnail, bounded before any RGBA allocation: the header is
/// read first, so a 49 KiB file that claims a huge canvas is rejected rather than expanded.
pub fn decode_artwork(id: &[u8; 32], data: &[u8]) -> Option<Artwork> {
    if data.is_empty() || data.len() > protocol::limits::MAX_AUDIO_ARTWORK_BYTES {
        return None;
    }
    let reader =
        image::ImageReader::with_format(std::io::Cursor::new(data), image::ImageFormat::Jpeg);
    let (width, height) = reader.into_dimensions().ok()?;
    if width == 0
        || height == 0
        || width > protocol::limits::MAX_AUDIO_ARTWORK_EDGE
        || height > protocol::limits::MAX_AUDIO_ARTWORK_EDGE
    {
        return None;
    }
    let reader =
        image::ImageReader::with_format(std::io::Cursor::new(data), image::ImageFormat::Jpeg);
    let image = reader.decode().ok()?.into_rgba8();
    if image.width() != width || image.height() != height {
        return None;
    }
    Some(Artwork {
        id: *id,
        rgba: Arc::from(image.into_raw().into_boxed_slice()),
        width,
        height,
    })
}

impl ViewModel {
    /// A decoded cover wakes the window; decoding itself never runs on the render thread.
    pub fn set_audio_art_wake(&mut self, wake: Arc<dyn Fn() + Send + Sync>) {
        self.audio.set_art_wake(wake);
    }

    /// Moves decoded covers into the player's cache and onto the cards that are on screen now;
    /// a repaint alone must show the image, without waiting for another authoritative status.
    pub fn drain_artwork(&mut self) -> bool {
        let changed = self.audio.drain_artwork();
        if changed {
            for card in &mut self.audio_player.cards {
                card.artwork = card.artwork_id.and_then(|id| self.audio.artwork_for(id));
            }
        }
        changed
    }

    pub fn request_audio_status(&mut self, now: Instant) {
        if !self
            .inflight
            .iter()
            .any(|r| r.request == Request::Audio(AudioRequest::Snapshot))
        {
            let id = self.begin_request(Request::Audio(AudioRequest::Snapshot), now);
            self.outgoing
                .push((id, Msg::GetAudioStatus(ipc::GetAudioStatus {})));
        }
    }
    pub fn apply_audio_devices(&mut self, id: u64, mut page: ipc::AudioDevices) {
        if !self.audio.device_requests.contains(&(id, page.kind)) {
            return;
        }
        if page.complete {
            self.audio
                .device_requests
                .retain(|(request, _)| *request != id);
            self.inflight.retain(|request| request.id != id);
        }
        if let Some(index) = self
            .audio
            .devices
            .iter()
            .position(|old| old.kind == page.kind)
        {
            let old = &mut self.audio.devices[index];
            if old.revision > page.revision {
                return;
            }
            if old.revision == page.revision && !old.complete {
                if old.devices.len() + page.devices.len() > 64
                    || page
                        .devices
                        .iter()
                        .any(|device| old.devices.iter().any(|prior| prior.id == device.id))
                {
                    self.audio.error =
                        Some(text(self.look.language, Key::AudioDeviceUnavailable).into());
                    self.audio
                        .device_requests
                        .retain(|(request, _)| *request != id);
                    self.inflight.retain(|request| request.id != id);
                    return;
                }
                old.devices.append(&mut page.devices);
                old.complete = page.complete;
            } else {
                *old = page;
            }
        } else {
            self.audio.devices.push(page);
        }
    }
    pub fn apply_audio_status(&mut self, request_id: u64, status: ipc::AudioStatus) {
        let correlated_snapshot = self.audio.pending_snapshot_id == Some(request_id);
        if self.audio.status.as_ref().is_some_and(|old| {
            old.revision > status.revision
                || old.revision == status.revision && !correlated_snapshot
        }) {
            return;
        }
        let Ok(model) = crate::audio_wire::decode_status(&status) else {
            self.audio.available = false;
            self.audio.fail(self.look.language, Key::AudioUnavailable);
            self.audio.artwork.clear();
            self.audio.art_requested.clear();
            self.audio_player.replace_cards(Vec::new(), false);
            return;
        };
        let target_observed = self.audio.pending_target.as_ref().is_none_or(|target| {
            status
                .config
                .as_ref()
                .is_some_and(|config| config.listener_node_id.as_slice() == target)
        });
        if (!self.audio.dirty || self.audio.pending && self.audio.pending_mode.is_none())
            && (self.audio.system_intent.is_none() || correlated_snapshot && target_observed)
        {
            if let Some(config) = status.config.as_ref() {
                self.audio.draft = config.clone();
            }
            self.audio.dirty = false;
        }
        let intent_observed = self.audio.system_intent.is_none_or(|(role, enabled)| {
            status.system_sharing_requested == Some(enabled)
                && (!enabled || status.role == role as i32)
                || enabled && !status.system_sharing_error.is_empty()
        });
        let settles_request = if self.audio.system_intent.is_some() {
            // A push can predate this request even with a newer status revision.
            // Only the read requested after this mutation's persistence ACK settles it.
            correlated_snapshot && target_observed && intent_observed
        } else {
            self.audio
                .pending_revision
                .is_some_and(|revision| status.revision > revision)
                && self.audio.pending_mode.is_none_or(|mode| {
                    status
                        .config
                        .as_ref()
                        .is_some_and(|config| config.mode == mode)
                })
        };
        if settles_request {
            self.audio.settle();
        }
        self.audio.available = true;
        let mut cards = map_cards(
            &model,
            self.status
                .as_ref()
                .map_or(&[][..], |status| &status.members),
        );
        let observed_active = self.audio.system_intent.is_none()
            && status.system_sharing_requested != Some(false)
            && model.sessions.iter().any(|session| {
                session.local_role == model::AudioRole::Listener
                    && session.state == model::SessionState::Running
            });
        // The id is present only once the core's bounded cache is complete, so a `None` → `Some`
        // transition is exactly the one moment to ask once — never on a position revision.
        let wanted = wanted_artworks(&model);
        let fresh = self.audio.plan_artwork(&wanted);
        self.audio.prune_art_inflight();
        for card in &mut cards {
            card.artwork = card.artwork_id.and_then(|id| self.audio.artwork_for(id));
        }
        self.audio_player.replace_cards(cards, observed_active);
        self.audio.status = Some(status);
        for request in fresh {
            let id = self.begin_request(Request::Audio(AudioRequest::Artwork), Instant::now());
            self.audio.note_art_request(id, request);
            self.outgoing.push((
                id,
                Msg::GetAudioArtwork(ipc::GetAudioArtwork {
                    session_id: request.session_id.to_vec(),
                    session_token: request.token,
                    artwork_id: request.id.to_vec(),
                }),
            ));
        }
        if correlated_snapshot && !self.audio.pending {
            self.audio.pending_snapshot_id = None;
        }
    }
    pub(crate) fn refresh_audio_peer_names(&mut self) {
        let Some(audio) = &self.audio.status else {
            return;
        };
        let members = self
            .status
            .as_ref()
            .map_or(&[][..], |status| &status.members);
        for card in &mut self.audio_player.cards {
            if let Some(session) = audio
                .sessions
                .iter()
                .find(|session| matches_id(&card.stream_id, &session.session_id))
            {
                card.source_name = peer_name(members, &session.peer);
            }
        }
    }
    fn request_audio_reconciliation(&mut self) {
        let id = self.begin_request(Request::Audio(AudioRequest::Snapshot), Instant::now());
        self.audio.pending_snapshot_id = Some(id);
        self.outgoing
            .push((id, Msg::GetAudioStatus(ipc::GetAudioStatus {})));
    }
    pub fn audio_result(&mut self, id: u64, kind: AudioRequest, result: ipc::OpResult) {
        if kind == AudioRequest::Artwork {
            // An unavailable cover cache is an honest "no image", not an audio failure: no
            // error banner, no retry, and the player keeps its neutral mark.
            let _ = (id, result);
            return;
        }
        if kind == AudioRequest::Devices {
            self.audio
                .device_requests
                .retain(|(request, _)| *request != id);
        }
        if kind == AudioRequest::AuxiliaryMutation {
            if !result.ok {
                self.audio.error =
                    Some(text(self.look.language, error_key(&result.error_code)).into());
            } else {
                self.request_audio_status(Instant::now());
            }
            return;
        }
        if kind == AudioRequest::Mutation && self.audio.pending_id != Some(id) {
            return;
        }
        if !result.ok {
            let system_request =
                kind == AudioRequest::Mutation && self.audio.system_intent.is_some();
            let key = if self.audio.system_intent.is_some() {
                system_error_key(&result.error_code)
            } else {
                error_key(&result.error_code)
            };
            self.audio.fail(self.look.language, key);
            if key == Key::AudioUnavailable {
                self.audio.available = false;
                self.audio_player.replace_cards(Vec::new(), false);
            }
            if system_request {
                self.request_audio_reconciliation();
            }
            return;
        }
        // ACK means persisted intent, not active audio. Correlate a fresh read so an
        // earlier queued failure cannot retire a newer switch or hide its OFF action.
        if kind == AudioRequest::Mutation {
            if self.audio.system_intent.is_some() {
                self.request_audio_reconciliation();
            } else {
                self.request_audio_status(Instant::now());
            }
        }
    }
    pub fn tick_audio(&mut self, now: Instant) {
        // A cover is best-effort: its timeout retires the request silently and never turns an
        // otherwise healthy audio session into an unavailable subsystem.
        let expired = self.inflight.iter().any(|r| {
            matches!(
                r.request,
                Request::Audio(kind) if kind != AudioRequest::Artwork
            ) && now >= r.deadline
        });
        if expired || self.audio.deadline().is_some_and(|d| now >= d) {
            self.inflight
                .retain(|r| !matches!(r.request, Request::Audio(_)) || now < r.deadline);
            self.audio.available = false;
            self.audio.fail(self.look.language, Key::AudioUnavailable);
        }
        self.inflight.retain(|r| {
            !(matches!(r.request, Request::Audio(AudioRequest::Artwork)) && now >= r.deadline)
        });
        self.audio
            .art_inflight
            .retain(|(id, _)| self.inflight.iter().any(|request| request.id == *id));
        self.audio
            .device_requests
            .retain(|(id, _)| self.inflight.iter().any(|request| request.id == *id));
    }
    pub fn audio_action(&mut self, action: &AudioAction, now: Instant) {
        match action {
            AudioAction::EditConfig(config) => {
                self.audio.draft.clone_from(config.as_ref());
                self.audio.dirty = true;
                return;
            }
            AudioAction::OpenSettings => {
                self.appearance_open = false;
                self.modal = Some(Modal::Audio);
                self.request_audio_status(now);
                return;
            }
            AudioAction::Refresh => {
                self.request_audio_status(now);
                return;
            }
            AudioAction::Player(AudioPlayerAction::OpenSettings) => {
                self.audio_action(&AudioAction::OpenSettings, now);
                return;
            }
            AudioAction::Player(AudioPlayerAction::HidePlayer) => {
                self.audio_player.open = false;
                return;
            }
            _ => {}
        }
        if let AudioAction::ListDevices(kind) = action {
            if self
                .audio
                .device_requests
                .iter()
                .any(|(_, pending_kind)| *pending_kind == *kind as i32)
            {
                return;
            }
            let id = self.begin_request(Request::Audio(AudioRequest::Devices), now);
            self.audio.device_requests.push((id, *kind as i32));
            self.outgoing.push((
                id,
                Msg::ListAudioDevices(ipc::ListAudioDevices { kind: *kind as i32 }),
            ));
            return;
        }
        // Fail-closed stop/mute/release remain admissible even when a status request failed.
        let urgent = matches!(
            action,
            AudioAction::Share(false)
                | AudioAction::Receive(false)
                | AudioAction::StopSystemAudio
                | AudioAction::SetMicrophoneMute(true)
                | AudioAction::SetPtt(false)
                | AudioAction::Player(AudioPlayerAction::Stop(_))
                | AudioAction::Player(AudioPlayerAction::SetMuted { muted: true, .. })
        );
        if (!self.audio.available && !urgent) || !self.connection.is_connected() {
            self.audio.error = Some(text(self.look.language, Key::AudioUnavailable).into());
            return;
        }
        if self.audio.pending && !urgent {
            return;
        }
        let Some(status) = self.audio.status.as_ref() else {
            return;
        };
        let revision = status.revision;
        let status_role = status.role;
        let message = match action {
            AudioAction::Share(enabled) | AudioAction::Receive(enabled) => {
                let role = if matches!(action, AudioAction::Share(_)) {
                    ipc::AudioRole::Source
                } else {
                    ipc::AudioRole::Listener
                };
                if *enabled
                    && role == ipc::AudioRole::Source
                    && self.audio.draft.listener_node_id.len() != 32
                {
                    self.audio.error =
                        Some(text(self.look.language, Key::AudioSelectReceiver).into());
                    return;
                }
                Msg::SetSystemAudioSharing(ipc::SetSystemAudioSharing {
                    enabled: *enabled,
                    role: role as i32,
                    listener_node_id: if role == ipc::AudioRole::Source {
                        self.audio.draft.listener_node_id.clone()
                    } else {
                        Vec::new()
                    },
                    expected_revision: revision,
                })
            }
            AudioAction::Retarget(target) => {
                if !self.audio.sharing_requested(ipc::AudioRole::Source) {
                    return;
                }
                Msg::SetSystemAudioSharing(ipc::SetSystemAudioSharing {
                    enabled: true,
                    role: ipc::AudioRole::Source as i32,
                    listener_node_id: target.to_vec(),
                    expected_revision: revision,
                })
            }
            AudioAction::ApplyConfig => {
                if ::ipc::audio::validate_config(&self.audio.draft).is_err() {
                    self.audio.error =
                        Some(text(self.look.language, Key::AudioInvalidConfig).into());
                    return;
                }
                Msg::SetAudio(ipc::SetAudio {
                    config: Some(self.audio.draft.clone()),
                    expected_revision: revision,
                })
            }
            AudioAction::StopSystemAudio => {
                Msg::SetSystemAudioSharing(ipc::SetSystemAudioSharing {
                    enabled: false,
                    role: status_role,
                    listener_node_id: Vec::new(),
                    expected_revision: revision,
                })
            }
            AudioAction::SetMicrophoneMute(muted) => {
                Msg::SetMicrophoneMute(ipc::SetMicrophoneMute {
                    muted: *muted,
                    expected_revision: revision,
                })
            }
            AudioAction::SetPtt(pressed) => Msg::SetPtt(ipc::SetPtt {
                pressed: *pressed,
                expected_revision: revision,
            }),
            AudioAction::Player(AudioPlayerAction::SetMode(mode)) => {
                if !self.audio.player_mode(true).enabled
                    || !matches!(mode, ipc::AudioMode::MixAll | ipc::AudioMode::FollowFocus)
                {
                    return;
                }
                let Some(current) = status.config.as_ref() else {
                    return;
                };
                if current.mode == *mode as i32 {
                    return;
                }
                let mut config = current.clone();
                config.mode = *mode as i32;
                Msg::SetAudio(ipc::SetAudio {
                    config: Some(config),
                    expected_revision: revision,
                })
            }
            AudioAction::Player(intent) => {
                let Some(message) = player_request(status, intent) else {
                    self.audio.error = Some(text(self.look.language, Key::AudioStaleMedia).into());
                    return;
                };
                message
            }
            _ => return,
        };
        if matches!(
            action,
            AudioAction::Share(false) | AudioAction::Receive(false) | AudioAction::StopSystemAudio
        ) && self.audio.system_intent.is_some()
        {
            if let Some(superseded) = self.audio.pending_id {
                self.inflight.retain(|request| request.id != superseded);
                self.outgoing.retain(|(id, _)| *id != superseded);
            }
            if let Some(snapshot) = self.audio.pending_snapshot_id {
                self.inflight.retain(|request| request.id != snapshot);
                self.outgoing.retain(|(id, _)| *id != snapshot);
            }
        }
        let auxiliary = (self.audio.system_intent.is_some() || self.audio.pending_mode.is_some())
            && !matches!(
                action,
                AudioAction::Share(_)
                    | AudioAction::Receive(_)
                    | AudioAction::Retarget(_)
                    | AudioAction::StopSystemAudio
            );
        let kind = if auxiliary {
            AudioRequest::AuxiliaryMutation
        } else {
            AudioRequest::Mutation
        };
        let id = self.begin_request(Request::Audio(kind), now);
        match action {
            AudioAction::Retarget(target) => {
                self.audio.system_intent = Some((ipc::AudioRole::Source, true));
                self.audio.draft.listener_node_id.clear();
                self.audio.draft.listener_node_id.extend_from_slice(target);
            }
            AudioAction::Share(enabled) => {
                self.audio.system_intent = Some((ipc::AudioRole::Source, *enabled));
            }
            AudioAction::Receive(enabled) => {
                self.audio.system_intent = Some((ipc::AudioRole::Listener, *enabled));
            }
            AudioAction::StopSystemAudio => {
                self.audio.system_intent = Some((
                    if status_role == ipc::AudioRole::Listener as i32 {
                        ipc::AudioRole::Listener
                    } else {
                        ipc::AudioRole::Source
                    },
                    false,
                ));
            }
            _ => {}
        }
        if let AudioAction::SetPtt(pressed) = action {
            self.audio.gui_ptt_held = *pressed;
        }
        if auxiliary {
            self.outgoing.push((id, message));
            return;
        }
        self.audio.pending = true;
        self.audio.pending_id = Some(id);
        self.audio.pending_snapshot_id = None;
        self.audio.pending_mode = match action {
            AudioAction::Player(AudioPlayerAction::SetMode(mode)) => Some(*mode as i32),
            _ => None,
        };
        self.audio.retargeting = matches!(action, AudioAction::Retarget(_));
        self.audio.pending_target = match action {
            AudioAction::Retarget(target) => Some(*target),
            AudioAction::Share(true) => {
                self.audio.draft.listener_node_id.as_slice().try_into().ok()
            }
            _ => None,
        };
        self.audio.pending_revision = Some(revision);
        self.audio.pending_since = Some(now);
        self.audio.error = None;
        self.outgoing.push((id, message));
    }
}

fn eligible(session: &model::SessionStatus) -> bool {
    session.local_role == model::AudioRole::Listener
        && matches!(
            session.state,
            model::SessionState::PathValidation
                | model::SessionState::Running
                | model::SessionState::Rebuffering
                | model::SessionState::DevicePaused
        )
}
fn media_allowed(session: &model::SessionStatus) -> bool {
    eligible(session)
        && session
            .media_info
            .as_ref()
            .is_some_and(|info| info.session_token != 0 && info.revision != 0)
        && session.consent.local.allow_system_receive
        && session.consent.remote.allow_system_send
        && session.consent.remote.allow_media_control
}
pub fn stream_id(id: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(id.len() * 2);
    for byte in id {
        let _ = write!(out, "{byte:02x}");
    }
    out
}
fn seek_target(session: &model::SessionStatus) -> Option<AudioSeekTarget> {
    if !media_allowed(session) {
        return None;
    }
    let media = session.media_info.as_ref()?;
    if !media.commands.seek {
        return None;
    }
    let position = media.position_ms?;
    let duration = media.duration_ms?;
    let (min_ms, max_ms) = media.seek_min_ms.zip(media.seek_max_ms)?;
    if duration == 0 || position > duration || min_ms > max_ms || max_ms > duration {
        return None;
    }
    Some(AudioSeekTarget {
        session_token: media.session_token,
        media_revision: media.revision,
        min_ms,
        max_ms,
    })
}

pub fn map_cards(
    status: &model::AudioStatus,
    members: &[ipc::MemberStatus],
) -> Vec<AudioPlayerCard> {
    if !status.enabled {
        return Vec::new();
    }
    status
        .sessions
        .iter()
        .filter_map(|s| {
            let path = crate::audio_path::AudioPathObservation::from_model(s);
            let diagnosed_failure = s.local_role == model::AudioRole::Listener
                && s.state == model::SessionState::Failed
                && path.is_some_and(|observation| observation.has_diagnostic());
            if !eligible(s) && !diagnosed_failure {
                return None;
            }
            let media = s.media_info.as_ref();
            let playing = media.is_some_and(|m| m.playback == model::PlaybackState::Playing);
            let allowed = media_allowed(s);
            Some(AudioPlayerCard {
                stream_id: stream_id(&s.session_id),
                source_name: peer_name(members, &s.peer),
                title: media
                    .filter(|m| !m.title.is_empty())
                    .map(|m| m.title.clone()),
                subtitle: media
                    .filter(|m| !m.subtitle.is_empty())
                    .map(|m| m.subtitle.clone()),
                artwork_id: media.and_then(|m| m.artwork_id),
                artwork: None,
                has_media_session: media.is_some(),
                app: AppKind::System,
                position_ms: media.and_then(|m| m.position_ms),
                duration_ms: media.and_then(|m| m.duration_ms),
                seek: seek_target(s),
                playing,
                can_prev: allowed && media.is_some_and(|m| m.commands.previous),
                can_next: allowed && media.is_some_and(|m| m.commands.next),
                can_play_pause: allowed
                    && media.is_some_and(|m| {
                        m.commands.toggle
                            || if playing {
                                m.commands.pause
                            } else {
                                m.commands.play
                            }
                    }),
                muted: s.muted,
                gain_percent: s.gain_percent,
                path,
                state: match s.state {
                    model::SessionState::Failed => StreamState::Failed,
                    model::SessionState::DevicePaused => StreamState::Degraded,
                    model::SessionState::PathValidation | model::SessionState::Rebuffering => {
                        StreamState::Buffering
                    }
                    _ => match s.health_state {
                        model::StreamHealth::Ready => StreamState::Ready,
                        model::StreamHealth::Buffering => StreamState::Buffering,
                        model::StreamHealth::Degraded => StreamState::Degraded,
                    },
                },
                background: status.mode == model::AudioMode::FollowFocus && !s.focus_primary,
            })
        })
        .collect()
}

pub(crate) fn peer_name(members: &[ipc::MemberStatus], id: &[u8]) -> String {
    members
        .iter()
        .find(|member| matches_id(&member.node_id, id))
        .filter(|member| !member.display_name.trim().is_empty())
        .map_or_else(
            || stream_id(&id[..id.len().min(4)]),
            |member| member.display_name.clone(),
        )
}

pub(crate) fn matches_id(hex: &str, id: &[u8]) -> bool {
    hex.len() == id.len() * 2
        && hex
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .zip(id)
            .all(|(pair, byte)| {
                let digit = |b: u8| (b as char).to_digit(16);
                digit(pair[0])
                    .zip(digit(pair[1]))
                    .is_some_and(|(hi, lo)| (hi * 16 + lo) as u8 == *byte)
            })
}

pub fn player_request(status: &ipc::AudioStatus, action: &AudioPlayerAction) -> Option<Msg> {
    let id = match action {
        AudioPlayerAction::Previous(id)
        | AudioPlayerAction::Next(id)
        | AudioPlayerAction::PlayPause(id)
        | AudioPlayerAction::Stop(id) => id,
        AudioPlayerAction::SetMuted { stream_id, .. }
        | AudioPlayerAction::SetGain { stream_id, .. }
        | AudioPlayerAction::Seek { stream_id, .. } => stream_id,
        _ => return None,
    };
    let model = crate::audio_wire::decode_status(status).ok()?;
    if !model.enabled {
        return None;
    }
    let s = model
        .sessions
        .iter()
        .find(|s| eligible(s) && stream_id(&s.session_id) == *id)?;
    match action {
        AudioPlayerAction::Stop(_) => Some(Msg::StopAudio(ipc::StopAudio {
            peer_node_id: s.peer.to_vec(),
            session_id: s.session_id.to_vec(),
            stop_all: false,
        })),
        AudioPlayerAction::SetMuted { muted, .. } => {
            Some(Msg::SetAudioPeerGain(ipc::SetAudioPeerGain {
                peer_node_id: s.peer.to_vec(),
                gain_percent: u32::from(s.gain_percent),
                muted: *muted,
                expected_revision: status.revision,
            }))
        }
        AudioPlayerAction::SetGain { percent, .. } if *percent <= 100 => {
            Some(Msg::SetAudioPeerGain(ipc::SetAudioPeerGain {
                peer_node_id: s.peer.to_vec(),
                gain_percent: u32::from(*percent),
                muted: s.muted,
                expected_revision: status.revision,
            }))
        }
        AudioPlayerAction::Seek {
            target,
            position_ms,
            ..
        } if seek_target(s) == Some(*target)
            && (target.min_ms..=target.max_ms).contains(position_ms) =>
        {
            Some(Msg::SendMediaCommand(ipc::SendMediaCommand {
                peer_node_id: s.peer.to_vec(),
                session_id: s.session_id.to_vec(),
                session_token: target.session_token,
                media_revision: target.media_revision,
                command: ipc::AudioMediaCommand::Seek as i32,
                position_ms: Some(*position_ms),
            }))
        }
        AudioPlayerAction::Previous(_)
        | AudioPlayerAction::Next(_)
        | AudioPlayerAction::PlayPause(_)
            if media_allowed(s) =>
        {
            let m = s.media_info.as_ref()?;
            let command = match action {
                AudioPlayerAction::Previous(_) if m.commands.previous => {
                    ipc::AudioMediaCommand::Previous
                }
                AudioPlayerAction::Next(_) if m.commands.next => ipc::AudioMediaCommand::Next,
                AudioPlayerAction::PlayPause(_) if m.commands.toggle => {
                    ipc::AudioMediaCommand::Toggle
                }
                AudioPlayerAction::PlayPause(_)
                    if m.playback == model::PlaybackState::Playing && m.commands.pause =>
                {
                    ipc::AudioMediaCommand::Pause
                }
                AudioPlayerAction::PlayPause(_)
                    if m.playback != model::PlaybackState::Playing && m.commands.play =>
                {
                    ipc::AudioMediaCommand::Play
                }
                _ => return None,
            };
            Some(Msg::SendMediaCommand(ipc::SendMediaCommand {
                peer_node_id: s.peer.to_vec(),
                session_id: s.session_id.to_vec(),
                session_token: m.session_token,
                media_revision: m.revision,
                command: command as i32,
                position_ms: None,
            }))
        }
        _ => None,
    }
}

/// A generic single-instance show signal wakes the GUI; only then read its bounded intent file.
pub static NAVIGATION_WAKE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn consume_navigation(view: &mut ViewModel, startup: bool) {
    use std::io::Read;
    let from_file = view.config.paths.as_ref().and_then(|paths| {
        let file = paths.state_dir().join("gui-audio-navigation");
        let mut value = String::new();
        let result = std::fs::File::open(&file)
            .and_then(|handle| handle.take(16).read_to_string(&mut value));
        if result.is_ok() {
            let _ = std::fs::remove_file(file);
            Some(value)
        } else {
            None
        }
    });
    let intent = from_file.or_else(|| {
        if !startup {
            return None;
        }
        std::env::args().find_map(|arg| match arg.as_str() {
            "--audio-player" => Some("player".to_owned()),
            "--audio-settings" => Some("settings".to_owned()),
            _ => None,
        })
    });
    match intent.as_deref() {
        Some("player") => view.audio_player.open = true,
        Some("settings") => view.audio_action(&AudioAction::OpenSettings, Instant::now()),
        _ => {}
    }
}

#[cfg(test)]
mod artwork_attach_tests {
    use super::*;
    use crate::audio_player::{AppKind, StreamState};
    use crate::view_model::{CoreEndpoint, LocalConfig};

    #[test]
    fn a_decoded_cover_reaches_the_current_card_without_a_new_status() {
        let mut vm = ViewModel::new(
            CoreEndpoint::Ready {
                description: "test-endpoint".into(),
            },
            LocalConfig {
                display_name: "W".into(),
                ..Default::default()
            },
        );
        vm.audio_player.cards = vec![AudioPlayerCard {
            stream_id: "stream".into(),
            source_name: "WORKSTATION".into(),
            title: Some("Title".into()),
            subtitle: None,
            artwork_id: Some([5; 32]),
            artwork: None,
            has_media_session: true,
            app: AppKind::System,
            position_ms: None,
            duration_ms: None,
            seek: None,
            playing: true,
            can_prev: false,
            can_play_pause: false,
            can_next: false,
            muted: false,
            gain_percent: 100,
            state: StreamState::Ready,
            path: None,
            background: false,
        }];
        vm.audio.art_requested.push(ArtRequest {
            session_id: [1; 16],
            token: 9,
            id: [5; 32],
        });
        let image = image::RgbImage::from_pixel(4, 4, image::Rgb([10, 20, 30]));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode_image(&image)
            .expect("encode fixture");
        vm.audio.accept_artwork(ipc::AudioArtwork {
            session_id: vec![1; 16],
            session_token: 9,
            artwork_id: vec![5; 32],
            mime: "image/jpeg".into(),
            data: jpeg,
            width: 4,
            height: 4,
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut attached = false;
        while Instant::now() < deadline {
            if vm.drain_artwork() {
                attached = vm.audio_player.cards[0].artwork.is_some();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            attached,
            "a decoded cover must reach the current card without waiting for another status"
        );
        assert_eq!(
            vm.audio_player.cards[0]
                .artwork
                .as_ref()
                .unwrap()
                .rgba
                .len(),
            4 * 4 * 4
        );
    }

    #[test]
    fn a_duplicate_reply_is_not_queued_twice() {
        let mut vm = ViewModel::new(
            CoreEndpoint::Ready {
                description: "test-endpoint".into(),
            },
            LocalConfig {
                display_name: "W".into(),
                ..Default::default()
            },
        );
        vm.audio_player.cards = vec![AudioPlayerCard {
            stream_id: "stream".into(),
            source_name: "WORKSTATION".into(),
            title: Some("Title".into()),
            subtitle: None,
            artwork_id: Some([6; 32]),
            artwork: None,
            has_media_session: true,
            app: AppKind::System,
            position_ms: None,
            duration_ms: None,
            seek: None,
            playing: true,
            can_prev: false,
            can_play_pause: false,
            can_next: false,
            muted: false,
            gain_percent: 100,
            state: StreamState::Ready,
            path: None,
            background: false,
        }];
        vm.audio.art_requested.push(ArtRequest {
            session_id: [1; 16],
            token: 9,
            id: [6; 32],
        });
        let image = image::RgbImage::from_pixel(4, 4, image::Rgb([1, 2, 3]));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode_image(&image)
            .expect("encode fixture");
        let reply = |data: Vec<u8>| ipc::AudioArtwork {
            session_id: vec![1; 16],
            session_token: 9,
            artwork_id: vec![6; 32],
            mime: "image/jpeg".into(),
            data,
            width: 4,
            height: 4,
        };
        vm.audio.accept_artwork(reply(jpeg.clone()));
        vm.audio.accept_artwork(reply(jpeg));
        assert_eq!(
            vm.audio
                .art_worker
                .inner
                .inflight
                .lock()
                .expect("inflight")
                .len(),
            1,
            "an identical hash already being decoded is not queued twice"
        );
    }
}

#[cfg(test)]
mod audio_timeout_tests {
    use super::*;
    use crate::view_model::{CoreEndpoint, Inflight, LocalConfig};

    #[test]
    fn artwork_timeout_is_best_effort_and_never_disables_audio() {
        let mut vm = ViewModel::new(
            CoreEndpoint::Ready {
                description: "test-endpoint".into(),
            },
            LocalConfig {
                display_name: "W".into(),
                ..Default::default()
            },
        );
        vm.audio.available = true;
        let now = Instant::now();
        vm.inflight.push(Inflight {
            id: 7,
            request: Request::Audio(AudioRequest::Artwork),
            deadline: now - Duration::from_secs(1),
            clipboard_target: None,
        });
        vm.tick_audio(now);
        assert!(
            vm.audio.available,
            "a cover timeout is not an audio failure"
        );
        assert!(vm.audio.error.is_none());
        assert!(
            vm.inflight.is_empty(),
            "the timed-out cover request is retired"
        );
        // The generic audio timeout still fails the subsystem for a real audio request.
        vm.inflight.push(Inflight {
            id: 8,
            request: Request::Audio(AudioRequest::Mutation),
            deadline: now - Duration::from_secs(1),
            clipboard_target: None,
        });
        vm.tick_audio(now);
        assert!(!vm.audio.available);
        assert!(vm.audio.error.is_some());
    }
}
