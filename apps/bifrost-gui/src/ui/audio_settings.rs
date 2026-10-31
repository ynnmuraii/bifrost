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
    egui::Frame::NONE.inner_margin(24.0).show(ui, |ui| {
        contents(ui, pal, vm, actions);
    });
}

fn contents(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel, actions: &mut Vec<Action>) {
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
    let editable = audio.available && audio.status.is_some() && !audio.pending;
    let mut config = audio.draft.clone();
    let mut valid_group = true;
    let mut apply = false;
    let mut use_listener = false;
    ui.spacing_mut().item_spacing.y = 10.0;
    super::audio_summary::draw(ui, pal, vm, actions);
    if let Some(error) = &audio.error {
        widgets::wrapped(ui, error, 13.0, super::theme::fam::sans(), pal.accent_ink);
    }
    if audio.pending {
        widgets::hint(ui, pal, text(lang, Key::AudioPending));
    }
    ui.add_enabled_ui(editable, |ui| {
        section(ui, pal, text(lang, Key::AudioRoleSection));
        setting(ui, pal, text(lang, Key::AudioRole), |ui| {
            let previous = config.role;
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
            if previous != config.role {
                config.enabled = false;
                config.listener_node_id.clear();
                config.group_id.clear();
            }
            widgets::hint(ui, pal, text(lang, Key::AudioRoleWarning));
        });
        if config.role == AudioRole::Source as i32 {
            setting(ui, pal, text(lang, Key::AudioListenerTarget), |ui| {
                let previous = config.listener_node_id.clone();
                peer_selector(ui, vm, "audio-listener", &mut config.listener_node_id);
                if previous != config.listener_node_id {
                    inherit_listener_group(
                        &mut config,
                        audio
                            .status
                            .as_ref()
                            .and_then(|status| status.config.as_ref()),
                    );
                }
            });
        }
        if config.role == AudioRole::Listener as i32
            && audio
                .status
                .as_ref()
                .and_then(|status| status.config.as_ref())
                .is_some_and(|saved| saved.role == AudioRole::Listener as i32)
        {
            widgets::hint(ui, pal, text(lang, Key::AudioThisComputerListener));
        } else {
            use_listener =
                primary_action(ui, pal, text(lang, Key::AudioUseThisListener), true).clicked();
        }
        let group_ready = config.group_id.len() == 16
            && (config.role == AudioRole::Listener as i32 || config.listener_node_id.len() == 32);
        widgets::hint(
            ui,
            pal,
            text(
                lang,
                if group_ready {
                    Key::AudioGroupAutomatic
                } else {
                    Key::AudioGroupPending
                },
            ),
        );
        ui.add_enabled(
            group_ready || config.enabled,
            egui::Checkbox::new(&mut config.enabled, text(lang, Key::AudioEnabled)),
        );

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

        section(ui, pal, text(lang, Key::AudioMode));
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
            widgets::hint(
                ui,
                pal,
                text(
                    lang,
                    if config.mode == AudioMode::FollowFocus as i32 {
                        Key::AudioFollowFocusHint
                    } else {
                        Key::AudioMixAllHint
                    },
                ),
            );
        });
        section(ui, pal, text(lang, Key::AudioProfile));
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
            widgets::hint(
                ui,
                pal,
                text(
                    lang,
                    if config.profile == AudioProfile::Wifi as i32 {
                        Key::AudioWifiHint
                    } else {
                        Key::AudioEthernetHint
                    },
                ),
            );
        });

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
        ui.checkbox(&mut mic.muted, text(lang, Key::AudioMicSavedMute));
        ui.checkbox(&mut mic.ptt_enabled, text(lang, Key::AudioPttEnabled));
        if mic.ptt_enabled {
            setting(ui, pal, text(lang, Key::AudioPttHotkey), |ui| {
                text_field(
                    ui,
                    text(lang, Key::AudioPttHotkey),
                    &mut mic.ptt_hotkey,
                    128,
                );
            });
        }
        if mic.control_enabled {
            device_selector(
                ui,
                pal,
                vm,
                AudioDeviceKind::Microphone,
                Key::AudioMicDevice,
                &mut mic.capture_device,
                actions,
            );
            setting(ui, pal, text(lang, Key::AudioPttTarget), |ui| {
                peer_selector(ui, vm, "audio-mic-target", &mut mic.pinned_target_node_id);
            });
        }
        ui.add_space(12.0);
        ui.collapsing(text(lang, Key::AudioAdvanced), |ui| {
            valid_group = advanced(ui, pal, vm, &mut config);
        });
        widgets::hint(ui, pal, text(lang, Key::AudioDraftHint));
        let changed = audio
            .status
            .as_ref()
            .and_then(|status| status.config.as_ref())
            .is_some_and(|saved| saved != &config);
        apply =
            primary_action(ui, pal, text(lang, Key::AudioApply), valid_group && changed).clicked();
    });
    if editable && config != audio.draft {
        actions.push(Action::Audio(AudioAction::EditConfig(Box::new(config))));
    }
    if use_listener {
        actions.push(Action::Audio(AudioAction::UseThisListener));
    }
    if apply {
        actions.push(Action::Audio(AudioAction::ApplyConfig));
    }
}

