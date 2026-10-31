//! Authoritative audio privacy status and revision-bound tray commands.
use crate::menu::{Action, Item};
use protocol::ipc::{self as wire, gui_frame::Msg};
use std::time::{Duration, Instant};
use ui_text::{text, Key, Lang};

pub(super) const POLL_INTERVAL: Duration = Duration::from_secs(3);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Default)]
pub(super) struct Audio {
    pub snapshot: Option<wire::AudioStatus>,
    available: bool,
    transport_connected: bool,
    revision_current: bool,
    fault: bool,
    query: Option<(u64, Instant)>,
    mutation: Option<(u64, Instant)>,
}

impl Audio {
    pub fn queried(&mut self, id: u64, now: Instant) {
        self.transport_connected = true;
        self.query = Some((id, now));
    }

    pub fn can_query(&self) -> bool {
        self.query.is_none()
    }

    pub fn disconnected(&mut self) {
        self.transport_connected = false;
        self.revision_current = false;
        self.available = false;
        self.query = None;
        self.mutation = None;
    }

    pub fn expire(&mut self, now: Instant) {
        if [self.query, self.mutation]
            .into_iter()
            .flatten()
            .any(|(_, sent)| now.duration_since(sent) >= RESPONSE_TIMEOUT)
        {
            self.query = None;
            self.mutation = None;
            self.available = false;
            self.fault = true;
        }
    }

    pub fn receive(&mut self, id: u64, msg: &Msg) -> bool {
        match msg {
            Msg::AudioStatus(status) => {
                if ipc::audio::validate_status(status).is_err() {
                    self.available = false;
                    self.fault = true;
                    return false;
                }
                if id != 0 && self.query.map(|(request, _)| request) != Some(id) {
                    return false;
                }
                if id != 0 {
                    self.query = None;
                }
                if self.revision_current
                    && self
                        .snapshot
                        .as_ref()
                        .is_some_and(|old| old.revision > status.revision)
                {
                    return false;
                }
                self.snapshot = Some(status.clone());
                self.revision_current = true;
                self.transport_connected = true;
                self.available = true;
                self.fault = false;
                false
            }
            Msg::OpResult(result) => {
                let is_query = self.query.map(|(request, _)| request) == Some(id);
                let is_mutation = self.mutation.map(|(request, _)| request) == Some(id);
                if !is_query && !is_mutation {
                    return false;
                }
                if is_query {
                    // A status query must return a snapshot, not a success acknowledgement.
                    self.query = None;
                    self.available = false;
                    self.fault = !result.ok;
                }
                if is_mutation {
                    self.mutation = None;
                    self.fault = !result.ok;
                    // Acknowledgement is not a new authoritative snapshot.
                    self.available = false;
                }
                is_mutation
            }
            _ => false,
        }
    }

    pub fn request(&self, action: &Action) -> Option<Msg> {
        if !self.transport_connected {
            return None;
        }
        let status = self.snapshot.as_ref()?;
        let request = match action {
            Action::StopAllAudio => Some(Msg::StopAudio(wire::StopAudio {
                peer_node_id: Vec::new(),
                session_id: Vec::new(),
                stop_all: true,
            })),
            Action::AudioReceiveMute(peer) if status.role == wire::AudioRole::Listener as i32 => {
                let session = status.sessions.iter().find(|session| {
                    &session.peer == peer && session.local_role == wire::AudioRole::Listener as i32
                })?;
                Some(Msg::SetAudioPeerGain(wire::SetAudioPeerGain {
                    peer_node_id: peer.clone(),
                    gain_percent: session.gain_percent,
                    muted: !session.muted,
                    expected_revision: status.revision,
                }))
            }
            Action::MicMute => Some(Msg::SetMicrophoneMute(wire::SetMicrophoneMute {
                muted: !status.microphone.as_ref()?.muted,
                expected_revision: status.revision,
            })),
            Action::MicPttRelease => Some(Msg::SetPtt(wire::SetPtt {
                pressed: false,
                expected_revision: status.revision,
            })),
            _ => None,
        }?;
        let safety_only = matches!(
            &request,
            Msg::StopAudio(_)
                | Msg::SetPtt(wire::SetPtt { pressed: false, .. })
                | Msg::SetMicrophoneMute(wire::SetMicrophoneMute { muted: true, .. })
                | Msg::SetAudioPeerGain(wire::SetAudioPeerGain { muted: true, .. })
        );
        ((self.available && self.mutation.is_none()) || safety_only).then_some(request)
    }

    pub fn sent(&mut self, id: u64, now: Instant) {
        self.mutation = Some((id, now));
    }

