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
//! Mock mode writes `polish-<state>-<theme>-<width>x<height>.png` at 1920×1080,
//! 1745×1091 and 1280×720, including every modal, appearance and disconnected states.
//! `--prefix NAME` overrides the prefix; `--zoom PCT` sets the real-mode zoom picture.
//! Rendering is offscreen through egui_kittest/wgpu and requires a working GPU adapter.
//! `--placement` writes only the owner placement sequence (before, preview, accepted)
//! at 1745×1091 in RU/EN and light/dark, using mocks and real move/drop actions.
//! `--gap-links` writes owner, dragged-paddle and vertical-gap views at 1745×1091
//! in RU/EN and light/dark, using only mocked topology and real layout actions.
//! `--metadata` writes only the map at 1745×1091 in RU/EN and light/dark, with the
//! member OS names and the native display resolutions the map now labels.

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use lws_gui::egui;
use lws_gui::gui_settings::{Look, Theme};
use lws_gui::ui::{self, Env, UiState};
use lws_gui::view_model::{
    self, Action, Bridge, CliArgs, CoreEndpoint, LocalConfig, Modal, ServiceStart, ViewModel,
    SERVICE_START_TIMEOUT,
};
use protocol::ipc;
use protocol::ipc::gui_frame::Msg;
use protocol::ipc::GuiFrame;

