//! The window of the mockup, drawn with no display and no GPU.
//!
//! `egui::Context::run_ui` lays the widgets out and returns the shapes, so these tests read the
//! text the window actually emitted, click where a button was drawn, and check the repaint
//! contract of the build spec: an idle window schedules no frame at all.

mod support;

use std::time::{Duration, Instant};

use lws_gui::egui::{self, Shape};
use lws_gui::gui_settings::{FieldMode, Look, Theme};
use lws_gui::ui::{self, Env, Output, UiState};
use lws_gui::view_model::{Action, Modal, ViewModel, SERVICE_START_TIMEOUT};
use protocol::ipc;
use protocol::ipc::gui_frame::Msg;
use support::{event, pairing_prompt, status_with, two_node_doc, view, PEER_NODE, SELF_NODE};

const SIZE: egui::Vec2 = egui::vec2(1280.0, 800.0);

/// One frame of the window: the text it painted (with the tight box around each glyph's ink, in
/// the frame's own coordinates), the rectangles it filled, and what it produced.
struct Frame {
    texts: Vec<(String, egui::Rect)>,
    rects: Vec<egui::Rect>,
    output: Output,
}

struct Harness {
    ctx: egui::Context,
    state: UiState,
    now: Instant,
}

impl Harness {
    fn new() -> Self {
        let ctx = egui::Context::default();
        ui::install_fonts(&ctx);
        Harness {
            ctx,
            state: UiState::new(Instant::now()),
            now: Instant::now(),
        }
    }

    fn frame(
        &mut self,
        vm: &ViewModel,
        animate: bool,
        size: egui::Vec2,
        events: Vec<egui::Event>,
    ) -> Frame {
        let mut input = egui::RawInput::default();
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        input.screen_rect = Some(rect);
        input.viewports.insert(
            egui::ViewportId::ROOT,
            egui::ViewportInfo {
                inner_rect: Some(rect),
                ..Default::default()
            },
        );
        input.events = events;
        let env = Env {
            now: self.now,
            animate,
        };
        let mut produced = Output::default();
        let state = &mut self.state;
        let output = self.ctx.run_ui(input, |ui| {
            produced = ui::draw(ui, vm, state, env);
        });
        let mut frame = Frame {
            texts: Vec::new(),
            rects: Vec::new(),
            output: produced,
        };
        for clipped in &output.shapes {
            collect(&clipped.shape, &mut frame);
        }
        assert!(
            output
                .viewport_output
                .values()
                .all(|viewport| viewport.commands.iter().all(|command| !matches!(
                    command,
                    egui::ViewportCommand::Close
                        | egui::ViewportCommand::Minimized(true)
                        | egui::ViewportCommand::Visible(false)
                ))),
            "drawing widgets must not hide or close the native window"
        );
        output.drop_without_applying_deltas();
        frame
    }

    /// Two frames: the first one sizes the panels and the font atlas, the second is the window.
    fn settle(&mut self, vm: &ViewModel, animate: bool) -> Frame {
        self.frame(vm, animate, SIZE, Vec::new());
        self.frame(vm, animate, SIZE, Vec::new())
    }
}

fn collect(shape: &Shape, frame: &mut Frame) {
    match shape {
        Shape::Text(text) => frame.texts.push((
            text.galley.text().to_owned(),
            text.galley.mesh_bounds.translate(text.pos.to_vec2()),
        )),
        Shape::Rect(rect) => frame.rects.push(rect.rect),
        Shape::Vec(shapes) => shapes.iter().for_each(|shape| collect(shape, frame)),
        _ => {}
    }
}

fn shows(frame: &Frame, needle: &str) -> bool {
    frame.texts.iter().any(|(text, _)| text.contains(needle))
}

fn expect(frame: &Frame, needles: &[&str]) {
    for needle in needles {
        assert!(
            shows(frame, needle),
            "missing {needle:?} in {:?}",
            frame.texts
        );
    }
}

/// A connected window with two computers, the layout document and an owner's status.
fn connected() -> ViewModel {
    let mut vm = view();
    vm.on_connected("endpoint");
    vm.config.listen_address = "192.168.1.24".to_owned();
    vm.apply(event(Msg::Status(status_with("LOCAL"))));
    vm.apply(event(Msg::TopologyDoc(two_node_doc(14))));
    vm
}

#[test]
fn the_main_screen_shows_the_top_bar_the_map_the_dock_and_the_rail() {
    let vm = connected();
    let mut harness = Harness::new();
    let frame = harness.settle(&vm, false);
    expect(
        &frame,
        &[
            "MULTIDESKFLOW",
            "Служба работает",
            "192.168.1.24",
            "этот компьютер",
            "01 — КАРТА МОНИТОРОВ",
            "2 компьютера · 2 экрана",
            "Workstation",
            "linux-box",
            "1920×1080",
            "ввод здесь",
            "в сети",
            "ЭТОТ КОМПЬЮТЕР",
            "ОКНО СОПРЯЖЕНИЯ",
            "ПОДКЛЮЧИТЬСЯ ПО АДРЕСУ",
            "КОД СОПРЯЖЕНИЯ",
            "Открыть окно сопряжения",
            "Подключиться",
            "Код появится после подключения",
        ],
    );
    assert!(shows(&frame, ":47901"), "the port the other computer dials");
    for removed in [
        "Связь краёв",
        "Сюда идёт ввод",
        "Размер приблизительный",
        "Тяните компьютер — края примагнитятся",
        "Ввод ",
        "осн.",
        "← ввод",
        "ввод →",
    ] {
        assert!(
            !frame.texts.iter().any(|(text, _)| text == removed),
            "{removed} must not be drawn"
        );
    }
}

