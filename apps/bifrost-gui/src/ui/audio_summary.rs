//! Human audio state; backend diagnostics stay behind Details.
use eframe::egui;
use protocol::ipc::{self, AudioRole};
use ui_text::{text, Key, Lang};

use super::{theme::Palette, widgets};
use crate::audio::AudioAction;
use crate::view_model::{Action, ViewModel};

pub(super) fn draw(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel, actions: &mut Vec<Action>) {
    let lang = vm.look.language;
    let held_id = ui.id().with("audio-summary-ptt-held");
    let was_held = ui
        .data_mut(|data| data.get_temp::<bool>(held_id))
        .unwrap_or(false);
    let mut held = false;
    let mut release_requested = false;
    egui::Frame::new()
        .fill(pal.bg2)
        .corner_radius(super::theme::R)
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                widgets::text(
                    ui,
                    text(lang, Key::AudioDiagnostics),
                    16.0,
                    super::theme::fam::sans_semibold(),
                    pal.ink,
                );
                if widgets::small_button(ui, pal, text(lang, Key::AudioRefresh), !vm.audio.pending)
                    .clicked()
                {
                    actions.push(Action::Audio(AudioAction::Refresh));
                }
            });
            let Some(status) = &vm.audio.status else {
                widgets::hint(
                    ui,
                    pal,
                    text(
                        lang,
                        if vm.audio.available {
                            Key::AudioWaiting
                        } else {
                            Key::AudioUnavailable
                        },
                    ),
                );
                return;
            };
            if !vm.audio.available {
                widgets::hint(ui, pal, text(lang, Key::AudioUnavailable));
            }
            if status.sessions.is_empty() {
                widgets::hint(ui, pal, text(lang, Key::AudioIdle));
            }
            for session in &status.sessions {
                widgets::wrapped(
                    ui,
                    &session_summary(vm, session),
                    13.0,
                    super::theme::fam::sans(),
                    pal.ink,
                );
                if session.local_role == AudioRole::Source as i32 {
                    let capture = match session.capture_active {
                        Some(true) => text(lang, Key::AudioSystemCaptureActive).to_owned(),
                        Some(false) => text(lang, Key::AudioSystemCaptureInactive).to_owned(),
                        None => format!(
                            "{}: {}",
                            text(lang, Key::AudioCapabilityCapture),
                            text(lang, Key::AudioUnknown)
                        ),
                    };
                    widgets::hint(ui, pal, &capture);
                }
            }
            if let Some(mic) = &status.microphone {
                let mut summary = format!(
                    "{} · {}",
                    text(
                        lang,
                        if mic.muted {
                            Key::AudioMicMuted
                        } else {
                            Key::AudioMicNotMuted
                        }
                    ),
                    text(
                        lang,
                        if mic.control_active {
                            Key::AudioMicControlActive
                        } else {
                            Key::AudioMicControlInactive
                        }
                    )
                );
                if !mic.target_node_id.is_empty() {
                    summary.push_str(&format!(" · {}", peer_name(vm, &mic.target_node_id)));
                }
                if mic.capture_active {
                    summary.push_str(&format!(" · {}", text(lang, Key::AudioMicCaptureActive)));
                }
                if mic.ptt_pressed {
                    summary.push_str(&format!(" · {}", text(lang, Key::AudioPttPressed)));
                }
                widgets::wrapped(ui, &summary, 13.0, super::theme::fam::sans(), pal.ink);
            }
            let connected = vm.connection.is_connected();
            let available = connected && vm.audio.available;
            ui.horizontal_wrapped(|ui| {
                if widgets::button(
                    ui,
                    pal,
                    text(lang, Key::AudioStopAll),
                    widgets::Kind::Plain,
                    connected && !status.sessions.is_empty(),
                )
                .clicked()
                {
                    actions.push(Action::Audio(AudioAction::StopAll));
                }
                if let Some(mic) = &status.microphone {
                    // A retained unmuted snapshot must never disable urgent mute.
                    let can_mute = if mic.muted {
                        available && mic.control_active && !vm.audio.pending
                    } else {
                        connected
                    };
                    if widgets::small_button(
                        ui,
                        pal,
                        text(
                            lang,
                            if mic.muted {
                                Key::AudioMicUnmute
                            } else {
                                Key::AudioMicMute
                            },
                        ),
                        can_mute,
                    )
                    .clicked()
                    {
                        actions.push(Action::Audio(AudioAction::SetMicrophoneMute(!mic.muted)));
                    }
                    let ptt_enabled = status
                        .config
                        .as_ref()
                        .and_then(|c| c.microphone.as_ref())
                        .is_some_and(|m| m.control_enabled && m.ptt_enabled);
                    if ptt_enabled {
                        let can_hold = available
                            && mic.control_active
                            && (was_held || vm.audio.gui_ptt_held || !vm.audio.pending);
                        let response = widgets::small_button(
                            ui,
                            pal,
                            text(lang, Key::AudioPttPress),
                            can_hold,
                        );
                        // Response helpers acquire egui memory: evaluate outside the input lock.
                        let pointer_held = response.is_pointer_button_down_on();
                        let focused_button = response.has_focus();
                        held = can_hold
                            && ui.ctx().input(|input| {
                                input.focused
                                    && (pointer_held
                                        || focused_button
                                            && (input.key_down(egui::Key::Space)
                                                || input.key_down(egui::Key::Enter)))
                            });
                    }
                    if mic.ptt_pressed
                        && widgets::small_button(
                            ui,
                            pal,
                            text(lang, Key::AudioPttRelease),
                            connected,
                        )
                        .clicked()
                    {
                        held = false;
                        release_requested = true;
                    }
                }
            });
            ui.collapsing(text(lang, Key::AudioDetails), |ui| {
                backend_details(ui, pal, status, lang);
                for session in &status.sessions {
                    if let Some(health) = &session.health {
                        widgets::hint(ui, pal, &session_summary(vm, session));
                        widgets::hint(
                            ui,
                            pal,
                            &format!(
                                "{}: {} / {} / {} / {} / {} / {} / {}",
                                text(lang, Key::AudioHealthCounters),
                                health.received_packets,
                                health.lost_frames,
                                health.late_frames,
                                health.concealed_frames,
                                health.underruns,
                                health.discontinuities,
                                health.limiter_frames
                            ),
                        );
                    }
                }
            });
        });
    // Also release when the microphone/config/snapshot disappears, not only on pointer-up.
    if release_requested || held != was_held || !held && vm.audio.gui_ptt_held {
        actions.push(Action::Audio(AudioAction::SetPtt(held)));
    }
    ui.data_mut(|data| data.insert_temp(held_id, held));
}

