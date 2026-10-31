//! Lightweight desktop tray. It observes an existing core; it never starts one.
mod audio;
mod geometry;
mod menu;
#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;
#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

use menu::Action;
use protocol::ipc::{self as wire, gui_frame::Msg};
use std::{ffi::OsString, process::ExitCode, time::Duration};
use tokio::sync::{mpsc, watch};
use ui_text::{fill, text, Key, Lang};

/// The language the window stored, or the system's own when the file says nothing.
fn language_of(file: Option<&std::path::Path>) -> Lang {
    file.and_then(|file| std::fs::read_to_string(file).ok())
        .and_then(|text| ui_text::language_in(&text))
        .unwrap_or_default()
}

#[derive(Clone, Default)]
struct State {
    status: Option<wire::Status>,
    topology: Option<wire::TopologyDoc>,
    audio: audio::Audio,
    local_name: String,
    /// The window's own `gui.toml`, read every time the menu is built.
    look_file: Option<std::path::PathBuf>,
}
impl State {
    fn items(&self) -> Vec<menu::Item> {
        // The window's own language, read every time the menu is built: a choice made in the
        // window shows up here without restarting the tray.
        let lang = language_of(self.look_file.as_deref());
        let mut items = menu::items(self.status.as_ref(), self.topology.as_ref(), lang);
        if !self.local_name.is_empty() {
            for item in &mut items {
                if matches!(item.action, Some(Action::Switch(_)))
                    && item.label == text(lang, Key::SendInputToThisComputer)
                {
                    item.label = fill(lang, Key::SendInputTo, &[&self.local_name]);
                }
            }
        }
        let audio_items = self.audio.items(lang);
        let end = items.len().saturating_sub(2);
        items.splice(end..end, audio_items);
        items
    }
}

impl State {
    fn tooltip(&self) -> String {
        let lang = language_of(self.look_file.as_deref());
        self.audio.labels(lang).join("\n")
    }
}

/// Runs the per-user tray until explicitly exited. Global flags are retained for GUI launches.
pub fn run_tray(args: Vec<String>) -> ExitCode {
    let _guard = match bifrost_desktop::acquire(bifrost_desktop::Role::Tray) {
        Ok(bifrost_desktop::InstanceOutcome::Started(guard)) => guard,
        Ok(bifrost_desktop::InstanceOutcome::Existing) => return ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Не удалось запустить трей: {error}");
            return ExitCode::FAILURE;
        }
    };
    #[cfg(any(windows, target_os = "linux"))]
    {
        let paths = paths(&args);
        let local_name = paths
            .as_ref()
            .ok()
            .and_then(|paths| config::Config::load(paths.config_file()).ok())
            .map(|config| config.node.display_name)
            .unwrap_or_default();
        // The window's own file, remembered so the menu can follow its language.
        let look_file = paths
            .as_ref()
            .ok()
            .map(|paths| paths.config_dir().join("gui.toml"));
        let endpoint =
            paths.and_then(|paths| ipc::transport::gui_endpoint(&paths).map_err(|e| e.to_string()));
        if let Err(error) = &endpoint {
            eprintln!("Не удалось определить IPC службы: {error}");
        }
        let flags: Vec<OsString> = args.into_iter().map(Into::into).collect();
        let (commands, rx) = mpsc::unbounded_channel();
        let exit_commands = commands.clone();
        if let Err(error) = _guard.set_show_handler(move || {
            let _ = exit_commands.send(Action::Exit);
        }) {
            eprintln!("bifrost tray: update listener: {error}");
            return ExitCode::FAILURE;
        }
        let (state_tx, state) = watch::channel(State {
            local_name,
            look_file,
            ..Default::default()
        });
        let thread = std::thread::Builder::new()
            .name("bifrost-tray-ipc".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime.block_on(worker(endpoint.ok(), flags, rx, state_tx)),
                    Err(error) => eprintln!("Не удалось запустить IPC трея: {error}"),
                }
            });
        let Ok(thread) = thread else {
            return ExitCode::FAILURE;
        };
        let Some(ready) = _guard.tray_ready() else {
            return ExitCode::FAILURE;
        };
        let result = platform::run(state, commands.clone(), ready);
        let _ = commands.send(Action::Exit);
        let _ = thread.join();
        match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Не удалось запустить трей: {error}");
                ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = args;
        ExitCode::FAILURE
    }
}

