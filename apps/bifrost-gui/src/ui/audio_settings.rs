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
    let mut apply = false;
    ui.spacing_mut().item_spacing.y = 12.0;
    let connected = vm.connection.is_connected();
    let share = audio.sharing_requested(AudioRole::Source);
    let receive = audio.sharing_requested(AudioRole::Listener);
    let locked = share || receive || audio.pending || audio.system_intent.is_some();
    let mut mode = if receive {
        AudioRole::Listener
    } else if share || config.role == AudioRole::Source as i32 {
        AudioRole::Source
    } else {
        AudioRole::Listener
    };

    // One question: receive or share. The choice is local UI intent until the single control
    // below sends the existing sharing command; while that command owns the route it stays
    // locked, so the mode never claims a role the service has not accepted.
    ui.add_enabled_ui(editable && !locked, |ui| {
        let labels = [
            text(lang, Key::AudioReceiveMode),
            text(lang, Key::AudioShareMode),
        ];
        let selected = usize::from(mode == AudioRole::Source);
        let mut pick = None;
        if ui.available_width() >= 330.0 {
            pick = widgets::seg(ui, pal, &labels, selected);
        } else {
            // A narrow dialog keeps both choices inside the surface: they wrap as plain pills
            // instead of painting a segmented control wider than the dialog.
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                for (index, label) in labels.iter().enumerate() {
                    let kind = if index == selected {
                        widgets::Kind::Solid
                    } else {
                        widgets::Kind::Plain
                    };
                    if widgets::button(ui, pal, label, kind, true).clicked() {
                        pick = Some(index);
                    }
                }
            });
        }
        if let Some(pick) = pick {
            mode = if pick == 1 {
                AudioRole::Source
            } else {
                AudioRole::Listener
            };
            config.role = mode as i32;
        }
    });
    if locked {
        widgets::hint(ui, pal, text(lang, Key::AudioModeLocked));
    }

    if mode == AudioRole::Source {
        let previous_target = config.listener_node_id.as_slice().try_into().ok();
        ui.add_enabled_ui(editable, |ui| {
            setting(ui, pal, text(lang, Key::AudioListenerTarget), |ui| {
                peer_selector(ui, vm, "audio-listener", &mut config.listener_node_id);
            });
        });
        if config.listener_node_id.len() != 32 {
            widgets::hint(ui, pal, text(lang, Key::AudioSelectReceiver));
        }
        if editable && share {
            if let Ok(target) = config.listener_node_id.as_slice().try_into() {
                if previous_target != Some(target) {
                    actions.push(Action::Audio(AudioAction::Retarget(target)));
                }
            } else {
                config
                    .listener_node_id
                    .clone_from(&audio.draft.listener_node_id);
            }
        }
    }

    let on = if mode == AudioRole::Source {
        share
    } else {
        receive
    };
    let toggle = if mode == AudioRole::Source {
        if on {
            Key::AudioStopShare
        } else {
            Key::AudioStartShare
        }
    } else if on {
        Key::AudioStopReceive
    } else {
        Key::AudioStartReceive
    };
    let can_toggle = if on {
        connected
    } else {
        editable && connected && (mode != AudioRole::Source || config.listener_node_id.len() == 32)
    };
    // One large control, then the same state in words. The button is the only hit target here;
    // its accessible name is the action it sends, so Space/Enter and the screen reader agree.
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        if power_button(ui, pal, on, can_toggle, text(lang, toggle)).clicked() {
            actions.push(Action::Audio(if mode == AudioRole::Source {
                AudioAction::Share(!on)
            } else {
                AudioAction::Receive(!on)
            }));
        }
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                power_marker(ui, pal, on);
                widgets::wrapped(
                    ui,
                    text(lang, power_heading(mode, on)),
                    17.0,
                    super::theme::fam::sans_semibold(),
                    pal.ink,
                );
            });
            widgets::wrapped(
                ui,
                text(lang, Key::AudioPowerHint),
                12.0,
                super::theme::fam::sans(),
                pal.ink2,
            );
            // The heading states the requested policy; this line is the authoritative detail.
            if let Some(state) = power_detail(vm, lang, mode, on) {
                widgets::wrapped(ui, &state, 13.0, super::theme::fam::sans(), pal.ink);
            }
            if let Some(error) = audio.error_message(lang) {
                widgets::wrapped(ui, error, 12.0, super::theme::fam::sans(), pal.accent_ink);
            }
        });
    });

    // Everything technical stays one level down: path reason, ports and counters are
    // diagnostic material, not the settings' first sentence.
    ui.collapsing(text(lang, Key::AudioStateDetails), |ui| {
        ui.set_width(ui.available_width());
        super::audio_summary::draw(ui, pal, vm, actions);
    });

    ui.add_space(12.0);
    ui.collapsing(text(lang, Key::AudioAdvanced), |ui| {
        ui.add_enabled_ui(editable, |ui| {
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
            permissions(ui, pal, vm, &mut config);

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
            let valid_group = advanced(ui, pal, vm, &mut config);
            widgets::hint(ui, pal, text(lang, Key::AudioDraftHint));
            let changed = audio
                .status
                .as_ref()
                .and_then(|status| status.config.as_ref())
                .is_some_and(|saved| saved != &config);
            apply = primary_action(ui, pal, text(lang, Key::AudioApply), valid_group && changed)
                .clicked();
        });
    });
    if editable && config != audio.draft {
        actions.insert(0, Action::Audio(AudioAction::EditConfig(Box::new(config))));
    }
    if apply {
        actions.push(Action::Audio(AudioAction::ApplyConfig));
    }
}