#[test]
fn a_link_whose_display_is_missing_is_named_on_the_map() {
    // Nothing is said while every link names a display that was reported.
    let mut harness = Harness::new();
    let frame = harness.settle(&connected(), false);
    assert!(
        !shows(&frame, "неактивна"),
        "a usable link earns no notice: {:?}",
        frame.texts
    );

    let end = |node: &str, display: &str, edge: &str| {
        Some(ipc::EndpointView {
            node_id: node.to_owned(),
            display_id: display.to_owned(),
            edge: edge.to_owned(),
            segment_start: 100.0,
            segment_end: 500.0,
        })
    };
    let mut vm = connected();
    let mut doc = two_node_doc(15);
    doc.links = vec![ipc::LinkView {
        id: "contact".to_owned(),
        from: end(SELF_NODE, "\\\\.\\DISPLAY1", "RIGHT"),
        to: end(PEER_NODE, "eDP-1", "LEFT"),
        bidirectional: true,
        active: false,
        inactive_reason: "eDP-1".to_owned(),
        auto_displays: Vec::new(),
    }];
    vm.apply(event(Msg::TopologyDoc(doc)));

    let frame = harness.settle(&vm, false);
    expect(
        &frame,
        &["Связь contact неактивна: экран «eDP-1» не найден"],
    );
}

#[test]
fn collapsed_pairing_keeps_only_its_clear_expand_handle() {
    let mut vm = connected();
    vm.look.dock_open = false;
    let mut harness = Harness::new();
    let frame = harness.settle(&vm, false);
    expect(&frame, &["Сопряжение"]);
    assert!(!shows(&frame, "Открыть окно сопряжения"));
    assert!(!shows(&frame, "Сопряжение  ?"));
    let at = egui::pos2((SIZE.x - 64.0) / 2.0, SIZE.y - 16.0);
    let frame = harness.frame(&vm, false, SIZE, click_at(at));
    assert!(frame.output.actions.contains(&Action::ToggleDock));
}

#[test]
fn opening_pairing_from_modal_keeps_root_viewport_and_expands_dock() {
    let mut vm = connected();
    vm.look.dock_open = false;
    vm.local(&Action::ToggleModal(Modal::Pairs));
    let mut harness = Harness::new();
    harness.settle(&vm, false);
    harness.now += Duration::from_secs(1);
    harness.settle(&vm, false);
    let at = text_position(&mut harness, &vm, "Открыть окно сопряжения");
    let frame = harness.frame(&vm, false, SIZE, click_at(at));
    assert!(frame
        .output
        .actions
        .contains(&Action::StartPairing { window_s: 120 }));
    for action in frame.output.actions {
        vm.local(&action);
    }
    assert!(vm.look.dock_open);
    assert_eq!(vm.modal, None);
    for _ in 0..20 {
        harness.now += Duration::from_millis(16);
        let frame = harness.frame(&vm, false, SIZE, Vec::new());
        assert!(!frame.texts.is_empty());
    }
}

#[test]
fn both_themes_draw_the_same_window() {
    let mut vm = connected();
    for theme in [Theme::Light, Theme::Dark] {
        vm.look = Look {
            theme,
            ..Look::default()
        };
        let mut harness = Harness::new();
        let frame = harness.settle(&vm, false);
        expect(&frame, &["MULTIDESKFLOW", "2 компьютера · 2 экрана"]);
    }
}

#[test]
fn a_window_with_no_service_says_so_and_offers_to_start_it() {
    let mut vm = view();
    vm.on_disconnected("no core".to_owned());
    let mut harness = Harness::new();
    let frame = harness.settle(&vm, false);
    expect(
        &frame,
        &["Служба не запущена", "Запустить", "Нет данных от службы"],
    );
    assert!(!shows(&frame, "Служба работает"));
}

/// How many of the service banner's buttons a frame drew: there is only ever one.
fn banner_buttons(frame: &Frame) -> usize {
    frame
        .texts
        .iter()
        .filter(|(drawn, _)| matches!(drawn.as_str(), "Запустить" | "Ещё раз"))
        .count()
}

/// One button in every state of the banner: «Запустить» and off while a start is in flight,
/// «Ещё раз» with the reason once a start has failed.
#[test]
fn the_service_banner_offers_one_button_whatever_it_says() {
    let mut vm = view();
    vm.on_disconnected("no core".to_owned());
    let mut harness = Harness::new();
    let frame = harness.settle(&vm, false);
    expect(&frame, &["Служба не запущена", "Запустить"]);
    assert_eq!(banner_buttons(&frame), 1, "{:?}", frame.texts);
    let at = text_position(&mut harness, &vm, "Запустить");
    let frame = harness.frame(&vm, false, SIZE, click_at(at));
    assert!(
        frame.output.actions.contains(&Action::StartCore),
        "{:?}",
        frame.output.actions
    );

    // A start in flight: the same button, drawn disabled, and no second one beside it.
    let now = Instant::now();
    let mut waiting = vm.clone();
    waiting.begin_service_start(now);
    let mut harness = Harness::new();
    let frame = harness.settle(&waiting, false);
    expect(&frame, &["Служба не запущена", "Запустить"]);
    assert_eq!(banner_buttons(&frame), 1, "{:?}", frame.texts);
    let at = text_position(&mut harness, &waiting, "Запустить");
    let frame = harness.frame(&waiting, false, SIZE, click_at(at));
    assert!(
        !frame.output.actions.contains(&Action::StartCore),
        "the button is off while a start is in flight: {:?}",
        frame.output.actions
    );

    // The start failed: the reason is on screen and the button asks for another try.
    let mut failed = waiting.clone();
    failed.tick(now + SERVICE_START_TIMEOUT);
    let mut harness = Harness::new();
    let frame = harness.settle(&failed, false);
    expect(
        &frame,
        &[
            "Служба не запущена",
            "Ещё раз",
            "Служба не ответила за 15 с",
        ],
    );
    assert_eq!(banner_buttons(&frame), 1, "{:?}", frame.texts);
    let at = text_position(&mut harness, &failed, "Ещё раз");
    let frame = harness.frame(&failed, false, SIZE, click_at(at));
    assert!(
        frame.output.actions.contains(&Action::StartCore),
        "{:?}",
        frame.output.actions
    );
}