pub(super) fn peer_name(vm: &ViewModel, id: &[u8]) -> String {
    let member = vm.status.as_ref().and_then(|status| {
        status
            .members
            .iter()
            .find(|member| matches_id(&member.node_id, id))
    });
    if let Some(member) = member.filter(|member| !member.display_name.trim().is_empty()) {
        return member.display_name.clone();
    }
    short_hex(id)
}

fn matches_id(hex: &str, id: &[u8]) -> bool {
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

fn short_hex(id: &[u8]) -> String {
    use std::fmt::Write;
    let mut result = String::with_capacity(8);
    for byte in id.iter().take(4) {
        let _ = write!(result, "{byte:02x}");
    }
    result
}

fn session_summary(vm: &ViewModel, session: &ipc::AudioSessionStatus) -> String {
    let lang = vm.look.language;
    let state = lifecycle(session.state);
    let direction = if session.local_role == AudioRole::Source as i32 {
        match session.sending {
            Some(true) => Key::AudioSharing,
            Some(false) => Key::AudioNotSending,
            None => Key::AudioSendingUnknown,
        }
    } else if matches!(
        ipc::AudioSessionState::try_from(session.state),
        Ok(ipc::AudioSessionState::Running | ipc::AudioSessionState::Rebuffering)
    ) {
        Key::AudioReceiving
    } else {
        Key::AudioListenerFrom
    };
    let mut summary = format!(
        "{} {} · {}",
        text(lang, direction),
        peer_name(vm, &session.peer),
        text(lang, state)
    );
    if let Some(latency) = session.latency_estimate_ms {
        summary.push_str(&format!(
            " · ~{latency} {}",
            text(lang, Key::AudioMilliseconds)
        ));
    }
    summary
}

fn lifecycle(state: i32) -> Key {
    match ipc::AudioSessionState::try_from(state).ok() {
        Some(ipc::AudioSessionState::Idle) => Key::AudioStateIdle,
        Some(ipc::AudioSessionState::AwaitConsent) => Key::AudioStateConsent,
        Some(ipc::AudioSessionState::Preparing) => Key::AudioStatePreparing,
        Some(ipc::AudioSessionState::AwaitCommit) => Key::AudioStateCommit,
        Some(ipc::AudioSessionState::PathValidation) => Key::AudioStatePath,
        Some(ipc::AudioSessionState::Running) => Key::AudioStateRunning,
        Some(ipc::AudioSessionState::Rebuffering) => Key::AudioStateRebuffering,
        Some(ipc::AudioSessionState::DevicePaused) => Key::AudioStateDevicePaused,
        Some(ipc::AudioSessionState::Stopping) => Key::AudioStateStopping,
        Some(ipc::AudioSessionState::Failed) => Key::AudioStateFailed,
        _ => Key::AudioUnknown,
    }
}

fn backend_details(ui: &mut egui::Ui, pal: &Palette, status: &ipc::AudioStatus, lang: Lang) {
    for (capability, key) in [
        (
            ipc::AudioBackendCapability::SystemCapture,
            Key::AudioCapabilityCapture,
        ),
        (
            ipc::AudioBackendCapability::Render,
            Key::AudioCapabilityRender,
        ),
        (
            ipc::AudioBackendCapability::EndpointMute,
            Key::AudioCapabilityMute,
        ),
        (
            ipc::AudioBackendCapability::GlobalPtt,
            Key::AudioCapabilityPtt,
        ),
        (
            ipc::AudioBackendCapability::MediaSessions,
            Key::AudioCapabilityMedia,
        ),
    ] {
        let report = status
            .backend
            .iter()
            .find(|b| b.capability == capability as i32);
        let availability = match report.map(|b| b.availability) {
            Some(v) if v == ipc::AudioAvailability::Available as i32 => Key::AudioAvailable,
            Some(v) if v == ipc::AudioAvailability::Unsupported as i32 => Key::AudioUnsupported,
            _ => Key::AudioUnknown,
        };
        widgets::hint(
            ui,
            pal,
            &format!("{}: {}", text(lang, key), text(lang, availability)),
        );
        if let Some(report) =
            report.filter(|b| b.availability != ipc::AudioAvailability::Available as i32)
        {
            let reason = match ipc::AudioUnavailableReason::try_from(report.reason).ok() {
                Some(ipc::AudioUnavailableReason::FeatureNotBuilt) => Key::AudioReasonFeature,
                Some(ipc::AudioUnavailableReason::NotSupportedByPlatform) => {
                    Key::AudioReasonPlatform
                }
                Some(ipc::AudioUnavailableReason::OsBuildTooOld) => Key::AudioReasonOs,
                Some(ipc::AudioUnavailableReason::ProcessExclusionUnavailable) => {
                    Key::AudioReasonExclusion
                }
                Some(ipc::AudioUnavailableReason::ReleaseObservationUnavailable) => {
                    Key::AudioReasonRelease
                }
                Some(ipc::AudioUnavailableReason::ServerUnavailable) => Key::AudioReasonServer,
                Some(ipc::AudioUnavailableReason::PermissionDenied) => Key::AudioReasonPermission,
                Some(ipc::AudioUnavailableReason::DeviceGone) => Key::AudioReasonDevice,
                Some(ipc::AudioUnavailableReason::DeviceNotSelected) => Key::AudioReasonSelection,
                Some(ipc::AudioUnavailableReason::WorkerUnavailable) => Key::AudioReasonWorker,
                _ => Key::AudioUnknown,
            };
            widgets::hint(ui, pal, text(lang, reason));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_is_eight_lowercase_hex_digits() {
        assert_eq!(short_hex(&[0, 15, 128, 255, 42]), "000f80ff");
        assert!(matches_id("000F80FF", &[0, 15, 128, 255]));
        assert!(!matches_id("000f80fz", &[0, 15, 128, 255]));
        assert!(!matches_id("000f80", &[0, 15, 128, 255]));
    }

    #[test]
    fn trusted_names_override_session_metadata_and_running_does_not_imply_sending() {
        use crate::view_model::{CoreEndpoint, LocalConfig};
        let mut vm = ViewModel::new(
            CoreEndpoint::Ready {
                description: "test".into(),
            },
            LocalConfig::default(),
        );
        vm.look.language = Lang::En;
        let peer = vec![0xab; 32];
        vm.status = Some(ipc::Status {
            members: vec![ipc::MemberStatus {
                node_id: "ab".repeat(32),
                display_name: "LAPTOP".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let mut session = ipc::AudioSessionStatus {
            peer: peer.clone(),
            source_name: "untrusted metadata".into(),
            local_role: AudioRole::Source as i32,
            state: ipc::AudioSessionState::Running as i32,
            sending: Some(false),
            latency_estimate_ms: Some(42),
            ..Default::default()
        };
        assert_eq!(peer_name(&vm, &peer), "LAPTOP");
        assert_eq!(
            session_summary(&vm, &session),
            "System audio is not being sent to LAPTOP · Running · ~42 ms"
        );
        session.sending = None;
        assert!(
            session_summary(&vm, &session).starts_with(text(Lang::En, Key::AudioSendingUnknown))
        );
        session.sending = Some(true);
        assert!(session_summary(&vm, &session).starts_with(text(Lang::En, Key::AudioSharing)));
        vm.status = None;
        assert_eq!(peer_name(&vm, &peer), "abababab");
    }
}