fn inherit_listener_group(config: &mut ipc::AudioConfig, saved: Option<&ipc::AudioConfig>) {
    config.group_id = saved
        .filter(|saved| {
            saved.role == AudioRole::Source as i32
                && saved.listener_node_id == config.listener_node_id
        })
        .map_or_else(Vec::new, |saved| saved.group_id.clone());
    if config.group_id.len() != 16 {
        config.enabled = false;
    }
}

fn primary_action(ui: &mut egui::Ui, pal: &Palette, label: &str, enabled: bool) -> egui::Response {
    // Match the shared pill while allowing its full accessibility label to wrap in small modals.
    ui.scope(|ui| {
        ui.spacing_mut().button_padding = egui::vec2(18.0, 8.0);
        ui.add_enabled(
            enabled,
            egui::Button::new(
                egui::RichText::new(label)
                    .font(super::theme::font(13.0, super::theme::fam::sans_medium()))
                    .color(pal.bg),
            )
            .fill(pal.ink)
            .stroke(egui::Stroke::NONE)
            .corner_radius(super::theme::R_BTN)
            .min_size(egui::vec2(0.0, 44.0))
            .wrap(),
        )
    })
    .inner
}

fn slider(
    ui: &mut egui::Ui,
    pal: &Palette,
    enabled: bool,
    slider: egui::Slider<'_>,
) -> egui::Response {
    ui.scope(|ui| {
        // The same visible rail and circular handle as Appearance's particle-speed control.
        ui.spacing_mut().slider_width = (ui.available_width() - 80.0).clamp(80.0, 200.0);
        let visuals = ui.visuals_mut();
        visuals.widgets.inactive.bg_fill = pal.line;
        visuals.widgets.hovered.bg_fill = pal.line;
        visuals.widgets.active.bg_fill = pal.line;
        visuals.selection.bg_fill = pal.accent;
        visuals.slider_trailing_fill = true;
        visuals.handle_shape = egui::style::HandleShape::Circle;
        ui.add_enabled(enabled, slider)
    })
    .inner
}

fn text_field(ui: &mut egui::Ui, label: &str, value: &mut String, limit: usize) {
    let response = ui.add(
        egui::TextEdit::singleline(value)
            .desired_width((ui.available_width() - 8.0).max(24.0))
            .char_limit(limit),
    );
    response.widget_info(|| {
        let mut info =
            egui::WidgetInfo::text_edit(ui.is_enabled(), value.as_str(), value.as_str(), "");
        info.label = Some(label.to_owned());
        info
    });
}

