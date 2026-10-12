//! The view model, on its own: no window, no core, no display.
//!
//! These are the tests the build spec §6 asks for: a `Status` fills the screen and a changed
//! focus changes «куда идёт ввод»; a pairing prompt is answered with its own attempt id; a
//! document older than the one the editor holds does not become the editor's base; a
//! `LAYOUT_CONFLICT` sets the hint that says what to do; the editor's document round-trips
//! through the configuration schema; and a disconnect keeps the last known state visible.

mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use lws_gui::view_model::{
    Action, Bridge, GuiClient, Launch, Request, Restart, RestartStep, Side, ViewModel,
    SERVICE_STOP_TIMEOUT,
};
use protocol::ipc::gui_frame::Msg;
use support::{
    answer, attempt_id, event, pairing_prompt, status_with, two_node_doc, view, PEER_NODE,
    SELF_NODE,
};

#[test]
fn a_status_fills_the_screen_and_the_focus_says_where_input_goes() {
    let mut vm = view();
    vm.apply(event(Msg::Status(status_with("LOCAL"))));

    assert_eq!(vm.focus_line(), "Ввод здесь");
    assert_eq!(vm.link_line(), "Есть связь");
    assert_eq!(vm.backend_label(), "Windows");
    assert_eq!(
        vm.short_id_of_self().as_deref(),
        Some("aa11aa11"),
        "the owner's id is this computer's id while this computer owns input"
    );
    // This computer is not one of its own peers: the list is the *other* computers, and it
    // says who has the keyboard in words that are not the core's.
    assert_eq!(
        vm.member_lines(),
        vec!["linux-box — в сети, не управляет клавиатурой"]
    );
    assert!(
        vm.limits()
            .iter()
            .any(|line| line.contains("не завершает сеанс автоматически")),
        "RECEIVER_TAKEOVER is a limitation a human has to read"
    );
    assert_eq!(vm.delay_line(), None, "nobody measures latency at 0 µs");
    assert_eq!(vm.owner_lines().0, "Этот компьютер");

    // The whole point of the screen: the same window, one frame later, says something else.
    vm.apply(event(Msg::Status(status_with("REMOTE"))));
    assert_eq!(vm.focus_line(), "Ввод на linux-box");
    assert_eq!(vm.focus_detail().as_deref(), Some("раскладка ru"));
    assert_eq!(vm.delay_line(), None);

    let mut status = status_with("REMOTE");
    status.rtt_p95_us = 12_400;
    vm.apply(event(Msg::Status(status)));
    assert_eq!(vm.delay_line().as_deref(), Some("Задержка: 12 мс"));

    let mut blocked = status_with("BLOCKED");
    blocked.focus = "BLOCKED".to_owned();
    blocked.last_error_code = "AGENT_LOST".to_owned();
    vm.apply(event(Msg::Status(blocked)));
    assert_eq!(vm.focus_line(), "Ввод остановлен из-за ошибки");
    assert!(vm
        .limits()
        .iter()
        .any(|line| line.contains("Последнее сообщение: агент ввода потерян")));
}

#[test]
fn nobody_is_in_the_pair_yet() {
    let mut vm = view();
    let mut status = status_with("LOCAL");
    status.members.clear();
    status.connected = false;
    status.is_owner = false;
    status.owner_node_id.clear();
    vm.apply(event(Msg::Status(status)));

    assert_eq!(vm.link_line(), "Нет связи");
    assert_eq!(
        vm.member_lines(),
        vec!["Пока никого: откройте окно сопряжения"]
    );
    assert_eq!(vm.short_id_of_self(), None, "no id is invented");
    assert_eq!(vm.owner_lines().0, "нет владельца");
}

#[test]
fn a_pairing_prompt_is_answered_with_its_own_attempt() {
    let mut vm = view();
    vm.apply(event(Msg::PairingPrompt(pairing_prompt())));

    assert_eq!(vm.pairing.code().as_deref(), Some("482 913"));
    let attempt = vm
        .pairing
        .attempt_id()
        .expect("the prompt names an attempt");

    // What the «Да, совпадают» button produces, and what it puts on the wire.
    let (request, message) = Action::Answer {
        attempt_id: attempt.clone(),
        approve: true,
    }
    .wire()
    .expect("an answer is a request");
    assert_eq!(request, Request::PairingAnswer);
    match message {
        Msg::PairingAnswer(answer) => {
            assert_eq!(answer.attempt_id, attempt_id());
            assert!(answer.approve);
        }
        other => panic!("expected PairingAnswer, got {other:?}"),
    }

    // And the refusal carries the same id with `approve: false`.
    match (Action::Answer {
        attempt_id: attempt,
        approve: false,
    })
    .wire()
    .expect("an answer is a request")
    .1
    {
        Msg::PairingAnswer(answer) => assert!(!answer.approve),
        other => panic!("expected PairingAnswer, got {other:?}"),
    }
}