fn audio_state(vm: &ViewModel, lang: Lang, mode: AudioRole, on: bool) -> Option<String> {
    let audio = &vm.audio;
    if audio.retargeting {
        return Some(text(lang, Key::AudioSwitchingReceiver).to_owned());
    }
    if let Some((role, enabled)) = audio.system_intent {
        if !enabled {
            return Some(text(lang, Key::AudioPending).to_owned());
        }
        return Some(
            text(
                lang,
                if role == AudioRole::Source {
                    Key::AudioSharingRequested
                } else {
                    Key::AudioReceivingReady
                },
            )
            .to_owned(),
        );
    }
    if audio.pending {
        return Some(text(lang, Key::AudioPending).to_owned());
    }
    if !on {
        return Some(text(lang, Key::AudioIdle).to_owned());
    }
    let status = audio.status.as_ref()?;
    let running = status.sessions.iter().find(|session| {
        session.local_role == mode as i32 && session.state == ipc::AudioSessionState::Running as i32
    });
    match running {
        Some(session) => Some(format!(
            "{} · {}",
            text(
                lang,
                if mode == AudioRole::Source {
                    Key::AudioShareOn
                } else {
                    Key::AudioReceiveOn
                },
            ),
            super::audio_summary::peer_name(vm, &session.peer)
        )),
        None => Some(
            text(
                lang,
                if mode == AudioRole::Source {
                    Key::AudioSharingRequested
                } else {
                    Key::AudioReceivingReady
                },
            )
            .to_owned(),
        ),
    }
}

/// The visible diameter of the settings power control and its one hit target.
const POWER_BUTTON: f32 = 64.0;

fn power_heading(mode: AudioRole, on: bool) -> Key {
    match (mode, on) {
        (AudioRole::Source, true) => Key::AudioPowerShareOn,
        (AudioRole::Source, false) => Key::AudioPowerShareOff,
        // The caller already resolves every non-Source role to Listener; Unspecified follows
        // that same selection instead of inventing a third state.
        (_, true) => Key::AudioPowerReceiveOn,
        (_, false) => Key::AudioPowerReceiveOff,
    }
}

fn power_detail(vm: &ViewModel, lang: Lang, mode: AudioRole, on: bool) -> Option<String> {
    let audio = &vm.audio;
    // Plain OFF is already the heading; the old "no sessions" sentence would only repeat it.
    let quiet_off = !on && audio.system_intent.is_none() && !audio.pending && !audio.retargeting;
    if quiet_off {
        return None;
    }
    audio_state(vm, lang, mode, on)
}

fn power_marker(ui: &mut egui::Ui, pal: &Palette, on: bool) {
    // A static mark, so the state is not carried by the accent colour alone.
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(8.0), egui::Sense::hover());
    let centre = rect.center();
    let painter = ui.painter();
    if on {
        painter.circle_filled(centre, 3.0, pal.accent);
    } else {
        painter.circle_stroke(centre, 2.75, egui::Stroke::new(1.5, pal.ink2));
    }
}

fn power_arc(
    centre: egui::Pos2,
    radius: f32,
    from: f32,
    to: f32,
    stroke: egui::Stroke,
) -> egui::epaint::CubicBezierShape {
    let point = |angle: f32| centre + egui::vec2(angle.cos(), angle.sin()) * radius;
    let tangent = |angle: f32| egui::vec2(-angle.sin(), angle.cos()) * radius;
    let start = point(from);
    let end = point(to);
    let bend = 4.0 / 3.0 * ((to - from) / 4.0).tan();
    egui::epaint::CubicBezierShape::from_points_stroke(
        [
            start,
            start + tangent(from) * bend,
            end - tangent(to) * bend,
            end,
        ],
        false,
        egui::Color32::TRANSPARENT,
        stroke,
    )
}