    pub fn labels(&self, lang: Lang) -> Vec<String> {
        let mut labels = Vec::new();
        if !self.available {
            labels.push(text(lang, Key::TrayAudioUnavailable).into());
        }
        if self.fault {
            labels.push(text(lang, Key::TrayAudioFault).into());
        }
        if let Some(status) = &self.snapshot {
            if status
                .backend
                .iter()
                .any(|backend| backend.availability == wire::AudioAvailability::Unavailable as i32)
            {
                labels.push(text(lang, Key::TrayAudioFault).into());
            }
            for session in &status.sessions {
                let key = if session.local_role == wire::AudioRole::Source as i32 {
                    match session.sending {
                        Some(true) => Key::TrayAudioSharing,
                        Some(false) => Key::AudioNotSending,
                        None => Key::AudioSendingUnknown,
                    }
                } else {
                    Key::TrayAudioReceiving
                };
                // Retain privacy information even while a connection/query is unavailable.
                if matches!(
                    wire::AudioSessionState::try_from(session.state),
                    Ok(wire::AudioSessionState::Running
                        | wire::AudioSessionState::Rebuffering
                        | wire::AudioSessionState::DevicePaused
                        | wire::AudioSessionState::Stopping)
                ) {
                    labels.push(format!("{}: {}", text(lang, key), peer_name(session)));
                }
                if session.local_role == wire::AudioRole::Source as i32 {
                    if let Some(active) = session.capture_active {
                        labels.push(
                            text(
                                lang,
                                if active {
                                    Key::AudioSystemCaptureActive
                                } else {
                                    Key::AudioSystemCaptureInactive
                                },
                            )
                            .into(),
                        );
                    }
                }
                if session.state == wire::AudioSessionState::Failed as i32 {
                    labels.push(format!(
                        "{}: {}",
                        text(lang, Key::TrayAudioFault),
                        peer_name(session)
                    ));
                }
            }
            if let Some(mic) = &status.microphone {
                if mic.control_active {
                    labels.push(format!(
                        "{}: {}",
                        text(lang, Key::TrayAudioMicrophoneControl),
                        hex(&mic.target_node_id)
                    ));
                }
                if mic.capture_active {
                    labels.push(text(lang, Key::TrayAudioCaptureActive).into());
                }
            }
            if labels.is_empty() {
                labels.push(text(lang, Key::TrayAudioIdle).into());
            }
        }
        labels
    }

    pub fn items(&self, lang: Lang) -> Vec<Item> {
        let mut items: Vec<_> = self
            .labels(lang)
            .into_iter()
            .map(|label| Item {
                label,
                action: None,
            })
            .collect();
        for (key, action) in [
            (Key::TrayAudioPlayer, Action::OpenAudioPlayer),
            (Key::TrayAudioSettings, Action::OpenAudioSettings),
            (Key::TrayAudioStopAll, Action::StopAllAudio),
        ] {
            let enabled = matches!(action, Action::OpenAudioPlayer | Action::OpenAudioSettings)
                || self.request(&action).is_some();
            items.push(Item {
                label: text(lang, key).into(),
                action: enabled.then_some(action),
            });
        }
        if let Some(status) = &self.snapshot {
            for session in &status.sessions {
                if status.role == wire::AudioRole::Listener as i32
                    && session.local_role == wire::AudioRole::Listener as i32
                {
                    let action = Action::AudioReceiveMute(session.peer.clone());
                    items.push(Item {
                        label: format!(
                            "{}{}: {}",
                            if session.muted { "✓ " } else { "" },
                            text(lang, Key::TrayAudioMuteReceive),
                            peer_name(session)
                        ),
                        action: self.request(&action).is_some().then_some(action),
                    });
                }
            }
        }
        for (key, action) in [
            (Key::TrayAudioMuteMicrophone, Action::MicMute),
            (Key::TrayAudioReleasePtt, Action::MicPttRelease),
        ] {
            let muted = matches!(action, Action::MicMute)
                && self
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.microphone.as_ref())
                    .is_some_and(|m| m.muted);
            items.push(Item {
                label: format!("{}{}", if muted { "✓ " } else { "" }, text(lang, key)),
                action: self.request(&action).is_some().then_some(action),
            });
        }
        items
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(value, "{byte:02x}");
    }
    value
}