#[test]
fn every_modal_shows_its_own_real_data() {
    let mut vm = connected();
    vm.settings.lan.candidates = vec!["192.168.1.24".to_owned(), "10.0.0.5".to_owned()];
    let cases: [(Modal, &[&str]); 5] = [
        (
            Modal::Pairs,
            &["Пары", "linux-box", "BB22·BB22", "в сети", "Удалить"],
        ),
        (
            Modal::Keys,
            // The emergency chord is the one `Status` carries, key by key.
            &[
                "Клавиши",
                "АВАРИЙНАЯ",
                "F12",
                "Shift",
                "Открыть файл настроек",
            ],
        ),
        (Modal::Clip, &["Буфер обмена", "Включён", "Только текст"]),
        (
            Modal::Diag,
            &[
                "Диагностика",
                "Сохранить диагностику",
                "Отчёт ещё не сохранялся",
            ],
        ),
        (
            Modal::Settings,
            &["Настройки", "АДРЕС В LAN", "Перезапустить", "Остановить"],
        ),
    ];
    for (modal, needles) in cases {
        vm.local(&Action::CloseModal);
        vm.local(&Action::ToggleModal(modal));
        let mut harness = Harness::new();
        // The reveal takes 0.32 s of the window's clock; step past it.
        let size = egui::vec2(1280.0, 720.0);
        harness.frame(&vm, false, size, Vec::new());
        harness.now += Duration::from_millis(500);
        harness.frame(&vm, false, size, Vec::new());
        let frame = harness.frame(&vm, false, size, Vec::new());
        expect(&frame, needles);
        if modal == Modal::Keys {
            for absent in [
                "ПЕРЕКЛЮЧЕНИЕ",
                "ВЗЯТЬ УПРАВЛЕНИЕ",
                "F11",
                "ИСТОЧНИК",
                "config.toml · секци",
            ] {
                assert!(!shows(&frame, absent), "unexpected {absent:?}");
            }
        }
    }
}

#[test]
fn the_pairing_dock_shows_the_real_window_countdown_and_the_real_code() {
    let mut vm = connected();
    vm.apply(event(Msg::PairingWindow(protocol::ipc::PairingWindow {
        open: true,
        address: "192.168.1.24:47901".to_owned(),
        seconds_remaining: 90,
        attempt_id: support::attempt_id(),
    })));
    let mut harness = Harness::new();
    let frame = harness.settle(&vm, false);
    expect(
        &frame,
        &[
            "192.168.1.24:47901",
            "Ждём, пока другой компьютер подключится…",
            "Закрыть окно · скоро",
        ],
    );
    assert!(
        frame
            .texts
            .iter()
            .any(|(t, _)| t.starts_with('[') || t.contains("] ")),
        "the bar: {:?}",
        frame.texts
    );
    assert!(frame
        .texts
        .iter()
        .any(|(t, _)| t.contains(" с") && t.contains(']')));

    vm.apply(event(Msg::PairingPrompt(pairing_prompt())));
    vm.local(&Action::ToggleDetails(true));
    let frame = harness.settle(&vm, false);
    expect(
        &frame,
        &[
            "подключается — коды совпадают?",
            "4",
            "8",
            "2",
            "9",
            "1",
            "3",
            "Да, совпадают",
            "Нет",
            "Свернуть",
            "D4:E5:F6",
            "A1:B2:C3",
        ],
    );
}

/// The resource contract: what the window asks for when nothing happens.
#[test]
fn an_idle_window_asks_for_no_frame_and_an_animated_one_for_thirty_a_second() {
    let mut vm = connected();
    let mut harness = Harness::new();

    assert_eq!(vm.look.field, FieldMode::Attract);
    let frame = harness.settle(&vm, true);
    assert_eq!(
        frame.output.repaint_after, None,
        "an idle window must not repaint on a timer"
    );

    // The field moves: 30 fps, and not faster.
    vm.look.field = FieldMode::Repel;
    let frame = harness.frame(
        &vm,
        true,
        SIZE,
        vec![egui::Event::PointerMoved(egui::pos2(400.0, 300.0))],
    );
    assert_eq!(frame.output.repaint_after, Some(ui::FIELD_FRAME));
    assert!(ui::FIELD_FRAME >= Duration::from_millis(33));

    // The pointer stays where it is: the particles keep the state it put them in — five
    // seconds of a still pointer is not a reset — and the frames stop once they have settled.
    let mut frames = 0;
    while harness
        .frame(&vm, true, SIZE, Vec::new())
        .output
        .repaint_after
        .is_some()
    {
        harness.now += ui::FIELD_FRAME;
        frames += 1;
        assert!(frames < 200, "the field never settles");
    }
    assert!(frames > 1, "a pointer on the map moves the field");
    assert!(
        harness.state.field.displacement() > 20.0,
        "the particles hold the state the pointer put them in: {}",
        harness.state.field.displacement()
    );
    // A frame that some other event causes draws the same settled state and asks for nothing.
    harness.now += ui::FIELD_FRAME;
    let frame = harness.frame(&vm, true, SIZE, Vec::new());
    assert_eq!(frame.output.repaint_after, None);
    assert!(harness.state.field.displacement() > 20.0);

    // The window is hidden, minimised or unfocused: no animation frame at all.
    let frame = harness.settle(&vm, false);
    assert_eq!(frame.output.repaint_after, None);

    // A modal covers the map: the field behind it does not animate either.
    vm.local(&Action::ToggleModal(Modal::Keys));
    let frame = harness.settle(&vm, true);
    assert_eq!(frame.output.repaint_after, None);
    assert!(
        harness.state.field.displacement() > 20.0,
        "a covered map freezes"
    );
}

