//! Offscreen PNGs of every screen, in every state (build spec §7).
//!
//! ```text
//! cargo run -p lws-gui --example shots
//! ```
//!
//! Each screen is drawn into an `egui_kittest` harness with the wgpu renderer and written to
//! `target/gui-shots/<screen>-<state>.png`. This is an example and not a test on purpose:
//! `cargo test -p lws-gui` has to pass on a host with no display and no GPU, and a test that
//! needs an adapter would break that promise. The states are the same ones the headless
//! screen tests use, so a picture and an assertion can never disagree about what is drawn.

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};

use egui_kittest::Harness;
use lws_gui::egui;
use lws_gui::screens;
use lws_gui::view_model::{Action, CoreEndpoint, LocalConfig, Request, Screen, Side, ViewModel};
use protocol::ipc::gui_frame::Msg;
use protocol::ipc::GuiFrame;

/// Where the pictures go: `target/` is git-ignored on purpose.
const OUT_DIR: &str = "target/gui-shots";

/// The size of one picture. Wide enough that no screen has to scroll: a screenshot of a
/// scrolled-out line would be a picture of something the window never showed at once.
const CANVAS: [f32; 2] = [1180.0, 1020.0];

fn main() -> std::process::ExitCode {
    let dir = PathBuf::from(OUT_DIR);
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!("cannot create {}: {error}", dir.display());
        return std::process::ExitCode::FAILURE;
    }
    let mut written = 0usize;
    for (name, state) in states() {
        for screen in Screen::ALL {
            let mut state = state.clone();
            state.screen = screen;
            let file = dir.join(format!("{}-{name}.png", file_name(screen)));
            match shoot(state, &file) {
                Ok(()) => {
                    println!("{}", file.display());
                    written += 1;
                }
                Err(why) => {
                    eprintln!("cannot render {}: {why}", file.display());
                    eprintln!(
                        "the wgpu setup that was tried is egui_kittest's default: every native \
                         backend (DX12/Vulkan/GL) with a CPU adapter preferred, and no display \
                         handle (a headless render). Without either there is nothing left to \
                         try — the other renderer of this window is glow, which needs a window."
                    );
                    return std::process::ExitCode::FAILURE;
                }
            }
        }
    }
    println!("{written} pictures in {}", dir.display());
    std::process::ExitCode::SUCCESS
}

/// Draws one screen once and writes the pixel buffer to `file`.
fn shoot(state: ViewModel, file: &Path) -> Result<(), String> {
    let drawn = AssertUnwindSafe(|| {
        let mut harness = Harness::builder()
            .with_size(egui::vec2(CANVAS[0], CANVAS[1]))
            .wgpu()
            .build_ui(move |ui| {
                draw(&state, ui);
            });
        harness.run();
        harness.render()
    });
    // wgpu reports "no adapter" by panicking inside the harness builder (kittest calls
    // `expect` on the render state), so a failed adapter is an unwind, not a `Result`.
    match std::panic::catch_unwind(drawn) {
        Ok(Ok(image)) => image.save(file).map_err(|error| error.to_string()),
        Ok(Err(why)) => Err(why),
        Err(payload) => Err(panic_text(&payload)),
    }
}

/// The message of a caught panic, when it has one.
fn panic_text(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        return (*text).to_owned();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    "unwound without a message".to_owned()
}

/// One screen, drawn the way [`lws_gui::app::GuiApp`] draws it.
fn draw(state: &ViewModel, ui: &mut egui::Ui) {
    let mut actions = Vec::new();
    egui::Panel::top("lws-gui-header").show(ui, |ui| {
        actions.extend(screens::header(state, ui));
        actions.extend(screens::outcome_banner(state, ui));
    });
    egui::CentralPanel::default_margins().show(ui, |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                actions.extend(match state.screen {
                    Screen::Status => screens::status::draw(state, ui),
                    Screen::Pairing => screens::pairing::draw(state, ui),
                    Screen::Layout => screens::layout::draw(state, ui),
                    Screen::Settings => screens::settings::draw(state, ui),
                });
            });
    });
    // The pictures are of the window as it is, so the actions a screen offered are dropped
    // here: nothing in this example talks to a core.
    let _ = actions;
}

/// The file name of a screen.
fn file_name(screen: Screen) -> &'static str {
    match screen {
        Screen::Status => "status",
        Screen::Pairing => "pairing",
        Screen::Layout => "layout",
        Screen::Settings => "settings",
    }
}

// ---------------------------------------------------------------- the states

/// The seven states of the build spec, as `ViewModel`s built from real frames.
fn states() -> Vec<(&'static str, ViewModel)> {
    vec![
        ("disconnected", disconnected()),
        ("connected", connected("LOCAL")),
        ("remote", connected("REMOTE")),
        ("pairing-window", pairing_window()),
        ("pairing-prompt", pairing_prompt()),
        ("pending-edit", pending_edit()),
        ("layout-conflict", layout_conflict()),
    ]
}

fn view() -> ViewModel {
    ViewModel::new(
        CoreEndpoint::Ready {
            description: r"\\.\pipe\lan-workspace-S-1-5-21-0-0-0-1001".to_owned(),
        },
        LocalConfig {
            display_name: "Workstation".to_owned(),
            switch_hotkey: "Ctrl+Alt+Shift+Right".to_owned(),
            claim_hotkey: "Ctrl+Alt+Shift+F11".to_owned(),
            ..LocalConfig::default()
        },
    )
}

fn disconnected() -> ViewModel {
    let mut vm = view();
    vm.on_disconnected("no core is listening".to_owned());
    vm
}