fn paths(args: &[String]) -> Result<config::Paths, String> {
    // The same parser the window and the core use: the tray is started with the window's own
    // arguments, and `--root=/path` must name the same directory in both.
    config::CliPaths::parse(args).resolve()
}

async fn worker(
    endpoint: Option<ipc::transport::Endpoint>,
    flags: Vec<OsString>,
    mut commands: mpsc::UnboundedReceiver<Action>,
    state: watch::Sender<State>,
) {
    let mut id = 0u64;
    loop {
        let connection = async {
            match &endpoint {
                Some(endpoint) => ipc::transport::connect(endpoint).await.ok(),
                None => {
                    std::future::pending::<()>().await;
                    None
                }
            }
        };
        let stream = tokio::select! {
            stream = connection => stream,
            action = commands.recv() => {
                if offline_action(action, &flags) { return; }
                continue;
            }
        };
        if let Some(stream) = stream {
            let mut link = ipc::GuiLink::new(stream);
            state.send_modify(|s| s.audio = audio::Audio::default());
            let mut poll = tokio::time::interval(audio::POLL_INTERVAL);
            let mut failed = false;
            for msg in [
                Msg::GetStatus(wire::GetStatus {}),
                Msg::GetTopology(wire::GetTopology {}),
                Msg::GetAudioStatus(wire::GetAudioStatus {}),
            ] {
                id += 1;
                if matches!(&msg, Msg::GetAudioStatus(_)) {
                    state.send_modify(|s| s.audio.queried(id, std::time::Instant::now()));
                }
                if link.send(id, msg).await.is_err() {
                    failed = true;
                    break;
                }
            }
            while !failed {
                tokio::select! {
                    _ = poll.tick() => {
                        state.send_modify(|s| s.audio.expire(std::time::Instant::now()));
                        if state.borrow().audio.can_query() {
                            id += 1;
                            state.send_modify(|s| s.audio.queried(id, std::time::Instant::now()));
                            failed = link.send(id, Msg::GetAudioStatus(wire::GetAudioStatus {})).await.is_err();
                        }
                    }
                    frame = link.recv() => match frame {
                        Ok(frame) => match frame.msg {
                            Some(Msg::Status(status)) => { state.send_modify(|s| s.status = Some(status)); }
                            Some(Msg::TopologyDoc(topology)) => { state.send_modify(|s| s.topology = Some(topology)); }
                            Some(msg) => {
                                let mut refresh = false;
                                state.send_modify(|s| refresh = s.audio.receive(frame.request_id, &msg));
                                if refresh && state.borrow().audio.can_query() {
                                    id += 1;
                                    state.send_modify(|s| s.audio.queried(id, std::time::Instant::now()));
                                    failed = link.send(id, Msg::GetAudioStatus(wire::GetAudioStatus {})).await.is_err();
                                }
                            }
                            None => {}
                        },
                        Err(_) => break,
                    },
                    action = commands.recv() => {
                        let msg = match action {
                            Some(Action::Stop) => Some(Msg::Shutdown(wire::Shutdown {})),
                            Some(Action::Switch(peer)) => {
                                // Recheck against live state, not a menu opened before a state change.
                                let allowed = state.borrow().items().iter().any(|item| item.action == Some(Action::Switch(peer.clone())));
                                allowed.then_some(Msg::SwitchTo(wire::SwitchTo { peer }))
                            }
                            Some(Action::GameGuardToggle) => state.borrow().status.as_ref().map(|status| Msg::SetGameGuard(wire::SetGameGuard {
                                mode: if status.game_guard_mode == "always" { "auto" } else { "always" }.to_owned(),
                                block_apps: status.game_guard_block_apps.clone(),
                                allow_apps: status.game_guard_allow_apps.clone(),
                            })),
                            Some(action @ (Action::StopAllAudio | Action::AudioReceiveMute(_) | Action::MicMute | Action::MicPttRelease)) => state.borrow().audio.request(&action),
                            other => { if offline_action(other, &flags) { return; } None }
                        };
                        if let Some(msg) = msg {
                            id += 1;
                            if matches!(&msg, Msg::StopAudio(_) | Msg::SetAudioPeerGain(_) | Msg::SetMicrophoneMute(_) | Msg::SetPtt(_)) {
                                state.send_modify(|s| s.audio.sent(id, std::time::Instant::now()));
                            }
                            failed = link.send(id, msg).await.is_err();
                        }
                    }
                }
            }
            state.send_modify(|s| {
                s.status = None;
                s.topology = None;
                s.audio.disconnected();
            });
        }
        let delay = tokio::time::sleep(Duration::from_secs(3));
        tokio::pin!(delay);
        loop {
            tokio::select! {
                _ = &mut delay => break,
                action = commands.recv() => { if offline_action(action, &flags) { return; } }
            }
        }
    }
}

