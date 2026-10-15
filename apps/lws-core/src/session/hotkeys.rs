use super::{fr, Session};
use protocol::ipc::{self, agent_frame::Msg};

pub(super) struct Pending {
    request: crate::gui::Request,
    wanted: config::HotkeyBindings,
    rollback: bool,
    deadline: std::time::Instant,
}

pub(crate) fn bindings(config: &config::HotkeyBindings) -> Vec<ipc::HotkeyBinding> {
    config.iter().map(|(action, chord)| ipc::HotkeyBinding {
        action: match action {
            config::HotkeyAction::EmergencyRelease => ipc::HotkeyAction::EmergencyRelease,
            config::HotkeyAction::SwitchNext => ipc::HotkeyAction::SwitchNext,
            config::HotkeyAction::ReturnLocal => ipc::HotkeyAction::ReturnLocal,
            config::HotkeyAction::ClaimOwnership => ipc::HotkeyAction::ClaimOwnership,
        } as i32,
        chord: chord.to_owned(), available: false, detail: "Awaiting agent registration".to_owned(),
    }).collect()
}

impl Session {
    pub(super) async fn hotkeys_tick(&mut self) {
        if self.hotkey_pending.as_ref().is_some_and(|pending| std::time::Instant::now() >= pending.deadline) {
            self.agent.stop().await;
            self.hotkeys_agent_lost().await;
        }
    }
    pub(super) async fn hotkeys_agent_lost(&mut self) {
        for binding in &mut self.hotkey_status {
            binding.available = false;
            binding.detail = "Agent disconnected".to_owned();
        }
        if let Some(pending) = self.hotkey_pending.take() {
            self.hotkey_answer(pending.request, false, "Agent disconnected; saved bindings unchanged").await;
        }
        while let Some(request) = self.hotkey_queue.pop_front() {
            self.hotkey_answer(request, false, "Agent disconnected").await;
        }
    }
    pub fn set_hotkeys_source(&mut self, hotkeys: config::HotkeyBindings) {
        self.hotkey_status = bindings(&hotkeys);
        self.hotkeys = hotkeys.clone();
        self.agent.set_hotkeys(hotkeys);
    }

    pub(super) async fn queue_hotkey(&mut self, request: crate::gui::Request) {
        self.hotkey_queue.push_back(request);
        self.start_hotkey_update().await;
    }

    async fn start_hotkey_update(&mut self) {
        if self.hotkey_status.iter().any(|binding| binding.detail == "Awaiting agent registration") {
            return;
        }
        while self.hotkey_pending.is_none() {
            let Some(request) = self.hotkey_queue.pop_front() else { break };
            let ipc::gui_frame::Msg::SetHotkey(set) = &request.message else { continue };
            let action = match ipc::HotkeyAction::try_from(set.action) {
                Ok(ipc::HotkeyAction::EmergencyRelease) => config::HotkeyAction::EmergencyRelease,
                Ok(ipc::HotkeyAction::SwitchNext) => config::HotkeyAction::SwitchNext,
                Ok(ipc::HotkeyAction::ReturnLocal) => config::HotkeyAction::ReturnLocal,
                Ok(ipc::HotkeyAction::ClaimOwnership) => config::HotkeyAction::ClaimOwnership,
                _ => { self.hotkey_answer(request, false, "Unknown hotkey action").await; continue }
            };
            let mut wanted = self.hotkeys.clone();
            wanted.set(action, set.chord.clone());
            if let Err(error) = wanted.validate() {
                self.hotkey_answer(request, false, &error.to_string()).await;
                continue;
            }
            if self.appearance_path.is_none() {
                self.hotkey_answer(request, false, "Configuration path unavailable").await;
                continue;
            }
            if let Err(error) = self.agent.send(Msg::SetHotkeys(ipc::SetHotkeys { bindings: bindings(&wanted) })).await {
                self.hotkey_answer(request, false, &error).await;
                continue;
            }
            self.hotkey_pending = Some(Pending { request, wanted, rollback: false, deadline: std::time::Instant::now() + std::time::Duration::from_secs(30) });
        }
    }

    pub(super) async fn on_hotkeys_result(&mut self, result: ipc::HotkeysResult) {
        let Some(mut pending) = self.hotkey_pending.take() else {
            self.hotkey_status = if result.bindings.is_empty() {
                bindings(&self.hotkeys).into_iter().map(|mut binding| {
                    binding.detail = result.detail.clone();
                    binding
                }).collect()
            } else { result.bindings };
            self.start_hotkey_update().await;
            return;
        };
        if pending.rollback {
            if result.success { self.hotkey_status = result.bindings; }
            else { self.agent.stop().await; }
            let detail = if result.success { "Configuration write failed; bindings restored" } else { "Configuration write failed; rollback failed, agent stopped" };
            self.hotkey_answer(pending.request, false, detail).await;
        } else if !result.success {
            self.hotkey_answer(pending.request, false, &result.detail).await;
        } else {
            let path = self.appearance_path.clone().expect("validated configuration path");
            let wanted = pending.wanted.clone();
            let saved = tokio::task::spawn_blocking(move || config::lock::update_locked(&path, |config| config.input.hotkeys = wanted)).await;
            if matches!(saved, Ok(Ok(()))) {
                self.hotkeys = pending.wanted;
                self.emergency_hotkey = self.hotkeys.emergency_release.clone();
                self.agent.set_hotkeys(self.hotkeys.clone());
                self.hotkey_status = result.bindings;
                self.hotkey_answer(pending.request, true, "").await;
            } else {
                pending.rollback = true;
                if self.agent.send(Msg::SetHotkeys(ipc::SetHotkeys { bindings: bindings(&self.hotkeys) })).await.is_ok() {
                    self.hotkey_pending = Some(pending);
                    return;
                }
                self.agent.stop().await;
                self.hotkey_answer(pending.request, false, "Configuration write failed; agent stopped to restore saved bindings").await;
            }
        }
        self.start_hotkey_update().await;
    }

    async fn hotkey_answer(&self, request: crate::gui::Request, success: bool, detail: &str) {
        let frame = (request.request_id, ipc::gui_frame::Msg::HotkeysResult(ipc::HotkeysResult { success, detail: detail.to_owned(), bindings: self.hotkey_status.clone() }));
        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), request.out.send(frame)).await;
    }

    pub(super) async fn dispatch_hotkey(&mut self, action: i32) {
        match ipc::HotkeyAction::try_from(action) {
            Ok(ipc::HotkeyAction::ClaimOwnership) => { self.claim().await; }
            Ok(ipc::HotkeyAction::EmergencyRelease) => { self.feed(fr::Input::Local(fr::LocalInput::Emergency)).await; }
            Ok(ipc::HotkeyAction::ReturnLocal) if self.router.focus() == fr::FocusState::Remote => {
                if let Some(peer) = self.router.lease().source().map(|lease| lease.peer) {
                    self.switch_to(&crate::command::PeerRef::new(crate::command::node_id_hex(&peer))).await;
                }
            }
            Ok(ipc::HotkeyAction::SwitchNext) if self.router.focus() == fr::FocusState::Local => {
                let mut peers = self.manager.members().await;
                peers.sort();
                if let Some(peer) = peers.first() {
                    self.switch_to(&crate::command::PeerRef::new(crate::command::node_id_hex(peer))).await;
                }
            }
            _ => {}
        }
    }
}