fn connected(focus: &str) -> ViewModel {
    let mut vm = view();
    vm.on_connected(r"\\.\pipe\lan-workspace-S-1-5-21-0-0-0-1001");
    vm.apply(event(Msg::Status(status(focus))));
    vm.apply(event(Msg::TopologyDoc(document(7))));
    if focus == "REMOTE" {
        vm.local(&Action::ShowScreen(Screen::Status));
    }
    vm
}

fn pairing_window() -> ViewModel {
    let mut vm = connected("LOCAL");
    vm.local(&Action::ShowScreen(Screen::Pairing));
    vm.apply(event(Msg::PairingWindow(protocol::ipc::PairingWindow {
        open: true,
        address: "192.168.1.10:47901".to_owned(),
        seconds_remaining: 118,
        attempt_id: vec![0x42; 16],
    })));
    vm
}

fn pairing_prompt() -> ViewModel {
    let mut vm = connected("LOCAL");
    vm.local(&Action::ShowScreen(Screen::Pairing));
    vm.apply(event(Msg::PairingPrompt(protocol::ipc::PairingPrompt {
        attempt_id: vec![0x42; 16],
        peer_name: "linux-box".to_owned(),
        sas_code: "482913".to_owned(),
        local_fingerprint: "A1:B2:C3:D4:E5:F6:07:18:29:3A:4B:5C:6D:7E:8F:90".to_owned(),
        peer_fingerprint: "F0:E1:D2:C3:B4:A5:96:87:78:69:5A:4B:3C:2D:1E:0F".to_owned(),
    })));
    vm.local(&Action::ToggleDetails(true));
    vm
}

fn pending_edit() -> ViewModel {
    let mut vm = connected("LOCAL");
    vm.local(&Action::ShowScreen(Screen::Layout));
    let mut status = status("LOCAL");
    status.layout_pending_edits = 1;
    vm.apply(event(Msg::Status(status)));
    vm.apply(event(Msg::PendingEdits(protocol::ipc::PendingEdits {
        edits: vec![protocol::ipc::PendingEdit {
            edit_id: vec![0x11; 16],
            base_revision: 6,
            base_author: SELF.to_owned(),
            author_node_id: SELF.to_owned(),
            layout_toml: b"revision = 7".to_vec(),
        }],
    })));
    vm
}

fn layout_conflict() -> ViewModel {
    let mut vm = connected("LOCAL");
    vm.local(&Action::ShowScreen(Screen::Layout));
    vm.local(&Action::SetSide(Side::Right));
    let id = vm.begin_request(Request::UpdateTopology, std::time::Instant::now());
    vm.apply(GuiFrame {
        seq: 2,
        request_id: id,
        msg: Some(Msg::OpResult(protocol::ipc::OpResult {
            ok: false,
            error_code: "LAYOUT_CONFLICT".to_owned(),
        })),
    });
    vm
}

const SELF: &str = "aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11";
const PEER: &str = "bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22bb22";

fn event(msg: Msg) -> GuiFrame {
    GuiFrame {
        seq: 1,
        request_id: 0,
        msg: Some(msg),
    }
}

fn status(focus: &str) -> protocol::ipc::Status {
    protocol::ipc::Status {
        role: "owner".to_owned(),
        focus: focus.to_owned(),
        active_peer: if focus == "REMOTE" {
            "linux-box".to_owned()
        } else {
            String::new()
        },
        connected: true,
        clipboard_enabled: true,
        emergency_hotkey: "Ctrl+Alt+Shift+F12".to_owned(),
        rtt_p95_us: 12_400,
        rtt_jitter_us: 300,
        last_error_code: String::new(),
        backend: "platform-win".to_owned(),
        capabilities_unavailable: vec!["RECEIVER_TAKEOVER".to_owned(), "TEXT_CLIPBOARD".to_owned()],
        portal_state: String::new(),
        is_owner: true,
        owner_node_id: SELF.to_owned(),
        owner_serial: 7,
        members: vec![
            protocol::ipc::MemberStatus {
                node_id: SELF.to_owned(),
                display_name: "Workstation".to_owned(),
                online: true,
                is_owner: true,
                link_state: "READY".to_owned(),
            },
            protocol::ipc::MemberStatus {
                node_id: PEER.to_owned(),
                display_name: "linux-box".to_owned(),
                online: true,
                is_owner: false,
                link_state: "READY".to_owned(),
            },
        ],
        layout_revision: 7,
        layout_author: SELF.to_owned(),
        layout_pending_edits: 0,
        layout_editable: true,
        active_layout: if focus == "REMOTE" {
            "ru".to_owned()
        } else {
            String::new()
        },
        agent_degraded_reason: String::new(),
        agent_restart_circuit_open: false,
        transition_disabled_reason: String::new(),
    }
}

fn document(revision: u64) -> protocol::ipc::TopologyDoc {
    protocol::ipc::TopologyDoc {
        revision,
        author_node_id: SELF.to_owned(),
        members: vec![
            protocol::ipc::MemberTopology {
                node_id: SELF.to_owned(),
                display_name: "Workstation".to_owned(),
                online: true,
                displays: vec![display("\\\\.\\DISPLAY1", 0.0, 0.0, 1920.0, 1080.0, true)],
            },
            protocol::ipc::MemberTopology {
                node_id: PEER.to_owned(),
                display_name: "linux-box".to_owned(),
                online: true,
                displays: vec![display("eDP-1", 0.0, 0.0, 1600.0, 900.0, false)],
            },
        ],
        links: Vec::new(),
    }
}

fn display(id: &str, x: f64, y: f64, w: f64, h: f64, known: bool) -> protocol::ipc::DisplayView {
    protocol::ipc::DisplayView {
        id: id.to_owned(),
        logical_x: x,
        logical_y: y,
        logical_w: w,
        logical_h: h,
        rotation_degrees: 0,
        primary: known,
        native_known: known,
        scale: 1.0,
    }
}