fn paint_power_glyph(painter: &egui::Painter, centre: egui::Pos2, ink: egui::Color32) {
    let stroke = egui::Stroke::new(1.8, ink);
    let radius = 11.0;
    // The two arcs meet at the bottom and leave a gap at the top for the stem. The stem and
    // arc bounds are -12 and +12 from the shifted centre, so the glyph is optically centred.
    let centre = centre + egui::vec2(0.0, 1.0);
    let gap = 32.0_f32.to_radians();
    let top = -std::f32::consts::FRAC_PI_2;
    let bottom = std::f32::consts::FRAC_PI_2;
    let left = 3.0 * std::f32::consts::FRAC_PI_2;
    painter.add(power_arc(centre, radius, top + gap, bottom, stroke));
    painter.add(power_arc(centre, radius, bottom, left - gap, stroke));
    painter.line_segment(
        [
            centre + egui::vec2(0.0, -radius - 2.0),
            centre + egui::vec2(0.0, -radius * 0.25),
        ],
        stroke,
    );
}

fn power_button(
    ui: &mut egui::Ui,
    pal: &Palette,
    on: bool,
    enabled: bool,
    label: &str,
) -> egui::Response {
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::Vec2::splat(POWER_BUTTON), sense);
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Button,
            enabled && ui.is_enabled(),
            on,
            label,
        )
    });
    if enabled {
        response
            .clone()
            .on_hover_cursor(egui::CursorIcon::PointingHand);
    }
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let hover = enabled && response.hovered();
    let down = enabled && response.is_pointer_button_down_on();
    let (mut fill, mut border, mut ink) = if on {
        (pal.accent, pal.accent, pal.on_accent)
    } else {
        (pal.bg2, pal.line, pal.ink)
    };
    if hover {
        if on {
            fill = widgets::brighten(fill, 1.08);
            border = fill;
        } else {
            fill = pal.ink;
            border = pal.ink;
            ink = pal.bg;
        }
    }
    if down {
        fill = widgets::brighten(fill, 0.94);
    }
    if !enabled {
        (fill, border, ink) = (widgets::dim(fill), widgets::dim(border), widgets::dim(ink));
    }
    let centre = rect.center();
    let radius = POWER_BUTTON * 0.5;
    let painter = ui.painter();
    painter.circle_filled(centre, radius, fill);
    painter.circle_stroke(centre, radius - 0.5, egui::Stroke::new(1.0, border));
    if response.has_focus() {
        painter.circle_stroke(centre, radius + 3.0, egui::Stroke::new(2.0, pal.accent));
    }
    paint_power_glyph(painter, centre, ink);
    response
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

fn edit_group(config: &mut ipc::AudioConfig, bytes: Vec<u8>) {
    config.group_id_override = Some(config.role == AudioRole::Source as i32 && !bytes.is_empty());
    config.group_id = bytes;
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
        valid_group =
            parsed.is_some() && (config.group_id_override != Some(true) || !group.is_empty());
        if group != previous {
            if let Some(bytes) = parsed {
                edit_group(config, bytes);
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

fn policy_override(
    ui: &mut egui::Ui,
    pal: &Palette,
    lang: Lang,
    key: Key,
    value: &mut Option<bool>,
) -> bool {
    let old = *value;
    let mut choice = value.map_or(-1, i32::from);
    setting(ui, pal, text(lang, key), |ui| {
        enum_choice(
            ui,
            text(lang, key),
            &mut choice,
            &[
                (-1, text(lang, Key::AudioPermissionDefault)),
                (0, text(lang, Key::AudioPermissionDeny)),
                (1, text(lang, Key::AudioPermissionAllow)),
            ],
            lang,
        );
    });
    *value = match choice {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    };
    old != *value
}

fn permissions(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel, config: &mut ipc::AudioConfig) {
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
                ui.add_enabled_ui(can_send || peer.system_send_override.is_some(), |ui| {
                    changed |= policy_override(
                        ui,
                        pal,
                        lang,
                        Key::AudioSendPermission,
                        &mut peer.system_send_override,
                    );
                });
                ui.add_enabled_ui(
                    can_receive || peer.system_receive_override.is_some(),
                    |ui| {
                        changed |= policy_override(
                            ui,
                            pal,
                            lang,
                            Key::AudioReceivePermission,
                            &mut peer.system_receive_override,
                        );
                    },
                );
                ui.horizontal_wrapped(|ui| {
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
    use super::{edit_group, hex, parse_hex};

    #[test]
    fn advanced_group_edits_pin_only_valid_source_groups_and_clear_explicitly() {
        let mut config = crate::audio::default_config();
        config.group_id = vec![7; 16];
        assert_eq!(config.group_id_override, Some(false));
        edit_group(&mut config, vec![8; 16]);
        assert_eq!(config.group_id, vec![8; 16]);
        assert_eq!(config.group_id_override, Some(true));
        edit_group(&mut config, Vec::new());
        assert!(config.group_id.is_empty());
        assert_eq!(config.group_id_override, Some(false));
        config.role = protocol::ipc::AudioRole::Listener as i32;
        edit_group(&mut config, vec![9; 16]);
        assert_eq!(config.group_id_override, Some(false));
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