#[test]
fn an_open_pairing_window_ticks_once_a_second_and_nothing_faster() {
    let mut vm = connected();
    vm.apply(event(Msg::PairingWindow(protocol::ipc::PairingWindow {
        open: true,
        address: "192.168.1.24:47901".to_owned(),
        seconds_remaining: 120,
        attempt_id: support::attempt_id(),
    })));
    let mut harness = Harness::new();
    let frame = harness.settle(&vm, true);
    assert_eq!(frame.output.repaint_after, Some(Duration::from_secs(1)));
    // With the field moving too, the faster of the two wins — never a sum of both.
    vm.look.field = FieldMode::Attract;
    let frame = harness.frame(
        &vm,
        true,
        SIZE,
        vec![egui::Event::PointerMoved(egui::pos2(400.0, 300.0))],
    );
    assert_eq!(frame.output.repaint_after, Some(ui::FIELD_FRAME));
}

fn click_at(pos: egui::Pos2) -> Vec<egui::Event> {
    let button = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    vec![egui::Event::PointerMoved(pos), button(true), button(false)]
}

#[test]
fn clicking_the_rail_opens_a_section_and_the_theme_button_asks_for_the_other_theme() {
    let vm = connected();
    let mut harness = Harness::new();
    harness.settle(&vm, false);

    // The rail: 64 px at the right edge, the first button's centre is 12 + 22 below the bar.
    let frame = harness.frame(
        &vm,
        false,
        SIZE,
        click_at(egui::pos2(SIZE.x - 32.0, 60.0 + 12.0 + 22.0)),
    );
    assert!(
        frame
            .output
            .actions
            .contains(&Action::ToggleModal(Modal::Pairs)),
        "{:?}",
        frame.output.actions
    );

    // Appearance is the sixth slot of that same column, right under the five sections.
    let at = egui::pos2(SIZE.x - 32.0, 60.0 + 12.0 + 5.0 * 50.0 + 22.0);
    let frame = harness.frame(&vm, false, SIZE, click_at(at));
    assert!(frame.output.actions.contains(&Action::ToggleAppearance));
    // Nothing sits at the bottom of the rail any more.
    let frame = harness.frame(
        &vm,
        false,
        SIZE,
        click_at(egui::pos2(SIZE.x - 32.0, SIZE.y - 40.0)),
    );
    assert!(
        frame.output.actions.is_empty(),
        "{:?}",
        frame.output.actions
    );
}

#[test]
fn the_dial_button_sends_the_typed_address_and_map_commands_are_absent() {
    let mut vm = connected();
    vm.pairing.address = "192.168.1.10:47901".to_owned();
    let mut harness = Harness::new();
    harness.settle(&vm, false);
    // Buttons are found by what they drew: click the centre of the text of each.
    let find = |harness: &mut Harness, vm: &ViewModel, label: &str| -> egui::Pos2 {
        let mut input = egui::RawInput::default();
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, SIZE);
        input.screen_rect = Some(rect);
        let env = Env {
            now: harness.now,
            animate: false,
        };
        let state = &mut harness.state;
        let mut found = None;
        let output = harness.ctx.run_ui(input, |ui| {
            let _ = ui::draw(ui, vm, state, env);
        });
        fn walk(shape: &Shape, label: &str, found: &mut Option<egui::Pos2>) {
            match shape {
                Shape::Text(text) if text.galley.text() == label => {
                    *found = Some(text.pos + text.galley.size() / 2.0);
                }
                Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, label, found)),
                _ => {}
            }
        }
        for clipped in &output.shapes {
            walk(&clipped.shape, label, &mut found);
        }
        output.drop_without_applying_deltas();
        found.unwrap_or_else(|| panic!("{label} was not drawn"))
    };

    let at = find(&mut harness, &vm, "Подключиться");
    let frame = harness.frame(&vm, false, SIZE, click_at(at));
    assert!(
        frame
            .output
            .actions
            .contains(&Action::PairWith("192.168.1.10:47901".to_owned())),
        "{:?}",
        frame.output.actions
    );

    for removed in ["↔ Передать ввод → linux-box", "Применить", "Авто"]
    {
        assert!(!shows(&frame, removed), "{removed} must not be drawn");
    }
}

