//! Fixtures shared by the window's tests: a view model with no window and no core.
//!
//! Every value here is a `GuiFrame` the core could really send — the tests are about what the
//! window does with them, not about a made-up shape of the contract.

#![allow(dead_code)]

use lws_gui::view_model::{CoreEndpoint, LocalConfig, ViewModel};
use protocol::ipc;
use protocol::ipc::gui_frame::Msg;
use protocol::ipc::GuiFrame;

/// The node id of «this computer» in the fixtures.
pub const SELF_NODE: &str = "aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11";
/// The node id of the other computer in the fixtures.
pub const PEER_NODE: &str = "bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22";

/// A window that knows where the core is and nothing else.
pub fn view() -> ViewModel {
    ViewModel::new(
        CoreEndpoint::Ready {
            description: r"\\.\pipe\lan-workspace-S-1-5-21-0-0-0-1001".to_owned(),
        },
        LocalConfig {
            display_name: "Workstation".to_owned(),
            ..LocalConfig::default()
        },
    )
}

/// A frame carrying one message; `request_id` is 0 for events, as the contract says.
pub fn event(msg: Msg) -> GuiFrame {
    GuiFrame {
        seq: 1,
        request_id: 0,
        msg: Some(msg),
    }
}

/// A frame answering the request `request_id` named.
pub fn answer(request_id: u64, msg: Msg) -> GuiFrame {
    GuiFrame {
        seq: 2,
        request_id,
        msg: Some(msg),
    }
}

/// A `Status` of two members with this computer owning input.
pub fn status_with(focus: &str) -> ipc::Status {
    ipc::Status {
        role: "owner".to_owned(),
        focus: focus.to_owned(),
        active_peer: if focus == "REMOTE" {
            "linux-box".to_owned()
        } else {
            String::new()
        },
        connected: true,
        clipboard_enabled: true,
        discovery_enabled: true,
        discovered_peers: Vec::new(),
        edge_effects: true,
        hide_remote_cursor: true,
        emergency_hotkey: "Ctrl+Alt+Shift+F12".to_owned(),
        hotkeys: config::HotkeyBindings::default()
            .iter()
            .map(|(action, chord)| ipc::HotkeyBinding {
                action: match action {
                    config::HotkeyAction::EmergencyRelease => 1,
                    config::HotkeyAction::SwitchNext => 2,
                    config::HotkeyAction::ReturnLocal => 3,
                    config::HotkeyAction::ClaimOwnership => 4,
                },
                chord: chord.to_owned(),
                available: true,
                detail: String::new(),
            })
            .collect(),
        rtt_p95_us: 0,
        rtt_jitter_us: 0,
        last_error_code: String::new(),
        backend: "platform-win".to_owned(),
        capabilities_unavailable: vec!["RECEIVER_TAKEOVER".to_owned()],
        portal_state: String::new(),
        agent_degraded_reason: String::new(),
        agent_restart_circuit_open: false,
        transition_disabled_reason: String::new(),
        is_owner: true,
        local_node_id: SELF_NODE.to_owned(),
        owner_node_id: SELF_NODE.to_owned(),
        owner_serial: 7,
        members: vec![
            ipc::MemberStatus {
                node_id: SELF_NODE.to_owned(),
                display_name: "Workstation".to_owned(),
                online: true,
                is_owner: true,
                link_state: "READY".to_owned(),
            },
            ipc::MemberStatus {
                node_id: PEER_NODE.to_owned(),
                display_name: "linux-box".to_owned(),
                online: true,
                is_owner: false,
                link_state: "READY".to_owned(),
            },
        ],
        layout_revision: 7,
        layout_author: SELF_NODE.to_owned(),
        layout_pending_edits: 0,
        layout_editable: true,
        active_layout: if focus == "REMOTE" {
            "ru".to_owned()
        } else {
            String::new()
        },
    }
}

/// A document with this computer's screen and the other computer's laptop panel.
pub fn two_node_doc(revision: u64) -> ipc::TopologyDoc {
    ipc::TopologyDoc {
        revision,
        author_node_id: SELF_NODE.to_owned(),
        forgotten_node_ids: Vec::new(),
        members: vec![
            ipc::MemberTopology {
                node_id: SELF_NODE.to_owned(),
                display_name: "Workstation".to_owned(),
                system_name: String::new(),
                online: true,
                displays: vec![display("\\\\.\\DISPLAY1", 0.0, 0.0, 1920.0, 1080.0, true)],
            },
            ipc::MemberTopology {
                node_id: PEER_NODE.to_owned(),
                display_name: "linux-box".to_owned(),
                system_name: String::new(),
                online: true,
                displays: vec![display("eDP-1", 0.0, 0.0, 1600.0, 900.0, false)],
            },
        ],
        links: Vec::new(),
    }
}

/// One display of a member.
fn display(id: &str, x: f64, y: f64, w: f64, h: f64, primary: bool) -> ipc::DisplayView {
    ipc::DisplayView {
        id: id.to_owned(),
        native_width: w as u32,
        native_height: h as u32,
        logical_x: x,
        logical_y: y,
        logical_w: w,
        logical_h: h,
        rotation_degrees: 0,
        primary,
        native_known: primary,
        scale: 1.0,
    }
}

/// A prompt with the six-digit code of ADR-010.
pub fn pairing_prompt() -> ipc::PairingPrompt {
    ipc::PairingPrompt {
        attempt_id: vec![0x42; 16],
        peer_name: "linux-box".to_owned(),
        sas_code: "482913".to_owned(),
        local_fingerprint: "A1:B2:C3".to_owned(),
        peer_fingerprint: "D4:E5:F6".to_owned(),
    }
}

/// The attempted id of [`pairing_prompt`].
pub fn attempt_id() -> Vec<u8> {
    vec![0x42; 16]
}
