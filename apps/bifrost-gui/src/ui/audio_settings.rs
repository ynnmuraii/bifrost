//! Native audio settings: editable drafts and authoritative, privacy-visible status.
use eframe::egui;
use protocol::ipc::{self, AudioDeviceKind, AudioMode, AudioProfile, AudioRole};
use ui_text::{text, Key, Lang};

use super::theme::Palette;
use super::{modal, widgets};
use crate::audio::AudioAction;
use crate::view_model::{Action, ViewModel};

/// Draw settings without issuing IPC or inferring successful activation.
pub fn body(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel, actions: &mut Vec<Action>) {
    if !ui.ctx().fonts(|fonts| {
        fonts
            .definitions()
            .families
            .contains_key(&super::theme::fam::sans_medium())
    }) {
        super::install_fonts(ui.ctx());
        ui.ctx().request_repaint();
        return;
    }
    let lang = vm.look.language;
    let audio = &vm.audio;
    let ready = audio.available && audio.status.is_some();
    let editable = ready && !audio.pending;
    let mut config = audio.draft.clone();
    let mut valid_group = true;
    let mut apply = false;
    let mut create_group = false;
    egui::ScrollArea::vertical()
        .id_salt("audio-settings-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 10.0;
            diagnostics(ui, pal, vm, actions);
            if let Some(error) = &audio.error {
                widgets::wrapped(ui, error, 13.0, super::theme::fam::sans(), pal.accent);
            }
            if !ready {
                widgets::hint(
                    ui,
                    pal,
                    text(
                        lang,
                        if audio.available {
                            Key::AudioWaiting
                        } else {
                            Key::AudioUnavailable
                        },
                    ),
                );
            }
            if audio.pending {
                widgets::hint(ui, pal, text(lang, Key::AudioPending));
            }
            if widgets::small_button(ui, pal, text(lang, Key::AudioRefresh), !audio.pending)
                .clicked()
            {
                actions.push(Action::Audio(AudioAction::Refresh));
            }
            ui.add_enabled_ui(editable, |ui| {
                section(ui, pal, text(lang, Key::AudioSettings));
                ui.checkbox(&mut config.enabled, text(lang, Key::AudioEnabled));
                setting(ui, pal, text(lang, Key::AudioRole), |ui| {
                    enum_choice(
                        ui,
                        "audio-role",
                        &mut config.role,
                        &[
                            (AudioRole::Source as i32, text(lang, Key::AudioSource)),
                            (AudioRole::Listener as i32, text(lang, Key::AudioListener)),
                        ],
                        lang,
                    );
                    widgets::hint(ui, pal, text(lang, Key::AudioRoleWarning));
                });
                setting(ui, pal, text(lang, Key::AudioListenerTarget), |ui| {
                    peer_selector(ui, vm, "audio-listener", &mut config.listener_node_id);
                });
                setting(ui, pal, text(lang, Key::AudioGroup), |ui| {
                    // Keep partial text locally so malformed input never changes the wire draft.
                    let id = ui.id().with("audio-group-input");
                    let mut group = ui
                        .data_mut(|data| data.get_temp::<String>(id))
                        .unwrap_or_else(|| hex(&config.group_id));
                    let label = ui.label(text(lang, Key::AudioGroup));
                    let response = ui
                        .add(
                            egui::TextEdit::singleline(&mut group)
                                .desired_width((ui.available_width() - 8.0).max(24.0))
                                .char_limit(32),
                        )
                        .labelled_by(label.id);
                    let parsed = parse_hex(&group, 16);
                    valid_group = parsed.is_some() && (!config.enabled || !group.is_empty());
                    if response.changed() {
                        if let Some(bytes) = parsed {
                            config.group_id = bytes;
                        }
                        ui.data_mut(|data| data.insert_temp(id, group));
                    } else if !response.has_focus() && valid_group && group != hex(&config.group_id)
                    {
                        ui.data_mut(|data| data.remove::<String>(id));
                    }
                    if config.role == AudioRole::Listener as i32
                        && widgets::small_button(ui, pal, text(lang, Key::AudioCreateGroup), true)
                            .clicked()
                    {
                        ui.data_mut(|data| data.remove::<String>(id));
                        create_group = true;
                    }
                    widgets::hint(
                        ui,
                        pal,
                        text(
                            lang,
                            if valid_group {
                                Key::AudioGroupHint
                            } else {
                                Key::AudioInvalidGroup
                            },
                        ),
                    );
                });
                setting(ui, pal, text(lang, Key::AudioMode), |ui| {
                    enum_choice(
                        ui,
                        "audio-mode",
                        &mut config.mode,
                        &[
                            (AudioMode::MixAll as i32, text(lang, Key::AudioMixAll)),
                            (
                                AudioMode::FollowFocus as i32,
                                text(lang, Key::AudioFollowFocus),
                            ),
                        ],
                        lang,
                    );
                });
                setting(ui, pal, text(lang, Key::AudioProfile), |ui| {
                    enum_choice(
                        ui,
                        "audio-profile",
                        &mut config.profile,
                        &[
                            (
                                AudioProfile::Ethernet as i32,
                                text(lang, Key::AudioEthernet),
                            ),
                            (AudioProfile::Wifi as i32, text(lang, Key::AudioWifi)),
                        ],
                        lang,
                    );
                });
                section(ui, pal, text(lang, Key::AudioDevices));
                device_selector(
                    ui,
                    pal,
                    vm,
                    AudioDeviceKind::Output,
                    Key::AudioOutputDevice,
                    &mut config.output_device,
                    actions,
                );
                device_selector(
                    ui,
                    pal,
                    vm,
                    AudioDeviceKind::Capture,
                    Key::AudioCaptureDevice,
                    &mut config.capture_device,
                    actions,
                );
                ui.checkbox(
                    &mut config.output_fallback_to_default,
                    text(lang, Key::AudioOutputFallback),
                );
                ui.checkbox(
                    &mut config.resume_system_audio,
                    text(lang, Key::AudioResume),
                );
                section(ui, pal, text(lang, Key::AudioTuning));
                setting(ui, pal, text(lang, Key::AudioUdpPort), |ui| {
                    let label = ui.label(text(lang, Key::AudioUdpPort));
                    ui.add(egui::DragValue::new(&mut config.udp_port).range(1..=65535))
                        .labelled_by(label.id);
                });
                setting(ui, pal, text(lang, Key::AudioDebounce), |ui| {
                    let label = ui.label(text(lang, Key::AudioDebounce));
                    ui.add(egui::DragValue::new(&mut config.focus_debounce_ms).range(100..=2000))
                        .labelled_by(label.id);
                });
                setting(ui, pal, text(lang, Key::AudioRamp), |ui| {
                    let label = ui.label(text(lang, Key::AudioRamp));
                    ui.add(egui::DragValue::new(&mut config.focus_ramp_ms).range(20..=1000))
                        .labelled_by(label.id);
                });
                setting(ui, pal, text(lang, Key::AudioBackgroundGain), |ui| {
                    ui.add(
                        egui::Slider::new(&mut config.background_gain_db, -60..=0)
                            .text(text(lang, Key::AudioBackgroundGain)),
                    );
                });
                ui.checkbox(
                    &mut config.background_mute,
                    text(lang, Key::AudioBackgroundMute),
                );
                section(ui, pal, text(lang, Key::AudioPermissions));
                widgets::hint(ui, pal, text(lang, Key::AudioPermissionHint));
                permissions(ui, pal, vm, &mut config, actions);
                section(ui, pal, text(lang, Key::AudioMicrophone));
                widgets::hint(ui, pal, text(lang, Key::AudioMicPrivacy));
                let mic = config
                    .microphone
                    .get_or_insert_with(|| ipc::AudioMicrophoneConfig {
                        muted: true,
                        capture_device: "default".into(),
                        ..Default::default()
                    });
                ui.checkbox(&mut mic.control_enabled, text(lang, Key::AudioMicControl));
                device_selector(
                    ui,
                    pal,
                    vm,
                    AudioDeviceKind::Microphone,
                    Key::AudioMicDevice,
                    &mut mic.capture_device,
                    actions,
                );
                ui.checkbox(
                    &mut mic.allow_default_mic_changes,
                    text(lang, Key::AudioAllowDefaultMic),
                );
                ui.checkbox(&mut mic.muted, text(lang, Key::AudioMicSavedMute));
                ui.checkbox(&mut mic.ptt_enabled, text(lang, Key::AudioPttEnabled));
                setting(ui, pal, text(lang, Key::AudioPttHotkey), |ui| {
                    let label = ui.label(text(lang, Key::AudioPttHotkey));
                    ui.add(
                        egui::TextEdit::singleline(&mut mic.ptt_hotkey)
                            .desired_width((ui.available_width() - 8.0).max(24.0))
                            .char_limit(128),
                    )
                    .labelled_by(label.id);
                });
                setting(ui, pal, text(lang, Key::AudioPttTarget), |ui| {
                    peer_selector(ui, vm, "audio-mic-target", &mut mic.pinned_target_node_id);
                });
                widgets::hint(ui, pal, text(lang, Key::AudioMicStreamingDeferred));
                // Deferred streaming is not a working toggle or a fabricated virtual endpoint.
                ui.add_enabled(
                    false,
                    egui::Checkbox::new(
                        &mut mic.streaming_enabled,
                        text(lang, Key::AudioMicStreaming),
                    ),
                );
                ui.add_enabled(
                    false,
                    egui::Checkbox::new(
                        &mut mic.experimental_acknowledged,
                        text(lang, Key::AudioExperimentalAcknowledged),
                    ),
                );
                setting(ui, pal, text(lang, Key::AudioVirtualDevice), |ui| {
                    let label = ui.label(text(lang, Key::AudioVirtualDevice));
                    ui.add_enabled(
                        false,
                        egui::TextEdit::singleline(&mut mic.virtual_device)
                            .desired_width((ui.available_width() - 8.0).max(24.0)),
                    )
                    .labelled_by(label.id);
                });
                widgets::hint(ui, pal, text(lang, Key::AudioDraftHint));
                let changed = config != audio.draft
                    || audio
                        .status
                        .as_ref()
                        .and_then(|s| s.config.as_ref())
                        .is_some_and(|saved| saved != &config);
                apply = widgets::button(
                    ui,
                    pal,
                    text(lang, Key::AudioApply),
                    widgets::Kind::Solid,
                    valid_group && changed,
                )
                .clicked();
            });
        });
    if editable && config != audio.draft {
        actions.push(Action::Audio(AudioAction::EditConfig(Box::new(config))));
    }
    if create_group {
        actions.push(Action::Audio(AudioAction::CreateGroup));
    }
    if apply {
        actions.push(Action::Audio(AudioAction::ApplyConfig));
    }
}