#[test]
fn a_narrow_window_folds_the_dock_into_two_rows_and_keeps_every_cell() {
    let vm = connected();
    let mut harness = Harness::new();
    let narrow = egui::vec2(1000.0, 800.0);
    harness.frame(&vm, false, narrow, Vec::new());
    let frame = harness.frame(&vm, false, narrow, Vec::new());
    expect(
        &frame,
        &[
            "ЭТОТ КОМПЬЮТЕР",
            "ОКНО СОПРЯЖЕНИЯ",
            "ПОДКЛЮЧИТЬСЯ ПО АДРЕСУ",
            "КОД СОПРЯЖЕНИЯ",
        ],
    );
}

fn text_position(harness: &mut Harness, vm: &ViewModel, label: &str) -> egui::Pos2 {
    fn find(shape: &Shape, label: &str) -> Option<egui::Pos2> {
        match shape {
            Shape::Text(text) if text.galley.text() == label => {
                Some(text.pos + text.galley.size() / 2.0)
            }
            Shape::Vec(shapes) => shapes.iter().find_map(|shape| find(shape, label)),
            _ => None,
        }
    }
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SIZE)),
        ..Default::default()
    };
    let output = harness.ctx.run_ui(input, |ui| {
        let _ = ui::draw(
            ui,
            vm,
            &mut harness.state,
            Env {
                now: harness.now,
                animate: false,
            },
        );
    });
    let at = output
        .shapes
        .iter()
        .find_map(|shape| find(&shape.shape, label));
    output.drop_without_applying_deltas();
    at.unwrap_or_else(|| panic!("{label:?} was not drawn"))
}

fn key(key: egui::Key) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    }
}

fn dry_bridge() -> lws_gui::view_model::Bridge {
    use lws_gui::view_model::{Bridge, GuiClient, Launch};
    // No endpoint: this cannot reach the user's running service or launch a core.
    Bridge::new(
        connected(),
        GuiClient::start(None, Launch::with_flags(Vec::new())),
    )
}

fn dispatch_frame(bridge: &mut lws_gui::view_model::Bridge, frame: Frame, now: Instant) {
    for action in frame.output.actions {
        bridge.dispatch(action, now);
    }
}

fn assert_no_layout_requests(bridge: &lws_gui::view_model::Bridge) {
    use lws_gui::view_model::Request;
    assert!(
        bridge
            .view()
            .inflight
            .iter()
            .all(|request| !matches!(request.request, Request::UpdateTopology)),
        "{:?}",
        bridge.view().inflight
    );
}

#[test]
fn map_drag_frames_focus_loss_modal_close_and_reconnect_never_save() {
    let mut bridge = dry_bridge();
    // Explicitly touch the pair: the right-arrow step must overlap the peer.
    // A keyboard edit is otherwise valid in the new free-placement map, including
    // gaps and disconnected computers.
    let editor = &mut bridge.view_mut().editor;
    let local = editor
        .nodes
        .iter()
        .position(|node| node.node_id == SELF_NODE)
        .unwrap();
    let peer = editor
        .nodes
        .iter()
        .position(|node| node.node_id == PEER_NODE)
        .unwrap();
    editor.placement[local].x = editor.placement[peer].x - editor.shapes[local].w;
    editor.placement[local].y = editor.placement[peer].y;
    editor.baseline.clone_from(&editor.placement);
    let mut harness = Harness::new();
    harness.settle(bridge.view(), false);

    // Real map widget focus and its arrow-key handler, not Action::wire in isolation.
    let monitor = egui::Id::new(("mon", &SELF_NODE.to_owned(), 0_usize));
    harness
        .ctx
        .memory_mut(|memory| memory.request_focus(monitor));
    let frame = harness.frame(bridge.view(), false, SIZE, vec![key(egui::Key::ArrowRight)]);
    assert!(frame
        .output
        .actions
        .iter()
        .any(|action| matches!(action, Action::NudgeNode { .. })));
    dispatch_frame(&mut bridge, frame, harness.now);
    assert!(
        !bridge.view().editor.dirty(),
        "an invalid keyboard drop rolls back"
    );
    assert_no_layout_requests(&bridge);

    // Drag the rendered monitor. Start the drag on one frame and move again on another,
    // because egui records the grab point when the drag threshold is first crossed.
    let at = text_position(&mut harness, bridge.view(), "1920×1080");
    let press = egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::NONE,
    };
    let mut moved = false;
    for events in [
        vec![egui::Event::PointerMoved(at), press],
        vec![egui::Event::PointerMoved(at + egui::vec2(24.0, 0.0))],
        vec![egui::Event::PointerMoved(at + egui::vec2(72.0, 0.0))],
    ] {
        let frame = harness.frame(bridge.view(), false, SIZE, events);
        moved |= frame
            .output
            .actions
            .iter()
            .any(|action| matches!(action, Action::MoveNode { .. }));
        dispatch_frame(&mut bridge, frame, harness.now);
    }
    assert!(moved, "the real map drag must produce a local move");
    let frame = harness.frame(
        bridge.view(),
        false,
        SIZE,
        vec![
            egui::Event::PointerButton {
                pos: at + egui::vec2(72.0, 0.0),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerGone,
            egui::Event::WindowFocused(false),
        ],
    );
    dispatch_frame(&mut bridge, frame, harness.now);
    assert_no_layout_requests(&bridge);

    bridge.dispatch(Action::ToggleModal(Modal::Keys), harness.now);
    harness.settle(bridge.view(), false);
    let frame = harness.frame(bridge.view(), false, SIZE, vec![key(egui::Key::Escape)]);
    assert!(frame.output.actions.contains(&Action::CloseModal));
    dispatch_frame(&mut bridge, frame, harness.now);
    assert_no_layout_requests(&bridge);

    bridge
        .view_mut()
        .on_disconnected("dry reconnect".to_owned());
    bridge.view_mut().on_connected("endpoint");
    bridge
        .view_mut()
        .apply(event(Msg::TopologyDoc(two_node_doc(13))));
    bridge.view_mut().tick(harness.now);
    bridge.pump();
    let frame = harness.settle(bridge.view(), false);
    dispatch_frame(&mut bridge, frame, harness.now);
    assert_no_layout_requests(&bridge);
}