#[test]
fn a_pairing_window_tells_the_user_what_to_type() {
    let mut vm = view();
    vm.apply(event(Msg::PairingWindow(protocol::ipc::PairingWindow {
        open: true,
        address: "192.168.1.10:47901".to_owned(),
        seconds_remaining: 118,
        attempt_id: attempt_id(),
    })));
    let window = vm.pairing.window.as_ref().expect("the window is open");
    assert_eq!(window.address, "192.168.1.10:47901");
    assert_eq!(window.seconds_remaining, 118);

    // The user closes it in the window; the core closes its own by timeout.
    vm.local(&Action::ClosePairingWindow);
    assert!(vm.pairing.window.is_none());

    // A closed window clears what it opened, too.
    vm.apply(event(Msg::PairingWindow(protocol::ipc::PairingWindow {
        open: false,
        address: String::new(),
        seconds_remaining: 0,
        attempt_id: Vec::new(),
    })));
    assert!(vm.pairing.window.is_none());
    assert!(vm.pairing.prompt.is_none());
}

#[test]
fn an_older_document_does_not_become_the_editors_base() {
    let mut vm = view();
    vm.apply(event(Msg::TopologyDoc(two_node_doc(7))));
    assert_eq!(vm.editor.revision, 7);
    assert_eq!(vm.editor.nodes.len(), 2);

    let mut older = two_node_doc(4);
    older.members.truncate(1);
    older.author_node_id = PEER_NODE.to_owned();
    vm.apply(event(Msg::TopologyDoc(older)));

    assert_eq!(vm.editor.revision, 7, "the revision never goes back");
    assert_eq!(vm.editor.nodes.len(), 2, "and neither does the document");
    assert_eq!(vm.editor.author, SELF_NODE);

    // A document the core applies moves the base the next edit names.
    vm.apply(event(Msg::TopologyDoc(two_node_doc(9))));
    assert_eq!(vm.editor.revision, 9);
}

#[test]
fn a_layout_conflict_says_what_to_do_about_it() {
    let mut vm = view();
    vm.apply(event(Msg::TopologyDoc(two_node_doc(7))));
    let now = Instant::now();
    let id = vm.begin_request(Request::UpdateTopology, now);
    vm.apply(answer(
        id,
        Msg::OpResult(protocol::ipc::OpResult {
            ok: false,
            error_code: "LAYOUT_CONFLICT".to_owned(),
        }),
    ));

    let conflict = vm
        .conflict
        .clone()
        .expect("the conflict is kept for the screen");
    assert_eq!(
        conflict,
        "Раскладку изменили на другом компьютере. Обновите документ и повторите."
    );
    let outcome = vm.outcome.as_ref().expect("a failure is reported");
    assert!(!outcome.ok);
    assert!(outcome.line().starts_with("Не получилось: "));
    assert!(
        vm.inflight.is_empty(),
        "the answer retired the request it belonged to"
    );

    // «Обновить» is the way out, and it forgets the conflict it was about.
    vm.local(&Action::Refresh);
    assert!(vm.conflict.is_none());
}

#[test]
fn the_editors_document_round_trips_through_the_configuration_schema() {
    let mut vm = view();
    vm.apply(event(Msg::Status(status_with("LOCAL"))));
    vm.apply(event(Msg::TopologyDoc(two_node_doc(7))));

    // The user says the other computer stands to the right, then nudges its rectangle: a drag
    // is a gesture that *chooses* the edge, so the peer snaps onto it and the document carries
    // the snapped position — the picture and «Применить» are the same arrangement.
    vm.local(&Action::SetSide(Side::Right));
    vm.local(&Action::MoveDisplay {
        node_id: PEER_NODE.to_owned(),
        display_id: "eDP-1".to_owned(),
        dx: 120.0,
        dy: -30.0,
    });

    let document = vm.layout_toml();

    // 1. What the core does with it: `validate_layout_toml` parses the bare document.
    let topology: config::TopologyConfig =
        toml::from_str(&document).expect("the core's own parse of an edited document");
    assert_eq!(topology.revision, 7);
    assert_eq!(topology.displays.len(), 2);
    assert_eq!(topology.links.len(), 1);
    let link = &topology.links[0];
    assert_eq!(link.from.node, format!("peer:{SELF_NODE}"));
    assert_eq!(link.to.node, format!("peer:{PEER_NODE}"));
    assert_eq!(link.from.edge.to_string(), "right");
    assert_eq!(link.to.edge.to_string(), "left");
    assert!(link.from.start < link.from.end && link.from.end > 0.0);
    let moved = topology
        .displays
        .iter()
        .find(|display| display.node == format!("peer:{PEER_NODE}"))
        .expect("the other computer's display is in the document");
    assert_eq!(
        moved.logical,
        [1920.0, 0.0, 1600.0, 900.0],
        "the peer sits at this computer's right edge (1920) and lines up with it: the drag \
         +120/-30 pointed right, so the radio still says «Справа» and the block snaps back onto \
         that edge instead of overlapping"
    );

    // 2. The acceptance test: the same text is a valid `[topology]` section of a config file.
    let config = config::Config::from_str(&as_config_file(&document))
        .expect("the editor emits a valid section");
    config.validate().expect("and a valid document");
    assert_eq!(config.topology.links.len(), 1);
}