fn offline_action(action: Option<Action>, flags: &[OsString]) -> bool {
    match action {
        None | Some(Action::Exit) => true,
        Some(action @ (Action::Open | Action::OpenAudioPlayer | Action::OpenAudioSettings)) => {
            let mut launch = flags.to_vec();
            if let Some((argument, destination)) = navigation(&action) {
                let args: Vec<_> = flags
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect();
                match paths(&args).and_then(|paths| {
                    std::fs::create_dir_all(paths.state_dir())
                        .and_then(|()| {
                            std::fs::write(
                                paths.state_dir().join("gui-audio-navigation"),
                                destination,
                            )
                        })
                        .map_err(|error| error.to_string())
                }) {
                    Ok(()) => launch.push(argument.into()),
                    Err(error) => {
                        eprintln!("bifrost tray: audio navigation: {error}");
                        return false;
                    }
                }
            }
            if let Err(error) = bifrost_desktop::spawn_role(bifrost_desktop::Role::Gui, &launch) {
                eprintln!("Не удалось открыть окно: {error}");
            }
            false
        }
        _ => false,
    }
}

fn navigation(action: &Action) -> Option<(&'static str, &'static str)> {
    match action {
        Action::OpenAudioPlayer => Some(("--audio-player", "player")),
        Action::OpenAudioSettings => Some(("--audio-settings", "settings")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_navigation_uses_only_fixed_destinations() {
        assert_eq!(
            navigation(&Action::OpenAudioPlayer),
            Some(("--audio-player", "player"))
        );
        assert_eq!(
            navigation(&Action::OpenAudioSettings),
            Some(("--audio-settings", "settings"))
        );
        assert_eq!(navigation(&Action::Open), None);
        assert_eq!(navigation(&Action::AudioReceiveMute(vec![1; 32])), None);
    }

    #[test]
    fn stopped_service_still_offers_audio_navigation_without_audio_commands() {
        let state = State::default();
        let items = state.items();
        assert!(items
            .iter()
            .any(|item| item.action == Some(Action::OpenAudioPlayer)));
        assert!(items
            .iter()
            .any(|item| item.action == Some(Action::OpenAudioSettings)));
        assert!(!items.iter().any(|item| matches!(
            item.action,
            Some(
                Action::StopAllAudio
                    | Action::MicMute
                    | Action::MicPttRelease
                    | Action::AudioReceiveMute(_)
            )
        )));
    }
}