#[test]
fn enter_in_the_dial_field_only_pairs_even_with_a_dirty_layout() {
    use lws_gui::view_model::Request;
    let mut bridge = dry_bridge();
    let address = "192.168.1.10:47901";
    bridge.view_mut().pairing.address = address.to_owned();
    let peer = bridge
        .view()
        .editor
        .nodes
        .iter()
        .position(|node| node.node_id == PEER_NODE)
        .unwrap();
    let at = bridge.view().editor.placement[peer];
    bridge.dispatch(
        Action::MoveNode {
            node_id: PEER_NODE.to_owned(),
            x: at.x + 16.0,
            y: at.y,
        },
        Instant::now(),
    );
    let mut harness = Harness::new();
    harness.settle(bridge.view(), false);
    let at = text_position(&mut harness, bridge.view(), address);
    let frame = harness.frame(bridge.view(), false, SIZE, click_at(at));
    dispatch_frame(&mut bridge, frame, harness.now);
    let frame = harness.frame(bridge.view(), false, SIZE, vec![key(egui::Key::Enter)]);
    assert_eq!(
        frame.output.actions,
        vec![Action::PairWith(address.to_owned())]
    );
    dispatch_frame(&mut bridge, frame, harness.now);
    assert_no_layout_requests(&bridge);
    assert_eq!(bridge.view().inflight.len(), 1);
    assert_eq!(bridge.view().inflight[0].request, Request::PairWith);

    // Closing a dirty window drops only its client; it has no save-on-drop action.
    // Keep the request ledger outside the bridge to make the final state observable.
    let final_requests = bridge.view().inflight.clone();
    drop(bridge);
    assert_eq!(final_requests.len(), 1);
    assert_eq!(final_requests[0].request, Request::PairWith);
}

#[test]
fn map_zoom_buttons_and_wheel_only_change_gui_local_look() {
    let mut vm = connected();
    let mut harness = Harness::new();
    harness.settle(&vm, false);
    let percent = text_position(&mut harness, &vm, "100%");
    let frame = harness.frame(&vm, false, SIZE, click_at(percent + egui::vec2(48.0, 0.0)));
    let wanted = frame
        .output
        .actions
        .iter()
        .find_map(|action| match action {
            Action::SetLook(look) => Some(*look),
            _ => None,
        })
        .expect("the + button changes local zoom");
    assert_eq!(wanted.zoom_percent, 105);
    assert!(frame
        .output
        .actions
        .iter()
        .all(|action| action.wire().is_none()));
    vm.local(&Action::SetLook(wanted));
    let wheel = |y| {
        vec![
            egui::Event::PointerMoved(egui::pos2(250.0, 230.0)),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, y),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    };
    let frame = harness.frame(&vm, false, SIZE, wheel(-32.0));
    assert!(frame.output.actions.contains(&Action::SetLook(Look {
        zoom_percent: 100,
        ..vm.look
    })));
    vm.look.zoom_percent = 150;
    let frame = harness.frame(&vm, false, SIZE, wheel(32.0));
    assert!(!frame
        .output
        .actions
        .iter()
        .any(|action| matches!(action, Action::SetLook(_))));
    vm.modal = Some(Modal::Keys);
    let frame = harness.frame(&vm, false, SIZE, wheel(-32.0));
    assert!(!frame
        .output
        .actions
        .iter()
        .any(|action| matches!(action, Action::SetLook(_))));
}

#[test]
fn removed_layout_controls_stay_absent_and_pairing_close_is_disabled() {
    let mut vm = connected();
    let mut harness = Harness::new();
    let frame = harness.settle(&vm, false);
    for removed in [
        "ПРИМЕНЕНО",
        "Расположить автоматически",
        "Применить",
        "СТОРОНА",
    ] {
        assert!(!shows(&frame, removed), "{removed} must not be drawn");
    }
    vm.modal = None;
    vm.apply(event(Msg::PairingWindow(protocol::ipc::PairingWindow {
        open: true,
        address: "127.0.0.1:47901".to_owned(),
        seconds_remaining: 60,
        attempt_id: support::attempt_id(),
    })));
    harness.settle(&vm, false);
    let at = text_position(&mut harness, &vm, "Закрыть окно · скоро");
    let frame = harness.frame(&vm, false, SIZE, click_at(at));
    assert!(
        frame.output.actions.is_empty(),
        "unimplemented close must not send a command"
    );
}

/// The two appearance switches read the service's own answer: a core that has the effects on
/// shows them on, and flipping one sends the other back exactly as the service reported it.
#[test]
fn the_appearance_switches_show_what_the_service_reports() {
    let mut vm = connected();
    assert!(
        vm.status.as_ref().expect("status").edge_effects,
        "the fixture's service has the edge effects on"
    );
    vm.appearance_open = true;
    let mut harness = Harness::new();
    harness.settle(&vm, false);
    let at = text_position(
        &mut harness,
        &vm,
        "Прятать курсор, пока ввод на другой машине",
    );
    let frame = harness.frame(&vm, false, SIZE, click_at(at));
    assert!(
        frame.output.actions.contains(&Action::SetAppearance {
            edge_effects: true,
            hide_remote_cursor: false,
        }),
        "{:?}",
        frame.output.actions
    );
}