fn peer_name(session: &wire::AudioSessionStatus) -> String {
    if session.source_name.is_empty() {
        hex(&session.peer)
    } else {
        session.source_name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> wire::AudioStatus {
        wire::AudioStatus {
            revision: 7,
            role: wire::AudioRole::Listener as i32,
            mode: wire::AudioMode::MixAll as i32,
            config: Some(wire::AudioConfig {
                role: wire::AudioRole::Listener as i32,
                mode: wire::AudioMode::MixAll as i32,
                profile: wire::AudioProfile::Ethernet as i32,
                udp_port: 47903,
                output_device: "default".into(),
                capture_device: "default_monitor".into(),
                focus_debounce_ms: 150,
                focus_ramp_ms: 100,
                background_gain_db: -12,
                microphone: Some(wire::AudioMicrophoneConfig {
                    capture_device: "default".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            microphone: Some(wire::AudioMicrophoneState::default()),
            ..Default::default()
        }
    }

    fn connected() -> Audio {
        let mut audio = Audio::default();
        audio.receive(0, &Msg::AudioStatus(snapshot()));
        audio
    }

    fn session() -> wire::AudioSessionStatus {
        wire::AudioSessionStatus {
            peer: vec![3; 32],
            session_id: vec![4; 16],
            source_name: "AUDIO SOURCE".into(),
            local_role: wire::AudioRole::Listener as i32,
            state: wire::AudioSessionState::Running as i32,
            profile: wire::AudioProfile::Ethernet as i32,
            health_state: wire::AudioStreamHealth::Ready as i32,
            kind: wire::AudioKind::System as i32,
            health: Some(wire::AudioHealth::default()),
            consent: Some(wire::AudioSessionConsent {
                local: Some(wire::AudioConsentFlags::default()),
                remote: Some(wire::AudioConsentFlags::default()),
            }),
            gain_percent: 37,
            ..Default::default()
        }
    }

    #[test]
    fn receive_mute_preserves_gain_and_never_changes_snapshot() {
        let mut audio = connected();
        audio.snapshot.as_mut().unwrap().sessions.push(session());
        let peer = vec![3; 32];
        let Some(Msg::SetAudioPeerGain(request)) =
            audio.request(&Action::AudioReceiveMute(peer.clone()))
        else {
            panic!("gain request");
        };
        assert_eq!(request.peer_node_id, peer);
        assert_eq!(request.gain_percent, 37);
        assert_eq!(request.expected_revision, 7);
        assert!(request.muted);
        assert!(!audio.snapshot.as_ref().unwrap().sessions[0].muted);
        audio.snapshot.as_mut().unwrap().role = wire::AudioRole::Source as i32;
        assert!(audio
            .request(&Action::AudioReceiveMute(vec![3; 32]))
            .is_none());
    }

    #[test]
    fn stop_and_ptt_release_are_explicit_and_not_optimistic() {
        let mut audio = connected();
        let Some(Msg::StopAudio(stop)) = audio.request(&Action::StopAllAudio) else {
            panic!("stop request");
        };
        assert!(stop.stop_all);
        assert!(stop.peer_node_id.is_empty() && stop.session_id.is_empty());
        audio.sent(10, Instant::now());
        let Some(Msg::SetPtt(ptt)) = audio.request(&Action::MicPttRelease) else {
            panic!("release request");
        };
        assert!(!ptt.pressed);
        assert_eq!(ptt.expected_revision, 7);
        audio.receive(
            10,
            &Msg::OpResult(wire::OpResult {
                ok: true,
                error_code: String::new(),
            }),
        );
        assert!(audio.request(&Action::MicMute).is_some());
        assert_eq!(audio.snapshot.as_ref().unwrap().revision, 7);
        assert!(
            !audio
                .snapshot
                .as_ref()
                .unwrap()
                .microphone
                .as_ref()
                .unwrap()
                .muted
        );
    }

    #[test]
    fn unknown_core_disables_actions_but_timeout_keeps_local_safety() {
        let now = Instant::now();
        let mut audio = Audio::default();
        audio.queried(1, now);
        audio.receive(
            1,
            &Msg::OpResult(wire::OpResult {
                ok: false,
                error_code: "UnknownCommand".into(),
            }),
        );
        assert!(audio.request(&Action::StopAllAudio).is_none());
        assert!(audio
            .labels(Lang::En)
            .contains(&text(Lang::En, Key::TrayAudioUnavailable).to_owned()));
        let mut audio = connected();
        audio.queried(2, now);
        audio.expire(now + RESPONSE_TIMEOUT);
        assert!(audio.request(&Action::MicMute).is_some());
        assert!(audio.can_query());
    }

    #[test]
    fn correlated_snapshots_are_monotonic_and_reconnect_resets_namespace() {
        let mut audio = connected();
        let mut status = snapshot();
        status.revision = 6;
        audio.receive(0, &Msg::AudioStatus(status));
        assert_eq!(audio.snapshot.as_ref().unwrap().revision, 7);
        audio.queried(8, Instant::now());
        let mut status = snapshot();
        status.revision = 9;
        audio.receive(999, &Msg::AudioStatus(status.clone()));
        assert_eq!(audio.snapshot.as_ref().unwrap().revision, 7);
        audio.receive(8, &Msg::AudioStatus(status));
        assert_eq!(audio.snapshot.as_ref().unwrap().revision, 9);
        audio.disconnected();
        audio.queried(10, Instant::now());
        let mut restarted = snapshot();
        restarted.revision = 1;
        audio.receive(10, &Msg::AudioStatus(restarted));
        assert_eq!(audio.snapshot.as_ref().unwrap().revision, 1);
    }

    #[test]
    fn privacy_is_separate_from_input_and_survives_disconnect() {
        let mut audio = connected();
        audio.snapshot.as_mut().unwrap().sessions.push(session());
        let mic = audio
            .snapshot
            .as_mut()
            .unwrap()
            .microphone
            .as_mut()
            .unwrap();
        mic.control_active = true;
        mic.capture_active = true;
        mic.target_node_id = vec![5; 32];
        audio.disconnected();
        for lang in [Lang::En, Lang::Ru] {
            let labels = audio.labels(lang).join("\\n");
            assert!(labels.contains("AUDIO SOURCE"));
            assert!(labels.contains(text(lang, Key::TrayAudioReceiving)));
            assert!(labels.contains(text(lang, Key::TrayAudioMicrophoneControl)));
            assert!(labels.contains(text(lang, Key::TrayAudioCaptureActive)));
            assert!(audio
                .items(lang)
                .iter()
                .any(|item| item.action == Some(Action::OpenAudioSettings)));
            assert!(audio.request(&Action::StopAllAudio).is_none());
        }
    }

    #[test]
    fn malformed_snapshot_is_not_published() {
        let mut audio = connected();
        let mut bad = snapshot();
        bad.role = 0;
        bad.revision = 8;
        audio.receive(0, &Msg::AudioStatus(bad));
        assert_eq!(audio.snapshot.as_ref().unwrap().revision, 7);
        assert!(audio.request(&Action::StopAllAudio).is_some());
    }

    #[test]
    fn source_recipient_and_fault_are_persistent_privacy_labels() {
        let mut audio = connected();
        let mut sending = session();
        sending.local_role = wire::AudioRole::Source as i32;
        sending.capture_active = Some(true);
        sending.sending = Some(true);
        audio.snapshot.as_mut().unwrap().sessions.push(sending);
        for lang in [Lang::En, Lang::Ru] {
            assert!(audio
                .labels(lang)
                .iter()
                .any(|label| label.contains(text(lang, Key::TrayAudioSharing))
                    && label.contains("AUDIO SOURCE")));
        }
        audio.snapshot.as_mut().unwrap().sessions[0].state = wire::AudioSessionState::Failed as i32;
        assert!(audio
            .labels(Lang::En)
            .iter()
            .any(|label| label.contains(text(Lang::En, Key::TrayAudioFault))));
    }

    #[test]
    fn microphone_mute_uses_authoritative_revision_without_changing_state() {
        let audio = connected();
        let Some(Msg::SetMicrophoneMute(request)) = audio.request(&Action::MicMute) else {
            panic!("microphone request");
        };
        assert!(request.muted);
        assert_eq!(request.expected_revision, 7);
        assert!(
            !audio
                .snapshot
                .as_ref()
                .unwrap()
                .microphone
                .as_ref()
                .unwrap()
                .muted
        );
    }

    #[test]
    fn unavailable_pending_transport_only_allows_transitions_toward_safety() {
        let mut audio = connected();
        audio.snapshot.as_mut().unwrap().sessions.push(session());
        audio.sent(10, Instant::now());
        audio.available = false;
        assert!(audio.request(&Action::StopAllAudio).is_some());
        assert!(audio.request(&Action::MicPttRelease).is_some());
        assert!(audio.request(&Action::MicMute).is_some());
        let peer = vec![3; 32];
        let Some(Msg::SetAudioPeerGain(gain)) =
            audio.request(&Action::AudioReceiveMute(peer.clone()))
        else {
            panic!("safety mute");
        };
        assert!(gain.muted);
        assert_eq!(gain.gain_percent, 37);
        assert_eq!(gain.expected_revision, 7);
        audio.snapshot.as_mut().unwrap().sessions[0].muted = true;
        audio
            .snapshot
            .as_mut()
            .unwrap()
            .microphone
            .as_mut()
            .unwrap()
            .muted = true;
        assert!(audio.request(&Action::AudioReceiveMute(peer)).is_none());
        assert!(audio.request(&Action::MicMute).is_none());
        audio.disconnected();
        assert!(audio.request(&Action::StopAllAudio).is_none());
        assert!(audio.request(&Action::MicPttRelease).is_none());
    }
}