const OUT_DIR: &str = "target/ui-shots";
const SIZES: [[u32; 2]; 3] = [[1920, 1080], [1745, 1091], [1280, 720]];

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
    if args.iter().any(|arg| arg == "--updates") {
        return update_shots(&dir);
    }
    if args.iter().any(|arg| arg == "--metadata") {
        if let Err(error) = std::fs::create_dir_all(&dir) {
            eprintln!("{error}");
            return std::process::ExitCode::FAILURE;
        }
        for (theme, theme_tag) in [(Theme::Light, "light"), (Theme::Dark, "dark")] {
            for (lang_tag, lang) in [("ru", ui_text::Lang::Ru), ("en", ui_text::Lang::En)] {
                let file = dir.join(format!("metadata-{lang_tag}-{theme_tag}.png"));
                if let Err(error) = shoot(mockup(theme, None, lang), &file, [1745, 1091], false) {
                    eprintln!("{error}");
                    return std::process::ExitCode::FAILURE;
                }
                println!("{}", file.display());
            }
        }
        return std::process::ExitCode::SUCCESS;
    }
    if args.iter().any(|arg| arg == "--gap-links") {
        return gap_link_shots(&dir, option("--prefix").as_deref().unwrap_or("gap-links"));
    }
    if args.iter().any(|arg| arg == "--placement") {
        return placement_shots(&dir, option("--prefix").as_deref().unwrap_or("placement"));
    }
    let real = args.iter().any(|arg| arg == "--live" || arg == "--root");
    let prefix =
        option("--prefix").unwrap_or_else(|| if real { "real" } else { "polish" }.to_owned());
    let lang = match option("--lang").as_deref() {
        Some("en") => ui_text::Lang::En,
        _ => ui_text::Lang::Ru,
    };
    let zoom = option("--zoom")
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(150)
        .clamp(70, 150);
    let ascii = args.iter().any(|arg| arg == "--ascii");
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
            view.look.language = lang;
            view.look.theme = theme;
            if ascii {
                view.look.link_style = lws_gui::gui_settings::LinkStyle::Ascii;
            }
            view.modal = None;
            states.push((format!("{prefix}-main-{tag}"), view.clone()));
            let mut zoomed = view.clone();
            zoomed.look.zoom_percent = zoom;
            states.push((format!("{prefix}-zoom-{tag}"), zoomed));
            if let Some(owner) = with_owner_left_peer(&view) {
                states.push((format!("{prefix}-owner150-{tag}"), owner.clone()));
                let mut owner100 = owner;
                owner100.look.zoom_percent = 100;
                states.push((format!("{prefix}-owner100-{tag}"), owner100));
            }
            states.push((format!("{prefix}-window-{tag}"), with_window(view.clone())));
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
            states.push((format!("{prefix}-main-{tag}"), mockup(theme, None, lang)));
            states.push((
                format!("{prefix}-main-window-{tag}"),
                with_window(mockup(theme, None, lang)),
            ));
            let mut appearance = mockup(theme, None, lang);
            appearance.appearance_open = true;
            states.push((format!("{prefix}-appearance-{tag}"), appearance));
            let mut collapsed = mockup(theme, None, lang);
            collapsed.look.dock_open = false;
            states.push((format!("{prefix}-collapsed-{tag}"), collapsed));
            let mut offline = mockup(theme, None, lang);
            offline.on_disconnected("Служба недоступна — автономный снимок".to_owned());
            states.push((format!("{prefix}-offline-{tag}"), offline));
            // The banner's two other states: a start in flight, and a start that failed.
            let mut starting = mockup(theme, None, lang);
            starting.on_disconnected("Служба недоступна — автономный снимок".to_owned());
            starting.settings.service.start = ServiceStart::Starting {
                since: Instant::now(),
            };
            states.push((format!("{prefix}-offline-starting-{tag}"), starting));
            let mut failed = mockup(theme, None, lang);
            failed.on_disconnected("Служба недоступна — автономный снимок".to_owned());
            failed.settings.service.start = ServiceStart::Failed {
                reason: ui_text::fill_named(
                    lang,
                    ui_text::Key::TheServiceDidNotAnswerInSeconds,
                    &[("seconds", &SERVICE_START_TIMEOUT.as_secs())],
                ),
            };
            states.push((format!("{prefix}-offline-failed-{tag}"), failed));
            for modal in Modal::ALL {
                states.push((
                    format!("{prefix}-modal-{}-{tag}", modal_name(modal)),
                    mockup(theme, Some(modal), lang),
                ));
            }
        }
        for (name, state) in states {
            // The main screen is shot as a live window — the field simulating, the ribbon of
            // «ASCII-изолиния» flowing — and the rest as they are when nothing moves.
            let live = name.ends_with(&format!("main-{tag}"));
            for size in SIZES {
                let file = dir.join(format!("{name}-{}x{}.png", size[0], size[1]));
                match shoot(state.clone(), &file, size, live) {
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
    }
    println!("{written} pictures in {}", dir.display());
    std::process::ExitCode::SUCCESS
}

fn update_shots(dir: &Path) -> std::process::ExitCode {
    use lws_gui::updater::{Release, State};
    if let Err(error) = std::fs::create_dir_all(dir) {
        eprintln!("{error}");
        return std::process::ExitCode::FAILURE;
    }
    let release = Release {
        tag_name: "v0.1.4".into(),
        name: Some("Bifrost 0.1.4".into()),
        body: Some("Improved reliability and keyboard sharing.".into()),
        html_url: lws_gui::updater::RELEASES.into(),
        assets: Vec::new(),
        draft: false,
        prerelease: false,
    };
    for (theme_tag, theme) in [("light", Theme::Light), ("dark", Theme::Dark)] {
        for (lang_tag, lang) in [("ru", ui_text::Lang::Ru), ("en", ui_text::Lang::En)] {
            for (state_tag, state) in [
                ("current", State::Current),
                ("checking", State::Checking),
                ("available", State::Available(release.clone())),
                (
                    "error",
                    State::Error("GitHub: HTTP 403 (rate limit)".into()),
                ),
                ("startup", State::Available(release.clone())),
            ] {
                let mut view = mockup(theme, Some(Modal::Settings), lang);
                view.update = state;
                if state_tag == "startup" {
                    view.modal = None;
                    view.update_notice = true;
                }
                let file = dir.join(format!("update-{state_tag}-{lang_tag}-{theme_tag}.png"));
                if let Err(error) = shoot(view, &file, [1745, 1091], false) {
                    eprintln!("{error}");
                    return std::process::ExitCode::FAILURE;
                }
                println!("{}", file.display());
            }
        }
    }
    std::process::ExitCode::SUCCESS
}

/// Deterministic offscreen alternative to pointer dragging: the same model operations
/// as the map, followed by a mocked core acknowledgment of the actual submitted links.
fn placement_shots(dir: &Path, prefix: &str) -> std::process::ExitCode {
    let render = || -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
        for (lang_tag, lang) in [("ru", ui_text::Lang::Ru), ("en", ui_text::Lang::En)] {
            for (theme_tag, theme) in [("light", Theme::Light), ("dark", Theme::Dark)] {
                let mut vm = mockup(theme, None, lang);
                let mut doc = ipc::TopologyDoc {
                    revision: 15,
                    author_node_id: WORKSTATION.to_owned(),
                    forgotten_node_ids: Vec::new(),
                    members: vec![
                        ipc::MemberTopology {
                            node_id: WORKSTATION.to_owned(),
                            display_name: "WORKSTATION".to_owned(),
                            system_name: "Windows 11".to_owned(),
                            online: true,
                            displays: vec![
                                display("DISPLAY2", 0.0, 240.0, 1920.0, 1080.0, false, true),
                                display("DISPLAY1", 1920.0, 0.0, 1080.0, 1920.0, true, true),
                            ],
                        },
                        ipc::MemberTopology {
                            node_id: LAPTOP.to_owned(),
                            display_name: "LAPTOP".to_owned(),
                            system_name: "CachyOS".to_owned(),
                            online: true,
                            displays: vec![display("eDP-1", 0.0, 0.0, 1745.0, 1091.0, true, true)],
                        },
                    ],
                    links: Vec::new(),
                };
                vm.apply(event(Msg::TopologyDoc(doc.clone())));
                let owner = vm
                    .editor
                    .nodes
                    .iter()
                    .position(|n| n.node_id == *WORKSTATION)
                    .ok_or("missing mock workstation")?;
                let laptop = vm
                    .editor
                    .nodes
                    .iter()
                    .position(|n| n.node_id == *LAPTOP)
                    .ok_or("missing mock laptop")?;
                let scale = vm.editor.scale;
                let origin = lws_gui::map::Pt::new(80.0, 35.0);
                let target =
                    lws_gui::map::Pt::new(origin.x + 3000.0 * scale, origin.y + 1000.0 * scale);
                vm.editor.placement[owner] = origin;
                vm.editor.placement[laptop] = lws_gui::map::Pt::new(target.x + 45.0, target.y);
                vm.editor.baseline.clone_from(&vm.editor.placement);
                let save = |state: &ViewModel, phase: &str| -> Result<(), String> {
                    let file = dir.join(format!(
                        "{prefix}-{phase}-{lang_tag}-{theme_tag}-1745x1091.png"
                    ));
                    shoot(state.clone(), &file, [1745, 1091], false)?;
                    println!("{}", file.display());
                    Ok(())
                };
                save(&vm, "before")?;
                vm.move_node(&LAPTOP, target.x + 2.0, target.y);
                save(&vm, "preview")?;
                vm.local(&Action::DropNode(LAPTOP.to_owned()));
                let sent = vm
                    .editor
                    .save
                    .sent
                    .clone()
                    .ok_or("placement drop did not submit contacts")?;
                if sent.links.is_empty() {
                    return Err("placement drop submitted no concrete contacts".to_owned());
                }
                let endpoint = |end: &config::EndpointConfig| ipc::EndpointView {
                    node_id: if end.display == "eDP-1" {
                        LAPTOP.to_owned()
                    } else {
                        WORKSTATION.to_owned()
                    },
                    display_id: end.display.clone(),
                    edge: format!("{:?}", end.edge).to_uppercase(),
                    segment_start: end.start,
                    segment_end: end.end,
                };
                doc.links = sent
                    .links
                    .iter()
                    .map(|link| ipc::LinkView {
                        id: link.id.clone(),
                        from: Some(endpoint(&link.from)),
                        to: Some(endpoint(&link.to)),
                        bidirectional: link.bidirectional,
                        active: true,
                        inactive_reason: String::new(),
                        auto_displays: Vec::new(),
                    })
                    .collect();
                vm.apply(GuiFrame {
                    request_id: sent.request_id,
                    ..event(Msg::OpResult(ipc::OpResult {
                        ok: true,
                        ..Default::default()
                    }))
                });
                doc.revision += 1;
                vm.apply(event(Msg::TopologyDoc(doc)));
                save(&vm, "accepted")?;
            }
        }
        Ok(())
    };
    match render() {
        Ok(()) => {
            println!("12 pictures in {}", dir.display());
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("cannot render placement sequence: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn gap_link_shots(dir: &Path, prefix: &str) -> std::process::ExitCode {
    let render = || -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
        for (lang_tag, lang) in [("ru", ui_text::Lang::Ru), ("en", ui_text::Lang::En)] {
            for (theme_tag, theme) in [("light", Theme::Light), ("dark", Theme::Dark)] {
                for phase in ["owner", "dragged", "vertical"] {
                    let vertical = phase == "vertical";
                    let mut vm = mockup(theme, None, lang);
                    vm.look.link_style = lws_gui::gui_settings::LinkStyle::Ascii;
                    vm.look.zoom_percent = 100;
                    vm.pairing.prompt = None;
                    vm.pairing.details = false;
                    if let Some(status) = vm.status.as_mut() {
                        status.members.retain(|member| member.node_id != *HTPC);
                        if let Some(pc) = status
                            .members
                            .iter_mut()
                            .find(|member| member.node_id == *WORKSTATION)
                        {
                            pc.display_name = "PC".to_owned();
                        }
                    }
                    vm.look.pan_x = 0;
                    vm.look.pan_y = 0;
                    vm.apply(event(Msg::TopologyDoc(ipc::TopologyDoc {
                        revision: 16,
                        author_node_id: WORKSTATION.to_owned(),
                        forgotten_node_ids: Vec::new(),
                        members: vec![
                            ipc::MemberTopology {
                                node_id: WORKSTATION.to_owned(),
                                display_name: "PC".to_owned(),
                                system_name: "Windows 11".to_owned(),
                                online: true,
                                displays: vec![
                                    display("01", 0.0, 420.0, 1920.0, 1080.0, false, true),
                                    display("02", 1920.0, 0.0, 1080.0, 1920.0, true, true),
                                ],
                            },
                            ipc::MemberTopology {
                                node_id: LAPTOP.to_owned(),
                                display_name: "LAPTOP".to_owned(),
                                system_name: "CachyOS".to_owned(),
                                online: true,
                                displays: vec![display("03", 0.0, 0.0, 1745.0, 1091.0, true, true)],
                            },
                        ],
                        links: vec![ipc::LinkView {
                            id: "owner-gap".to_owned(),
                            from: Some(ipc::EndpointView {
                                node_id: WORKSTATION.to_owned(),
                                display_id: "02".to_owned(),
                                edge: if vertical { "BOTTOM" } else { "RIGHT" }.to_owned(),
                                segment_start: if vertical { 0.0 } else { 414.5 },
                                segment_end: if vertical { 1080.0 } else { 1505.5 },
                            }),
                            to: Some(ipc::EndpointView {
                                node_id: LAPTOP.to_owned(),
                                display_id: "03".to_owned(),
                                edge: if vertical { "TOP" } else { "LEFT" }.to_owned(),
                                segment_start: 0.0,
                                segment_end: if vertical { 1080.0 } else { 1091.0 },
                            }),
                            bidirectional: true,
                            active: true,
                            inactive_reason: String::new(),
                            auto_displays: Vec::new(),
                        }],
                    })));
                    let owner = vm
                        .editor
                        .nodes
                        .iter()
                        .position(|n| n.node_id == *WORKSTATION)
                        .ok_or("missing gap owner")?;
                    let laptop = vm
                        .editor
                        .nodes
                        .iter()
                        .position(|n| n.node_id == *LAPTOP)
                        .ok_or("missing gap laptop")?;
                    let scale = vm.editor.scale;
                    let width = if vertical {
                        3665.0 * scale
                    } else {
                        4745.0 * scale + 72.0
                    };
                    let height = if vertical {
                        3011.0 * scale + 72.0
                    } else {
                        1920.0 * scale
                    };
                    let origin = lws_gui::map::Pt::new(
                        (lws_gui::map::STAGE_W - width) / 2.0,
                        (lws_gui::map::STAGE_H - height) / 2.0,
                    );
                    vm.editor.placement[owner] = origin;
                    vm.editor.placement[laptop] = if vertical {
                        lws_gui::map::Pt::new(
                            origin.x + 1920.0 * scale,
                            origin.y + 1920.0 * scale + 72.0,
                        )
                    } else {
                        lws_gui::map::Pt::new(
                            origin.x + 3000.0 * scale + 72.0,
                            origin.y + 414.5 * scale,
                        )
                    };
                    vm.editor.baseline.clone_from(&vm.editor.placement);
                    let drag = if phase == "dragged" {
                        let span = (
                            (414.5 + 150.0) * scale + origin.y,
                            (1505.5 - 150.0) * scale + origin.y,
                        );
                        vm.local(&Action::MovePaddle {
                            link_id: "owner-gap".to_owned(),
                            from: true,
                            start: span.0,
                            end: span.1,
                        });
                        Some(span)
                    } else {
                        None
                    };
                    let file = dir.join(format!(
                        "{prefix}-{phase}-{lang_tag}-{theme_tag}-1745x1091.png"
                    ));
                    shoot_gap(vm, &file, drag)?;
                    println!("{}", file.display());
                }
            }
        }
        Ok(())
    };
    match render() {
        Ok(()) => {
            println!("12 pictures in {}", dir.display());
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("cannot render gap links: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn shoot_gap(state: ViewModel, file: &Path, drag: Option<(f64, f64)>) -> Result<(), String> {
    shoot_with_paddle(state, file, [1745, 1091], false, drag)
}

fn real_snapshot(args: CliArgs) -> Result<ViewModel, String> {
    let (view, client) = view_model::boot(args);
    let mut bridge = Bridge::new(view, client);
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut history_requested = false;
    loop {
        bridge.pump();
        if !history_requested && bridge.view().connection.is_connected() {
            bridge.dispatch(Action::RefreshClipboardHistory, Instant::now());
            history_requested = true;
        }
        let view = bridge.view();
        if view.connection.is_connected()
            && view.status.is_some()
            && view.editor.has_document()
            && view.inflight.is_empty()
        {
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
        if Instant::now() >= deadline {
            return Err(format!(
                "no complete status/topology/history snapshot: {:?}",
                bridge.view().connection
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn modal_name(modal: Modal) -> &'static str {
    match modal {
        Modal::Pairs => "pairs",
        Modal::Keys => "keys",
        Modal::Clip => "clip",
        Modal::Diag => "diag",
        Modal::Settings => "settings",
    }
}

fn shoot(state: ViewModel, file: &Path, size: [u32; 2], live: bool) -> Result<(), String> {
    shoot_with_paddle(state, file, size, live, None)
}

fn shoot_with_paddle(
    state: ViewModel,
    file: &Path,
    size: [u32; 2],
    live: bool,
    paddle: Option<(f64, f64)>,
) -> Result<(), String> {
    let drawn = AssertUnwindSafe(|| {
        let mut ui_state = UiState::new(Instant::now());
        let mut harness = Harness::builder()
            .with_size(egui::vec2(size[0] as f32, size[1] as f32))
            .wgpu()
            .build_ui(move |ui| {
                let env = Env {
                    now: Instant::now(),
                    animate: live,
                };
                if let Some(span) = paddle {
                    if ui.input(|input| input.pointer.primary_down()) {
                        ui_state.paddle_drag = Some(ui::PaddleDrag {
                            link_id: "owner-gap".to_owned(),
                            from: true,
                            span,
                            grab: egui::pos2(900.0, 450.0),
                            handle: ui::PaddleHandle::Move,
                        });
                    }
                }
                let _ = ui::draw(ui, &state, &mut ui_state, env);
            });
        ui::install_fonts(&harness.ctx);
        if paddle.is_some() {
            // Hold only the harness pointer: the actual map paints its dragged accent.
            let pos = egui::pos2(900.0, 450.0);
            harness.event(egui::Event::WindowFocused(true));
            harness.event(egui::Event::PointerMoved(pos));
            harness.event(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            });
        }
        if live {
            // A window showing a live field has the pointer over the map: that is what lets the
            // field simulate, and «ASCII-изолиния» draws its ribbon of particles only then.
            harness.event(egui::Event::PointerMoved(egui::pos2(
                size[0] as f32 * 0.3,
                size[1] as f32 * 0.45,
            )));
        }
        if live {
            // The field eases along the wall clock: the ribbon needs a few seconds of it to be
            // the arc it draws rather than the marks still leaving their places.
            for _ in 0..30 {
                std::thread::sleep(Duration::from_millis(100));
                harness.run_steps(2);
            }
        }
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

/// One monitor of a mockup.
///
/// The laptop is the owner's real CachyOS panel: a 1920×1200 mode under KDE's 110 % scale,
/// so the compositor hands the map the 1745×1091 logical rectangle it draws.
fn display(
    id: &str,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    primary: bool,
    known: bool,
) -> ipc::DisplayView {
    let (native_width, native_height) = if id == "eDP-1" {
        (1920.0, 1200.0)
    } else {
        (w, h)
    };
    ipc::DisplayView {
        id: id.to_owned(),
        native_width: if known { native_width as u32 } else { 0 },
        native_height: if known { native_height as u32 } else { 0 },
        logical_x: x,
        logical_y: y,
        logical_w: w,
        logical_h: h,
        rotation_degrees: 0,
        primary,
        native_known: known,
        scale: if known {
            native_width / w.max(1.0)
        } else {
            1.0
        },
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
fn mockup(theme: Theme, modal: Option<Modal>, lang: ui_text::Lang) -> ViewModel {
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
        language: lang,
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
        hotkeys: lws_gui::view_model::hotkeys::ROWS
            .iter()
            .map(|(action, _, chord)| ipc::HotkeyBinding {
                action: *action,
                chord: (*chord).to_owned(),
                available: true,
                detail: String::new(),
            })
            .collect(),
        backend: "platform-win".to_owned(),
        // `[appearance]` as `config::AppearanceConfig` defaults it: both effects on.
        edge_effects: true,
        hide_remote_cursor: true,
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
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    vm.apply(event(Msg::ClipboardHistory(ipc::ClipboardHistory {
        items: vec![
            ipc::ClipboardHistoryEntry {
                id: 3,
                direction: ipc::ClipboardHistoryDirection::Received as i32,
                peer_name: "LAPTOP".to_owned(),
                time_unix_ms: now_ms.saturating_sub(12_000),
                size_bytes: 86,
                preview: "Release notes — готово ✓".to_owned(),
            },
            ipc::ClipboardHistoryEntry {
                id: 2,
                direction: ipc::ClipboardHistoryDirection::Sent as i32,
                peer_name: "LAPTOP".to_owned(),
                time_unix_ms: now_ms.saturating_sub(75_000),
                size_bytes: 1_048_576,
                preview: "SELECT name, display_id FROM workspace_displays;".to_owned(),
            },
            ipc::ClipboardHistoryEntry {
                id: 1,
                direction: ipc::ClipboardHistoryDirection::Received as i32,
                peer_name: "HTPC".to_owned(),
                time_unix_ms: now_ms.saturating_sub(240_000),
                size_bytes: 43,
                preview: "https://example.org/meeting/notes".to_owned(),
            },
        ],
    })));
    vm.apply(event(Msg::TopologyDoc(ipc::TopologyDoc {
        revision: 14,
        author_node_id: WORKSTATION.to_owned(),
        forgotten_node_ids: Vec::new(),
        members: vec![
            ipc::MemberTopology {
                node_id: LAPTOP.to_owned(),
                display_name: "LAPTOP".to_owned(),
                system_name: "CachyOS".to_owned(),
                online: true,
                displays: vec![display("eDP-1", 0.0, 0.0, 1745.0, 1091.0, true, true)],
            },
            ipc::MemberTopology {
                node_id: WORKSTATION.to_owned(),
                display_name: "WORKSTATION".to_owned(),
                system_name: "Windows 11".to_owned(),
                online: true,
                displays: vec![
                    display("DISPLAY2", 0.0, 180.0, 1920.0, 1080.0, false, true),
                    display("DISPLAY1", 1920.0, 0.0, 2560.0, 1440.0, true, true),
                ],
            },
            ipc::MemberTopology {
                node_id: HTPC.to_owned(),
                display_name: "HTPC".to_owned(),
                system_name: "Ubuntu 24.04".to_owned(),
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
    vm.pairing.details = true;
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
        native_width: display.native_w as u32,
        native_height: display.native_h as u32,
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
        forgotten_node_ids: view.editor.forgotten_nodes.clone(),
        members: view
            .editor
            .nodes
            .iter()
            .map(|node| ipc::MemberTopology {
                system_name: String::new(),
                node_id: node.node_id.clone(),
                display_name: node.display_name.clone(),
                online: node.online,
                displays: node.displays.iter().map(wire).collect(),
            })
            .collect(),
        links: view.editor.links.clone(),
    };
    document.members.push(ipc::MemberTopology {
        system_name: String::new(),
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

/// The owner's reference arrangement, rendered from the real reported displays only.
/// This is an offscreen copy: it never applies a placement to the service.
fn with_owner_left_peer(view: &ViewModel) -> Option<ViewModel> {
    let pc = view
        .editor
        .nodes
        .iter()
        .find(|node| node.displays.len() == 2)?;
    let laptop = view
        .editor
        .nodes
        .iter()
        .find(|node| node.node_id != pc.node_id && node.displays.len() == 1)?;
    let document = ipc::TopologyDoc {
        revision: view.editor.revision,
        author_node_id: view.editor.author.clone(),
        forgotten_node_ids: view.editor.forgotten_nodes.clone(),
        members: view
            .editor
            .nodes
            .iter()
            .map(|node| ipc::MemberTopology {
                system_name: String::new(),
                node_id: node.node_id.clone(),
                display_name: node.display_name.clone(),
                online: node.online,
                displays: node
                    .displays
                    .iter()
                    .map(|display| ipc::DisplayView {
                        id: display.id.clone(),
                        native_width: display.native_w as u32,
                        native_height: display.native_h as u32,
                        logical_x: display.x,
                        logical_y: display.y,
                        logical_w: display.w,
                        logical_h: display.h,
                        rotation_degrees: display.rotation,
                        primary: display.primary,
                        native_known: display.native_known,
                        scale: 1.0,
                    })
                    .collect(),
            })
            .collect(),
        links: vec![ipc::LinkView {
            id: "owner-left-reference".to_owned(),
            from: auto(&pc.node_id, "LEFT"),
            to: auto(&laptop.node_id, "RIGHT"),
            bidirectional: true,
            active: true,
            inactive_reason: String::new(),
            auto_displays: Vec::new(),
        }],
    };
    let mut owner = view.clone();
    owner.apply(event(Msg::TopologyDoc(document)));
    owner.look.zoom_percent = 150;
    owner.look.pan_x = 0;
    owner.look.pan_y = 0;
    Some(owner)
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