fn advanced(
    ui: &mut egui::Ui,
    pal: &Palette,
    vm: &ViewModel,
    config: &mut ipc::AudioConfig,
) -> bool {
    let lang = vm.look.language;
    let mut valid_group = true;
    setting(ui, pal, text(lang, Key::AudioGroup), |ui| {
        let id = ui.id().with("audio-group-input");
        let mut group = ui
            .data_mut(|data| data.get_temp::<String>(id))
            .unwrap_or_else(|| hex(&config.group_id));
        let previous = group.clone();
        text_field(ui, text(lang, Key::AudioGroup), &mut group, 32);
        let parsed = parse_hex(&group, 16);
        valid_group = parsed.is_some() && (!config.enabled || !group.is_empty());
        if group != previous {
            if let Some(bytes) = parsed {
                config.group_id = bytes;
            }
            ui.data_mut(|data| data.insert_temp(id, group));
        }
        if !valid_group {
            widgets::hint(ui, pal, text(lang, Key::AudioInvalidGroup));
        }
    });
    setting(ui, pal, text(lang, Key::AudioUdpPort), |ui| {
        ui.add(egui::DragValue::new(&mut config.udp_port).range(1..=65535))
            .widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::DragValue,
                    ui.is_enabled(),
                    text(lang, Key::AudioUdpPort),
                )
            });
    });
    setting(ui, pal, text(lang, Key::AudioDebounce), |ui| {
        ui.add(egui::DragValue::new(&mut config.focus_debounce_ms).range(100..=2000))
            .widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::DragValue,
                    ui.is_enabled(),
                    text(lang, Key::AudioDebounce),
                )
            });
    });
    setting(ui, pal, text(lang, Key::AudioRamp), |ui| {
        ui.add(egui::DragValue::new(&mut config.focus_ramp_ms).range(20..=1000))
            .widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::DragValue,
                    ui.is_enabled(),
                    text(lang, Key::AudioRamp),
                )
            });
    });
    setting(ui, pal, text(lang, Key::AudioBackgroundGain), |ui| {
        let response = slider(
            ui,
            pal,
            true,
            egui::Slider::new(&mut config.background_gain_db, -60..=0),
        );
        response.widget_info(|| {
            egui::WidgetInfo::slider(
                ui.is_enabled(),
                f64::from(config.background_gain_db),
                text(lang, Key::AudioBackgroundGain),
            )
        });
    });
    ui.checkbox(
        &mut config.background_mute,
        text(lang, Key::AudioBackgroundMute),
    );
    ui.checkbox(
        &mut config.output_fallback_to_default,
        text(lang, Key::AudioOutputFallback),
    );
    ui.checkbox(
        &mut config.resume_system_audio,
        text(lang, Key::AudioResume),
    );
    if let Some(mic) = &mut config.microphone {
        ui.checkbox(
            &mut mic.allow_default_mic_changes,
            text(lang, Key::AudioAllowDefaultMic),
        );
        widgets::hint(ui, pal, text(lang, Key::AudioMicStreamingDeferred));
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
            ui.add_enabled_ui(false, |ui| {
                text_field(
                    ui,
                    text(lang, Key::AudioVirtualDevice),
                    &mut mic.virtual_device,
                    256,
                );
            });
        });
    }
    valid_group
}

fn section(ui: &mut egui::Ui, pal: &Palette, label: &str) {
    ui.add_space(16.0);
    widgets::text(ui, label, 16.0, super::theme::fam::sans_semibold(), pal.ink);
    ui.add_space(4.0);
}

