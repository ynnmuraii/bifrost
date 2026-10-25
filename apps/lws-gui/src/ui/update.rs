//! Updater controls share the settings rows' type, palette and buttons.
use super::theme::{fam, font, Palette};
use super::widgets::{self, Kind};
use crate::updater::{Action as Update, State, RELEASES};
use crate::view_model::{Action, ViewModel};
use eframe::egui;
use ui_text::{fill, text, Key};

fn button(
    ui: &mut egui::Ui,
    pal: &Palette,
    vm: &ViewModel,
    key: Key,
    action: Update,
    actions: &mut Vec<Action>,
) {
    let kind = if matches!(
        action,
        Update::Confirm | Update::Install | Update::ConfirmAll | Update::InstallAll
    ) {
        Kind::Hot
    } else {
        Kind::Plain
    };
    let enabled = if matches!(
        action,
        Update::ConfirmAll | Update::InstallAll | Update::Confirm | Update::Install
    ) {
        !mesh_busy(vm)
            && (!matches!(action, Update::ConfirmAll | Update::InstallAll)
                || vm.connection.is_connected())
    } else {
        true
    };
    if widgets::button(ui, pal, text(vm.look.language, key), kind, enabled).clicked() {
        actions.push(Action::Update(action));
    }
}
pub(super) fn settings(
    ui: &mut egui::Ui,
    pal: &Palette,
    vm: &ViewModel,
    actions: &mut Vec<Action>,
) {
    let lang = vm.look.language;
    let busy = matches!(
        vm.update,
        State::Checking | State::Downloading { .. } | State::Restarting
    ) || mesh_busy(vm);
    widgets::duo(ui, |ui| {
        widgets::text(
            ui,
            &format!("Bifrost {}", protocol::app_version()),
            13.0,
            fam::sans(),
            pal.ink2,
        );
        if widgets::button(ui, pal, text(lang, Key::CheckUpdates), Kind::Plain, !busy).clicked() {
            actions.push(Action::Update(Update::Check));
        }
    });
    match &vm.update {
        State::Idle => {}
        State::Checking => {
            ui.horizontal(|ui| {
                ui.spinner();
                widgets::hint(ui, pal, text(lang, Key::CheckingUpdates));
            });
        }
        State::Current => {
            widgets::hint(ui, pal, text(lang, Key::UpToDate));
        }
        State::Available(release) => {
            widgets::hint(
                ui,
                pal,
                &fill(
                    lang,
                    Key::UpdateAvailable,
                    &[&release.tag_name.trim_start_matches('v')],
                ),
            );
            if let Some(notes) = &release.body {
                ui.add(egui::Label::new(notes_job(
                    pal,
                    notes,
                    ui.available_width(),
                )));
            }
            if vm.update_confirm && !vm.update_all_confirm {
                widgets::hint(ui, pal, text(lang, Key::UpdateConfirm));
                widgets::duo(ui, |ui| {
                    button(ui, pal, vm, Key::InstallUpdate, Update::Install, actions);
                    button(ui, pal, vm, Key::Cancel, Update::Cancel, actions);
                });
            } else if !vm.update_all_confirm {
                widgets::duo(ui, |ui| {
                    ui.hyperlink_to(text(lang, Key::UpdateDetails), RELEASES);
                    button(ui, pal, vm, Key::InstallUpdate, Update::Confirm, actions);
                });
            }
        }
        State::Downloading { received, total } => {
            widgets::hint(ui, pal, text(lang, Key::DownloadingUpdate));
            if let Some(total) = total.filter(|v| *v > 0) {
                ui.add(egui::ProgressBar::new(*received as f32 / total as f32).show_percentage());
            } else {
                ui.spinner();
                widgets::hint(ui, pal, &ui_text::clipboard_size(lang, *received));
            }
        }
        State::Restarting => {
            ui.spinner();
            widgets::hint(ui, pal, text(lang, Key::UpdateConfirm));
        }
        State::Error(error) => {
            widgets::hint(ui, pal, text(lang, Key::UpdateError));
            widgets::hint(ui, pal, error);
            ui.hyperlink_to(text(lang, Key::UpdateDetails), RELEASES);
        }
    }
    if vm.update_all_confirm {
        widgets::hint(ui, pal, text(lang, Key::UpdatePeersFirst));
        widgets::duo(ui, |ui| {
            button(
                ui,
                pal,
                vm,
                Key::UpdateAllComputers,
                Update::InstallAll,
                actions,
            );
            button(ui, pal, vm, Key::Cancel, Update::Cancel, actions);
        });
    } else if !busy && checked_release(vm).is_some() {
        button(
            ui,
            pal,
            vm,
            Key::UpdateAllComputers,
            Update::ConfirmAll,
            actions,
        );
    }
    progress(ui, pal, vm);
}

fn mesh_busy(vm: &ViewModel) -> bool {
    vm.status.as_ref().is_some_and(|status| {
        (!status.update_notice.is_empty() && status.update_nodes.is_empty())
            || status.update_nodes.iter().any(|node| {
                matches!(
                    node.status.as_str(),
                    "pending" | "downloading" | "restarting"
                )
            })
    })
}

fn checked_release(vm: &ViewModel) -> Option<&crate::updater::Release> {
    vm.checked_release.as_ref().or_else(|| match &vm.update {
        State::Available(release) => Some(release),
        _ => None,
    })
}

fn remote_notice(lang: ui_text::Lang, notice: &str) -> String {
    let mut fields = notice.splitn(4, '\t');
    if fields.next() == Some("remote-update") {
        if let (Some(requester), Some(version), Some(detail)) =
            (fields.next(), fields.next(), fields.next())
        {
            let mut message = fill(lang, Key::RemoteUpdateFromVersion, &[requester, version]);
            if !detail.is_empty() {
                message.push_str(" · ");
                message.push_str(detail);
            }
            return message;
        }
    }
    fill(lang, Key::RemoteUpdateNotice, &[notice])
}

