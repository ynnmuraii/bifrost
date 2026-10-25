//! Background jobs and the orderly hand-off to the newly verified executable.
use crate::updater::{self, Action, State};
use crate::view_model::{self, Bridge, Modal};
use eframe::egui;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};
use ui_text::{fill, Key};

pub(crate) static RESTART: AtomicBool = AtomicBool::new(false);
static TARGET: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
enum Event {
    Checked(Result<updater::Release, String>, bool),
    Progress(u64, Option<u64>),
    Installed(Result<std::fs::File, String>),
}
pub(crate) struct Runtime {
    startup: Option<Instant>,
    sender: mpsc::Sender<Event>,
    receiver: mpsc::Receiver<Event>,
    stopping: Option<crate::core_exit::Wait>,
    busy: bool,
    install_lock: Option<std::fs::File>,
    coordinator_due: Instant,
}
impl Runtime {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        if let Ok(exe) = std::env::current_exe().and_then(std::fs::canonicalize) {
            updater::clean_old(&exe);
            let _ = TARGET.set(exe);
        }
        Self {
            startup: Some(Instant::now() + Duration::from_secs(3)),
            sender,
            receiver,
            stopping: None,
            busy: false,
            install_lock: None,
            coordinator_due: Instant::now(),
        }
    }
    fn check(&mut self, ctx: &egui::Context, manual: bool) {
        if self.busy {
            return;
        }
        self.busy = true;
        let sender = self.sender.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = sender.send(Event::Checked(updater::check(), manual));
            ctx.request_repaint();
        });
    }
    pub fn action(&mut self, action: Action, bridge: &mut Bridge, ctx: &egui::Context) {
        let mesh_busy = bridge.view().status.as_ref().is_some_and(|status| {
            status.update_nodes.iter().any(|row| {
                matches!(
                    row.status.as_str(),
                    "pending" | "downloading" | "restarting"
                )
            })
        });
        if mesh_busy
            && matches!(
                action,
                Action::Install | Action::InstallAll | Action::Confirm | Action::ConfirmAll
            )
        {
            return;
        }
        match action {
            Action::ConfirmAll => {
                let view = bridge.view_mut();
                view.update_confirm = true;
                view.update_all_confirm = true;
                view.modal = Some(Modal::Settings);
            }
            Action::InstallAll => {
                if self.busy {
                    return;
                }
                if let Some(release) = &bridge.view().checked_release {
                    if let Ok(version) = release.version() {
                        bridge.dispatch(
                            view_model::Action::UpdateAll(version.to_string()),
                            Instant::now(),
                        );
                        bridge.view_mut().update_confirm = false;
                        bridge.view_mut().update_all_confirm = false;
                    }
                }
            }
            Action::RemoteEnabled(enabled) => {
                bridge.dispatch(view_model::Action::SetRemoteUpdate(enabled), Instant::now())
            }
            Action::Check => {
                self.startup = None;
                if !self.busy {
                    bridge.view_mut().update = State::Checking;
                    self.check(ctx, true);
                }
            }
            Action::Startup(enabled) => bridge.view_mut().look.check_updates = enabled,
            Action::Later => bridge.view_mut().update_notice = false,
            Action::Skip => {
                let view = bridge.view_mut();
                if let State::Available(release) = &view.update {
                    if let Ok(v) = release.version() {
                        view.look.skipped_update = Some([v.major, v.minor, v.patch]);
                    }
                }
                view.update_notice = false;
            }
            Action::Confirm => {
                let view = bridge.view_mut();
                view.update_notice = false;
                view.update_all_confirm = false;
                view.update_confirm = true;
                view.modal = Some(Modal::Settings);
            }
            Action::Cancel => {
                bridge.view_mut().update_confirm = false;
                bridge.view_mut().update_all_confirm = false;
            }
            Action::Install => {
                if self.busy {
                    return;
                }
                let State::Available(release) = &bridge.view().update else {
                    return;
                };
                let release = release.clone();
                let lang = bridge.view().look.language;
                let sender = self.sender.clone();
                let ctx = ctx.clone();
                self.busy = true;
                bridge.view_mut().update_confirm = false;
                bridge.view_mut().update = State::Downloading {
                    received: 0,
                    total: None,
                };
                std::thread::spawn(move || {
                    let result = (|| {
                        let exe = std::env::current_exe()
                            .and_then(std::fs::canonicalize)
                            .map_err(|e| e.to_string())?;
                        let lock = updater::install_lock(&exe)?;
                        let bytes = updater::prepare(&release, &mut |received, total| {
                            let _ = sender.send(Event::Progress(received, total));
                            ctx.request_repaint();
                        })?;
                        // Keep the process lock through the orderly restart handoff.
                        updater::replace(&exe, &bytes).map_err(|error| {
                            if error.kind() == std::io::ErrorKind::PermissionDenied {
                                fill(
                                    lang,
                                    Key::UpdateNoPermission,
                                    &[&exe.parent().unwrap_or(&exe).display().to_string()],
                                )
                            } else {
                                error.to_string()
                            }
                        })?;
                        let _ = TARGET.set(exe);
                        Ok(lock)
                    })();
                    let _ = sender.send(Event::Installed(result));
                    ctx.request_repaint();
                });
            }
        }
    }
    pub fn tick(&mut self, bridge: &mut Bridge, ctx: &egui::Context) {
        let now = Instant::now();
        if now >= self.coordinator_due {
            bridge.dispatch(
                view_model::Action::RegisterUpdateCoordinator(std::process::id()),
                now,
            );
            self.coordinator_due = now + Duration::from_secs(5);
        }
        ctx.request_repaint_after(self.coordinator_due.saturating_duration_since(now));
        if self.stopping.is_none()
            && bridge
                .view()
                .status
                .as_ref()
                .is_some_and(|status| status.update_notice.starts_with("restart-gui:"))
        {
            if TARGET.get().is_some() {
                let paths = bridge.view().config.paths.clone();
                let pid = bridge.view().core.as_ref().and_then(|core| core.pid);
                if let Some(paths) = paths {
                    match crate::core_exit::Wait::new(&paths, pid, now) {
                        Ok(wait) => {
                            let _ = lws_desktop::stop_tray();
                            self.stopping = Some(wait);
                            self.busy = true;
                            bridge.view_mut().update = State::Restarting;
                        }
                        Err(error) => bridge.view_mut().update = State::Error(error),
                    }
                }
            }
        }
        if let Some(startup) = self.startup {
            if now >= startup {
                self.startup = None;
                if bridge.view().look.check_updates {
                    self.check(ctx, false);
                }
            } else {
                ctx.request_repaint_after(startup - now);
            }
        }
        for event in self.receiver.try_iter() {
            match event {
                Event::Checked(result, manual) => {
                    self.busy = false;
                    let view = bridge.view_mut();
                    let result = result.and_then(|release| {
                        view.checked_release = Some(release.clone());
                        updater::offer(
                            &release,
                            protocol::app_version(),
                            view.look.skipped_update,
                            manual,
                        )
                        .map(|offer| {
                            if offer {
                                State::Available(release)
                            } else {
                                State::Current
                            }
                        })
                    });
                    match result {
                        Ok(state) => {
                            tracing::info!(
                                manual,
                                current_version = env!("CARGO_PKG_VERSION"),
                                update_available = matches!(state, State::Available(_)),
                                "release update check completed"
                            );
                            view.update_notice = !manual && matches!(state, State::Available(_));
                            view.update = state;
                        }
                        Err(error) if manual => {
                            view.update = State::Error(view_model::short_error(&error))
                        }
                        Err(_) => {}
                    }
                }
                Event::Progress(received, total) => {
                    bridge.view_mut().update = State::Downloading { received, total }
                }
                Event::Installed(Err(error)) => {
                    self.busy = false;
                    bridge.view_mut().update = State::Error(view_model::short_error(&error));
                }
                Event::Installed(Ok(lock)) => {
                    self.install_lock = Some(lock);
                    // Shutdown may drop the GUI link before core has released its listeners.
                    bridge.shutdown_for_update(now);
                    if let Err(error) = lws_desktop::stop_tray() {
                        if lws_desktop::tray_is_running() {
                            self.busy = false;
                            bridge.view_mut().update = State::Error(error.to_string());
                            continue;
                        }
                    }
                    bridge.view_mut().update = State::Restarting;
                    let wait = bridge
                        .view()
                        .config
                        .paths
                        .as_ref()
                        .ok_or_else(|| {
                            "Cannot wait for core shutdown: settings directories are unknown"
                                .to_owned()
                        })
                        .and_then(|paths| {
                            crate::core_exit::Wait::new(
                                paths,
                                bridge.view().core.as_ref().and_then(|core| core.pid),
                                now,
                            )
                        });
                    match wait {
                        Ok(wait) => self.stopping = Some(wait),
                        Err(error) => {
                            self.busy = false;
                            bridge.view_mut().update = State::Error(error);
                        }
                    }
                }
            }
        }
        if let Some(wait) = &self.stopping {
            let step = wait.poll(now);
            if step == crate::core_exit::Step::Exited && !lws_desktop::tray_is_running() {
                self.stopping = None;
                RESTART.store(true, Ordering::Release);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else if wait.timed_out(now) {
                self.stopping = None;
                self.busy = false;
                bridge.view_mut().update = State::Error(
                    ui_text::text(bridge.view().look.language, Key::UpdateStopFailed).into(),
                );
            }
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

/// Preserve every global path override and explicitly start the fresh core in the new GUI.
pub fn restart_command(exe: &std::path::Path, args: &[String]) -> std::process::Command {
    let mut command = std::process::Command::new(exe);
    command.arg("gui").arg("--updated").args(args);
    command
}
pub(crate) fn restart(args: &[String]) -> std::io::Result<()> {
    let exe = TARGET
        .get()
        .ok_or_else(|| std::io::Error::other("No verified replacement target"))?;
    let mut command = restart_command(exe, args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Same PID: a systemd --user unit must not see its GUI/main process exit.
        Err(command.exec())
    }
    #[cfg(windows)]
    {
        command.spawn().map(|_| ())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restart_retains_config_flags() {
        let args = vec!["--root".into(), "scratch folder".into()];
        let command = restart_command(std::path::Path::new("bifrost"), &args);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["gui", "--updated", "--root", "scratch folder"]
        );
    }
}