fn setting(ui: &mut egui::Ui, pal: &Palette, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.push_id(label, |ui| {
        if ui.available_width() < 520.0 {
            widgets::wrapped(
                ui,
                &label.to_uppercase(),
                11.0,
                super::theme::fam::mono(),
                pal.ink2,
            );
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
    let label = if selected.is_empty() {
        text(vm.look.language, Key::AudioNoTarget).to_owned()
    } else {
        super::audio_summary::peer_name(vm, selected)
    };
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
                    .filter(|peer| peer.node_id != status.local_node_id)
                {
                    if let Some(bytes) = parse_hex(&peer.node_id, 32) {
                        let name = super::audio_summary::peer_name(vm, &bytes);
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
        let label = if selected == "default" {
            text(lang, Key::AudioDefaultDevice)
        } else if selected == "default_monitor" {
            text(lang, Key::AudioDefaultCapture)
        } else {
            current
                .map(|device| device.name.as_str())
                .filter(|name| !name.is_empty())
                .unwrap_or(text(lang, Key::AudioDeviceMissing))
        };
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
        .filter(|member| member.node_id != status.local_node_id)
    {
        let Some(node_id) = parse_hex(&member.node_id, 32) else {
            continue;
        };
        found = true;
        let index = config.peers.iter().position(|peer| peer.node_id == node_id);
        let can_configure = index.is_some() || config.peers.len() < 3;
        let can_send =
            config.role == AudioRole::Source as i32 && config.listener_node_id == node_id;
        let can_receive = config.role == AudioRole::Listener as i32;
        let can_media = config.role == AudioRole::Source as i32;
        let mut added = ipc::AudioPeerConfig {
            volume_percent: 100,
            ..Default::default()
        };
        let peer = if let Some(index) = index {
            &mut config.peers[index]
        } else {
            &mut added
        };
        let mut changed = false;
        let mut remove = false;
        ui.push_id(&member.node_id, |ui| {
            ui.add_space(8.0);
            widgets::text(
                ui,
                &super::audio_summary::peer_name(vm, &node_id),
                14.0,
                super::theme::fam::sans_semibold(),
                pal.ink,
            );
            ui.add_enabled_ui(can_configure, |ui| {
                ui.horizontal_wrapped(|ui| {
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
                        .add_enabled(
                            can_media || peer.allow_media_control,
                            egui::Checkbox::new(
                                &mut peer.allow_media_control,
                                text(lang, Key::AudioMediaPermission),
                            ),
                        )
                        .changed();
                });
                let volume_label = widgets::hint(ui, pal, text(lang, Key::AudioPeerVolume));
                changed |= slider(
                    ui,
                    pal,
                    can_receive,
                    egui::Slider::new(&mut peer.volume_percent, 0..=100).suffix("%"),
                )
                .labelled_by(volume_label.id)
                .changed();
                ui.collapsing(text(lang, Key::AudioMicrophone), |ui| {
                    changed |= ui
                        .checkbox(
                            &mut peer.allow_mic_control,
                            text(lang, Key::AudioMicControlPermission),
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
                    if index.is_some() {
                        remove =
                            widgets::small_button(ui, pal, text(lang, Key::AudioRemovePeer), true)
                                .clicked();
                    }
                });
            });
            // Starting is permitted only by applied consent, never by a draft checkbox.
            let can_start = member.online
                && vm.audio.status.as_ref().is_some_and(|status| {
                    status.enabled
                        && status.config.as_ref().is_some_and(|saved| {
                            saved.peers.iter().any(|peer| {
                                peer.node_id == node_id
                                    && if status.role == AudioRole::Source as i32 {
                                        peer.allow_system_send && saved.listener_node_id == node_id
                                    } else {
                                        peer.allow_system_receive
                                    }
                            })
                        })
                });
            if primary_action(ui, pal, text(lang, Key::AudioStart), can_start).clicked() {
                actions.push(Action::Audio(AudioAction::Start(node_id.clone())));
            }
            ui.separator();
        });
        if let Some(index) = index {
            if remove {
                config.peers.remove(index);
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
        if status
            .members
            .iter()
            .any(|member| parse_hex(&member.node_id, 32).as_ref() == Some(&peer.node_id))
        {
            return true;
        }
        widgets::hint(ui, pal, text(lang, Key::AudioUntrustedPeer));
        widgets::hint(ui, pal, &super::audio_summary::peer_name(vm, &peer.node_id));
        !ui.push_id(&peer.node_id, |ui| {
            widgets::small_button(ui, pal, text(lang, Key::AudioRemovePeer), true).clicked()
        })
        .inner
    });
}

#[cfg(test)]
mod tests {
    use super::{hex, inherit_listener_group, parse_hex};

    #[test]
    fn source_listener_selection_never_reuses_another_listeners_group() {
        let mut saved = crate::audio::default_config();
        saved.listener_node_id = vec![1; 32];
        saved.group_id = vec![7; 16];
        let mut draft = saved.clone();
        draft.listener_node_id = vec![2; 32];
        draft.enabled = true;
        inherit_listener_group(&mut draft, Some(&saved));
        assert!(draft.group_id.is_empty());
        assert!(!draft.enabled);
        draft.listener_node_id = saved.listener_node_id.clone();
        inherit_listener_group(&mut draft, Some(&saved));
        assert_eq!(draft.group_id, saved.group_id);
        assert!(!draft.enabled);
    }

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
