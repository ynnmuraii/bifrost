//! Offscreen PNGs of the window: the main screen in both themes and every modal.
//!
//! ```text
//! cargo run -p lws-gui --example shots
//! ```
//! Pass `--root DIR --output DIR` (or `--live` for the user's default endpoint) to
//! snapshot a real core over IPC instead. This mode only queries state; it never
//! starts a core, applies a layout or sends a pairing decision. Both modes render
//! offscreen and never open a desktop window.
//!
//! `--prefix NAME` names the files (`foo-main-light.png`); `--zoom PCT` sets the scale of the
//! dedicated zoomed-map picture (150 by default). Real mode also draws one picture of a
//! vertical stack: the real snapshot plus one extra member placed below it, which is exactly
//! what a drag onto the bottom edge produces.
//!
//! Each state is drawn into an `egui_kittest` harness with the wgpu renderer and written to
//! `target/ui-shots/<name>.png`. This is an example and not a test on purpose: `cargo test`
//! has to pass on a host with no display and no GPU. The states are hand-built `ViewModel`s from
//! real `GuiFrame`s — the three computers of the mockup — so a picture can be laid next to
//! `docs/design/mockup` and compared.

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use lws_gui::egui;
use lws_gui::gui_settings::{Look, Theme};
use lws_gui::ui::{self, Env, UiState};
use lws_gui::view_model::{
    self, Action, Bridge, CliArgs, CoreEndpoint, LocalConfig, Modal, ViewModel,
};
use protocol::ipc;
use protocol::ipc::gui_frame::Msg;
use protocol::ipc::GuiFrame;

const OUT_DIR: &str = "target/ui-shots";
const CANVAS: [f32; 2] = [1280.0, 800.0];