fn progress(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel) {
    let Some(status) = &vm.status else { return };
    let lang = vm.look.language;
    if !status.update_notice.is_empty() && !status.update_notice.starts_with("restart-gui:") {
        widgets::hint(ui, pal, &remote_notice(lang, &status.update_notice));
    }
    for node in &status.update_nodes {
        let key = match node.status.as_str() {
            "pending" => Key::UpdatePending,
            "downloading" => Key::DownloadingUpdate,
            "restarting" => Key::UpdateRestarting,
            "done" => Key::UpdateDone,
            "refused" => Key::UpdateRefused,
            "unreachable" => Key::UpdateUnreachable,
            _ => continue,
        };
        let name = if node.display_name.is_empty() {
            &node.node_id
        } else {
            &node.display_name
        };
        let detail = if node.detail.is_empty() {
            String::new()
        } else {
            format!(" · {}", node.detail)
        };
        widgets::hint(
            ui,
            pal,
            &format!(
                "{name} · {} · {}{detail}",
                node.target_version,
                text(lang, key)
            ),
        );
    }
}

fn notes_job(pal: &Palette, notes: &str, width: f32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = width;
    job.wrap.max_rows = 3;
    job.wrap.overflow_character = Some('…');
    job.append(
        notes.trim(),
        0.0,
        egui::text::TextFormat {
            font_id: font(12.0, fam::sans()),
            color: pal.ink2,
            line_height: Some(17.4),
            ..Default::default()
        },
    );
    job
}
pub(super) fn notice(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel, actions: &mut Vec<Action>) {
    let remote_notice = vm.status.as_ref().is_some_and(|status| {
        !status.update_nodes.is_empty()
            || (!status.update_notice.is_empty()
                && !status.update_notice.starts_with("restart-gui:"))
    });
    if !vm.update_notice && !remote_notice {
        return;
    }
    let release = if vm.update_notice {
        checked_release(vm)
    } else {
        None
    };
    egui::Area::new(egui::Id::new("update-notice"))
        .anchor(egui::Align2::RIGHT_TOP, [-92.0, 72.0])
        .order(egui::Order::Foreground)
        .show(ui.ctx(), |ui| {
            egui::Frame::new()
                .fill(pal.bg)
                .stroke(egui::Stroke::new(1.0, pal.ink2))
                .inner_margin(16.0)
                .show(ui, |ui| {
                    ui.set_max_width(420.0);
                    if let Some(release) = release {
                        widgets::hint(
                            ui,
                            pal,
                            &fill(
                                vm.look.language,
                                Key::UpdateAvailable,
                                &[&release.tag_name.trim_start_matches('v')],
                            ),
                        );
                        ui.horizontal_wrapped(|ui| {
                            button(
                                ui,
                                pal,
                                vm,
                                Key::UpdateAllComputers,
                                Update::ConfirmAll,
                                actions,
                            );
                            button(ui, pal, vm, Key::InstallUpdate, Update::Confirm, actions);
                            button(ui, pal, vm, Key::UpdateLater, Update::Later, actions);
                            button(ui, pal, vm, Key::SkipUpdate, Update::Skip, actions);
                        });
                    }
                    progress(ui, pal, vm);
                });
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui_settings::Theme;

    #[test]
    fn remote_notice_localizes_requester_and_version_and_preserves_detail() {
        assert_eq!(
            remote_notice(
                ui_text::Lang::En,
                "remote-update\tLAPTOP\t0.1.7\tDownloading"
            ),
            "LAPTOP requested an update to version 0.1.7 · Downloading",
        );
        assert_eq!(
            remote_notice(ui_text::Lang::Ru, "remote-update\tLAPTOP\t0.1.7\t"),
            "LAPTOP запросил обновление до версии 0.1.7",
        );
        assert_eq!(
            remote_notice(ui_text::Lang::En, "Older core notice"),
            "Update request from another computer: Older core notice",
        );
    }

    #[test]
    fn incoming_updates_block_install_until_every_row_is_terminal() {
        let mut vm = ViewModel::new(
            crate::view_model::CoreEndpoint::Ready {
                description: "test".into(),
            },
            crate::view_model::LocalConfig::default(),
        );
        vm.status = Some(protocol::ipc::Status::default());
        for (status, busy) in [
            ("pending", true),
            ("downloading", true),
            ("restarting", true),
            ("done", false),
            ("refused", false),
            ("unreachable", false),
        ] {
            vm.status.as_mut().unwrap().update_nodes = vec![protocol::ipc::NodeUpdate {
                status: status.into(),
                ..Default::default()
            }];
            assert_eq!(mesh_busy(&vm), busy, "{status}");
        }
        let status = vm.status.as_mut().unwrap();
        status.update_nodes.clear();
        status.update_notice = "LAPTOP".into();
        assert!(mesh_busy(&vm));
    }

    #[test]
    fn release_notes_elide_after_three_actual_lines_in_both_themes() {
        let ctx = egui::Context::default();
        super::super::install_fonts(&ctx);
        for theme in [Theme::Light, Theme::Dark] {
            let output = ctx.run_ui(egui::RawInput::default(), |ui| {
                let galley = ui.painter().layout_job(notes_job(
                    &Palette::of(theme),
                    "First line\nSecond line\nThird line\nFourth line\nFifth line",
                    240.0,
                ));
                assert_eq!(galley.rows.len(), 3);
                assert!(galley.elided);
                assert_eq!(galley.rows[2].glyphs.last().unwrap().chr, '…');
            });
            output.drop_without_applying_deltas();
        }
    }
}