/// The document as a whole config file, the way `config.example.toml` holds it.
///
/// A TOML header is absolute, so `[[displays]]` inside the section body means a *top-level*
/// `displays` unless the section name is written into it — and the same is true of a header
/// for a nested field, which `toml` writes as `[links.from]` under its `[[links]]`. Every
/// header of the body is therefore rewritten, arrays of tables and sub-tables alike, and the
/// file gets the `[network]` floor every loadable configuration has (an empty
/// `network.listen_address` is one of the things `validate` refuses).
fn as_config_file(document: &str) -> String {
    let mut file = String::from(
        "config_version = 2\n\n[network]\nlisten_address = \"127.0.0.1\"\n\n[topology]\n",
    );
    for line in document.lines() {
        let header = line.trim_start();
        if let Some(rest) = header.strip_prefix("[[") {
            file.push_str("[[topology.");
            file.push_str(rest);
        } else if let Some(rest) = header.strip_prefix('[') {
            file.push_str("[topology.");
            file.push_str(rest);
        } else {
            file.push_str(line);
        }
        file.push('\n');
    }
    file
}

#[test]
fn the_document_of_the_first_revision_is_still_a_document() {
    let mut vm = view();
    vm.apply(event(Msg::TopologyDoc(protocol::ipc::TopologyDoc {
        revision: 0,
        author_node_id: String::new(),
        members: two_node_doc(0).members,
        links: Vec::new(),
    })));
    let file = as_config_file(&vm.layout_toml());
    let config = config::Config::from_str(&file).expect("revision 0 is a valid floor");
    config.validate().expect("and needs no author");
    assert_eq!(config.topology.revision, 0);
}

#[test]
fn a_disconnect_moves_to_the_disconnected_state_and_keeps_the_last_status() {
    let mut vm = view();
    vm.apply(event(Msg::Status(status_with("REMOTE"))));
    vm.apply(event(Msg::TopologyDoc(two_node_doc(7))));
    vm.on_connected(r"\\.\pipe\lan-workspace-S-1-5-21-0-0-0-1001");
    assert_eq!(vm.connection_label(), "Служба работает");

    vm.on_disconnected("the pipe was closed".to_owned());

    assert_eq!(vm.connection_label(), "Служба не запущена");
    assert!(vm.disconnected_reason().is_some());
    assert_eq!(
        vm.focus_line(),
        "Ввод на linux-box",
        "the last known state stays visible"
    );
    assert_eq!(vm.focus_detail().as_deref(), Some("раскладка ru"));
    assert_eq!(
        vm.editor.nodes.len(),
        2,
        "the editor keeps the document it was editing"
    );
}

#[test]
fn a_silent_core_is_reported_after_the_timeout() {
    let mut vm = view();
    vm.on_connected("endpoint");
    let now = Instant::now();
    let id = vm.begin_request(Request::GetStatus, now);

    vm.tick(now + Duration::from_secs(2));
    assert_eq!(vm.connection_label(), "Служба работает");

    vm.tick(now + Duration::from_secs(4));
    assert_eq!(vm.connection_label(), "Служба не отвечает");

    // The answer, whenever it comes, clears it.
    vm.apply(answer(id, Msg::Status(status_with("LOCAL"))));
    vm.tick(now + Duration::from_secs(5));
    assert_eq!(vm.connection_label(), "Служба работает");
    assert!(vm.inflight.is_empty());
}

#[test]
fn a_request_with_no_core_is_reported_as_not_sent() {
    let mut vm = view();
    vm.on_disconnected("no core".to_owned());
    let id = vm.begin_request(Request::StartPairing, Instant::now());
    vm.on_not_sent(id);

    assert!(
        vm.inflight.is_empty(),
        "nothing is left waiting for an answer"
    );
    vm.tick(Instant::now() + Duration::from_secs(10));
    assert_eq!(
        vm.connection_label(),
        "Служба не запущена",
        "a request that never left is not a silent core"
    );
    assert!(vm.log.iter().any(|line| line.contains("не отправлено")));
}

#[test]
fn a_clipboard_refusal_is_explained_where_the_switch_is() {
    let mut vm = view();
    vm.apply(event(Msg::Status(status_with("LOCAL"))));
    let id = vm.begin_request(Request::SetClipboard, Instant::now());
    vm.apply(answer(
        id,
        Msg::OpResult(protocol::ipc::OpResult {
            ok: false,
            error_code: "CLIPBOARD_DISABLED".to_owned(),
        }),
    ));
    assert_eq!(
        vm.settings.clipboard_note.as_deref(),
        Some("Буфер обмена выключен в настройках (config.toml)")
    );
}

#[test]
fn a_successful_pairing_promises_what_actually_happens() {
    let mut vm = view();
    vm.apply(event(Msg::PairingPrompt(pairing_prompt())));
    let id = vm.begin_request(Request::PairingAnswer, Instant::now());
    vm.apply(answer(
        id,
        Msg::OpResult(protocol::ipc::OpResult {
            ok: true,
            error_code: String::new(),
        }),
    ));
    let outcome = vm.outcome.as_ref().expect("the result is shown");
    assert!(outcome.ok);
    assert_eq!(
        outcome.line(),
        "Готово: linux-box сопряжён. Связь появится после перезапуска службы"
    );
}