fn section(ui: &mut egui::Ui, pal: &Palette, label: &str) {
    ui.add_space(16.0);
    widgets::text(ui, label, 16.0, super::theme::fam::sans_semibold(), pal.ink);
    ui.add_space(4.0);
}

fn setting(ui: &mut egui::Ui, pal: &Palette, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.push_id(label, |ui| {
        if ui.available_width() < 520.0 {
            widgets::hint(ui, pal, label);
            add(ui);
            ui.add_space(6.0);
        } else {
            modal::row(ui, pal, label, false, add);
        }
    });
}

fn enum_choice(
    ui: &mut egui::Ui,
    id: &str,
    selected: &mut i32,
    values: &[(i32, &str)],
    lang: Lang,
) {
    let label = values
        .iter()
        .find(|(v, _)| v == selected)
        .map_or(text(lang, Key::AudioUnknown), |(_, name)| *name);
    egui::ComboBox::from_id_salt(id)
        .selected_text(label)
        .width(ui.available_width().min(280.0))
        .show_ui(ui, |ui| {
            for (value, name) in values {
                ui.selectable_value(selected, *value, *name);
            }
        });
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(value, "{byte:02x}");
    }
    value
}

fn parse_hex(value: &str, bytes: usize) -> Option<Vec<u8>> {
    if value.is_empty() {
        return (bytes == 16).then(Vec::new);
    }
    if value.len() != bytes * 2 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let digit = |b: u8| (b as char).to_digit(16).map(|n| n as u8);
            Some(digit(pair[0])? * 16 + digit(pair[1])?)
        })
        .collect()
}

