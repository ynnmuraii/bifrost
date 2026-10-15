//! Lightweight desktop tray. It observes an existing core; it never starts one.
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
        items
    }
}

/// Runs the per-user tray until explicitly exited. Global flags are retained for GUI launches.
pub fn run_tray(args: Vec<String>) -> ExitCode {
    let _guard = match lws_desktop::acquire(lws_desktop::Role::Tray) {
        Ok(lws_desktop::InstanceOutcome::Started(guard)) => guard,
        Ok(lws_desktop::InstanceOutcome::Existing) => return ExitCode::SUCCESS,
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
        let (state_tx, state) = watch::channel(State {
            local_name,
            look_file,
            ..Default::default()
        });
        let thread = std::thread::Builder::new()
            .name("lws-tray-ipc".into())
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
    let mut root = None;
    let mut config_dir = None;
    let mut data = None;
    let mut state = None;
    let mut runtime = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(a, b)| (a, Some(b)));
        let slot = match flag {
            "--root" => &mut root,
            "--config-dir" => &mut config_dir,
            "--data-dir" => &mut data,
            "--state-dir" => &mut state,
            "--runtime-dir" => &mut runtime,
            _ => continue,
        };
        if let Some(value) = inline.or_else(|| args.next().map(String::as_str)) {
            *slot = Some(std::path::PathBuf::from(value));
        }
    }
    let defaults = config::default_paths().map_err(|e| e.to_string())?;
    let under = |name: &str| root.as_ref().map(|root| root.join(name));
    Ok(config::Paths::new(
        config_dir
            .or_else(|| under("config"))
            .unwrap_or_else(|| defaults.config_dir().to_path_buf()),
        data.or_else(|| under("data"))
            .unwrap_or_else(|| defaults.data_dir().to_path_buf()),
        state
            .or_else(|| under("state"))
            .unwrap_or_else(|| defaults.state_dir().to_path_buf()),
        runtime
            .or_else(|| under("run"))
            .or_else(|| defaults.runtime_dir().map(std::path::Path::to_path_buf)),
    ))
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
            let mut failed = false;
            for msg in [
                Msg::GetStatus(wire::GetStatus {}),
                Msg::GetTopology(wire::GetTopology {}),
            ] {
                id += 1;
                if link.send(id, msg).await.is_err() {
                    failed = true;
                    break;
                }
            }
            while !failed {
                tokio::select! {
                    frame = link.recv() => match frame {
                        Ok(frame) => match frame.msg {
                            Some(Msg::Status(status)) => { state.send_modify(|s| s.status = Some(status)); }
                            Some(Msg::TopologyDoc(topology)) => { state.send_modify(|s| s.topology = Some(topology)); }
                            _ => {}
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
                            other => { if offline_action(other, &flags) { return; } None }
                        };
                        if let Some(msg) = msg { id += 1; failed = link.send(id, msg).await.is_err(); }
                    }
                }
            }
            state.send_modify(|s| {
                s.status = None;
                s.topology = None;
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
        Some(Action::Open) => {
            if let Err(error) = lws_desktop::spawn_role(lws_desktop::Role::Gui, flags) {
                eprintln!("Не удалось открыть окно: {error}");
            }
            false
        }
        _ => false,
    }
}