/// Node ids are 64 hex digits; the mockup's names are given a recognisable prefix.
static WORKSTATION: LazyLock<String> = LazyLock::new(|| format!("{:0<64}", "aa11aa11"));
static LAPTOP: LazyLock<String> = LazyLock::new(|| format!("{:0<64}", "7f3a91c20b6ed4a8"));
static HTPC: LazyLock<String> = LazyLock::new(|| format!("{:0<64}", "c04e5b175b17"));

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let option = |name: &str| {
        args.windows(2)
            .find(|pair| pair[0] == name)
            .map(|pair| pair[1].clone())
    };
    let dir = option("--output").map_or_else(|| PathBuf::from(OUT_DIR), PathBuf::from);
    let real = args.iter().any(|arg| arg == "--live" || arg == "--root");
    let prefix = option("--prefix").unwrap_or_else(|| "real".to_owned());
    let zoom = option("--zoom")
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(150)
        .clamp(70, 150);
    let snapshot = if real {
        match real_snapshot(CliArgs::parse(args)) {
            Ok(view) => Some(view),
            Err(error) => {
                eprintln!("cannot snapshot core: {error}");
                return std::process::ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!("cannot create {}: {error}", dir.display());
        return std::process::ExitCode::FAILURE;
    }
    let mut written = 0usize;
    for theme in [Theme::Light, Theme::Dark] {
        let tag = if theme == Theme::Light {
            "light"
        } else {
            "dark"
        };
        let mut states = Vec::new();
        if let Some(snapshot) = &snapshot {
            let mut view = snapshot.clone();
            view.look.theme = theme;
            view.modal = None;
            states.push((format!("{prefix}-main-{tag}"), view.clone()));
            let mut zoomed = view.clone();
            zoomed.look.zoom_percent = zoom;
            states.push((format!("{prefix}-zoom-{tag}"), zoomed));
            states.push((
                format!("{prefix}-window-{tag}"),
                with_window(view.clone()),
            ));
            if let Some(stacked) = with_vertical_peer(&view) {
                states.push((format!("{prefix}-vertical-{tag}"), stacked));
            }
            for modal in Modal::ALL {
                view.modal = Some(modal);
                states.push((
                    format!("{prefix}-modal-{}-{tag}", modal_name(modal)),
                    view.clone(),
                ));
            }
        } else {
            states.push((format!("main-{tag}"), mockup(theme, None)));
            states.push((
                format!("main-window-{tag}"),
                with_window(mockup(theme, None)),
            ));
            for modal in Modal::ALL {
                states.push((
                    format!("modal-{}-{tag}", modal_name(modal)),
                    mockup(theme, Some(modal)),
                ));
            }
        }
        for (name, state) in states {
            let file = dir.join(format!("{name}.png"));
            match shoot(state, &file) {
                Ok(()) => {
                    println!("{}", file.display());
                    written += 1;
                }
                Err(why) => {
                    eprintln!("cannot render {}: {why}", file.display());
                    return std::process::ExitCode::FAILURE;
                }
            }
        }
    }
    println!("{written} pictures in {}", dir.display());
    std::process::ExitCode::SUCCESS
}

fn real_snapshot(args: CliArgs) -> Result<ViewModel, String> {
    let (view, client) = view_model::boot(args);
    let mut bridge = Bridge::new(view, client);
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut queried_edits = false;
    loop {
        bridge.pump();
        let view = bridge.view();
        if view.connection.is_connected() && view.status.is_some() && view.editor.has_document() {
            if !queried_edits {
                bridge.dispatch(Action::ListPendingEdits, Instant::now());
                queried_edits = true;
            } else if view.inflight.is_empty() {
                let mut snapshot = view.clone();
                if let Some(paths) = &snapshot.config.paths {
                    snapshot.look = lws_gui::gui_settings::load(&lws_gui::gui_settings::path_in(
                        paths.config_dir(),
                    ));
                }
                snapshot.pairing.details = snapshot.pairing.prompt.is_some();
                println!(
                    "data_source=IPC node_names={:?} topology_revision={} pairing_open={} sas={:?}",
                    snapshot
                        .editor
                        .nodes
                        .iter()
                        .map(|node| &node.display_name)
                        .collect::<Vec<_>>(),
                    snapshot.editor.revision,
                    snapshot.pairing.window.is_some(),
                    snapshot
                        .pairing
                        .prompt
                        .as_ref()
                        .map(|prompt| &prompt.sas_code),
                );
                return Ok(snapshot);
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "no complete status/topology snapshot: {:?}",
                bridge.view().connection
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn modal_name(modal: Modal) -> &'static str {
    match modal {
        Modal::Layout => "layout",
        Modal::Pairs => "pairs",
        Modal::Keys => "keys",
        Modal::Clip => "clip",
        Modal::Diag => "diag",
        Modal::Settings => "settings",
    }
}

fn shoot(state: ViewModel, file: &Path) -> Result<(), String> {
    let drawn = AssertUnwindSafe(|| {
        let mut ui_state = UiState::new(Instant::now());
        let mut harness = Harness::builder()
            .with_size(egui::vec2(CANVAS[0], CANVAS[1]))
            .wgpu()
            .build_ui(move |ui| {
                let env = Env {
                    now: Instant::now(),
                    animate: false,
                };
                let _ = ui::draw(ui, &state, &mut ui_state, env);
            });
        ui::install_fonts(&harness.ctx);
        harness.run_steps(3);
        // The dialog is revealed over 0.32 s of the wall clock; let it finish.
        std::thread::sleep(Duration::from_millis(450));
        harness.run_steps(4);
        harness.render()
    });
    match std::panic::catch_unwind(drawn) {
        Ok(Ok(image)) => image.save(file).map_err(|error| error.to_string()),
        Ok(Err(why)) => Err(why),
        Err(payload) => Err(panic_text(&payload)),
    }
}

fn panic_text(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        return (*text).to_owned();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    "unwound without a message".to_owned()
}

fn event(msg: Msg) -> GuiFrame {
    GuiFrame {
        seq: 1,
        request_id: 0,
        msg: Some(msg),
    }
}

fn display(
    id: &str,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    primary: bool,
    known: bool,
) -> ipc::DisplayView {
    ipc::DisplayView {
        id: id.to_owned(),
        logical_x: x,
        logical_y: y,
        logical_w: w,
        logical_h: h,
        rotation_degrees: 0,
        primary,
        native_known: known,
        scale: 1.0,
    }
}

fn auto(node: &str, edge: &str) -> Option<ipc::EndpointView> {
    Some(ipc::EndpointView {
        node_id: node.to_owned(),
        display_id: "auto".to_owned(),
        edge: edge.to_owned(),
        segment_start: 0.0,
        segment_end: 0.0,
    })
}

/// The three computers of the mockup: LAPTOP, this WORKSTATION with two displays, an HTPC that
/// is off; input is on the laptop.
fn mockup(theme: Theme, modal: Option<Modal>) -> ViewModel {
    let mut vm = ViewModel::new(
        CoreEndpoint::Ready {
            description: "pipe".to_owned(),
        },
        LocalConfig {
            display_name: "WORKSTATION".to_owned(),
            listen_address: "192.168.1.24".to_owned(),
            ..LocalConfig::default()
        },
    );
    vm.look = Look {
        theme,
        ..Look::default()
    };
    vm.on_connected("pipe");
    vm.settings.lan.candidates = vec!["192.168.1.24".to_owned(), "10.0.0.5".to_owned()];
    let member = |id: &str, name: &str, online: bool, owner: bool| ipc::MemberStatus {
        node_id: id.to_owned(),
        display_name: name.to_owned(),
        online,
        is_owner: owner,
        link_state: if online { "READY" } else { "CLOSING" }.to_owned(),
    };
    vm.apply(event(Msg::Status(ipc::Status {
        role: "owner".to_owned(),
        focus: "REMOTE".to_owned(),
        active_peer: "LAPTOP".to_owned(),
        connected: true,
        clipboard_enabled: true,
        emergency_hotkey: "Ctrl+Alt+Shift+F12".to_owned(),
        backend: "platform-win".to_owned(),
        is_owner: true,
        owner_node_id: WORKSTATION.to_owned(),
        owner_serial: 14,
        members: vec![
            member(&WORKSTATION, "WORKSTATION", true, true),
            member(&LAPTOP, "LAPTOP", true, false),
            member(&HTPC, "HTPC", false, false),
        ],
        layout_revision: 14,
        layout_author: WORKSTATION.to_owned(),
        layout_editable: true,
        ..ipc::Status::default()
    })));
    vm.apply(event(Msg::TopologyDoc(ipc::TopologyDoc {
        revision: 14,
        author_node_id: WORKSTATION.to_owned(),
        members: vec![
            ipc::MemberTopology {
                node_id: LAPTOP.to_owned(),
                display_name: "LAPTOP".to_owned(),
                online: true,
                displays: vec![display("eDP-1", 0.0, 0.0, 1920.0, 1200.0, true, true)],
            },
            ipc::MemberTopology {
                node_id: WORKSTATION.to_owned(),
                display_name: "WORKSTATION".to_owned(),
                online: true,
                displays: vec![
                    display("DISPLAY2", 0.0, 180.0, 1920.0, 1080.0, false, true),
                    display("DISPLAY1", 1920.0, 0.0, 2560.0, 1440.0, true, true),
                ],
            },
            ipc::MemberTopology {
                node_id: HTPC.to_owned(),
                display_name: "HTPC".to_owned(),
                online: false,
                displays: vec![display("HDMI-1", 0.0, 0.0, 3840.0, 2160.0, true, false)],
            },
        ],
        links: vec![
            ipc::LinkView {
                id: "link-0".to_owned(),
                from: auto(&LAPTOP, "RIGHT"),
                to: auto(&WORKSTATION, "LEFT"),
                bidirectional: true,
                active: true,
                inactive_reason: String::new(),
                auto_displays: Vec::new(),
            },
            ipc::LinkView {
                id: "link-1".to_owned(),
                from: auto(&WORKSTATION, "RIGHT"),
                to: auto(&HTPC, "LEFT"),
                bidirectional: true,
                active: true,
                inactive_reason: String::new(),
                auto_displays: Vec::new(),
            },
        ],
    })));
    // Waiting for the code to compare, as the mockup's dock shows it.
    vm.apply(event(Msg::PairingPrompt(ipc::PairingPrompt {
        attempt_id: vec![7; 16],
        peer_name: "LAPTOP".to_owned(),
        sas_code: "482913".to_owned(),
        local_fingerprint: "2C19 E07B 5A3F 8D61 …".to_owned(),
        peer_fingerprint: "7F3A 91C2 0B6E D4A8 …".to_owned(),
    })));
    if let Some(modal) = modal {
        vm.local(&Action::ToggleModal(modal));
    }
    vm
}

/// The real snapshot plus one more member placed directly below this computer, which is what a
/// drag onto the bottom edge produces: the map draws the pair as a `Bottom ↔ Top` link.
///
/// The extra member repeats the displays the dry core really reported, so the picture is the
/// real data with a real second computer rather than a mockup.
fn with_vertical_peer(view: &ViewModel) -> Option<ViewModel> {
    let local = view.editor.nodes.first()?.clone();
    if local.displays.is_empty() {
        return None;
    }
    let peer = "ab".repeat(32);
    let wire = |display: &lws_gui::view_model::EditorDisplay| ipc::DisplayView {
        id: display.id.clone(),
        logical_x: display.x,
        logical_y: display.y,
        logical_w: display.w,
        logical_h: display.h,
        rotation_degrees: display.rotation,
        primary: display.primary,
        native_known: display.native_known,
        scale: 1.0,
    };
    let mut view = view.clone();
    let mut document = ipc::TopologyDoc {
        revision: view.editor.revision,
        author_node_id: view.editor.author.clone(),
        members: view
            .editor
            .nodes
            .iter()
            .map(|node| ipc::MemberTopology {
                node_id: node.node_id.clone(),
                display_name: node.display_name.clone(),
                online: node.online,
                displays: node.displays.iter().map(wire).collect(),
            })
            .collect(),
        links: view.editor.links.clone(),
    };
    document.members.push(ipc::MemberTopology {
        node_id: peer.clone(),
        display_name: "LAPTOP".to_owned(),
        online: true,
        displays: local.displays.iter().map(wire).collect(),
    });
    document.links.push(ipc::LinkView {
        id: "final-vertical".to_owned(),
        from: auto(&local.node_id, "BOTTOM"),
        to: auto(&peer, "TOP"),
        bidirectional: true,
        active: true,
        inactive_reason: String::new(),
        auto_displays: Vec::new(),
    });
    view.apply(event(Msg::TopologyDoc(document)));
    if let Some(status) = view.status.as_mut() {
        status.members.push(ipc::MemberStatus {
            node_id: peer,
            display_name: "LAPTOP".to_owned(),
            online: true,
            is_owner: false,
            link_state: "READY".to_owned(),
        });
    }
    Some(view)
}

/// The same, with an open pairing window instead of a prompt.
fn with_window(mut vm: ViewModel) -> ViewModel {
    vm.apply(event(Msg::PairingWindow(ipc::PairingWindow {
        open: true,
        address: "192.168.1.24:47901".to_owned(),
        seconds_remaining: 87,
        attempt_id: vec![7; 16],
    })));
    vm.pairing.prompt = None;
    vm
}
