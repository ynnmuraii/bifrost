//! Every screen, in every state the build spec names, drawn with no window and no GPU.
//!
//! `egui::Context::run_ui` is egui's headless entry point: it lays the widgets out and returns
//! the shapes they produced, so this test reads the *text the screen actually emitted* rather
//! than trusting that nothing panicked. The states are hand-built `ViewModel`s (a disconnected
//! window, a connected pair, remote focus with an active layout, an open pairing window, a
//! pairing prompt, a pending edit and a layout conflict) — which is the concrete proof that
//! the window works before any real core is involved.
//!
//! The viewport is deliberately tall: egui culls a shape that is completely outside the clip
//! rectangle, and a screen that fits on one page is the only screen whose every line this
//! test can read.

mod support;

use lws_gui::egui::{self, Shape};
use lws_gui::screens;
use lws_gui::view_model::{Action, Request, Screen, Side, ViewModel};
use protocol::ipc::gui_frame::Msg;
use support::{
    answer, attempt_id, event, pairing_prompt, status_with, two_node_doc, view, PEER_NODE,
    SELF_NODE,
};

/// A headless pass over one screen of one state; the strings egui laid out, in paint order.
fn draw(state: &ViewModel, screen: Screen) -> Vec<String> {
    let mut state = state.clone();
    state.screen = screen;
    let mut input = egui::RawInput::default();
    // The viewport is deliberately tall: egui culls a shape that is completely outside the
    // clip rectangle, and «Настройки» is the tallest screen of the four (every block of it
    // is one page of the window's own scroll area), so it is the one that sizes this page.
    let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 5000.0));
    let viewport = egui::ViewportInfo {
        inner_rect: Some(rect),
        ..Default::default()
    };
    input.viewports.insert(egui::ViewportId::ROOT, viewport);
    input.screen_rect = Some(rect);
    let ctx = egui::Context::default();
    let mut pass = |ui: &mut egui::Ui| {
        // The same shape the app draws: the header, the outcome line, then one screen.
        let mut actions = Vec::new();
        egui::Panel::top("lws-gui-header").show(ui, |ui| {
            actions.extend(screens::header(&state, ui));
            actions.extend(screens::outcome_banner(&state, ui));
        });
        egui::CentralPanel::default_margins().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    actions.extend(match state.screen {
                        Screen::Status => screens::status::draw(&state, ui),
                        Screen::Pairing => screens::pairing::draw(&state, ui),
                        Screen::Layout => screens::layout::draw(&state, ui),
                        Screen::Settings => screens::settings::draw(&state, ui),
                    });
                });
        });
    };
    // A panel sizes itself from the content it drew, and the clip rectangle of the pass it is
    // drawing is the size it measured last time — so the first pass of a fresh context cuts
    // off everything that does not fit its guess. The window redraws every 250 ms, and this
    // test reads the second pass: what the window really shows, not what it shows first.
    ctx.run_ui(input.clone(), &mut pass)
        .drop_without_applying_deltas();
    let output = ctx.run_ui(input, &mut pass);
    let mut texts = Vec::new();
    for clipped in &output.shapes {
        collect(&clipped.shape, &mut texts);
    }
    // A `FullOutput` carries the font atlas it wants the renderer to upload, and dropping it
    // unapplied is a panic by design; this test has no renderer, so it says so explicitly.
    output.drop_without_applying_deltas();
    texts
}

/// Every string egui laid out, in paint order.
fn collect(shape: &Shape, texts: &mut Vec<String>) {
    match shape {
        Shape::Text(text) => texts.push(text.galley.text().to_owned()),
        Shape::Vec(shapes) => {
            for shape in shapes {
                collect(shape, texts);
            }
        }
        _ => {}
    }
}