fn peer_selector(ui: &mut egui::Ui, vm: &ViewModel, id: &str, selected: &mut Vec<u8>) {
    let current = hex(selected);
    let label = vm
        .status
        .as_ref()
        .and_then(|s| {
            s.members
                .iter()
                .find(|p| p.node_id.eq_ignore_ascii_case(&current))
        })
        .map(|p| p.display_name.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(if selected.is_empty() {
            text(vm.look.language, Key::AudioNoTarget)
        } else {
            &current
        });
    egui::ComboBox::from_id_salt(id)
        .selected_text(label)
        .width(ui.available_width().min(280.0))
        .show_ui(ui, |ui| {
            ui.selectable_value(
                selected,
                Vec::new(),
                text(vm.look.language, Key::AudioNoTarget),
            );
            if let Some(status) = &vm.status {
                for peer in status
                    .members
                    .iter()
                    .filter(|p| p.node_id != status.local_node_id)
                {
                    if let Some(bytes) = parse_hex(&peer.node_id, 32) {
                        let name = if peer.display_name.is_empty() {
                            &peer.node_id
                        } else {
                            &peer.display_name
                        };
                        ui.selectable_value(selected, bytes, name);
                    }
                }
            }
        });
}

fn device_selector(
    ui: &mut egui::Ui,
    pal: &Palette,
    vm: &ViewModel,
    kind: AudioDeviceKind,
    key: Key,
    selected: &mut String,
    actions: &mut Vec<Action>,
) {
    let lang = vm.look.language;
    setting(ui, pal, text(lang, key), |ui| {
        let page = vm
            .audio
            .devices
            .iter()
            .filter(|p| p.kind == kind as i32)
            .max_by_key(|p| p.revision);
        let current = page.and_then(|p| p.devices.iter().find(|d| d.id == *selected));
        let label = current
            .map(|d| d.name.as_str())
            .unwrap_or(selected.as_str());
        egui::ComboBox::from_id_salt(("audio-device", kind as i32))
            .selected_text(label)
            .width(ui.available_width().min(280.0))
            .show_ui(ui, |ui| {
                // The system default is a policy selector, not a synthetic discovered device.
                let (default_id, default_key) = if kind == AudioDeviceKind::Capture {
                    ("default_monitor", Key::AudioDefaultCapture)
                } else {
                    ("default", Key::AudioDefaultDevice)
                };
                ui.selectable_value(selected, default_id.to_owned(), text(lang, default_key));
                if let Some(page) = page {
                    for device in &page.devices {
                        ui.add_enabled_ui(device.available, |ui| {
                            ui.selectable_value(selected, device.id.clone(), &device.name);
                        });
                    }
                }
            });
        if current.is_some_and(|d| !d.available)
            || current.is_none() && selected != "default" && selected != "default_monitor"
        {
            widgets::hint(ui, pal, text(lang, Key::AudioDeviceMissing));
        }
        if page.is_some_and(|p| !p.complete) {
            widgets::hint(ui, pal, text(lang, Key::AudioDeviceListIncomplete));
        }
        if widgets::small_button(
            ui,
            pal,
            text(lang, Key::AudioRefreshDevices),
            !vm.audio.pending,
        )
        .clicked()
        {
            actions.push(Action::Audio(AudioAction::ListDevices(kind)));
        }
    });
}

fn permissions(
    ui: &mut egui::Ui,
    pal: &Palette,
    vm: &ViewModel,
    config: &mut ipc::AudioConfig,
    actions: &mut Vec<Action>,
) {
    let lang = vm.look.language;
    let Some(status) = &vm.status else {
        widgets::hint(ui, pal, text(lang, Key::AudioNoPeers));
        return;
    };
    let mut found = false;
    for member in status
        .members
        .iter()
        .filter(|p| p.node_id != status.local_node_id)
    {
        let Some(node_id) = parse_hex(&member.node_id, 32) else {
            continue;
        };
        found = true;
        let index = config.peers.iter().position(|p| p.node_id == node_id);
        let can_configure = index.is_some() || config.peers.len() < 3;
        let can_send =
            config.role == AudioRole::Source as i32 && config.listener_node_id == node_id;
        let can_receive = config.role == AudioRole::Listener as i32;
        let can_media = config.role == AudioRole::Source as i32;
        let mut added = ipc::AudioPeerConfig {
            volume_percent: 100,
            ..Default::default()
        };
        let peer = if let Some(i) = index {
            &mut config.peers[i]
        } else {
            &mut added
        };
        let mut changed = false;
        let mut remove = false;
        ui.push_id(&member.node_id, |ui| {
            let name = if member.display_name.is_empty() {
                &member.node_id
            } else {
                &member.display_name
            };
            ui.collapsing(name, |ui| {
                ui.add_enabled_ui(can_configure, |ui| {
                    changed |= ui
                        .add_enabled(
                            can_send || peer.allow_system_send,
                            egui::Checkbox::new(
                                &mut peer.allow_system_send,
                                text(lang, Key::AudioSendPermission),
                            ),
                        )
                        .changed();
                    changed |= ui
                        .add_enabled(
                            can_receive || peer.allow_system_receive,
                            egui::Checkbox::new(
                                &mut peer.allow_system_receive,
                                text(lang, Key::AudioReceivePermission),
                            ),
                        )
                        .changed();
                    changed |= ui
                        .checkbox(
                            &mut peer.allow_mic_control,
                            text(lang, Key::AudioMicControlPermission),
                        )
                        .changed();
                    changed |= ui
                        .add_enabled(
                            can_media || peer.allow_media_control,
                            egui::Checkbox::new(
                                &mut peer.allow_media_control,
                                text(lang, Key::AudioMediaPermission),
                            ),
                        )
                        .changed();
                    changed |= ui
                        .add_enabled(
                            peer.allow_mic_send,
                            egui::Checkbox::new(
                                &mut peer.allow_mic_send,
                                text(lang, Key::AudioMicSendPermission),
                            ),
                        )
                        .changed();
                    changed |= ui
                        .add_enabled(
                            peer.allow_mic_receive,
                            egui::Checkbox::new(
                                &mut peer.allow_mic_receive,
                                text(lang, Key::AudioMicReceivePermission),
                            ),
                        )
                        .changed();
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut peer.volume_percent, 0..=100)
                                .text(text(lang, Key::AudioPeerVolume)),
                        )
                        .changed();
                });
                if index.is_some() {
                    remove = widgets::small_button(ui, pal, text(lang, Key::AudioRemovePeer), true)
                        .clicked();
                }
                // Start is based only on applied authoritative configuration, never this draft.
                let can_start = member.online
                    && vm.audio.status.as_ref().is_some_and(|s| {
                        s.enabled
                            && s.config.as_ref().is_some_and(|saved| {
                                saved.peers.iter().any(|p| {
                                    p.node_id == node_id
                                        && (if s.role == AudioRole::Source as i32 {
                                            p.allow_system_send && saved.listener_node_id == node_id
                                        } else {
                                            p.allow_system_receive
                                        })
                                })
                            })
                    });
                if widgets::small_button(ui, pal, text(lang, Key::AudioStart), can_start).clicked()
                {
                    actions.push(Action::Audio(AudioAction::Start(node_id.clone())));
                }
            });
        });
        if let Some(i) = index {
            if remove {
                config.peers.remove(i);
            }
        } else if changed {
            added.node_id = node_id;
            config.peers.push(added);
        }
    }
    if !found {
        widgets::hint(ui, pal, text(lang, Key::AudioNoPeers));
    }
    config.peers.retain(|peer| {
        let trusted = status
            .members
            .iter()
            .any(|p| parse_hex(&p.node_id, 32).as_ref() == Some(&peer.node_id));
        if trusted {
            return true;
        }
        widgets::hint(ui, pal, text(lang, Key::AudioUntrustedPeer));
        widgets::hint(ui, pal, &hex(&peer.node_id));
        !ui.push_id(&peer.node_id, |ui| {
            widgets::small_button(ui, pal, text(lang, Key::AudioRemovePeer), true).clicked()
        })
        .inner
    });
}