/// The window speaks the language it is given, the switch hands it over, and the choice is
/// what `gui.toml` keeps.
#[test]
fn the_language_switch_changes_the_window_and_is_stored() {
    let mut vm = connected();
    vm.look.language = ui_text::Lang::En;
    let mut harness = Harness::new();
    let frame = harness.settle(&vm, false);
    expect(&frame, &["MULTIDESKFLOW", "01 — MONITOR MAP"]);
    assert!(
        !shows(&frame, "КАРТА МОНИТОРОВ"),
        "the Russian map line is gone"
    );

    // The row in Settings: RU | EN, in the shared rounded style.
    vm.look.language = ui_text::Lang::Ru;
    vm.local(&Action::ToggleModal(Modal::Settings));
    let size = egui::vec2(1280.0, 720.0);
    harness.frame(&vm, false, size, Vec::new());
    harness.now += Duration::from_millis(500);
    harness.frame(&vm, false, size, Vec::new());
    let frame = harness.frame(&vm, false, size, Vec::new());
    expect(&frame, &["ЯЗЫК", "RU", "EN"]);
    let at = text_position(&mut harness, &vm, "EN");
    let frame = harness.frame(&vm, false, size, click_at(at));
    assert_eq!(
        frame.output.actions,
        vec![Action::SetLook(Look {
            language: ui_text::Lang::En,
            ..vm.look
        })],
        "the switch asks for English"
    );

    // And that is the value the window writes to its own file.
    let dir = std::env::temp_dir().join(format!("lws-gui-language-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = lws_gui::gui_settings::path_in(&dir);
    let mut look = vm.look;
    look.language = ui_text::Lang::En;
    lws_gui::gui_settings::save(&path, &look).expect("the directory is created");
    assert_eq!(
        lws_gui::gui_settings::load(&path).language,
        ui_text::Lang::En
    );
    let text = std::fs::read_to_string(&path).expect("read");
    assert!(text.contains("language = \"en\""), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A backend that cannot deliver the clipboard says so, and the window does not offer a switch
/// that could not do anything.
#[test]
fn a_backend_that_cannot_deliver_the_clipboard_offers_no_switch() {
    let mut vm = connected();
    vm.status
        .as_mut()
        .expect("a connected window has a status")
        .capabilities_unavailable = vec!["TEXT_CLIPBOARD".to_owned()];
    vm.local(&Action::ToggleModal(Modal::Clip));
    let mut harness = Harness::new();
    let size = egui::vec2(1280.0, 720.0);
    // The reveal takes 0.32 s of the window's clock; step past it.
    harness.frame(&vm, false, size, Vec::new());
    harness.now += Duration::from_millis(500);
    harness.frame(&vm, false, size, Vec::new());
    let frame = harness.frame(&vm, false, size, Vec::new());
    expect(
        &frame,
        &[
            "не может передавать содержимое буфера обмена",
            "общий буфер обмена недоступен",
        ],
    );
    // Clicking the switch where it is drawn sends nothing: it is drawn disabled.
    let at = text_position(&mut harness, &vm, "Выключен");
    let frame = harness.frame(&vm, false, size, click_at(at));
    assert!(
        !frame
            .output
            .actions
            .iter()
            .any(|action| matches!(action, Action::SetClipboard(_))),
        "{:?}",
        frame.output.actions
    );
}

/// One history row is the metadata line (with a human size), the flat one-line
/// preview and «Копировать»; «Обновить» is gone, and clearing asks first.
#[test]
fn a_history_row_is_compact_and_clearing_asks_first() {
    let mut vm = connected();
    vm.local(&Action::ToggleModal(Modal::Clip));
    let age_ms: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is past 1970")
        .as_millis()
        .try_into()
        .expect("the clock fits in u64 milliseconds");
    vm.apply(event(Msg::ClipboardHistory(ipc::ClipboardHistory {
        items: vec![ipc::ClipboardHistoryEntry {
            id: 7,
            direction: ipc::ClipboardHistoryDirection::Received as i32,
            peer_name: "linux-box".to_owned(),
            time_unix_ms: age_ms - 5_000,
            size_bytes: 1_048_576,
            preview: "первая строка\nвторая строка".to_owned(),
        }],
    })));
    let mut harness = Harness::new();
    // The reveal takes 0.32 s of the window's clock; step past it.
    harness.frame(&vm, false, SIZE, Vec::new());
    harness.now += Duration::from_millis(500);
    let frame = harness.frame(&vm, false, SIZE, Vec::new());
    expect(
        &frame,
        &[
            "Получено",
            "linux-box",
            "· 1 МиБ",
            "первая строка вторая строка",
            "Копировать",
            "Очистить историю",
        ],
    );
    assert!(
        !shows(&frame, "Обновить"),
        "the history refreshes by itself"
    );

    // The first click only asks; the same spot now draws the confirmation.
    let at = text_position(&mut harness, &vm, "Очистить историю");
    let frame = harness.frame(&vm, false, SIZE, click_at(at));
    assert!(
        !frame
            .output
            .actions
            .contains(&Action::ClearClipboardHistory),
        "{:?}",
        frame.output.actions
    );
    let frame = harness.frame(&vm, false, SIZE, Vec::new());
    expect(&frame, &["Очистить историю на этом компьютере?", "Отмена"]);

    // «Отмена» puts the question away without clearing anything.
    let at = text_position(&mut harness, &vm, "Отмена");
    let frame = harness.frame(&vm, false, SIZE, click_at(at));
    assert!(!frame
        .output
        .actions
        .contains(&Action::ClearClipboardHistory));
    let frame = harness.frame(&vm, false, SIZE, Vec::new());
    assert!(!shows(&frame, "Очистить историю на этом компьютере?"));

    // Asking again and confirming is what sends the clear.
    let at = text_position(&mut harness, &vm, "Очистить историю");
    harness.frame(&vm, false, SIZE, click_at(at));
    let at = text_position(&mut harness, &vm, "Очистить историю");
    let frame = harness.frame(&vm, false, SIZE, click_at(at));
    assert!(
        frame
            .output
            .actions
            .contains(&Action::ClearClipboardHistory),
        "{:?}",
        frame.output.actions
    );
}

/// Every square badge of a frame and the glyph painted inside it: the four letters of the dock's
/// cells, the six boxes of a pairing code. A badge is one of the sizes below and its glyph is
/// painted inside it, which is enough to tell them apart from the window's other rectangles.
fn badges(frame: &Frame) -> Vec<(&str, egui::Rect, egui::Rect)> {
    /// A letter of the dock, a digit of the code.
    const SIZES: [egui::Vec2; 2] = [egui::vec2(20.0, 20.0), egui::vec2(34.0, 46.0)];
    let mut found = Vec::new();
    for (text, ink) in &frame.texts {
        if text.chars().count() != 1 {
            continue;
        }
        let badge = frame
            .rects
            .iter()
            .filter(|rect| SIZES.contains(&rect.size()))
            .filter(|rect| rect.expand(2.0).contains(ink.center()))
            .min_by(|a, b| {
                (a.center() - ink.center())
                    .length_sq()
                    .total_cmp(&(b.center() - ink.center()).length_sq())
            });
        if let Some(badge) = badge {
            found.push((text.as_str(), *badge, *ink));
        }
    }
    found
}

/// The owner's complaint: the letter of a badge sat low and to the left, because the glyph was
/// centred by the box the font measures rather than by the ink it paints.
#[test]
fn every_badge_centres_its_glyph_on_its_own_square() {
    let mut vm = connected();
    vm.apply(event(Msg::PairingWindow(protocol::ipc::PairingWindow {
        open: true,
        address: "192.168.1.24:47901".to_owned(),
        seconds_remaining: 90,
        attempt_id: support::attempt_id(),
    })));
    vm.apply(event(Msg::PairingPrompt(pairing_prompt())));
    vm.local(&Action::ToggleModal(Modal::Pairs));
    let mut harness = Harness::new();
    // The reveal takes 0.32 s of the window's clock; step past it.
    harness.frame(&vm, false, SIZE, Vec::new());
    harness.now += Duration::from_millis(500);
    let frame = harness.frame(&vm, false, SIZE, Vec::new());
    let mut letters: Vec<&str> = badges(&frame).iter().map(|(text, ..)| *text).collect();
    letters.sort_unstable();
    assert_eq!(
        letters,
        ["1", "2", "3", "4", "8", "9", "A", "B", "C", "D"],
        "the badges of the dock"
    );
    for (text, badge, ink) in badges(&frame) {
        let off = ink.center() - badge.center();
        assert!(
            off.x.abs() <= 1.0 && off.y.abs() <= 1.0,
            "«{text}» is painted at {ink:?}, {off:?} from the centre of {badge:?}"
        );
    }
}

/// The clipboard rows say how long ago in words, and the frame that draws them is the one due
/// when the soonest of them changes — a minute away, not a second.
#[test]
fn a_history_row_says_how_long_ago_in_words_and_asks_for_the_next_minute() {
    let mut vm = connected();
    vm.local(&Action::ToggleModal(Modal::Clip));
    let now_ms: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is past 1970")
        .as_millis()
        .try_into()
        .expect("the clock fits in u64 milliseconds");
    let entry = |id: u64, age_ms: u64| ipc::ClipboardHistoryEntry {
        id,
        direction: ipc::ClipboardHistoryDirection::Received as i32,
        peer_name: "linux-box".to_owned(),
        time_unix_ms: now_ms - age_ms,
        size_bytes: 56,
        preview: "текст".to_owned(),
    };
    vm.apply(event(Msg::ClipboardHistory(ipc::ClipboardHistory {
        items: vec![
            // 20 s into the eleventh minute: forty seconds to «12 минут назад».
            entry(1, 11 * 60_000 + 20_000),
            // A minute into the fifth hour: fifty-nine minutes to «5 часов назад».
            entry(2, 4 * 3_600_000 + 60_000),
        ],
    })));
    let mut harness = Harness::new();
    harness.frame(&vm, false, SIZE, Vec::new());
    harness.now += Duration::from_millis(500);
    let frame = harness.frame(&vm, false, SIZE, Vec::new());
    expect(&frame, &["· 11 минут назад ·", "· 4 часа назад ·"]);
    assert!(
        !shows(&frame, " с назад"),
        "a row must not count seconds: {:?}",
        frame.texts
    );
    let wait = frame
        .output
        .repaint_after
        .expect("the rows are counted in minutes, so a frame is due");
    assert!(
        (Duration::from_secs(35)..=Duration::from_secs(45)).contains(&wait),
        "the next frame is the one that relabels the oldest row: {wait:?}"
    );
}