/// Draws one screen and asserts every expected sentence appeared somewhere on it.
fn expect_on(state: &ViewModel, screen: Screen, expected: &[&str]) {
    let texts = draw(state, screen);
    assert!(!texts.is_empty(), "{screen:?} drew no text at all");
    for needle in expected {
        assert!(
            texts.iter().any(|text| text.contains(needle)),
            "{screen:?} did not show {needle:?}; it showed: {texts:?}"
        );
    }
}

/// Draws all four screens and asserts the sentences that are true on all of them.
fn expect_everywhere(state: &ViewModel, expected: &[&str]) {
    for screen in Screen::ALL {
        expect_on(state, screen, expected);
    }
}

#[test]
fn a_disconnected_window_says_what_to_do_about_it() {
    let mut state = view();
    state.on_disconnected("no core is listening".to_owned());
    expect_everywhere(
        &state,
        &[
            "LAN Workspace",
            "Служба не запущена",
            "Статус",
            "Сопряжение",
            "Экраны",
            "Настройки",
        ],
    );
    expect_on(
        &state,
        Screen::Status,
        &[
            "Запустить",
            "Подключиться снова",
            "Адрес службы: \\\\.\\pipe\\lan-workspace-S-1-5-21-0-0-0-1001",
            "Нет данных от службы",
        ],
    );
}

#[test]
fn a_connected_pair_shows_where_input_goes() {
    let mut state = view();
    state.on_connected("endpoint");
    state.apply(event(Msg::Status(status_with("REMOTE"))));
    state.apply(event(Msg::TopologyDoc(two_node_doc(7))));

    expect_everywhere(&state, &["Служба работает"]);
    expect_on(
        &state,
        Screen::Status,
        &[
            // The one line the window exists for, and the peer list without this computer.
            "Ввод на linux-box",
            "раскладка ru",
            "Есть связь",
            "linux-box — в сети, не управляет клавиатурой",
            "Система: Windows",
            "Этот компьютер",
            "Отдать управление",
            // The numbers a support question needs are one click away, not on the screen.
            "Подробнее",
            // KVM-056: the degradation the core reported, in words the owner can read.
            "Физический ввод на этом компьютере не завершает сеанс автоматически",
        ],
    );
    // Neither the short id nor the session number is a line of its own any more.
    let status = draw(&state, Screen::Status);
    assert!(
        !status
            .iter()
            .any(|text| text.contains("Короткий номер") || text.contains("сеанс владельца")),
        "the details are folded away: {status:?}"
    );
    // Nothing is thinner than a sentence: the latency line is absent while nobody measures it.
    let status = draw(&state, Screen::Status);
    assert!(!status.iter().any(|text| text.contains("Задержка")));
}

#[test]
fn an_open_pairing_window_shows_the_address_and_the_countdown() {
    let mut state = view();
    state.on_connected("endpoint");
    state.apply(event(Msg::PairingWindow(protocol::ipc::PairingWindow {
        open: true,
        address: "192.168.1.10:47901".to_owned(),
        seconds_remaining: 118,
        attempt_id: attempt_id(),
    })));

    expect_on(
        &state,
        Screen::Pairing,
        &[
            "Окно сопряжения открыто",
            "192.168.1.10:47901",
            "Скажите этот адрес владельцу другого компьютера",
            "Осталось 118 с",
            "Закрыть окно сопряжения",
            "Открыть окно сопряжения",
            "192.168.1.10:47901",
        ],
    );
}

#[test]
fn a_pairing_prompt_shows_the_code_and_the_two_answers() {
    let mut state = view();
    state.on_connected("endpoint");
    state.apply(event(Msg::PairingPrompt(pairing_prompt())));

    expect_on(
        &state,
        Screen::Pairing,
        &[
            "482 913",
            "Подключается linux-box",
            "Скажите партнёру: коды совпадают?",
            "Да, совпадают",
            "Нет, не совпадают",
            "Подробнее",
        ],
    );

    // ADR-010 keeps the full fingerprints behind «Подробнее».
    state.local(&Action::ToggleDetails(true));
    expect_on(
        &state,
        Screen::Pairing,
        &[
            "Отпечаток партнёра: D4:E5:F6",
            "Отпечаток этого компьютера: A1:B2:C3",
            "Свернуть",
        ],
    );
}