fn diagnostics(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel, actions: &mut Vec<Action>) {
    let lang = vm.look.language;
    section(ui, pal, text(lang, Key::AudioDiagnostics));
    let Some(status) = &vm.audio.status else {
        return;
    };
    let connected = vm.connection.is_connected();
    let available = connected && vm.audio.available;
    ui.horizontal_wrapped(|ui| {
        if widgets::small_button(
            ui,
            pal,
            text(lang, Key::AudioStopAll),
            connected && !status.sessions.is_empty(),
        )
        .clicked()
        {
            actions.push(Action::Audio(AudioAction::StopAll));
        }
        if let Some(mic) = &status.microphone {
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
            let held_id = ui.id().with("audio-ptt-held");
            let was_held = ui
                .data_mut(|data| data.get_temp::<bool>(held_id))
                .unwrap_or(false);
            let ptt_enabled = status
                .config
                .as_ref()
                .and_then(|c| c.microphone.as_ref())
                .is_some_and(|m| m.control_enabled && m.ptt_enabled);
            let can_hold =
                available && mic.control_active && ptt_enabled && (was_held || !vm.audio.pending);
            let response = widgets::small_button(ui, pal, text(lang, Key::AudioPttPress), can_hold);
            // Response helpers read egui memory; never call them while holding Context::input.
            let pointer_held = response.is_pointer_button_down_on();
            let focused_button = response.has_focus();
            let held = can_hold
                && ui.ctx().input(|input| {
                    input.focused
                        && (pointer_held
                            || focused_button
                                && (input.key_down(egui::Key::Space)
                                    || input.key_down(egui::Key::Enter)))
                });
            if held != was_held {
                actions.push(Action::Audio(AudioAction::SetPtt(held)));
                ui.data_mut(|data| data.insert_temp(held_id, held));
            }
            if mic.ptt_pressed
                && widgets::small_button(ui, pal, text(lang, Key::AudioPttRelease), connected)
                    .clicked()
            {
                actions.push(Action::Audio(AudioAction::SetPtt(false)));
            }
        }
    });
    ui.collapsing(text(lang, Key::AudioDevices), |ui| {
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
                    Some(ipc::AudioUnavailableReason::PermissionDenied) => {
                        Key::AudioReasonPermission
                    }
                    Some(ipc::AudioUnavailableReason::DeviceGone) => Key::AudioReasonDevice,
                    Some(ipc::AudioUnavailableReason::DeviceNotSelected) => {
                        Key::AudioReasonSelection
                    }
                    Some(ipc::AudioUnavailableReason::WorkerUnavailable) => Key::AudioReasonWorker,
                    _ => Key::AudioUnknown,
                };
                widgets::hint(ui, pal, text(lang, reason));
            }
        }
    });
    if status.sessions.is_empty() {
        widgets::hint(ui, pal, text(lang, Key::AudioIdle));
    }
    for session in &status.sessions {
        let state = match ipc::AudioSessionState::try_from(session.state).ok() {
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
        };
        let active = matches!(
            ipc::AudioSessionState::try_from(session.state),
            Ok(ipc::AudioSessionState::Running | ipc::AudioSessionState::Rebuffering)
        );
        let direction = if session.local_role == AudioRole::Source as i32 {
            match session.sending {
                Some(true) => Key::AudioSharing,
                Some(false) => Key::AudioNotSending,
                None => Key::AudioSendingUnknown,
            }
        } else if active {
            Key::AudioReceiving
        } else {
            Key::AudioListener
        };
        if session.local_role == AudioRole::Source as i32 {
            match session.capture_active {
                Some(active) => widgets::hint(
                    ui,
                    pal,
                    text(
                        lang,
                        if active {
                            Key::AudioSystemCaptureActive
                        } else {
                            Key::AudioSystemCaptureInactive
                        },
                    ),
                ),
                None => widgets::hint(
                    ui,
                    pal,
                    &format!(
                        "{}: {}",
                        text(lang, Key::AudioCapabilityCapture),
                        text(lang, Key::AudioUnknown)
                    ),
                ),
            };
        }
        let name = if session.source_name.is_empty() {
            hex(&session.peer)
        } else {
            session.source_name.clone()
        };
        widgets::hint(
            ui,
            pal,
            &format!("{} {} · {}", text(lang, direction), name, text(lang, state)),
        );
        if let Some(latency) = session.latency_estimate_ms {
            widgets::hint(
                ui,
                pal,
                &format!("{}: {latency}", text(lang, Key::AudioLatency)),
            );
        }
        if let Some(health) = &session.health {
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
    if let Some(mic) = &status.microphone {
        widgets::hint(
            ui,
            pal,
            text(
                lang,
                if mic.muted {
                    Key::AudioMicMuted
                } else {
                    Key::AudioMicNotMuted
                },
            ),
        );
        if mic.control_active {
            widgets::hint(ui, pal, text(lang, Key::AudioMicControlActive));
        }
        if mic.capture_active {
            widgets::hint(ui, pal, text(lang, Key::AudioMicCaptureActive));
        }
        if mic.ptt_pressed {
            widgets::hint(ui, pal, text(lang, Key::AudioPttPressed));
        }
        if !mic.target_node_id.is_empty() {
            widgets::hint(
                ui,
                pal,
                &format!(
                    "{}: {}",
                    text(lang, Key::AudioPttTarget),
                    hex(&mic.target_node_id)
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{hex, parse_hex};

    #[test]
    fn identifiers_are_explicit_exact_length_hex_not_discovery_guesses() {
        assert_eq!(parse_hex("", 16), Some(Vec::new()));
        assert_eq!(parse_hex("", 32), None);
        assert_eq!(
            parse_hex("00112233445566778899AABBCCDDEEFF", 16),
            Some((0..16).map(|n| n * 17).collect())
        );
        assert_eq!(parse_hex("001122", 16), None);
        assert_eq!(parse_hex("zz112233445566778899aabbccddeeff", 16), None);
        assert_eq!(parse_hex("я".repeat(16).as_str(), 16), None);
        assert_eq!(hex(&[0, 15, 128, 255]), "000f80ff");
    }
}