#[test]
fn the_pending_edit_buttons_name_the_edit_they_belong_to() {
    let mut vm = view();
    vm.apply(event(Msg::PendingEdits(protocol::ipc::PendingEdits {
        edits: vec![protocol::ipc::PendingEdit {
            edit_id: vec![0x11; 16],
            base_revision: 6,
            base_author: SELF_NODE.to_owned(),
            author_node_id: SELF_NODE.to_owned(),
            layout_toml: b"revision = 7".to_vec(),
        }],
    })));
    assert_eq!(vm.pending_edits.len(), 1);
    let edit = vm.pending_edits[0].edit_id.clone();

    match Action::ApplyPendingEdit(edit.clone()).wire() {
        Some((Request::ApplyPendingEdit, Msg::ApplyPendingEdit(apply))) => {
            assert_eq!(apply.edit_id, edit);
        }
        other => panic!("expected an ApplyPendingEdit request, got {other:?}"),
    }
    match Action::DiscardPendingEdit(edit.clone()).wire() {
        Some((Request::DiscardPendingEdit, Msg::DiscardPendingEdit(discard))) => {
            assert_eq!(discard.edit_id, edit);
        }
        other => panic!("expected a DiscardPendingEdit request, got {other:?}"),
    }
}

#[test]
fn every_wire_request_is_the_message_it_names() {
    let cases: Vec<(Action, Request)> = vec![
        (Action::Refresh, Request::GetStatus),
        (Action::Claim, Request::Claim),
        (Action::Abandon, Request::Abandon),
        (
            Action::StartPairing { window_s: 120 },
            Request::StartPairing,
        ),
        (
            Action::PairWith("192.168.1.10:47901".to_owned()),
            Request::PairWith,
        ),
        (
            Action::Answer {
                attempt_id: attempt_id(),
                approve: true,
            },
            Request::PairingAnswer,
        ),
        (Action::SetClipboard(true), Request::SetClipboard),
        (
            Action::UpdateTopology {
                layout_toml: b"links = []".to_vec(),
                expected_revision: 3,
                expected_author: vec![0xaa; 32],
            },
            Request::UpdateTopology,
        ),
        (Action::ListPendingEdits, Request::ListPendingEdits),
        (Action::ResetPortalGrants, Request::ResetPortalGrants),
        (Action::RevokePeer(vec![0xbb; 32]), Request::RevokePeer),
        (
            Action::ExportDiagnostics(String::new()),
            Request::ExportDiagnostics,
        ),
    ];
    for (action, expected) in cases {
        let (request, _) = action
            .wire()
            .unwrap_or_else(|| panic!("{action:?} is a request"));
        assert_eq!(request, expected, "{action:?}");
    }
    // «Обновить» asks for both halves; the greeting of §4 asks for exactly these two.
    assert_eq!(
        Action::Refresh.extra_wire().map(|(request, _)| request),
        Some(Request::GetTopology)
    );
    // The local ones never reach the core.
    assert!(Action::StartCore.wire().is_none());
    assert!(Action::ConnectAgain.wire().is_none());
    assert!(Action::ShowScreen(lws_gui::view_model::Screen::Layout)
        .wire()
        .is_none());
}

/// The preview is the arrangement «Применить» would send: the peer next to the chosen edge,
/// never on top of this computer, and the radio follows a drag.
#[test]
fn the_editor_places_the_peer_at_the_chosen_edge_and_never_over_it() {
    let mut vm = view();
    vm.apply(event(Msg::Status(status_with("LOCAL"))));
    vm.apply(event(Msg::TopologyDoc(two_node_doc(7))));

    // The document places both computers at their own origins; the window cannot draw that as
    // one picture, so it places the peer itself, on the default side, from the first frame.
    let bounds = |vm: &ViewModel, node: &str| {
        let node = vm
            .editor
            .nodes
            .iter()
            .find(|candidate| candidate.node_id == node)
            .expect("the node is in the document");
        let mut b = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for display in &node.displays {
            b = (
                b.0.min(display.x),
                b.1.min(display.y),
                b.2.max(display.x + display.w),
                b.3.max(display.y + display.h),
            );
        }
        b
    };
    let overlap = |a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)| {
        a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3
    };

    assert_eq!(vm.editor.side, Side::Right, "the default side");
    assert_eq!(
        bounds(&vm, PEER_NODE),
        (1920.0, 0.0, 3520.0, 900.0),
        "«Справа»: the peer starts where this computer's screen ends"
    );
    assert!(!overlap(bounds(&vm, SELF_NODE), bounds(&vm, PEER_NODE)));

    // Every side moves the picture, and none of them overlaps.
    let expected = [
        (Side::Left, (-1600.0, 0.0)),
        (Side::Top, (0.0, -900.0)),
        (Side::Bottom, (0.0, 1080.0)),
        (Side::Right, (1920.0, 0.0)),
    ];
    for (side, (x, y)) in expected {
        vm.local(&Action::SetSide(side));
        let peer = bounds(&vm, PEER_NODE);
        assert_eq!((peer.0, peer.1), (x, y), "{}", side.label());
        assert!(
            !overlap(bounds(&vm, SELF_NODE), peer),
            "{} must not overlap",
            side.label()
        );
    }

    // A drag is a way of choosing the edge: dragging the peer up-left from the right side
    // moves it to the left of this computer, and the radio says so.
    vm.local(&Action::SetSide(Side::Right));
    vm.local(&Action::MoveDisplay {
        node_id: PEER_NODE.to_owned(),
        display_id: "eDP-1".to_owned(),
        dx: -2000.0,
        dy: 0.0,
    });
    assert_eq!(vm.editor.side, Side::Left, "the drag chose the edge");
    assert_eq!(bounds(&vm, PEER_NODE), (-1600.0, 0.0, 0.0, 900.0));

    // And the edge the link crosses is the one the document names, on both sides.
    vm.local(&Action::SetSide(Side::Right));
    let (my_display, my_edge, their_display, their_edge, end) =
        vm.linked_span().expect("two placed computers have a link");
    assert_eq!(my_display, "\\\\.\\DISPLAY1");
    assert_eq!(their_display, "eDP-1");
    assert_eq!(
        (my_edge, their_edge),
        (config::Edge::Right, config::Edge::Left)
    );
    assert_eq!(
        end, 900.0,
        "the shorter of the two screens is the shared edge"
    );
}