#[test]
fn a_pending_edit_is_visible_from_both_screens_that_can_act_on_it() {
    let mut state = view();
    state.on_connected("endpoint");
    let mut status = status_with("LOCAL");
    status.layout_pending_edits = 1;
    state.apply(event(Msg::Status(status)));
    state.apply(event(Msg::TopologyDoc(two_node_doc(7))));
    state.apply(event(Msg::PendingEdits(protocol::ipc::PendingEdits {
        edits: vec![protocol::ipc::PendingEdit {
            edit_id: vec![0x11; 16],
            base_revision: 6,
            base_author: SELF_NODE.to_owned(),
            author_node_id: SELF_NODE.to_owned(),
            layout_toml: b"revision = 7".to_vec(),
        }],
    })));

    expect_on(
        &state,
        Screen::Status,
        &["Есть непринятая правка раскладки", "Перейти к раскладке"],
    );
    expect_on(
        &state,
        Screen::Layout,
        &[
            "Непринятая правка после разрыва связи",
            "Основа: ревизия 6, автор — компьютер aa11aa11",
            "Применить",
            "Отбросить",
        ],
    );
}

#[test]
fn a_layout_conflict_says_what_to_do() {
    let mut state = view();
    state.on_connected("endpoint");
    state.apply(event(Msg::Status(status_with("LOCAL"))));
    state.apply(event(Msg::TopologyDoc(two_node_doc(7))));
    let id = state.begin_request(Request::UpdateTopology, std::time::Instant::now());
    state.apply(answer(
        id,
        Msg::OpResult(protocol::ipc::OpResult {
            ok: false,
            error_code: "LAYOUT_CONFLICT".to_owned(),
        }),
    ));

    expect_everywhere(&state, &["Не получилось: раскладку изменили"]);
    expect_on(
        &state,
        Screen::Layout,
        &[
            "Раскладку изменили на другом компьютере. Обновите документ и повторите.",
            "Раскладку изменили",
            "Обновить",
        ],
    );
}

#[test]
fn a_document_the_window_cannot_place_asks_the_user_which_computer_is_this_one() {
    let mut state = view();
    state.on_connected("endpoint");
    // No owner is known, so nothing tells the window which member is this computer.
    let mut status = status_with("LOCAL");
    status.is_owner = false;
    status.owner_node_id.clear();
    status.members.clear();
    state.apply(event(Msg::Status(status)));
    state.apply(event(Msg::TopologyDoc(two_node_doc(7))));

    expect_on(
        &state,
        Screen::Layout,
        &[
            "Служба не сообщила, какой из компьютеров этот; выберите его сами",
            "Этот компьютер:",
            "Workstation",
            "linux-box",
        ],
    );
    expect_on(
        &state,
        Screen::Status,
        // The details header is drawn open or closed; what must not be on the screen is a
        // raw number, and without a document there is nothing to show either way.
        &["Подробнее"],
    );
}

#[test]
fn the_editor_draws_every_member_and_marks_an_approximate_size() {
    let mut state = view();
    state.on_connected("endpoint");
    state.apply(event(Msg::Status(status_with("LOCAL"))));
    state.apply(event(Msg::TopologyDoc(two_node_doc(7))));
    state.local(&Action::SetSide(Side::Right));

    expect_on(
        &state,
        Screen::Layout,
        &[
            "Применено: ревизия 7, автор — компьютер aa11aa11",
            // The local screen says whose it is; the peer's carries its own name.
            "Этот компьютер — Workstation",
            "linux-box",
            "1920×1080",
            "размер известен приблизительно",
            "Как стоят компьютеры",
            "Другой компьютер:",
            "Справа",
            "Применить",
            "Что будет отправлено",
        ],
    );

    // What the button would send names both computers.
    let preview = state.layout_toml();
    assert!(preview.contains(&format!("peer:{PEER_NODE}")));
    assert!(preview.contains(&format!("peer:{SELF_NODE}")));
}