/// Every line of «Ограничения и деградации» comes from what the core reported, on the four
/// hosts the pair is meant to run on.
#[test]
fn the_limits_follow_the_capabilities_the_core_reported() {
    let limits = |backend: &str, unavailable: &[&str], portal: &str| {
        let mut vm = view();
        let mut status = status_with("LOCAL");
        status.backend = backend.to_owned();
        status.capabilities_unavailable =
            unavailable.iter().map(|code| (*code).to_owned()).collect();
        status.portal_state = portal.to_owned();
        vm.apply(event(Msg::Status(status)));
        vm.limits()
    };

    // Windows and X11: a receiver that takes the session back on physical input and has the
    // emergency hotkey reports neither, so neither sentence appears — the old text claimed the
    // takeover was missing on any host that did not say otherwise.
    assert_eq!(limits("platform-win", &[], ""), Vec::<String>::new());
    assert_eq!(limits("platform-x11", &[], ""), Vec::<String>::new());

    // Wayland, GNOME 46: the portal grant is what stands between the user and input, and the
    // compositor provides neither takeover nor an emergency hotkey of its own.
    assert_eq!(
        limits(
            "platform-wayland",
            &["RECEIVER_TAKEOVER", "EMERGENCY_HOTKEY"],
            "permission_required"
        ),
        vec![
            "Физический ввод на этом компьютере не завершает сеанс автоматически — переключение только с источника".to_owned(),
            "Нет аварийной клавиши: прервать захват клавиатуры на этом компьютере можно только мышью".to_owned(),
            "На экране диалог разрешения портала — подтвердите его, иначе узел не сможет принять ввод".to_owned(),
            "Wayland: перехват ввода возможен только через порталы рабочего стола".to_owned(),
        ]
    );

    // KDE on Wayland: the same platform, but this desktop *does* hand the session back and
    // does have the hotkey, so the only Wayland line is the one about portals.
    let kde = limits("platform-wayland", &[], "");
    assert_eq!(
        kde,
        vec!["Wayland: перехват ввода возможен только через порталы рабочего стола".to_owned()]
    );

    // A refused grant is a different sentence, and it is the one that points at the settings.
    let denied = limits("platform-wayland", &[], "denied");
    assert_eq!(denied.len(), 2, "{denied:?}");
    assert!(denied[0].contains("Забыть разрешения порталов"));
}

// ---------------------------------------------------------------- the LAN address

/// A directory of this test's own, with a `config.toml` naming `address`.
fn settings_dir(address: &str) -> std::path::PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let dir = std::env::temp_dir().join(format!(
        "lws-gui-settings-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("config")).expect("a config directory");
    std::fs::write(
        dir.join("config/config.toml"),
        format!("config_version = 2\n\n[network]\nlisten_address = \"{address}\"\n"),
    )
    .expect("a config.toml");
    dir
}

/// The directories of a configuration this test wrote.
fn paths_of(dir: &std::path::Path) -> config::Paths {
    config::Paths::new(
        dir.join("config"),
        dir.join("data"),
        dir.join("state"),
        None,
    )
}

#[test]
fn the_address_list_keeps_the_value_from_the_file_and_adds_what_this_computer_has() {
    let mut vm = view();
    vm.config.listen_address = "10.0.0.5".to_owned();
    vm.settings.lan.candidates = vec!["192.168.1.10".to_owned(), "10.0.0.7".to_owned()];

    // The file names an address this computer does not have: it is in the list anyway, so the
    // value the user would lose by opening the dropdown is still offered to them.
    assert_eq!(
        vm.settings.lan.options(&vm.config.listen_address),
        vec!["192.168.1.10", "10.0.0.7", "10.0.0.5"]
    );
    assert_eq!(
        vm.settings
            .lan
            .selected(&vm.config.listen_address)
            .as_deref(),
        Some("10.0.0.5"),
        "the dropdown shows the file's own value until the user picks another"
    );
    vm.local(&Action::SelectLanAddress("192.168.1.10".to_owned()));
    assert_eq!(
        vm.settings
            .lan
            .selected(&vm.config.listen_address)
            .as_deref(),
        Some("192.168.1.10")
    );

    // An address that *is* one of this computer's own is not listed twice.
    vm.config.listen_address = "10.0.0.7".to_owned();
    assert_eq!(
        vm.settings.lan.options(&vm.config.listen_address),
        vec!["192.168.1.10", "10.0.0.7"]
    );

    // A machine with no file yet offers only its own addresses, and nothing selected while
    // the user has picked nothing — the pick itself outlives the file's value.
    vm.settings.lan.choice = None;
    vm.config.listen_address = String::new();
    assert_eq!(
        vm.settings.lan.options(&vm.config.listen_address),
        vec!["192.168.1.10", "10.0.0.7"]
    );
    assert_eq!(vm.settings.lan.selected(&vm.config.listen_address), None);
}

#[test]
fn a_loopback_config_value_is_neither_offered_nor_selected() {
    let mut vm = view();
    vm.config.listen_address = "127.0.0.1".to_owned();
    vm.settings.lan.candidates = vec!["192.168.1.37".to_owned(), "127.0.0.1".to_owned()];

    assert_eq!(
        vm.settings.lan.options(&vm.config.listen_address),
        vec!["192.168.1.37"],
        "the LAN picker must never offer loopback, even from a stale config"
    );
    assert_eq!(
        vm.settings
            .lan
            .selected(&vm.config.listen_address)
            .as_deref(),
        Some("192.168.1.37"),
        "a valid adapter address replaces a stale loopback selection"
    );
}

#[test]
fn the_address_button_writes_the_file_and_says_so() {
    let mut vm = view();
    let dir = settings_dir("127.0.0.1");
    vm.config.paths = Some(paths_of(&dir));
    vm.config.listen_address = "127.0.0.1".to_owned();
    vm.settings.lan.candidates = vec!["192.168.1.10".to_owned(), "10.0.0.7".to_owned()];
    vm.local(&Action::SelectLanAddress("192.168.1.10".to_owned()));

    vm.local(&Action::ApplyLanAddress);

    // The write went through the configuration's own writer: the file still loads, and it
    // names the address that was picked.
    let written = config::Config::load(paths_of(&dir).config_file()).expect("a valid config.toml");
    assert_eq!(written.network.listen_address, "192.168.1.10");
    let applied = vm
        .settings
        .lan
        .applied
        .as_ref()
        .expect("«Применить» answers in words");
    assert!(applied.ok, "{}", applied.line());
    assert_eq!(applied.text, "Адрес сохранён");
    // What the screen shows is what the file now names, and the pick is spent.
    assert_eq!(vm.config.listen_address, "192.168.1.10");
    assert_eq!(vm.settings.lan.choice, None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_address_that_cannot_be_written_is_said_in_words() {
    let mut vm = view();
    let dir = settings_dir("127.0.0.1");
    let damaged = "config_version = 2\n\n[network]\nlisten_address = 12\n";
    std::fs::write(dir.join("config/config.toml"), damaged).expect("a damaged config.toml");
    vm.config.paths = Some(paths_of(&dir));
    vm.config.listen_address = "127.0.0.1".to_owned();
    vm.settings.lan.candidates = vec!["192.168.1.10".to_owned()];
    vm.local(&Action::SelectLanAddress("192.168.1.10".to_owned()));

    vm.local(&Action::ApplyLanAddress);

    let applied = vm
        .settings
        .lan
        .applied
        .as_ref()
        .expect("a failure is a sentence, never a silence");
    assert!(!applied.ok, "{}", applied.line());
    assert!(
        applied.text.starts_with("адрес не сохранён ("),
        "the sentence carries the reason: {}",
        applied.text
    );
    // The damaged file was not rewritten behind the user's back.
    assert_eq!(
        std::fs::read_to_string(dir.join("config/config.toml")).expect("the file"),
        damaged
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn degraded_agent_status_exposes_its_reason_and_retry_state() {
    let mut vm = view();
    vm.apply(event(Msg::Status(status_with("LOCAL"))));
    assert_eq!(vm.agent_degraded_reason(), None);

    let mut degraded = status_with("BLOCKED");
    degraded.agent_degraded_reason = "Wayland permission was denied".to_owned();
    vm.apply(event(Msg::Status(degraded)));
    assert_eq!(
        vm.agent_degraded_reason(),
        Some("Wayland permission was denied")
    );

    let (request, message) = Action::RetryAgent.wire().expect("a retry request");
    assert_eq!(request, Request::RetryAgent);
    assert!(matches!(message, Msg::RetryAgent(_)));

    let mut open = status_with("BLOCKED");
    open.agent_restart_circuit_open = true;
    vm.apply(event(Msg::Status(open)));
    assert_eq!(vm.agent_degraded_reason(), Some("Служба ввода остановлена"));
}

#[test]
fn stopping_a_core_without_owning_its_pid_uses_shutdown_and_reports_disconnect() {
    let (request, message) = Action::StopCore.wire().expect("a shutdown request");
    assert_eq!(request, Request::Shutdown);
    assert!(matches!(message, Msg::Shutdown(_)));

    let mut vm = view();
    vm.on_connected("endpoint");
    let now = Instant::now();
    assert!(
        vm.begin_stop(now),
        "a connected core can be stopped by request"
    );
    assert!(vm.core.is_none(), "the window owns no pid");
    assert!(vm.settings.service.stopping.is_some());

    assert!(
        !vm.on_disconnected("the pipe was closed".to_owned()),
        "stopping must not launch a replacement core"
    );
    assert_eq!(vm.connection_label(), "Служба не запущена");
    assert_eq!(
        vm.settings
            .service
            .line
            .as_ref()
            .map(|line| line.text.as_str()),
        Some("Служба остановлена")
    );
    assert!(vm.settings.service.stopping.is_none());
}

#[test]
fn a_shutdown_timeout_only_falls_back_to_this_windows_pid() {
    let now = Instant::now();
    let mut vm = view();
    vm.on_connected("endpoint");
    assert!(vm.begin_stop(now));
    vm.begin_request(Request::Shutdown, now);
    assert_eq!(
        vm.take_timed_out_shutdown_pid(now + SERVICE_STOP_TIMEOUT),
        None,
        "a service not started by this window must never be killed"
    );
    assert!(
        vm.settings
            .service
            .line
            .as_ref()
            .is_some_and(|line| !line.ok),
        "failure is shown when no graceful shutdown answer arrives"
    );

    let mut owned = view();
    owned.core = Some(own_core(4242));
    owned.on_connected("endpoint");
    assert!(owned.begin_stop(now));
    owned.begin_request(Request::Shutdown, now);
    assert_eq!(
        owned.take_timed_out_shutdown_pid(now + SERVICE_STOP_TIMEOUT),
        Some(4242)
    );
}

#[test]
fn an_unsent_shutdown_request_releases_the_stop_controls() {
    let mut vm = view();
    vm.on_connected("endpoint");
    let now = Instant::now();
    assert!(vm.begin_stop(now));
    let id = vm.begin_request(Request::Shutdown, now);

    vm.on_not_sent(id);

    assert!(vm.settings.service.stopping.is_none());
    assert!(vm
        .settings
        .service
        .line
        .as_ref()
        .is_some_and(|line| !line.ok));
}

// ---------------------------------------------------------------- the service

/// A service this window started: the one shape of core the window may stop.
fn own_core(pid: u32) -> lws_gui::view_model::StartedCore {
    lws_gui::view_model::StartedCore {
        program: std::path::PathBuf::from("lws-core.exe"),
        pid: Some(pid),
    }
}

#[test]
fn the_restart_follows_the_pid_the_window_holds_and_says_so_when_it_holds_none() {
    let mut vm = view();
    let now = Instant::now();

    // No service of the window's own: the honest sentence, and nothing stopped.
    assert_eq!(vm.begin_restart(now), RestartStep::Refused);
    assert_eq!(
        vm.settings
            .service
            .line
            .as_ref()
            .map(|line| line.text.as_str()),
        Some("служба запущена не этим окном — остановите её сами")
    );
    assert_eq!(vm.settings.service.restart, None);

    // A pid the platform never gave is the same story: a guess is not a service to stop.
    vm.core = Some(lws_gui::view_model::StartedCore {
        program: std::path::PathBuf::from("lws-core.exe"),
        pid: None,
    });
    vm.on_connected("endpoint");
    assert_eq!(vm.begin_restart(now), RestartStep::Refused);
    assert_eq!(
        vm.settings
            .service
            .line
            .as_ref()
            .map(|line| line.text.as_str()),
        Some("номер процесса службы неизвестен — остановите её сами")
    );

    // The service the window started itself: that pid, and no other, is the one to stop.
    vm.core = Some(own_core(4242));
    assert_eq!(vm.begin_restart(now), RestartStep::Stop(4242));
    assert_eq!(
        vm.settings.service.restart,
        Some(Restart::Stopping { since: now })
    );
    // A second press while the wait is standing does nothing: there is one restart at a time.
    assert_eq!(vm.begin_restart(now), RestartStep::Refused);

    // And the wait is event-driven: the link dropping is the proof, and it is what makes the
    // window ask for the fresh core.
    assert!(vm.on_disconnected("the pipe was closed".to_owned()));
    assert_eq!(vm.settings.service.restart, Some(Restart::Starting));
    assert!(
        !vm.on_disconnected("the pipe was closed".to_owned()),
        "a link that drops while no restart waits is not a restart"
    );

    // The fresh core coming up is the end of it: the service line says it, and the address
    // sentence «Применить» was waiting for is the one that is left on screen.
    vm.settings.lan.applied = Some(lws_gui::view_model::Outcome::done("Адрес сохранён"));
    vm.on_core_started(Ok(own_core(4243)));
    assert_eq!(
        vm.settings.service.line.as_ref().expect("a sentence").text,
        "Служба перезапущена"
    );
    assert_eq!(
        vm.settings.lan.applied.expect("a sentence").text,
        "Адрес сохранён, служба перезапущена"
    );
}

#[test]
fn a_restart_that_does_not_wait_for_a_service_gives_up_in_words() {
    let mut vm = view();
    let now = Instant::now();
    vm.core = Some(own_core(4242));
    vm.on_connected("endpoint");
    vm.begin_restart(now);

    vm.tick(now + Duration::from_secs(5));
    assert_eq!(
        vm.settings.service.restart,
        Some(Restart::Stopping { since: now }),
        "the wait is still standing before its deadline"
    );

    vm.tick(now + SERVICE_STOP_TIMEOUT);
    assert_eq!(vm.settings.service.restart, None, "the wait has an end");
    let line = vm
        .settings
        .service
        .line
        .as_ref()
        .expect("the honest sentence");
    assert!(!line.ok);
    assert!(
        line.text.contains("служба не остановилась"),
        "{:?}",
        line.text
    );
    assert!(line.text.contains("остановите её сами"), "{:?}", line.text);
}

#[test]
fn a_restart_with_no_link_starts_the_fresh_core_without_waiting() {
    let mut vm = view();
    vm.core = Some(own_core(4242));

    // The window has no link to break, so there is nothing to wait for: the stop would
    // otherwise cost the user the whole wait for a proof that cannot arrive.
    assert_eq!(vm.begin_restart(Instant::now()), RestartStep::Start);
    assert_eq!(vm.settings.service.restart, Some(Restart::Starting));
}

#[test]
fn the_window_really_stops_the_process_it_started() {
    // A process that outlives the test unless it is stopped: `sleep` on Unix, `powershell` on
    // Windows. A host with neither cannot prove anything here and leaves nothing behind.
    let mut command = if cfg!(windows) {
        let mut command = std::process::Command::new("powershell");
        command.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 60"]);
        command
    } else {
        let mut command = std::process::Command::new("sleep");
        command.arg("60");
        command
    };
    let Ok(mut child) = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        eprintln!("no sleeper on this host: the stop path is not exercised");
        return;
    };

    lws_gui::view_model::stop_core(child.id()).expect("the stop is sent");

    // The window does not wait for the process — it waits for the link to drop — so this test
    // waits for it instead, with a deadline rather than the minute the sleeper was given.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut stopped = child.try_wait().expect("the child can be asked");
    while stopped.is_none() {
        assert!(
            Instant::now() < deadline,
            "the process was still running 20 s after the stop"
        );
        std::thread::sleep(Duration::from_millis(20));
        stopped = child.try_wait().expect("the child can be asked");
    }
    assert!(
        stopped.is_some(),
        "the core process the window stopped is gone"
    );
}

#[test]
fn the_created_config_file_is_reported_once_in_a_session() {
    let mut vm = view();
    vm.config.config_was_missing = true;
    assert!(
        !vm.settings.first_run,
        "the window has not started anything yet"
    );

    // The core the window started writes the file on its first run; that is worth one
    // sentence, on the screen where the file is now visible.
    vm.on_core_started(Ok(own_core(10)));
    assert!(vm.settings.first_run);
    vm.local(&Action::DismissFirstRun);
    assert!(!vm.settings.first_run);

    // A restart of the same window is not a first run, and a second window in this session
    // would be a session of its own: the sentence does not come back here.
    vm.on_core_started(Ok(own_core(11)));
    assert!(
        !vm.settings.first_run,
        "once per session, not once per start"
    );
}

#[test]
fn connection_after_first_run_reloads_the_created_config_for_settings() {
    let dir = settings_dir("192.168.1.37");
    let file = dir.join("config/config.toml");
    std::fs::remove_file(&file).expect("start without config");
    let paths = paths_of(&dir);
    let mut vm = view();
    vm.config = lws_gui::view_model::LocalConfig::load(&paths);
    assert!(vm.config.config_was_missing);

    std::fs::write(
        &file,
        "config_version = 2\n\n[network]\nlisten_address = \"192.168.1.37\"\n",
    )
    .expect("the core writes the first-run config before accepting the GUI");
    vm.on_connected("endpoint");

    assert_eq!(vm.config.listen_address, "192.168.1.37");
    assert!(!vm.config.config_was_missing);
    assert!(vm.settings.first_run_reported);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_machine_that_already_had_its_configuration_says_nothing_about_a_first_run() {
    let mut vm = view();
    vm.on_core_started(Ok(own_core(12)));
    assert!(!vm.settings.first_run);
    assert!(!vm.settings.first_run_reported);
}

// ---------------------------------------------------------------- the bridge

/// «Применить» through the window's own wiring: the file is written, and the address is only
/// in use once the service behind it is the fresh one — which this window did not start.
#[test]
fn the_address_button_through_the_bridge_writes_and_says_why_it_cannot_restart() {
    let mut bridge = Bridge::new(
        view(),
        GuiClient::start(None, Launch::with_flags(Vec::new())),
    );
    let dir = settings_dir("127.0.0.1");
    let view = bridge.view_mut();
    view.config.paths = Some(paths_of(&dir));
    view.config.listen_address = "127.0.0.1".to_owned();
    view.settings.lan.candidates = vec!["192.168.1.10".to_owned()];
    let now = Instant::now();

    bridge.dispatch(Action::SelectLanAddress("192.168.1.10".to_owned()), now);
    bridge.dispatch(Action::ApplyLanAddress, now);

    let written = config::Config::load(paths_of(&dir).config_file()).expect("a valid config.toml");
    assert_eq!(written.network.listen_address, "192.168.1.10");
    assert_eq!(
        bridge
            .view()
            .settings
            .lan
            .applied
            .as_ref()
            .expect("a sentence")
            .text,
        "Адрес сохранён"
    );
    // The window holds no service of its own here, so the answer is the honest sentence and
    // not a process it does not know.
    assert_eq!(
        bridge
            .view()
            .settings
            .service
            .line
            .as_ref()
            .expect("a sentence")
            .text,
        "служба запущена не этим окном — остановите её сами"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