#[test]
fn settings_offers_start_when_the_service_is_disconnected() {
    let state = view();
    expect_on(&state, Screen::Settings, &["Запустить службу"]);
    let settings = draw(&state, Screen::Settings);
    assert!(
        !settings
            .iter()
            .any(|text| text.contains("Перезапустить службу")),
        "a stopped service cannot be restarted"
    );
}
#[test]
fn the_settings_screen_only_offers_what_the_core_can_do() {
    let mut state = view();
    state.on_connected("endpoint");
    state.apply(event(Msg::Status(status_with("LOCAL"))));
    state.local(&Action::SelectRevoke(Some(PEER_NODE.to_owned())));

    expect_on(
        &state,
        Screen::Settings,
        &[
            "Общий буфер обмена",
            "Текст можно копировать на одном компьютере и вставлять на другом",
            "Аварийная клавиша: Ctrl+Alt+Shift+F12",
            "Клавиша переключения: Ctrl+Alt+Shift+Right",
            "Клавиша взятия управления: Ctrl+Alt+Shift+F11",
            "Забыть разрешения порталов",
            "Удалить компьютер из пары",
            "Сохранить диагностику",
            "Каталог состояния:",
        ],
    );
    // KVM-051: the core has no autostart, so the window has no switch for it.
    let settings = draw(&state, Screen::Settings);
    assert!(
        !settings
            .iter()
            .any(|text| text.to_lowercase().contains("автозапуск")),
        "there is no autostart switch to draw"
    );

    // The confirmation is a second step, and it names the computer it would revoke.
    state.local(&Action::ConfirmRevoke(true));
    expect_on(
        &state,
        Screen::Settings,
        &[
            "Удалить «linux-box» из пары? Сопрягать придётся заново",
            "Да, удалить",
            "Отмена",
        ],
    );
}

/// «Адрес в локальной сети»: the value from `config.toml`, the addresses to pick from, the
/// button that writes it and «Перезапустить службу» behind it.
#[test]
fn the_settings_screen_shows_the_address_and_the_service_behind_it() {
    let mut state = view();
    state.on_connected("endpoint");
    state.apply(event(Msg::Status(status_with("LOCAL"))));
    state.config.listen_address = "192.168.1.10".to_owned();
    state.settings.lan.candidates = vec!["192.168.1.10".to_owned(), "10.0.0.7".to_owned()];

    expect_on(
        &state,
        Screen::Settings,
        &[
            "Адрес в локальной сети",
            "Адрес этого компьютера в локальной сети: 192.168.1.10",
            "Другой компьютер должен иметь доступ к этому адресу",
            "Применить",
            "Служба",
            "Перезапустить службу",
        ],
    );

    // The address the file names, when this computer does not have it, is offered too: the
    // dropdown of the window holds it, so opening the screen never silently drops it.
    state.config.listen_address = "10.0.0.5".to_owned();
    expect_on(
        &state,
        Screen::Settings,
        &["Адрес этого компьютера в локальной сети: 10.0.0.5"],
    );

    // A machine with no address yet is told so, in the same plain words.
    state.config.listen_address = String::new();
    expect_on(
        &state,
        Screen::Settings,
        &["Адрес этого компьютера в локальной сети: не указан"],
    );

    // «Настройки созданы автоматически»: once, with a way to make it go away.
    state.settings.first_run = true;
    expect_on(
        &state,
        Screen::Settings,
        &[
            "Первый запуск",
            "Настройки созданы автоматически",
            "Служба создала файл config.toml при первом запуске",
            "Понятно",
        ],
    );
    state.local(&Action::DismissFirstRun);
    let settings = draw(&state, Screen::Settings);
    assert!(
        !settings
            .iter()
            .any(|text| text.contains("Настройки созданы автоматически")),
        "the notice is once per session"
    );
}
