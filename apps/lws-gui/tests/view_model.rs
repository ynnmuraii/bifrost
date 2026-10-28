//! The view model, on its own: no window, no core, no display.
//!
//! These are the tests the build spec §6 asks for: a `Status` fills the screen and a changed
//! focus changes «куда идёт ввод»; a pairing prompt is answered with its own attempt id; a
//! document older than the one the editor holds does not become the editor's base; a
//! `LAYOUT_CONFLICT` reloads and retries once; dropped drafts save latest-wins; the document round-trips
//! through the configuration schema; and a disconnect retains geometry without a stale target.

mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use lws_gui::view_model::layout::LinkState;
use lws_gui::view_model::{
    Action, Bridge, CliArgs, GuiClient, Launch, Modal, Request, Restart, RestartStep, ServiceStart,
    Side, ViewModel, SERVICE_START_TIMEOUT, SERVICE_STOP_TIMEOUT,
};
use protocol::ipc::gui_frame::Msg;
use support::{
    answer, attempt_id, event, pairing_prompt, status_with, two_node_doc, view, PEER_NODE,
    SELF_NODE,
};

#[test]
fn audit_disconnect_forgets_route_but_retains_map_geometry() {
    let mut vm = view();
    vm.on_connected("test");
    vm.apply(event(Msg::TopologyDoc(two_node_doc(1))));
    vm.apply(event(Msg::Status(status_with("REMOTE"))));
    assert!(vm.map_model().nodes.iter().any(|node| node.target));
    let placement = vm.editor.placement.clone();
    vm.on_disconnected("core stopped".into());
    let map = vm.map_model();
    assert_eq!(map.nodes.len(), 2);
    assert_eq!(vm.editor.placement, placement);
    assert!(map.nodes.iter().all(|node| !node.target));
    vm.on_connected("test");
    assert!(vm.map_model().nodes.iter().all(|node| !node.target));
    vm.apply(event(Msg::Status(status_with("LOCAL"))));
    assert!(vm.map_model().nodes.iter().any(|node| node.target));
}

#[test]
fn remote_unpair_notice_is_deduplicated_by_monotonic_id_not_name() {
    let mut vm = view();
    let mut status = status_with("LOCAL");
    status.unpair_notice = "Peer".into();
    vm.apply(event(Msg::Status(status.clone())));
    assert!(vm.unpair_notice.is_none(), "zero is not a notice");

    status.unpair_notice_id = 7;
    vm.apply(event(Msg::Status(status.clone())));
    assert_eq!(vm.unpair_notice.as_deref(), Some("Peer"));
    vm.local(&Action::DismissUnpairNotice);
    vm.apply(event(Msg::Status(status.clone())));
    assert!(
        vm.unpair_notice.is_none(),
        "a push must not reopen a dismissed notice"
    );

    status.unpair_notice_id = 6;
    status.unpair_notice = "Other peer".into();
    vm.apply(event(Msg::Status(status.clone())));
    assert!(
        vm.unpair_notice.is_none(),
        "older notices stay acknowledged"
    );

    vm.on_connected("test");
    status.unpair_notice_id = 7;
    vm.apply(event(Msg::Status(status.clone())));
    assert!(
        vm.unpair_notice.is_none(),
        "reconnect retains the high-water mark"
    );
    status.unpair_notice_id = 8;
    status.unpair_notice = "Peer".into();
    vm.apply(event(Msg::Status(status.clone())));
    assert_eq!(
        vm.unpair_notice.as_deref(),
        Some("Peer"),
        "same name, new event"
    );
    vm.local(&Action::DismissUnpairNotice);
    status.unpair_notice_id = 9;
    status.unpair_notice.clear();
    vm.apply(event(Msg::Status(status)));
    assert!(vm.unpair_notice.is_none(), "empty notices are not shown");

    let mut restarted = status_with("LOCAL");
    vm.apply(event(Msg::Status(restarted.clone())));
    assert!(vm.unpair_notice.is_none());
    restarted.unpair_notice_id = 1;
    restarted.unpair_notice = "Peer".into();
    vm.apply(event(Msg::Status(restarted)));
    assert_eq!(
        vm.unpair_notice.as_deref(),
        Some("Peer"),
        "restarted core has a fresh sequence"
    );

    for (lang, expected) in [
        (
            ui_text::Lang::Ru,
            "Peer удалил сопряжение с этим компьютером",
        ),
        (ui_text::Lang::En, "Peer removed pairing with this computer"),
    ] {
        assert_eq!(
            ui_text::fill(lang, ui_text::Key::RemoteUnpairNotice, &[&"Peer"]),
            expected
        );
    }
}

#[test]
fn suspected_unpair_requires_an_offline_flagged_member_and_uses_local_revoke() {
    let mut vm = view();
    for (online, flagged, expected) in [
        (true, false, false),
        (true, true, false),
        (false, false, false),
        (false, true, true),
    ] {
        let mut status = status_with("LOCAL");
        let peer = status
            .members
            .iter_mut()
            .find(|m| m.node_id == PEER_NODE)
            .unwrap();
        peer.online = online;
        peer.pairing_maybe_removed = flagged;
        vm.apply(event(Msg::Status(status)));
        let peers = vm.peers();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].pairing_maybe_removed, expected);
        assert_eq!(peers[0].node_id, PEER_NODE);
    }
    let action = Action::RevokePeer(lws_gui::view_model::node_id_bytes(PEER_NODE));
    let Some((Request::RevokePeer, Msg::RevokePeer(revoke))) = action.wire() else {
        panic!("fallback must use the existing local revoke");
    };
    assert_eq!(
        revoke.peer_node_id,
        lws_gui::view_model::node_id_bytes(PEER_NODE)
    );
    assert_eq!(
        ui_text::text(ui_text::Lang::Ru, ui_text::Key::PairingMaybeRemoved),
        "возможно, сопряжение удалено на том компьютере"
    );
    assert_eq!(
        ui_text::text(ui_text::Lang::En, ui_text::Key::PairingMaybeRemoved),
        "pairing may have been removed on that computer"
    );
}

#[test]
fn game_guard_pill_is_local_and_shows_the_reason_in_both_languages() {
    let mut vm = view();
    vm.apply(event(Msg::TopologyDoc(two_node_doc(1))));
    for lang in [ui_text::Lang::Ru, ui_text::Lang::En] {
        vm.look.language = lang;
        for reason in [
            "fullscreen",
            "cursor_confined",
            "presentation",
            "app_list",
            "always",
            "",
        ] {
            let mut status = status_with("LOCAL");
            status.game_guard_reason = reason.into();
            status.game_guard_mode = "auto".into();
            vm.apply(event(Msg::Status(status)));
            let map = vm.map_model();
            let local = map
                .nodes
                .iter()
                .find(|node| node.node_id == SELF_NODE)
                .expect("local node");
            assert_eq!(
                local.game_guard_badge,
                ui_text::game_guard_state(lang, reason)
            );
            assert!(map
                .nodes
                .iter()
                .filter(|node| node.node_id != SELF_NODE)
                .all(|node| node.game_guard_badge.is_empty()));
        }
    }
    let set = protocol::ipc::SetGameGuard {
        mode: "always".into(),
        ..Default::default()
    };
    assert!(matches!(
        Action::SetGameGuard(set).wire(),
        Some((Request::SetGameGuard, Msg::SetGameGuard(_)))
    ));
}

#[test]
fn coordinator_heartbeat_never_overwrites_user_feedback_or_logs() {
    let mut vm = view();
    let id = vm.begin_request(Request::UpdateAll, Instant::now());
    vm.apply(answer(
        id,
        Msg::OpResult(protocol::ipc::OpResult {
            ok: false,
            error_code: "REMOTE_UPDATES_DISABLED".into(),
        }),
    ));
    let failure = vm.outcome.as_ref().expect("user refusal").text.clone();
    let log = vm.log.clone();

    // The bridge reserves this ID for out-of-band coordinator heartbeats.
    vm.apply(answer(
        u64::MAX,
        Msg::OpResult(protocol::ipc::OpResult {
            ok: true,
            error_code: String::new(),
        }),
    ));
    vm.on_not_sent(u64::MAX);

    assert_eq!(
        vm.outcome.as_ref().expect("user refusal remains").text,
        failure
    );
    assert_eq!(vm.log, log);
}

#[test]
fn clipboard_destinations_are_online_trusted_remote_members_only() {
    let mut vm = view();
    let mut status = status_with("LOCAL");
    status.members.push(protocol::ipc::MemberStatus {
        node_id: "cc".repeat(32),
        display_name: "offline".to_owned(),
        online: false,
        ..Default::default()
    });
    status.discovered_peers.push(protocol::ipc::DiscoveredPeer {
        node_id: "dd".repeat(32),
        display_name: "unpaired".to_owned(),
        ..Default::default()
    });
    vm.apply(event(Msg::Status(status.clone())));
    assert_eq!(
        vm.clipboard_targets()
            .map(|member| member.node_id.as_str())
            .collect::<Vec<_>>(),
        vec![PEER_NODE],
    );
    status.local_node_id.clear();
    vm.apply(event(Msg::Status(status)));
    assert_eq!(
        vm.clipboard_targets().count(),
        0,
        "unknown local identity must not become a target"
    );
}

#[test]
fn clipboard_send_waits_for_remote_verdict_without_extending_other_requests() {
    let mut vm = view();
    let now = Instant::now();
    let send = vm.begin_request(Request::SendClipboardHistory, now);
    let status = vm.begin_request(Request::GetStatus, now);
    assert_eq!(
        vm.inflight
            .iter()
            .find(|request| request.id == send)
            .unwrap()
            .deadline,
        now + Duration::from_secs(32),
    );
    assert_eq!(
        vm.inflight
            .iter()
            .find(|request| request.id == status)
            .unwrap()
            .deadline,
        now + lws_gui::view_model::REQUEST_TIMEOUT,
    );
}

#[test]
fn clipboard_addressed_send_and_policy_requests_preserve_wire_values() {
    let Some((request, Msg::SendClipboardHistoryItem(item))) = (Action::SendClipboardHistory {
        id: 42,
        peer_node_id: PEER_NODE.to_owned(),
    })
    .wire() else {
        panic!("addressed history request");
    };
    assert_eq!(request, Request::SendClipboardHistory);
    assert_eq!(item.id, 42);
    assert_eq!(
        item.peer_node_id,
        lws_gui::view_model::node_id_bytes(PEER_NODE)
    );
    for policy in ["follow", "broadcast"] {
        let Some((request, Msg::SetClipboardPolicy(value))) = (Action::SetClipboardPolicy {
            policy: policy.to_owned(),
            settle_ms: 1200,
        })
        .wire() else {
            panic!("persistent routing request");
        };
        assert_eq!(request, Request::SetClipboardPolicy);
        assert_eq!(value.policy, policy);
        assert_eq!(value.settle_ms, 1200);
    }
    let mut vm = view();
    let id = vm.begin_request(Request::SetClipboardPolicy, Instant::now());
    verdict(&mut vm, id, None);
    assert!(vm
        .take_outgoing()
        .iter()
        .any(|(_, msg)| matches!(msg, Msg::GetStatus(_))));
    let mut status = status_with("LOCAL");
    status.clipboard_policy = "broadcast".to_owned();
    status.clipboard_settle_ms = 1200;
    vm.apply(event(Msg::Status(status)));
    assert_eq!(vm.status.as_ref().unwrap().clipboard_policy, "broadcast");
    assert_eq!(vm.status.as_ref().unwrap().clipboard_settle_ms, 1200);
}

#[test]
fn clipboard_send_results_keep_destination_name_and_localize_success_and_failure() {
    for lang in [ui_text::Lang::Ru, ui_text::Lang::En] {
        for ok in [true, false] {
            let mut vm = view();
            vm.look.language = lang;
            vm.apply(event(Msg::Status(status_with("LOCAL"))));
            let id = vm.begin_clipboard_send(PEER_NODE, Instant::now());
            let mut changed = status_with("LOCAL");
            changed.members.clear();
            vm.apply(event(Msg::Status(changed)));
            vm.apply(answer(
                id,
                Msg::OpResult(protocol::ipc::OpResult {
                    ok,
                    error_code: if ok { "" } else { "linux-box: offline" }.to_owned(),
                }),
            ));
            let expected = ui_text::fill_named(
                lang,
                if ok {
                    ui_text::Key::ClipboardSentTo
                } else {
                    ui_text::Key::ClipboardSendFailed
                },
                &[("target", &"linux-box"), ("detail", &"linux-box: offline")],
            );
            assert_eq!(vm.outcome.as_ref().unwrap().text, expected);
            assert!(vm.inflight.is_empty());
        }
    }
}

#[test]
fn clipboard_history_actions_map_to_metadata_core_copy_and_clear_requests() {
    assert!(matches!(
        Action::RefreshClipboardHistory.wire(),
        Some((Request::GetClipboardHistory, Msg::GetClipboardHistory(_)))
    ));
    let Some((request, Msg::GetClipboardHistoryItem(item))) =
        Action::CopyClipboardHistory(42).wire()
    else {
        panic!("copy must request a history item");
    };
    assert_eq!(request, Request::CopyClipboardHistory);
    assert_eq!(item.id, 42);
    assert!(item.copy_to_local, "the core must apply the text locally");
    assert!(matches!(
        Action::ClearClipboardHistory.wire(),
        Some((
            Request::ClearClipboardHistory,
            Msg::ClearClipboardHistory(_)
        ))
    ));
}

#[test]
fn clipboard_history_metadata_is_newest_first_and_full_text_is_not_retained() {
    let mut vm = view();
    vm.apply(event(Msg::ClipboardHistory(
        protocol::ipc::ClipboardHistory {
            items: vec![
                protocol::ipc::ClipboardHistoryEntry {
                    id: 1,
                    direction: protocol::ipc::ClipboardHistoryDirection::Sent as i32,
                    peer_name: "laptop".to_owned(),
                    time_unix_ms: 100,
                    size_bytes: 6,
                    preview: "older".to_owned(),
                },
                protocol::ipc::ClipboardHistoryEntry {
                    id: 2,
                    direction: protocol::ipc::ClipboardHistoryDirection::Received as i32,
                    peer_name: "desktop".to_owned(),
                    time_unix_ms: 200,
                    size_bytes: 1_048_576,
                    preview: "newer".to_owned(),
                },
            ],
        },
    )));
    assert_eq!(
        vm.clipboard_history
            .iter()
            .map(|item| item.id)
            .collect::<Vec<_>>(),
        vec![2, 1],
    );
    let latest = vm.clipboard_history[0].clone();
    assert_eq!(latest.peer_name, "desktop");
    assert_eq!(latest.time_unix_ms, 200);
    assert_eq!(latest.size_bytes, 1_048_576);
    assert_eq!(
        latest.direction,
        protocol::ipc::ClipboardHistoryDirection::Received as i32
    );
    vm.apply(event(Msg::ClipboardHistoryItem(
        protocol::ipc::ClipboardHistoryItem {
            id: 2,
            text: "not retained by the GUI".to_owned(),
            chunk_index: 0,
            chunk_count: 1,
        },
    )));
    assert_eq!(vm.clipboard_history[0], latest);
}

#[test]
fn opening_clipboard_history_and_clearing_it_refresh_metadata() {
    let mut vm = view();
    vm.local(&Action::ToggleModal(Modal::Clip));
    let requests = vm.take_outgoing();
    assert_eq!(requests.len(), 1);
    assert!(matches!(&requests[0].1, Msg::GetClipboardHistory(_)));
    vm.local(&Action::ToggleModal(Modal::Clip));
    assert!(vm.take_outgoing().is_empty(), "closing does not refresh");

    vm.clipboard_history
        .push(protocol::ipc::ClipboardHistoryEntry {
            id: 42,
            ..Default::default()
        });
    let id = vm.begin_request(Request::ClearClipboardHistory, Instant::now());
    verdict(&mut vm, id, None);
    assert!(
        vm.clipboard_history.is_empty(),
        "clear acknowledgment removes stale metadata"
    );
    let requests = vm.take_outgoing();
    assert_eq!(requests.len(), 1);
    assert!(matches!(&requests[0].1, Msg::GetClipboardHistory(_)));
}

/// The window never asks for a refresh: the core pushes the new entry.
#[test]
fn a_new_transfer_reaches_the_history_without_a_refresh_request() {
    let mut vm = view();
    vm.apply(event(Msg::ClipboardHistory(
        protocol::ipc::ClipboardHistory {
            items: vec![protocol::ipc::ClipboardHistoryEntry {
                id: 3,
                direction: protocol::ipc::ClipboardHistoryDirection::Sent as i32,
                peer_name: "laptop".to_owned(),
                time_unix_ms: 300,
                size_bytes: 86,
                preview: "fresh".to_owned(),
            }],
        },
    )));
    assert_eq!(vm.clipboard_history.len(), 1);
    assert_eq!(vm.clipboard_history[0].id, 3);
    assert!(
        vm.take_outgoing().is_empty(),
        "the pushed entry needs no request of its own"
    );
}

#[test]
fn clipboard_history_is_session_scoped_and_failed_clear_keeps_metadata() {
    let mut vm = view();
    vm.clipboard_history
        .push(protocol::ipc::ClipboardHistoryEntry {
            id: 42,
            ..Default::default()
        });
    let id = vm.begin_request(Request::ClearClipboardHistory, Instant::now());
    verdict(&mut vm, id, Some("FAILED"));
    assert_eq!(vm.clipboard_history.len(), 1);
    assert!(vm.take_outgoing().is_empty());
    vm.on_disconnected("core stopped".to_owned());
    assert!(vm.clipboard_history.is_empty());
}

#[test]
fn clipboard_history_copy_acknowledgment_is_local_and_localized() {
    let mut vm = view();
    vm.look.language = ui_text::Lang::En;
    let id = vm.begin_request(Request::CopyClipboardHistory, Instant::now());
    verdict(&mut vm, id, None);
    assert!(
        vm.take_outgoing().is_empty(),
        "copy must not request a peer transfer"
    );
    assert_eq!(
        Request::CopyClipboardHistory.label(vm.look.language),
        ui_text::text(ui_text::Lang::En, ui_text::Key::CopyFromTheHistory),
    );
    assert_eq!(
        vm.outcome.as_ref().unwrap().text,
        ui_text::text(ui_text::Lang::En, ui_text::Key::TheTextIsCopied),
    );
}

/// The «служба не запущена» banner: one button, and the wait it belongs to.
#[test]
fn the_service_banner_waits_for_the_core_it_asked_for() {
    let mut vm = view();
    vm.on_disconnected("no core".to_owned());
    assert_eq!(vm.service_start(), &ServiceStart::Idle);
    assert_eq!(vm.service_start().button(), ui_text::Key::Start);
    assert!(vm.service_start().reason().is_none());
    assert!(!vm.waiting_for_core());

    let now = Instant::now();
    vm.begin_service_start(now);
    assert!(vm.waiting_for_core(), "the banner shows the wait");
    assert_eq!(
        vm.service_start().button(),
        ui_text::Key::Start,
        "one button, and it still says «Запустить» while it waits"
    );
    assert!(
        vm.next_wakeup(now)
            .is_some_and(|after| after <= SERVICE_START_TIMEOUT),
        "the wait has an end"
    );

    // No core answered in time: the wait ends by itself, with the reason and «Ещё раз».
    vm.tick(now + SERVICE_START_TIMEOUT);
    let ServiceStart::Failed { reason } = vm.service_start().clone() else {
        panic!("a start that ran out of time has failed");
    };
    assert!(
        reason.contains(&SERVICE_START_TIMEOUT.as_secs().to_string()),
        "{reason}"
    );
    assert_eq!(vm.service_start().button(), ui_text::Key::OnceMore);
    assert!(!vm.waiting_for_core());
    assert!(vm.next_wakeup(now + SERVICE_START_TIMEOUT).is_none());

    // «Ещё раз» starts again, and this time the core answers: the banner goes quiet.
    vm.begin_service_start(now + SERVICE_START_TIMEOUT);
    assert!(vm.waiting_for_core());
    vm.on_connected("endpoint");
    assert!(vm.connection.is_connected());
    assert_eq!(
        vm.service_start(),
        &ServiceStart::Idle,
        "a core that answers leaves the banner nothing to say"
    );
    assert!(!vm.waiting_for_core());
}

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
    status.rtt_jitter_us = 700;
    vm.apply(event(Msg::Status(status)));
    assert_eq!(
        vm.delay_line().as_deref(),
        Some("Задержка: 12 мс, джиттер 0 мс"),
        "both numbers come from the core's window; a sub-millisecond jitter rounds down"
    );

    let mut status = status_with("REMOTE");
    status.rtt_p95_us = 12_400;
    status.rtt_jitter_us = 2_600;
    vm.apply(event(Msg::Status(status)));
    assert_eq!(
        vm.delay_line().as_deref(),
        Some("Задержка: 12 мс, джиттер 2 мс")
    );

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
    // The fixture's core knows its own id; this scenario is a core that has not answered
    // with one (and predates the field), which is when the window must invent nothing.
    status.local_node_id.clear();
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

    // The core says how long is left once; the window counts the rest down by its own clock.
    let left = vm
        .pairing
        .window_deadline
        .expect("a countdown")
        .saturating_duration_since(Instant::now());
    assert!(
        left > Duration::from_secs(116) && left <= Duration::from_secs(118),
        "{left:?}"
    );

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
fn unpair_topology_push_removes_the_card_and_preserves_exclusions_in_edits() {
    let mut vm = view();
    vm.apply(event(Msg::TopologyDoc(two_node_doc(7))));
    assert_eq!(vm.map_model().nodes.len(), 2);
    let mut removed = two_node_doc(8);
    removed.members.retain(|member| member.node_id == SELF_NODE);
    removed.links.clear();
    removed.forgotten_node_ids.push(PEER_NODE.to_owned());
    vm.apply(event(Msg::TopologyDoc(removed)));
    assert_eq!(vm.map_model().nodes.len(), 1);
    assert!(vm.map_model().links.is_empty());
    let layout: config::TopologyConfig = toml::from_str(&vm.layout_toml()).unwrap();
    assert_eq!(layout.forgotten_nodes, [PEER_NODE]);
}

/// Batch-8 audit: the outcome banner is about the core that answered, so losing that core
/// takes the sentence with it instead of leaving it under a «Служба не запущена» header.
#[test]
fn a_lost_service_takes_its_outcome_sentence_with_it() {
    let mut vm = view();
    vm.apply(event(Msg::Status(status_with("LOCAL"))));
    vm.apply(event(Msg::TopologyDoc(two_node_doc(7))));
    let id = vm.begin_request(Request::Claim, Instant::now());
    vm.apply(answer(
        id,
        Msg::OpResult(protocol::ipc::OpResult {
            ok: true,
            error_code: String::new(),
        }),
    ));
    assert!(
        vm.outcome.as_ref().is_some_and(|outcome| outcome.ok),
        "the success is shown while the service is there"
    );

    vm.on_disconnected("the service is gone".to_owned());
    assert!(
        vm.outcome.is_none(),
        "a sentence about a service that is gone must not stay on screen"
    );

    // And a new connection starts clean too: a sentence from a previous core would describe
    // the wrong one.
    vm.on_connected("gui.sock");
    assert!(vm.outcome.is_none());
}

/// The editor's index of a member, for reading `placement` and `shapes`.
fn index_of(vm: &ViewModel, node: &str) -> usize {
    vm.editor
        .nodes
        .iter()
        .position(|candidate| candidate.node_id == node)
        .expect("the node is in the document")
}

/// A document with an `auto` link between the two computers: this one's right edge to the
/// other's left edge.
fn linked_doc(revision: u64) -> protocol::ipc::TopologyDoc {
    let end = |node: &str, edge: &str| {
        Some(protocol::ipc::EndpointView {
            node_id: node.to_owned(),
            display_id: "auto".to_owned(),
            edge: edge.to_owned(),
            segment_start: 0.0,
            segment_end: 0.0,
        })
    };
    let mut doc = two_node_doc(revision);
    doc.links = vec![protocol::ipc::LinkView {
        id: "link-0".to_owned(),
        from: end(SELF_NODE, "RIGHT"),
        to: end(PEER_NODE, "LEFT"),
        bidirectional: true,
        active: true,
        inactive_reason: String::new(),
        auto_displays: vec!["\\\\.\\DISPLAY1".to_owned(), "eDP-1".to_owned()],
    }];
    doc
}

#[test]
fn a_dragged_computer_becomes_a_document_only_on_drop() {
    let mut vm = view();
    vm.on_connected("endpoint");
    vm.apply(event(Msg::Status(status_with("LOCAL"))));
    vm.apply(event(Msg::TopologyDoc(two_node_doc(7))));
    assert!(!vm.editor.dirty());
    assert!(vm.take_outgoing().is_empty(), "nothing to save yet");
    assert!(
        vm.map_model().links.is_empty(),
        "the document links nothing: the two are drawn apart"
    );

    // Drag the other computer to just right of this one, a little off.
    let (this, peer) = (index_of(&vm, SELF_NODE), index_of(&vm, PEER_NODE));
    let me = vm.editor.placement[this];
    let width = vm.editor.shapes[this].w;
    vm.local(&Action::MoveNode {
        node_id: PEER_NODE.to_owned(),
        x: me.x + width + 5.0,
        y: me.y + 1.0,
    });
    assert!(vm.editor.dirty());
    assert!((vm.editor.placement[peer].x - (me.x + width + 5.0)).abs() < 1e-6);
    assert_eq!(
        vm.editor.placement[peer].y,
        me.y + 1.0,
        "tangent offset is retained"
    );
    assert_eq!(vm.map_model().links.len(), 1, "the preview shows the link");
    assert_eq!(vm.editor.revision, 7, "and nothing was applied");

    assert!(vm.take_outgoing().is_empty(), "drag frames never save");
    vm.local(&Action::DropNode(PEER_NODE.to_owned()));
    let outgoing = vm.take_outgoing();
    assert_eq!(outgoing.len(), 1);
    let Msg::UpdateTopology(protocol::ipc::UpdateTopology {
        layout_toml,
        expected_revision,
        expected_author,
    }) = &outgoing[0].1
    else {
        panic!("a drop is saved through UpdateTopology");
    };
    assert_eq!(*expected_revision, 7);
    assert_eq!(
        expected_author,
        &lws_gui::view_model::node_id_bytes(SELF_NODE)
    );
    let document = String::from_utf8(layout_toml.clone()).expect("text");
    assert_eq!(document, vm.layout_toml());

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
    assert!(!link.from.is_auto() && !link.to.is_auto());
    assert_eq!(link.from.display, vm.editor.nodes[this].displays[0].id);
    assert_eq!(link.to.display, vm.editor.nodes[peer].displays[0].id);
    let moved = topology
        .displays
        .iter()
        .find(|display| display.node == format!("peer:{PEER_NODE}"))
        .expect("the other computer's display is in the document");
    assert_eq!(
        moved.logical,
        [0.0, 0.0, 1600.0, 900.0],
        "the displays keep their own coordinates: a drag edits links, not monitors"
    );

    // 2. The acceptance test: the same text is a valid `[topology]` section of a config file.
    let config = config::Config::from_str(&as_config_file(&document))
        .expect("the editor emits a valid section");
    config.validate().expect("and a valid document");
    assert_eq!(config.topology.links.len(), 1);

    assert!(
        vm.take_outgoing().is_empty(),
        "drawing never resends a save"
    );
}

#[test]
fn the_applied_links_are_drawn_as_the_whole_desktop_edge_and_survive_untouched() {
    let mut vm = view();
    vm.apply(event(Msg::Status(status_with("LOCAL"))));
    vm.apply(event(Msg::TopologyDoc(linked_doc(7))));
    let model = vm.map_model();
    assert_eq!(model.links.len(), 1, "{model:?}");
    assert!(
        model.links[0].whole_desktop,
        "an `auto` link is the whole desktop"
    );
    assert_eq!(model.links[0].state, LinkState::Idle);
    assert!(!model.dirty);
    // No draft: the document is sent back exactly as it was, link id included.
    let topology: config::TopologyConfig = toml::from_str(&vm.layout_toml()).expect("parse");
    assert_eq!(topology.links.len(), 1);
    assert_eq!(topology.links[0].id, "link-0");

    // Dragging the peer away unlinks the pair; putting it back restores the draft to clean.
    let before = vm.editor.placement.clone();
    vm.local(&Action::MoveNode {
        node_id: PEER_NODE.to_owned(),
        x: 700.0,
        y: 400.0,
    });
    assert!(vm.editor.dirty());
    assert!(vm.map_model().links.is_empty());
    let topology: config::TopologyConfig = toml::from_str(&vm.layout_toml()).expect("parse");
    assert!(
        topology.links.is_empty(),
        "a pair that no longer faces loses its link"
    );
    vm.local(&Action::CancelDrag);
    assert_eq!(vm.editor.placement, before);
    assert!(vm.take_outgoing().is_empty());

    // A link whose display is not reported stays in the document and is drawn as inactive.
    let mut doc = linked_doc(8);
    doc.links[0].active = false;
    doc.links[0].inactive_reason = "eDP-1".to_owned();
    vm.apply(event(Msg::TopologyDoc(doc)));
    assert_eq!(vm.map_model().links[0].state, LinkState::Inactive);
}

#[test]
fn unresolved_auto_link_remains_visible_with_a_reason_and_recovers_when_resolved() {
    let mut vm = view();
    vm.apply(event(Msg::Status(status_with("LOCAL"))));
    let reason = "auto layout link unresolved: a node it names reports no display on that edge";
    let mut doc = linked_doc(8);
    doc.links[0].active = false;
    doc.links[0].inactive_reason = reason.to_owned();
    doc.links[0].auto_displays.clear();
    vm.apply(event(Msg::TopologyDoc(doc)));
    let model = vm.map_model();
    assert_eq!(model.links.len(), 1);
    assert!(model.links[0].whole_desktop);
    assert_eq!(model.links[0].state, LinkState::Inactive);
    assert!(vm
        .inactive_link_lines()
        .iter()
        .any(|line| line.contains(reason)));
    let topology: config::TopologyConfig = toml::from_str(&vm.layout_toml()).expect("parse");
    assert_eq!(topology.links.len(), 1);
    assert_eq!(topology.links[0].from.display, config::AUTO_DISPLAY);
    assert_eq!(topology.links[0].to.display, config::AUTO_DISPLAY);

    vm.apply(event(Msg::TopologyDoc(linked_doc(9))));
    assert_eq!(vm.map_model().links[0].state, LinkState::Idle);
    assert!(vm.inactive_link_lines().is_empty());
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
        forgotten_node_ids: Vec::new(),
        members: two_node_doc(0).members,
        links: Vec::new(),
    })));
    let file = as_config_file(&vm.layout_toml());
    let config = config::Config::from_str(&file).expect("revision 0 is a valid floor");
    config.validate().expect("and needs no author");
    assert_eq!(config.topology.revision, 0);
}

#[test]
fn a_disconnect_keeps_geometry_but_forgets_the_last_input_target() {
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
        ui_text::text(vm.look.language, ui_text::Key::NoDataFromTheService)
    );
    assert!(vm.focus_detail().is_none());
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
fn shortcut_changes_wait_for_authoritative_registration_feedback() {
    let mut vm = view();
    vm.apply(event(Msg::Status(status_with("LOCAL"))));
    let (request, message) = Action::SetHotkey {
        action: protocol::ipc::HotkeyAction::SwitchNext as i32,
        chord: "Ctrl+F9".to_owned(),
    }
    .wire()
    .expect("hotkeys are written only by the service");
    assert_eq!(request, Request::SetHotkey);
    assert!(matches!(message, Msg::SetHotkey(_)));
    let id = vm.begin_request(request, Instant::now());
    let bindings = vec![protocol::ipc::HotkeyBinding {
        action: protocol::ipc::HotkeyAction::SwitchNext as i32,
        chord: "Ctrl+F9".to_owned(),
        available: true,
        detail: String::new(),
    }];
    vm.apply(answer(
        id,
        Msg::HotkeysResult(protocol::ipc::HotkeysResult {
            success: true,
            detail: String::new(),
            bindings: bindings.clone(),
        }),
    ));
    assert!(vm.inflight.is_empty());
    assert_eq!(vm.status.as_ref().unwrap().hotkeys, bindings);
    vm.apply(event(Msg::HotkeysResult(protocol::ipc::HotkeysResult {
        success: false,
        detail: "The portal refused this shortcut".to_owned(),
        bindings: Vec::new(),
    })));
    assert_eq!(vm.status.as_ref().unwrap().hotkeys, bindings);
    assert_eq!(
        vm.hotkeys_error.as_deref(),
        Some("The portal refused this shortcut")
    );
}

#[test]
fn appearance_and_other_modals_are_mutually_exclusive_and_share_close() {
    let mut vm = view();
    vm.local(&Action::ToggleModal(Modal::Settings));
    vm.local(&Action::ToggleAppearance);
    assert!(vm.appearance_open);
    assert_eq!(vm.modal, None);
    vm.local(&Action::CloseModal);
    assert!(!vm.appearance_open);
    vm.local(&Action::ToggleModal(Modal::Appearance));
    assert!(vm.appearance_open);
    assert_eq!(vm.modal, None);
    vm.local(&Action::ToggleModal(Modal::Settings));
    assert!(!vm.appearance_open);
    assert_eq!(vm.modal, Some(Modal::Settings));
    vm.local(&Action::ToggleAppearance);
    vm.local(&Action::ToggleAppearance);
    assert!(!vm.appearance_open);
    assert_eq!(vm.modal, None);
}

#[test]
fn appearance_toggles_send_both_values_and_follow_authoritative_status() {
    let mut vm = view();
    vm.apply(event(Msg::Status(status_with("LOCAL"))));
    let (request, message) = Action::SetAppearance {
        edge_effects: false,
        hide_remote_cursor: true,
    }
    .wire()
    .expect("appearance is a service setting");
    assert_eq!(request, Request::SetAppearance);
    let Msg::SetAppearance(settings) = message else {
        panic!("appearance action must send SetAppearance");
    };
    assert!(!settings.edge_effects);
    assert!(settings.hide_remote_cursor);
    let id = vm.begin_request(request, Instant::now());
    assert!(!vm.can_set_appearance());
    assert!(vm.status.as_ref().unwrap().edge_effects);
    vm.apply(answer(
        id,
        Msg::OpResult(protocol::ipc::OpResult {
            ok: true,
            error_code: String::new(),
        }),
    ));
    let refresh = vm
        .inflight
        .iter()
        .find(|r| r.request == Request::GetStatus)
        .unwrap()
        .id;
    let mut status = status_with("LOCAL");
    status.edge_effects = false;
    status.hide_remote_cursor = true;
    vm.apply(answer(refresh, Msg::Status(status)));
    assert!(!vm.status.as_ref().unwrap().edge_effects);
    assert!(vm.status.as_ref().unwrap().hide_remote_cursor);
    assert!(vm.can_set_appearance());
}

#[test]
fn a_clipboard_toggle_follows_the_pushed_status_without_asking_again() {
    for push_first in [false, true] {
        let mut vm = view();
        let mut initial = status_with("LOCAL");
        initial.clipboard_enabled = false;
        vm.apply(event(Msg::Status(initial)));

        vm.local(&Action::SetClipboard(true));
        let id = vm.begin_request(Request::SetClipboard, Instant::now());
        let mut status = status_with("LOCAL");
        status.clipboard_enabled = true;
        if push_first {
            // Core commits the toggle and pushes the Status before the OpResult.
            vm.apply(event(Msg::Status(status.clone())));
            assert!(
                vm.settings.clipboard_pending.is_none(),
                "the pushed status settles the switch on its own"
            );
        }
        vm.apply(answer(
            id,
            Msg::OpResult(protocol::ipc::OpResult {
                ok: true,
                error_code: String::new(),
            }),
        ));
        assert!(
            vm.take_outgoing().is_empty(),
            "the window must not re-query the status it was just pushed"
        );
        if !push_first {
            assert!(
                !vm.status.as_ref().unwrap().clipboard_enabled,
                "a bare OpResult announces no value"
            );
            vm.apply(event(Msg::Status(status)));
        }
        assert!(vm.status.as_ref().unwrap().clipboard_enabled);
        assert!(vm.settings.clipboard_pending.is_none());
    }
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
    let outcome = vm.outcome.as_ref().expect("a refusal is a toast");
    assert!(!outcome.ok);
    assert!(outcome.line(ui_text::Lang::Ru).contains("буфер"));
}

#[test]
fn a_successful_pairing_promises_what_actually_happens() {
    let mut vm = view();
    vm.apply(event(Msg::PairingPrompt(pairing_prompt())));
    let id = vm.begin_request(Request::PairingAnswer, Instant::now());
    vm.apply(event(Msg::PairingResult(protocol::ipc::PairingResult {
        outcome: protocol::ipc::PairingOutcome::Paired as i32,
        peer_name: pairing_prompt().peer_name,
    })));
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
        outcome.line(ui_text::Lang::Ru),
        "Готово: linux-box сопряжён"
    );
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
            Action::SetAppearance {
                edge_effects: true,
                hide_remote_cursor: false,
            },
            Request::SetAppearance,
        ),
        (
            Action::UpdateTopology {
                layout_toml: b"links = []".to_vec(),
                expected_revision: 3,
                expected_author: vec![0xaa; 32],
            },
            Request::UpdateTopology,
        ),
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
    assert!(Action::ToggleModal(Modal::Keys).wire().is_none());
    assert!(Action::MoveNode {
        node_id: PEER_NODE.to_owned(),
        x: 0.0,
        y: 0.0,
    }
    .wire()
    .is_none());
}

fn saving_view() -> ViewModel {
    let mut vm = view();
    vm.on_connected("endpoint");
    vm.apply(event(Msg::Status(status_with("LOCAL"))));
    vm.apply(event(Msg::TopologyDoc(two_node_doc(7))));
    vm
}

fn flush_target(vm: &ViewModel, a: usize, b: usize, side: Side) -> lws_gui::map::Pt {
    let at = vm.editor.placement[a];
    let (a, b) = (&vm.editor.shapes[a], &vm.editor.shapes[b]);
    match side {
        Side::Left => lws_gui::map::Pt::new(at.x - b.w, at.y),
        Side::Right => lws_gui::map::Pt::new(at.x + a.w, at.y),
        Side::Top => lws_gui::map::Pt::new(at.x, at.y - b.h),
        Side::Bottom => lws_gui::map::Pt::new(at.x, at.y + a.h),
    }
}

fn move_to_side(vm: &mut ViewModel, side: Side) {
    let this = index_of(vm, SELF_NODE);
    let peer = index_of(vm, PEER_NODE);
    let target = flush_target(vm, this, peer, side);
    vm.local(&Action::MoveNode {
        node_id: PEER_NODE.to_owned(),
        x: target.x,
        y: target.y,
    });
}

fn drop_to_side(vm: &mut ViewModel, side: Side) {
    move_to_side(vm, side);
    vm.local(&Action::DropNode(PEER_NODE.to_owned()));
}

fn take_save(vm: &mut ViewModel) -> (u64, protocol::ipc::UpdateTopology) {
    let outgoing = vm.take_outgoing();
    assert_eq!(outgoing.len(), 1, "{outgoing:?}");
    match outgoing.into_iter().next().expect("one request") {
        (id, Msg::UpdateTopology(update)) => (id, update),
        other => panic!("expected one UpdateTopology, got {other:?}"),
    }
}

fn verdict(vm: &mut ViewModel, id: u64, error: Option<&str>) {
    vm.apply(answer(
        id,
        Msg::OpResult(protocol::ipc::OpResult {
            ok: error.is_none(),
            error_code: error.unwrap_or_default().to_owned(),
        }),
    ));
}

#[test]
fn dropped_drafts_are_latest_wins_and_wait_for_the_applied_document() {
    let mut vm = saving_view();
    drop_to_side(&mut vm, Side::Right);
    let (id, first) = take_save(&mut vm);
    assert_eq!(first.expected_revision, 7);
    drop_to_side(&mut vm, Side::Top);
    drop_to_side(&mut vm, Side::Left);
    assert!(
        vm.take_outgoing().is_empty(),
        "only one save may be in flight"
    );
    verdict(&mut vm, id, None);
    assert!(
        vm.take_outgoing().is_empty(),
        "success alone does not advance the CAS base"
    );
    let mut status = status_with("LOCAL");
    status.layout_revision = 8;
    vm.apply(event(Msg::Status(status)));
    assert_eq!(vm.editor.revision, 7);
    assert!(
        vm.take_outgoing().is_empty(),
        "Status is not an applied document"
    );
    vm.apply(event(Msg::TopologyDoc(linked_doc(8))));
    let (_, latest) = take_save(&mut vm);
    assert_eq!(latest.expected_revision, 8);
    let topology: config::TopologyConfig =
        toml::from_str(std::str::from_utf8(&latest.layout_toml).expect("UTF-8")).expect("document");
    assert_eq!(topology.links.len(), 1);
    let link = &topology.links[0];
    assert_eq!(link.from.node, format!("peer:{PEER_NODE}"));
    assert_eq!(link.from.edge, config::Edge::Right);
    assert_eq!(link.to.node, format!("peer:{SELF_NODE}"));
    assert_eq!(link.to.edge, config::Edge::Left);
}

#[test]
fn conflict_reloads_and_retries_the_latest_drop_only_once() {
    let mut vm = saving_view();
    drop_to_side(&mut vm, Side::Right);
    let (id, _) = take_save(&mut vm);
    drop_to_side(&mut vm, Side::Top);
    verdict(&mut vm, id, Some("LAYOUT_CONFLICT"));
    let outgoing = vm.take_outgoing();
    assert_eq!(outgoing.len(), 1);
    let (reload_id, message) = &outgoing[0];
    assert!(matches!(message, Msg::GetTopology(_)));
    vm.apply(answer(*reload_id, Msg::TopologyDoc(linked_doc(9))));
    let (retry_id, retry) = take_save(&mut vm);
    assert_eq!(retry.expected_revision, 9);
    let topology: config::TopologyConfig =
        toml::from_str(std::str::from_utf8(&retry.layout_toml).expect("UTF-8")).expect("document");
    assert_eq!(topology.links[0].from.edge, config::Edge::Bottom);
    assert_eq!(topology.links[0].from.node, format!("peer:{PEER_NODE}"));
    verdict(&mut vm, retry_id, Some("LAYOUT_CONFLICT"));
    assert!(
        vm.take_outgoing().is_empty(),
        "a second conflict never loops"
    );
    assert!(vm.editor.save.wanted.is_none());
    assert!(vm.editor.save.sent.is_none());
    assert_eq!(vm.editor.placement, vm.editor.baseline);
    assert!(!vm.outcome.as_ref().expect("conflict toast").ok);
}

#[test]
fn invalid_drops_roll_back_without_sending_or_losing_an_accepted_drop() {
    let mut vm = saving_view();
    let baseline = vm.editor.placement.clone();
    vm.local(&Action::MoveNode {
        node_id: PEER_NODE.to_owned(),
        x: baseline[index_of(&vm, SELF_NODE)].x + 20.0,
        y: baseline[index_of(&vm, SELF_NODE)].y + 20.0,
    });
    vm.local(&Action::DropNode(PEER_NODE.to_owned()));
    assert_eq!(vm.editor.placement, baseline);
    assert!(vm.take_outgoing().is_empty());
    assert!(vm.flash.is_some(), "invalid placement gets a short hint");
    let this = index_of(&vm, SELF_NODE);
    let origin = vm.editor.placement[this];
    vm.local(&Action::MoveNode {
        node_id: PEER_NODE.to_owned(),
        x: origin.x + vm.editor.shapes[this].w / 3.0,
        y: origin.y + vm.editor.shapes[this].h / 3.0,
    });
    vm.local(&Action::DropNode(PEER_NODE.to_owned()));
    assert_eq!(
        vm.editor.placement, baseline,
        "overlapping desktops roll back"
    );
    assert!(vm.take_outgoing().is_empty());
    drop_to_side(&mut vm, Side::Right);
    let (id, _) = take_save(&mut vm);
    let accepted = vm.editor.placement.clone();
    vm.local(&Action::MoveNode {
        node_id: PEER_NODE.to_owned(),
        x: origin.x + 20.0,
        y: origin.y + 20.0,
    });
    vm.local(&Action::DropNode(PEER_NODE.to_owned()));
    assert_eq!(vm.editor.placement, accepted);
    assert!(vm.take_outgoing().is_empty());
    verdict(&mut vm, id, Some("BAD_REQUEST"));
    assert_eq!(vm.editor.placement, baseline);
    assert!(vm.editor.save.wanted.is_none());
    assert!(!vm.outcome.as_ref().expect("refusal toast").ok);
}

#[test]
fn same_revision_refresh_preserves_a_drag_without_saving_it() {
    let mut vm = saving_view();
    move_to_side(&mut vm, Side::Bottom);
    let draft = vm.editor.placement.clone();
    vm.apply(event(Msg::TopologyDoc(two_node_doc(7))));
    assert_eq!(vm.editor.placement, draft);
    assert!(vm.editor.dirty());
    assert!(vm.take_outgoing().is_empty());
    vm.local(&Action::DropNode(PEER_NODE.to_owned()));
    assert_eq!(take_save(&mut vm).1.expected_revision, 7);
}

#[test]
fn an_unchanged_drop_never_saves_and_success_is_a_quiet_hint() {
    let mut vm = saving_view();
    vm.local(&Action::DropNode(PEER_NODE.to_owned()));
    assert!(vm.take_outgoing().is_empty());
    drop_to_side(&mut vm, Side::Right);
    let (id, _) = take_save(&mut vm);
    verdict(&mut vm, id, None);
    assert!(
        vm.outcome.is_none(),
        "successful saves do not create an eight-second toast"
    );
    assert!(
        vm.flash.is_some(),
        "successful saves get a short acknowledgement"
    );
    vm.apply(event(Msg::TopologyDoc(linked_doc(8))));
    assert!(!vm.editor.dirty());
    vm.local(&Action::DropNode(PEER_NODE.to_owned()));
    assert!(vm.take_outgoing().is_empty());
}

#[test]
fn a_late_layout_verdict_after_timeout_still_releases_the_save_slot() {
    let mut vm = saving_view();
    drop_to_side(&mut vm, Side::Right);
    let (id, _) = take_save(&mut vm);
    drop_to_side(&mut vm, Side::Top);
    vm.tick(Instant::now() + Duration::from_secs(10));
    verdict(&mut vm, id, None);
    vm.apply(event(Msg::TopologyDoc(linked_doc(8))));
    let (next_id, next) = take_save(&mut vm);
    assert_ne!(next_id, id);
    assert_eq!(next.expected_revision, 8);
    verdict(&mut vm, id, Some("BAD_REQUEST"));
    assert_eq!(
        vm.editor
            .save
            .sent
            .as_ref()
            .expect("newer save remains")
            .request_id,
        next_id
    );
}

/// Dragging onto each facing edge previews and serialises the same desktop link.
#[test]
fn dragging_chooses_all_four_edges() {
    let mut vm = view();
    vm.apply(event(Msg::Status(status_with("LOCAL"))));
    vm.apply(event(Msg::TopologyDoc(two_node_doc(7))));
    let (this, peer) = (index_of(&vm, SELF_NODE), index_of(&vm, PEER_NODE));

    for side in Side::ALL {
        let target = flush_target(&vm, this, peer, side);
        // Flush snapping keeps the requested tangent coordinate.
        vm.local(&Action::MoveNode {
            node_id: PEER_NODE.to_owned(),
            x: target.x,
            y: target.y,
        });
        assert!((vm.editor.placement[peer].x - target.x).abs() < 1e-6);
        assert!((vm.editor.placement[peer].y - target.y).abs() < 1e-6);
        let links = vm.map_model().links;
        assert_eq!(links.len(), 1, "{}", side.label(ui_text::Lang::Ru));
        let expected_dir = match side {
            Side::Top | Side::Bottom => lws_gui::map::Dir::Vertical,
            Side::Left | Side::Right => lws_gui::map::Dir::Horizontal,
        };
        assert_eq!(links[0].dir, expected_dir);
        assert!(vm.editor.dirty(), "{}", side.label(ui_text::Lang::Ru));
        let document = vm.layout_toml();
        let topology: config::TopologyConfig = toml::from_str(&document).expect("parse");
        assert_eq!(topology.links.len(), 1);
        let link = &topology.links[0];
        assert!(!link.from.is_auto() && !link.to.is_auto());
        let (from, to, from_edge, to_edge) = match side {
            Side::Left => (PEER_NODE, SELF_NODE, "right", "left"),
            Side::Right => (SELF_NODE, PEER_NODE, "right", "left"),
            Side::Top => (PEER_NODE, SELF_NODE, "bottom", "top"),
            Side::Bottom => (SELF_NODE, PEER_NODE, "bottom", "top"),
        };
        assert_eq!(link.from.node, format!("peer:{from}"));
        assert_eq!(link.to.node, format!("peer:{to}"));
        assert_eq!(link.from.edge.to_string(), from_edge);
        assert_eq!(link.to.edge.to_string(), to_edge);
        config::Config::from_str(&as_config_file(&document))
            .expect("valid config")
            .validate()
            .expect("valid topology");
        // The peer never lies over this computer.
        let (a, b) = (
            (vm.editor.placement[this], &vm.editor.shapes[this]),
            (vm.editor.placement[peer], &vm.editor.shapes[peer]),
        );
        let overlap = a.0.x < b.0.x + b.1.w
            && b.0.x < a.0.x + a.1.w
            && a.0.y < b.0.y + b.1.h
            && b.0.y < a.0.y + a.1.h;
        assert!(
            !overlap,
            "{} must not overlap",
            side.label(ui_text::Lang::Ru)
        );
    }

    // Dragging the peer to this computer's left chooses «Слева», and the document then leaves
    // through the peer's right edge into this computer's left edge.
    let me = vm.editor.placement[this];
    let peer_width = vm.editor.shapes[peer].w;
    vm.local(&Action::MoveNode {
        node_id: PEER_NODE.to_owned(),
        x: me.x - peer_width - 4.0,
        y: me.y + 2.0,
    });
    let topology: config::TopologyConfig = toml::from_str(&vm.layout_toml()).expect("parse");
    let link = &topology.links[0];
    assert_eq!(link.from.node, format!("peer:{PEER_NODE}"));
    assert_eq!(link.from.edge.to_string(), "right");
    assert_eq!(link.to.node, format!("peer:{SELF_NODE}"));
    assert_eq!(link.to.edge.to_string(), "left");

    // A keyboard step moves the computer exactly as far as asked, without snapping.
    let before = vm.editor.placement[peer];
    vm.nudge_node(PEER_NODE, 8.0, -8.0);
    assert_eq!(vm.editor.placement[peer].x, before.x + 8.0);
    assert_eq!(vm.editor.placement[peer].y, before.y - 8.0);
}

#[test]
fn the_route_and_link_state_follow_the_core() {
    let mut vm = view();
    vm.apply(event(Msg::Status(status_with("LOCAL"))));
    vm.apply(event(Msg::TopologyDoc(linked_doc(7))));
    vm.on_connected("endpoint");
    let map = vm.map_model();
    let peer = index_of(&vm, PEER_NODE);
    assert!(!map.nodes[peer].target);
    assert_eq!(map.links[0].state, LinkState::Idle);

    // Input is on the other computer: the route says so, the link carries it, and the button
    // brings it back (a second `switch` while Remote is the return edge).
    vm.apply(event(Msg::Status(status_with("REMOTE"))));
    let map = vm.map_model();
    assert!(map.nodes[peer].target);
    assert_eq!(map.nodes[peer].focus, "ввод здесь");
    assert_eq!(map.links[0].state, LinkState::Active);
    assert_eq!(map.nodes[peer].mons[0].tag, "ввод");

    // An offline peer: nothing to switch to, and its link is drawn offline.
    let mut status = status_with("LOCAL");
    status.members[1].online = false;
    vm.apply(event(Msg::Status(status)));
    let mut doc = linked_doc(8);
    doc.members[1].online = false;
    vm.apply(event(Msg::TopologyDoc(doc)));
    assert_eq!(vm.map_model().links[0].state, LinkState::Offline);
    assert_eq!(vm.map_model().nodes[peer].focus, "офлайн");
    assert_eq!(vm.map_model().nodes[peer].mons[0].tag, "");
}

#[test]
fn the_headings_count_what_the_document_holds() {
    let mut vm = view();
    vm.apply(event(Msg::Status(status_with("LOCAL"))));
    vm.apply(event(Msg::TopologyDoc(linked_doc(14))));
    assert_eq!(vm.map_title(), "2 компьютера · 2 экрана");
    assert_eq!(vm.editor.revision, 14);
    let peers = vm.peers();
    assert_eq!(peers.len(), 1, "this computer is not its own peer");
    assert_eq!(peers[0].name, "linux-box");
    assert_eq!(peers[0].initials, "LI");
    assert_eq!(peers[0].meta, "BB22·BB22");
    assert!(peers[0].online);
    let map = vm.map_model();
    assert_eq!(map.screens, 2);
    assert_eq!(map.nodes[0].mons[0].num, "01");
    assert_eq!(map.nodes[0].mons[0].res, "1920×1080");
    assert_eq!(map.nodes[1].mons[0].num, "02");
    assert_eq!(
        map.nodes[1].mons[0].res, "≈ 1600×900",
        "a display whose size is a guess says so"
    );
}

#[test]
fn the_rail_opens_one_window_at_a_time_and_a_pair_is_removed_only_after_asking() {
    let mut vm = view();
    assert_eq!(vm.modal, None);
    vm.local(&Action::ToggleModal(Modal::Pairs));
    assert_eq!(vm.modal, Some(Modal::Pairs));
    vm.local(&Action::SelectRevoke(Some(PEER_NODE.to_owned())));
    assert_eq!(vm.settings.revoke.as_deref(), Some(PEER_NODE));
    vm.local(&Action::ToggleModal(Modal::Keys));
    assert_eq!(vm.modal, Some(Modal::Keys));
    assert_eq!(
        vm.settings.revoke, None,
        "a question does not outlive its window"
    );
    vm.local(&Action::ToggleModal(Modal::Keys));
    assert_eq!(vm.modal, None, "the same button closes it");
    vm.local(&Action::ToggleModal(Modal::Settings));
    vm.local(&Action::CloseModal);
    assert_eq!(vm.modal, None);
}

#[test]
fn a_pair_is_made_through_the_code_and_stays_on_screen_until_done() {
    let mut vm = view();
    vm.on_connected("endpoint");
    let now = Instant::now();
    vm.local(&Action::PairWith("192.168.1.10:47901".to_owned()));
    assert!(vm.pairing.dialing);
    let id = vm.begin_request(Request::PairWith, now);
    vm.apply(event(Msg::PairingPrompt(pairing_prompt())));
    assert_eq!(vm.pairing.code().as_deref(), Some("482 913"));
    vm.apply(answer(
        id,
        Msg::OpResult(protocol::ipc::OpResult {
            ok: true,
            error_code: String::new(),
        }),
    ));
    assert_eq!(
        vm.pairing.code().as_deref(),
        Some("482 913"),
        "the dial acknowledgement must preserve the code awaiting confirmation"
    );
    assert_eq!(vm.pairing.paired, None, "dialling is not a completed pair");
    let id = vm.begin_request(Request::PairingAnswer, now);
    vm.apply(event(Msg::PairingResult(protocol::ipc::PairingResult {
        outcome: protocol::ipc::PairingOutcome::Paired as i32,
        peer_name: pairing_prompt().peer_name,
    })));
    vm.apply(answer(
        id,
        Msg::OpResult(protocol::ipc::OpResult {
            ok: true,
            error_code: String::new(),
        }),
    ));
    assert!(!vm.pairing.dialing);
    assert!(
        vm.pairing.prompt.is_none(),
        "the code is gone once it was used"
    );
    assert_eq!(vm.pairing.paired.as_deref(), Some("linux-box"));
    vm.local(&Action::DonePairing);
    assert_eq!(vm.pairing.paired, None);

    // A refused ceremony leaves no «Пара создана» behind.
    vm.local(&Action::PairWith("192.168.1.10:47901".to_owned()));
    let id = vm.begin_request(Request::PairWith, now);
    vm.apply(answer(
        id,
        Msg::OpResult(protocol::ipc::OpResult {
            ok: false,
            error_code: "PAIRING_FAILED".to_owned(),
        }),
    ));
    assert!(!vm.pairing.dialing);
    assert_eq!(vm.pairing.paired, None);
}

#[test]
fn the_dial_acknowledgement_before_the_prompt_does_not_claim_a_pair() {
    let mut vm = view();
    vm.on_connected("endpoint");
    vm.local(&Action::PairWith("192.168.1.10:47901".to_owned()));
    let id = vm.begin_request(Request::PairWith, Instant::now());
    vm.apply(answer(
        id,
        Msg::OpResult(protocol::ipc::OpResult {
            ok: true,
            error_code: String::new(),
        }),
    ));
    assert!(!vm.pairing.dialing);
    assert_eq!(vm.pairing.paired, None, "the users have not confirmed yet");
    vm.apply(event(Msg::PairingPrompt(pairing_prompt())));
    assert_eq!(vm.pairing.code().as_deref(), Some("482 913"));
}

#[test]
fn a_request_that_runs_out_is_scheduled_exactly_once() {
    let mut vm = view();
    let now = Instant::now();
    assert_eq!(
        vm.next_wakeup(now),
        None,
        "an idle window waits for nothing"
    );
    vm.on_connected("endpoint");
    vm.begin_request(Request::GetStatus, now);
    let due = vm.next_wakeup(now).expect("a deadline");
    assert!(due <= Duration::from_secs(3));
    assert_eq!(
        vm.next_wakeup(now + Duration::from_secs(4)),
        None,
        "a deadline that has passed is not scheduled again"
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
            "Ожидается диалог разрешения портала — подтвердите его, иначе узел не сможет принять ввод".to_owned(),
            "Wayland: перехват ввода возможен только через порталы рабочего стола".to_owned(),
        ]
    );
    let restoring = limits("platform-wayland", &[], "restoring");
    assert_eq!(
        restoring,
        vec![
            "Разрешение портала восстанавливается из сохранённого токена; диалог не ожидается"
                .to_owned(),
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
    vm.settings.lan.candidates = vec!["192.168.1.10".to_owned(), "127.0.0.1".to_owned()];

    assert_eq!(
        vm.settings.lan.options(&vm.config.listen_address),
        vec!["192.168.1.10"],
        "the LAN picker must never offer loopback, even from a stale config"
    );
    assert_eq!(
        vm.settings
            .lan
            .selected(&vm.config.listen_address)
            .as_deref(),
        Some("192.168.1.10"),
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
    assert!(applied.ok, "{}", applied.line(ui_text::Lang::Ru));
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
    assert!(!applied.ok, "{}", applied.line(ui_text::Lang::Ru));
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

    vm.on_disconnected("the pipe was closed".to_owned());
    assert!(
        vm.settings.service.restart.is_none(),
        "stop never schedules a replacement"
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
fn a_failed_core_stop_reports_the_short_reason_in_both_languages() {
    for (lang, expected) in [
        (
            ui_text::Lang::Ru,
            "не удалось остановить службу (access denied)",
        ),
        (
            ui_text::Lang::En,
            "could not stop the service (access denied)",
        ),
    ] {
        let mut vm = view();
        vm.look.language = lang;
        vm.settings.service.restart = Some(Restart::Stopping {
            since: Instant::now(),
        });
        vm.on_core_stopped(Err("  access denied  \nmore details".to_owned()));
        let line = vm
            .settings
            .service
            .line
            .as_ref()
            .expect("the failure reason");
        assert!(!line.ok);
        assert_eq!(line.text, expected);
        assert_eq!(vm.settings.service.restart, None);
    }
}

#[test]
fn restart_uses_graceful_ipc_without_requiring_an_owned_pid() {
    for core in [
        None,
        Some(own_core(4242)),
        Some(lws_gui::view_model::StartedCore {
            program: std::path::PathBuf::from("bifrost"),
            pid: None,
        }),
    ] {
        let mut vm = view();
        let now = Instant::now();
        vm.core = core;
        vm.on_connected("endpoint");
        assert_eq!(vm.begin_restart(now), RestartStep::Stop);
        assert_eq!(
            vm.settings.service.restart,
            Some(Restart::Stopping { since: now })
        );
        assert_eq!(vm.begin_restart(now), RestartStep::Refused);
        vm.on_disconnected("the pipe was closed".into());
        assert_eq!(
            vm.settings.service.restart,
            Some(Restart::Stopping { since: now })
        );
    }
}

#[test]
fn completed_restart_updates_the_service_and_saved_address_messages() {
    let mut vm = view();
    vm.settings.service.restart = Some(Restart::Starting);

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
fn restart_deadline_is_owned_by_the_exit_probe_not_a_link_timeout() {
    let mut vm = view();
    let now = Instant::now();
    vm.on_connected("endpoint");
    vm.begin_restart(now);
    vm.tick(now + SERVICE_STOP_TIMEOUT);
    assert_eq!(
        vm.settings.service.restart,
        Some(Restart::Stopping { since: now })
    );
    assert_eq!(
        vm.next_wakeup(now + SERVICE_STOP_TIMEOUT),
        Some(Duration::from_millis(100))
    );
}

#[test]
fn a_restart_with_no_link_still_waits_for_process_and_listener_exit() {
    let mut vm = view();
    vm.core = Some(own_core(4242));
    let now = Instant::now();
    assert_eq!(vm.begin_restart(now), RestartStep::Start);
    assert_eq!(
        vm.settings.service.restart,
        Some(Restart::Stopping { since: now })
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_launched_core_is_reaped_after_it_exits() {
    // A harmless short-lived stand-in for `bifrost run`: no display or input involved.
    let started = lws_gui::view_model::launch_core(&Launch {
        program: std::path::PathBuf::from("/bin/sh"),
        flags: vec!["-c".into(), "exit 0".into()],
    })
    .expect("the child starts");
    let pid = started.pid.expect("the launched child's pid");
    assert_eq!(started.program, std::path::PathBuf::from("/bin/sh"));

    // Exited-but-unreaped children still have /proc entries, so exit alone cannot pass.
    let process = std::path::PathBuf::from(format!("/proc/{pid}"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while process.exists() {
        assert!(
            Instant::now() < deadline,
            "launched child {pid} was not reaped within 5 seconds"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
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
    let dir = settings_dir("192.168.1.10");
    let file = dir.join("config/config.toml");
    std::fs::remove_file(&file).expect("start without config");
    let paths = paths_of(&dir);
    let mut vm = view();
    vm.config = lws_gui::view_model::LocalConfig::load(&paths);
    assert!(vm.config.config_was_missing);

    std::fs::write(
        &file,
        "config_version = 2\n\n[network]\nlisten_address = \"192.168.1.10\"\n",
    )
    .expect("the core writes the first-run config before accepting the GUI");
    vm.on_connected("endpoint");

    assert_eq!(vm.config.listen_address, "192.168.1.10");
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

/// Apply writes the file and schedules a graceful restart even without an owned PID.
#[test]
fn the_address_button_through_the_bridge_writes_and_schedules_an_attached_core_restart() {
    let mut bridge = Bridge::new(
        view(),
        GuiClient::start(None, Launch::with_flags(Vec::new())),
    );
    let dir = settings_dir("127.0.0.1");
    let view = bridge.view_mut();
    view.config.paths = Some(config::Paths::new(
        dir.join("config"),
        dir.join("data"),
        dir.join("state"),
        Some(dir.join("run")),
    ));
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
    assert!(
        bridge.view().core.is_none(),
        "this window does not own a PID"
    );
    assert_eq!(
        bridge.view().settings.service.restart,
        Some(Restart::Stopping { since: now }),
    );
    assert!(
        bridge.view().settings.service.line.is_none(),
        "no PID-based refusal"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn concrete_doc(
    mut doc: protocol::ipc::TopologyDoc,
    links: &[config::LinkConfig],
) -> protocol::ipc::TopologyDoc {
    let endpoint = |e: &config::EndpointConfig| protocol::ipc::EndpointView {
        node_id: e.node.strip_prefix("peer:").unwrap_or(&e.node).to_owned(),
        display_id: e.display.clone(),
        edge: e.edge.to_string(),
        segment_start: e.start,
        segment_end: e.end,
    };
    doc.links = links
        .iter()
        .map(|link| protocol::ipc::LinkView {
            id: link.id.clone(),
            from: Some(endpoint(&link.from)),
            to: Some(endpoint(&link.to)),
            bidirectional: link.bidirectional,
            active: true,
            ..Default::default()
        })
        .collect();
    doc
}

#[test]
fn lower_half_contacts_keep_display_ids_and_round_trip_offsets() {
    let mut vm = saving_view();
    let a = index_of(&vm, SELF_NODE);
    let b = index_of(&vm, PEER_NODE);
    let origin = vm.editor.placement[a];
    vm.editor.placement[b] = lws_gui::map::Pt::new(
        origin.x + vm.editor.shapes[a].w,
        origin.y + vm.editor.shapes[a].h / 2.0,
    );
    let links = vm.editor_links();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].from.display, vm.editor.nodes[a].displays[0].id);
    assert_eq!(links[0].to.display, vm.editor.nodes[b].displays[0].id);
    assert!((links[0].from.start - 540.0).abs() < 1e-6);
    assert!(links[0].to.start.abs() < 1e-6);
    let relative = lws_gui::map::Pt::new(
        (vm.editor.placement[b].x - origin.x) / vm.editor.scale,
        (vm.editor.placement[b].y - origin.y) / vm.editor.scale,
    );
    let mut restored = saving_view();
    restored.apply(event(Msg::TopologyDoc(concrete_doc(
        two_node_doc(8),
        &links,
    ))));
    let ra = restored.editor.placement[a];
    let rb = restored.editor.placement[b];
    assert!(((rb.x - ra.x - lws_gui::map::GAP) / restored.editor.scale - relative.x).abs() < 1e-6);
    assert!(((rb.y - ra.y) / restored.editor.scale - relative.y).abs() < 1e-6);
    assert_eq!(restored.editor_links(), links);
    assert!(!restored.map_model().links[0].whole_desktop);
}

#[test]
fn disconnected_nonoverlapping_drop_is_accepted_without_relinking() {
    let mut vm = saving_view();
    vm.apply(event(Msg::TopologyDoc(linked_doc(8))));
    let peer = index_of(&vm, PEER_NODE);
    vm.editor.placement[peer] = lws_gui::map::Pt::new(800.0, 400.0);
    let dropped = vm.editor.placement.clone();
    vm.local(&Action::DropNode(PEER_NODE.to_owned()));
    assert_eq!(vm.editor.placement, dropped);
    let (_, request) = take_save(&mut vm);
    let topology: config::TopologyConfig =
        toml::from_str(std::str::from_utf8(&request.layout_toml).expect("UTF-8"))
            .expect("document");
    assert!(topology.links.is_empty());
}

#[test]
fn multiple_display_contacts_are_saved_individually() {
    let mut doc = two_node_doc(8);
    for member in &mut doc.members {
        let mut second = member.displays[0].clone();
        second.id.push_str("-lower");
        second.logical_y = second.logical_h;
        member.displays.push(second);
    }
    let mut vm = saving_view();
    vm.apply(event(Msg::TopologyDoc(doc.clone())));
    let a = index_of(&vm, SELF_NODE);
    let b = index_of(&vm, PEER_NODE);
    vm.editor.placement[b] = lws_gui::map::Pt::new(
        vm.editor.placement[a].x + vm.editor.shapes[a].w,
        vm.editor.placement[a].y,
    );
    let links = vm.editor_links();
    assert_eq!(
        links.len(),
        3,
        "each touching display pair keeps its own overlap"
    );
    assert!(links.iter().any(|l| l.from.display.ends_with("-lower")));
    assert!(links.iter().any(|l| l.to.display.ends_with("-lower")));
    assert_eq!(vm.map_model().links.len(), links.len());
    doc.revision = 9;
    let mut restored = saving_view();
    restored.apply(event(Msg::TopologyDoc(concrete_doc(doc, &links))));
    assert_eq!(restored.editor_links(), links);
    assert_eq!(restored.map_model().links.len(), 3);
}

#[test]
fn missing_concrete_display_is_drawn_inactive_not_as_auto() {
    let mut vm = saving_view();
    move_to_side(&mut vm, Side::Right);
    let links = vm.editor_links();
    let mut doc = concrete_doc(two_node_doc(8), &links);
    doc.members[1].displays[0].id = "replacement".to_owned();
    vm.apply(event(Msg::TopologyDoc(doc)));
    let model = vm.map_model();
    assert_eq!(model.links.len(), 1);
    assert_eq!(model.links[0].state, LinkState::Inactive);
    assert!(!model.links[0].whole_desktop);
    assert_eq!(vm.editor_links(), links);
}

#[test]
fn moving_an_auto_pair_on_the_same_side_replaces_it_with_exact_segments() {
    let mut vm = saving_view();
    vm.apply(event(Msg::TopologyDoc(linked_doc(8))));
    let a = index_of(&vm, SELF_NODE);
    let b = index_of(&vm, PEER_NODE);
    vm.editor.placement[b] = lws_gui::map::Pt::new(
        vm.editor.placement[a].x + vm.editor.shapes[a].w,
        vm.editor.placement[a].y + 12.5,
    );
    let links = vm.editor_links();
    assert_eq!(links.len(), 1);
    assert!(!links[0].from.is_auto());
    assert!((links[0].from.start - 12.5 / vm.editor.scale).abs() < 1e-6);
    vm.editor.placement[b].y += 7.25;
    let changed = vm.editor_links();
    assert!((changed[0].from.start - 19.75 / vm.editor.scale).abs() < 1e-6);
    assert_ne!(links, changed);
}

#[test]
fn a_drop_in_empty_desktop_bounding_space_is_not_an_overlap() {
    let mut doc = two_node_doc(8);
    let mut second = doc.members[0].displays[0].clone();
    second.id.push_str("-far");
    second.logical_x = 5000.0;
    doc.members[0].displays.push(second);
    let mut vm = saving_view();
    vm.apply(event(Msg::TopologyDoc(doc)));
    let a = index_of(&vm, SELF_NODE);
    let b = index_of(&vm, PEER_NODE);
    vm.editor.placement[b] = lws_gui::map::Pt::new(
        vm.editor.placement[a].x + 2500.0 * vm.editor.scale,
        vm.editor.placement[a].y,
    );
    let dropped = vm.editor.placement.clone();
    vm.local(&Action::DropNode(PEER_NODE.to_owned()));
    assert_eq!(vm.editor.placement, dropped);
    assert!(
        vm.editor_links().is_empty(),
        "local monitor holes are not external edges"
    );
    assert!(!vm.editor.dirty());
    assert!(vm.take_outgoing().is_empty(), "no link edits need a CAS");
}

#[test]
fn disconnected_placement_survives_refresh_and_the_cas_acknowledgement() {
    let mut vm = saving_view();
    let b = index_of(&vm, PEER_NODE);
    vm.editor.placement[b] = lws_gui::map::Pt::new(800.0, 400.0);
    vm.local(&Action::DropNode(PEER_NODE.to_owned()));
    let free = vm.editor.placement.clone();
    vm.apply(event(Msg::TopologyDoc(two_node_doc(7))));
    assert_eq!(vm.editor.placement, free);
    assert!(vm.take_outgoing().is_empty());

    vm.apply(event(Msg::TopologyDoc(linked_doc(8))));
    vm.editor.placement[b] = lws_gui::map::Pt::new(800.0, 400.0);
    vm.local(&Action::DropNode(PEER_NODE.to_owned()));
    let accepted = vm.editor.placement.clone();
    let (id, _) = take_save(&mut vm);
    verdict(&mut vm, id, None);
    vm.apply(event(Msg::TopologyDoc(two_node_doc(9))));
    assert_eq!(vm.editor.placement, accepted);
    assert_eq!(vm.editor.baseline, accepted);
    assert!(!vm.editor.dirty());
    assert!(vm.map_model().links.is_empty());
}

#[test]
fn legacy_mixed_auto_and_concrete_endpoints_reconstruct_and_survive() {
    let mut doc = linked_doc(8);
    let endpoint = doc.links[0].to.as_mut().expect("endpoint");
    endpoint.display_id = doc.members[1].displays[0].id.clone();
    endpoint.segment_start = 100.0;
    endpoint.segment_end = 800.0;
    let mut vm = saving_view();
    vm.apply(event(Msg::TopologyDoc(doc)));
    let model = vm.map_model();
    assert_eq!(model.links.len(), 1);
    assert!(model.links[0].whole_desktop);
    assert!((model.links[0].to.x - model.links[0].from.x - lws_gui::map::GAP).abs() < 1e-6);
    assert!((model.links[0].span.0 - (model.links[0].to.y - 350.0 * vm.editor.scale)).abs() < 1e-6);
    let links = vm.editor_links();
    assert!(links[0].from.is_auto());
    assert!(!links[0].to.is_auto());
    assert_eq!(links[0].to.start, 100.0);
    assert_eq!(links[0].to.end, 800.0);
}

#[test]
fn owner_portrait_lower_half_drop_saves_only_the_real_contact() {
    let mut doc = two_node_doc(8);
    doc.members[0].displays[0].id = "stable-landscape-interface-id".to_owned();
    doc.members[0].displays[0].logical_y = 240.0;
    let mut portrait = doc.members[0].displays[0].clone();
    portrait.id = "stable-portrait-interface-id".to_owned();
    portrait.logical_x = 1920.0;
    portrait.logical_y = 0.0;
    portrait.logical_w = 1080.0;
    portrait.logical_h = 1920.0;
    portrait.primary = false;
    doc.members[0].displays.push(portrait);
    doc.members[1].displays[0].logical_w = 1745.0;
    doc.members[1].displays[0].logical_h = 1091.0;
    let mut vm = saving_view();
    vm.apply(event(Msg::TopologyDoc(doc.clone())));
    let a = index_of(&vm, SELF_NODE);
    let b = index_of(&vm, PEER_NODE);
    let origin = vm.editor.placement[a];
    let monitor = vm.editor.shapes[a].mons[1];
    let y = origin.y + monitor.y + monitor.h / 2.0;
    vm.move_node(PEER_NODE, origin.x + monitor.x + monitor.w + 5.0, y);
    assert_eq!(vm.editor.placement[b].y, y);
    vm.local(&Action::DropNode(PEER_NODE.to_owned()));
    let (_, request) = take_save(&mut vm);
    let topology: config::TopologyConfig =
        toml::from_str(std::str::from_utf8(&request.layout_toml).expect("UTF-8"))
            .expect("contact document");
    assert_eq!(topology.links.len(), 1);
    let link = &topology.links[0];
    assert_eq!(link.from.display, "stable-portrait-interface-id");
    assert_eq!(link.to.display, "eDP-1");
    assert!((link.from.start - 960.0).abs() < 1e-6);
    assert!((link.from.end - 1920.0).abs() < 1e-6);
    assert!(link.to.start.abs() < 1e-6);
    assert!((link.to.end - 960.0).abs() < 1e-6);
    let drawn = vm.map_model();
    assert_eq!(drawn.links.len(), 1);
    assert!((drawn.links[0].span.0 - y).abs() < 1e-6);
    assert!((drawn.links[0].span.1 - origin.y - monitor.h).abs() < 1e-6);
    let mut restored = saving_view();
    restored.apply(event(Msg::TopologyDoc(concrete_doc(doc, &topology.links))));
    assert_eq!(restored.editor_links(), topology.links);
}

#[test]
fn the_map_names_the_operating_system_of_every_card_in_both_languages() {
    let mut doc = two_node_doc(8);
    doc.members[0].system_name = "Windows 11 25H2".to_owned();
    doc.members[1].system_name = "Ubuntu 24.04".to_owned();
    let mut vm = saving_view();
    vm.look.language = lws_gui::ui_text::Lang::Ru;
    vm.apply(event(Msg::TopologyDoc(doc.clone())));
    let model = vm.map_model();
    let this = index_of(&vm, SELF_NODE);
    let peer = index_of(&vm, PEER_NODE);
    assert_eq!(model.nodes[this].os, "этот компьютер · Windows 11 25H2");
    assert_eq!(model.nodes[peer].os, "в сети · Ubuntu 24.04");
    assert_eq!(
        model.nodes[peer].focus, "",
        "«в сети» moved onto the OS line and is not repeated below it"
    );

    vm.look.language = lws_gui::ui_text::Lang::En;
    let english = vm.map_model();
    assert_eq!(english.nodes[peer].os, "online · Ubuntu 24.04");

    // The peer goes offline: the core keeps the last name it had, the card shows it.
    let mut offline = doc.clone();
    offline.revision = 9;
    offline.members[1].online = false;
    vm.apply(event(Msg::TopologyDoc(offline)));
    let model = vm.map_model();
    assert!(!model.nodes[peer].online);
    assert_eq!(model.nodes[peer].os, "offline · Ubuntu 24.04");
    assert_eq!(
        model.nodes[peer].focus, "",
        "«offline» is on the OS line and is not repeated below it"
    );

    // A peer of a version that has no such field shows exactly what it showed before.
    let mut old = doc;
    old.revision = 10;
    old.members[1].system_name = String::new();
    vm.apply(event(Msg::TopologyDoc(old)));
    let model = vm.map_model();
    assert_eq!(model.nodes[peer].os, "");
    assert_eq!(model.nodes[peer].focus, "online");
}

#[test]
fn a_hostile_operating_system_name_is_bounded_before_the_map_shows_it() {
    // The core sanitises what a peer sent, and the window sanitises the document again: the
    // label must never carry a control character, a bidi override or an unbounded string.
    let mut doc = two_node_doc(8);
    doc.members[1].system_name = format!("\u{202e}\u{7}Ubuntu 24.04\u{202c} {}", "я".repeat(40));
    let mut vm = saving_view();
    vm.look.language = lws_gui::ui_text::Lang::Ru;
    vm.apply(event(Msg::TopologyDoc(doc)));
    let peer = index_of(&vm, PEER_NODE);
    let os = vm.map_model().nodes[peer].os.clone();
    let name = os.strip_prefix("в сети · ").expect("the state prefix");
    assert!(name.starts_with("Ubuntu 24.04"), "{name:?}");
    assert_eq!(name.chars().count(), 32, "{name:?}");
    assert!(!name.chars().any(char::is_control), "{name:?}");
    assert!(!name.contains(['\u{202e}', '\u{202c}']), "{name:?}");
}

/// M3: one parser behind the window, the tray and the core.
///
/// The window is started with the arguments; the tray gets the window's own arguments
/// unchanged; the core is started with `CliArgs::flags`. Before the shared parser
/// `--root=/path` was read by the tray alone, so the window resolved the default directories
/// and the two processes talked past each other.
#[test]
fn both_flag_spellings_name_the_same_directories_in_every_role() {
    let inline = CliArgs::parse([
        "--root=/work".to_owned(),
        "--data-dir=/elsewhere".to_owned(),
    ]);
    let separated = CliArgs::parse([
        "--root".to_owned(),
        "/work".to_owned(),
        "--data-dir".to_owned(),
        "/elsewhere".to_owned(),
    ]);
    assert_eq!(inline, separated);
    assert_eq!(inline.root.as_deref(), Some(std::path::Path::new("/work")));

    // What the window hands the core it starts: the separated form, same values.
    let forwarded = inline.flags();
    assert_eq!(forwarded, ["--root", "/work", "--data-dir", "/elsewhere"]);
    let core = CliArgs::parse(forwarded);
    assert_eq!(core, inline);
    // What the tray gets: the window's arguments, unchanged, through the same parser.
    let tray = CliArgs::parse([
        "--root=/work".to_owned(),
        "--data-dir=/elsewhere".to_owned(),
    ]);
    assert_eq!(tray, inline);

    let resolved = inline.resolve().expect("the defaults resolve");
    assert_eq!(core.resolve().expect("the defaults resolve"), resolved);
    assert_eq!(tray.resolve().expect("the defaults resolve"), resolved);
    assert_eq!(resolved.config_dir(), std::path::Path::new("/work/config"));
}

fn discovered_peer(id: &str, open: bool) -> protocol::ipc::DiscoveredPeer {
    protocol::ipc::DiscoveredPeer {
        node_id: id.to_owned(),
        display_name: "Nearby laptop".to_owned(),
        address: "192.168.1.42:47901".to_owned(),
        system_name: "Linux".to_owned(),
        pairing_open: open,
        protocol_version: protocol::PROTOCOL_MAJOR,
        supports_pairing_requests: true,
    }
}

#[test]
fn version_badge_decision_table_uses_semver_precedence_and_hides_unknowns() {
    for (current, highest, expected) in [
        ("", "0.1.7", false),
        ("unknown", "0.1.7", false),
        ("0.1.5", "", false),
        ("0.1.5", "0.1.7", true),
        ("0.1.7", "0.1.7", false),
        ("0.1.8", "0.1.7", false),
        ("0.1.7-dev", "0.1.7", true),
        ("0.1.7+one", "0.1.7+two", false),
        ("0.1.9", "0.1.10", true),
    ] {
        assert_eq!(
            lws_gui::view_model::version_is_older(current, highest),
            expected,
            "{current} / {highest}"
        );
    }
}

#[test]
fn discovery_hides_paired_members_and_replaces_expired_announcements() {
    let mut vm = view();
    vm.on_connected("test");
    let new_id = "cc".repeat(32);
    let mut status = status_with("LOCAL");
    status.discovered_peers = vec![
        discovered_peer(SELF_NODE, true),
        discovered_peer(PEER_NODE, true),
        discovered_peer(&new_id, false),
    ];
    vm.apply(event(Msg::Status(status.clone())));
    assert_eq!(
        vm.discovered_peers()
            .map(|peer| peer.node_id.as_str())
            .collect::<Vec<_>>(),
        vec![new_id.as_str()]
    );
    status.members.clear();
    vm.apply(event(Msg::TopologyDoc(two_node_doc(7))));
    vm.apply(event(Msg::Status(status.clone())));
    assert_eq!(
        vm.discovered_peers().count(),
        1,
        "topology membership also hides paired nodes"
    );
    status.discovery_enabled = false;
    vm.apply(event(Msg::Status(status.clone())));
    assert_eq!(vm.discovered_peers().count(), 0);
    status.discovery_enabled = true;
    status.discovered_peers.clear();
    vm.apply(event(Msg::Status(status.clone())));
    assert_eq!(
        vm.discovered_peers().count(),
        0,
        "expired entries are not retained"
    );
    status.discovered_peers.push(discovered_peer(&new_id, true));
    vm.apply(event(Msg::Status(status)));
    vm.on_disconnected("test".to_owned());
    assert_eq!(
        vm.discovered_peers().count(),
        0,
        "offline announcements are stale"
    );
}

#[test]
fn hiding_at_capacity_keeps_the_new_choice_and_evicts_only_the_oldest() {
    let mut vm = view();
    let limit = lws_gui::gui_settings::HIDDEN_DISCOVERED_LIMIT;
    for n in 0..limit {
        vm.local(&Action::HideDiscovered(format!("{n:064x}")));
    }
    let saved = vm.look.clone();
    vm.local(&Action::HideDiscovered(format!("{:064x}", 1)));
    vm.local(&Action::HideDiscovered("invalid".into()));
    assert_eq!(
        vm.look, saved,
        "duplicates and invalid IDs never evict choices"
    );
    let newest = "ff".repeat(32);
    vm.local(&Action::HideDiscovered(newest.clone()));
    assert_eq!(vm.look.hidden_discovered.len(), limit);
    assert!(!vm.look.hidden_discovered.contains(&format!("{:064x}", 0)));
    assert!(vm.look.hidden_discovered.contains(&format!("{:064x}", 1)));
    assert!(vm.look.hidden_discovered.contains(&newest));
    assert!(
        !saved.hidden_discovered.contains(&newest),
        "snapshots stay unchanged"
    );
    assert!(saved.hidden_discovered.contains(&format!("{:064x}", 0)));
    vm.local(&Action::ShowDiscovered(newest));
    assert_eq!(vm.look.hidden_discovered.len(), limit - 1);
    assert!(vm.take_outgoing().is_empty());
}

#[test]
fn hidden_discovery_is_local_reversible_and_follows_current_status() {
    let mut vm = view();
    vm.on_connected("test");
    let visible = "cc".repeat(32);
    let hidden = "dd".repeat(32);
    let mut status = status_with("LOCAL");
    status.discovered_peers = vec![
        discovered_peer(PEER_NODE, true),
        discovered_peer(&visible, false),
        discovered_peer(&hidden, true),
    ];
    vm.apply(event(Msg::Status(status.clone())));
    let before_editor = vm.editor.nodes.clone();
    let hide = Action::HideDiscovered(hidden.clone());
    assert!(matches!(
        hide.wire(),
        Some((_, Msg::MutePairingRequester(_)))
    ));
    vm.local(&hide);
    vm.local(&hide);
    vm.local(&Action::HideDiscovered("not-an-identity".into()));
    assert_eq!(vm.look.hidden_discovered.len(), 1);
    assert_eq!(
        vm.discovered_peers()
            .map(|peer| peer.node_id.as_str())
            .collect::<Vec<_>>(),
        vec![visible.as_str()]
    );
    assert_eq!(
        vm.hidden_discovered_peers()
            .map(|peer| peer.node_id.as_str())
            .collect::<Vec<_>>(),
        vec![hidden.as_str()]
    );
    assert_eq!(vm.editor.nodes, before_editor);
    assert_eq!(vm.status.as_ref(), Some(&status));
    assert!(
        vm.take_outgoing().is_empty(),
        "hiding never changes trust or topology"
    );

    let show = Action::ShowDiscovered(hidden.clone());
    assert!(show.wire().is_none());
    vm.local(&show);
    assert_eq!(vm.discovered_peers().count(), 2);
    assert_eq!(vm.hidden_discovered_peers().count(), 0);
    vm.local(&hide);

    vm.local(&Action::SetDiscovery(false));
    assert_eq!(
        vm.discovered_peers().count(),
        0,
        "pending discovery-off hides immediately"
    );
    assert_eq!(vm.hidden_discovered_peers().count(), 0);
    status.discovery_enabled = false;
    vm.apply(event(Msg::Status(status.clone())));
    assert_eq!(vm.discovered_peers().count(), 0);
    assert_eq!(vm.hidden_discovered_peers().count(), 0);
    status.discovery_enabled = true;
    vm.apply(event(Msg::Status(status.clone())));
    assert_eq!(vm.hidden_discovered_peers().count(), 1);

    status.members.push(protocol::ipc::MemberStatus {
        node_id: hidden.clone(),
        ..Default::default()
    });
    vm.apply(event(Msg::Status(status.clone())));
    assert_eq!(
        vm.hidden_discovered_peers().count(),
        0,
        "paired identities are never phantoms"
    );
    status.members.retain(|member| member.node_id != hidden);
    vm.apply(event(Msg::Status(status.clone())));
    assert_eq!(vm.hidden_discovered_peers().count(), 1);

    status.discovered_peers.clear();
    vm.apply(event(Msg::Status(status.clone())));
    assert_eq!(vm.discovered_peers().count(), 0);
    assert_eq!(
        vm.hidden_discovered_peers().count(),
        0,
        "TTL removal is authoritative"
    );
    assert!(
        vm.look.hidden_discovered.contains(&hidden),
        "TTL does not forget user choices"
    );
    status.discovered_peers.push(discovered_peer(&hidden, true));
    vm.apply(event(Msg::Status(status)));
    assert_eq!(
        vm.hidden_discovered_peers().count(),
        1,
        "reannouncement remains hidden"
    );
    vm.on_disconnected("test".into());
    assert_eq!(vm.hidden_discovered_peers().count(), 0);
    vm.local(&show);
    assert!(
        vm.look.hidden_discovered.is_empty(),
        "offline identities can be restored"
    );
}

#[test]
fn discovered_pairing_requests_permission_and_still_needs_sas() {
    let mut vm = view();
    vm.on_connected("test");
    let peer = discovered_peer(&"cc".repeat(32), true);
    let action = Action::connect_discovered(&peer);
    let Some((request, Msg::RequestPairing(dial))) = action.wire() else {
        panic!("discovery must send a pairing request");
    };
    assert_eq!(request, Request::PairingRequest);
    assert_eq!(dial.address, peer.address);
    vm.look.dock_open = false;
    vm.modal = Some(Modal::Pairs);
    vm.appearance_open = true;
    vm.local(&action);
    assert!(
        vm.look.dock_open,
        "map and dock use the same action handler"
    );
    assert!(vm.modal.is_none());
    assert!(!vm.appearance_open);
    assert!(
        !vm.pairing.dialing,
        "waiting for permission is not yet a SAS ceremony"
    );
    assert!(vm.pairing.paired.is_none());
    vm.apply(event(Msg::PairingPrompt(pairing_prompt())));
    assert_eq!(vm.pairing.attempt_id(), Some(attempt_id()));
    assert!(
        vm.pairing.paired.is_none(),
        "announcements and prompts cannot approve SAS"
    );
    assert!(vm.take_outgoing().is_empty(), "no automatic pairing answer");
    assert!(
        Action::PairWith("10.0.0.2:47901".to_owned())
            .wire()
            .is_some(),
        "manual dial remains available"
    );
}

#[test]
fn discovery_toggle_is_core_owned_and_settles_on_status_or_failure() {
    let mut vm = view();
    vm.on_connected("test");
    vm.apply(event(Msg::Status(status_with("LOCAL"))));
    let action = Action::SetDiscovery(false);
    let Some((request, Msg::SetDiscovery(change))) = action.wire() else {
        panic!("discovery setting must travel through core IPC");
    };
    assert!(!change.enabled);
    assert_eq!(request, Request::SetDiscovery);
    vm.local(&action);
    let id = vm.begin_request(request, Instant::now());
    assert!(!vm.discovery_enabled());
    assert!(
        vm.config.discovery_enabled,
        "GUI must not edit persisted config"
    );
    let mut status = status_with("LOCAL");
    status.discovery_enabled = false;
    vm.apply(event(Msg::Status(status)));
    assert_eq!(vm.settings.discovery_pending, None);
    assert!(!vm.config.discovery_enabled);
    vm.apply(answer(
        id,
        Msg::OpResult(protocol::ipc::OpResult {
            ok: true,
            error_code: String::new(),
        }),
    ));
    assert!(vm
        .take_outgoing()
        .iter()
        .any(|(_, msg)| matches!(msg, Msg::GetStatus(_))));
    vm.local(&Action::SetDiscovery(true));
    let id = vm.begin_request(Request::SetDiscovery, Instant::now());
    vm.apply(answer(
        id,
        Msg::OpResult(protocol::ipc::OpResult {
            ok: false,
            error_code: "PERSISTENCE_FAILED".to_owned(),
        }),
    ));
    assert_eq!(vm.settings.discovery_pending, None);
    assert!(!vm.discovery_enabled());
    vm.local(&Action::SetDiscovery(true));
    vm.on_disconnected("test".to_owned());
    assert_eq!(vm.settings.discovery_pending, None);
}

#[test]
fn local_config_reads_discovery_and_defaults_to_enabled() {
    let dir = settings_dir("192.168.1.10");
    let paths = paths_of(&dir);
    assert!(lws_gui::view_model::LocalConfig::load(&paths).discovery_enabled);
    let mut config = config::Config::load(paths.config_file()).expect("valid test config");
    config.discovery.enabled = false;
    config.save(paths.config_file()).expect("discovery config");
    assert!(!lws_gui::view_model::LocalConfig::load(&paths).discovery_enabled);
    std::fs::remove_dir_all(dir).expect("remove test config");
}

#[test]
fn mesh_actions_carry_checked_target_and_remote_policy_to_the_core() {
    let Some((Request::UpdateAll, Msg::UpdateAll(request))) =
        Action::UpdateAll("0.1.7".into()).wire()
    else {
        panic!("mesh update must reach the core");
    };
    assert_eq!(request.target_version, "0.1.7");
    let Some((Request::SetRemoteUpdate, Msg::SetRemoteUpdate(request))) =
        Action::SetRemoteUpdate(false).wire()
    else {
        panic!("remote policy must reach the core");
    };
    assert!(!request.enabled);
    let Some((Request::RegisterUpdateCoordinator, Msg::UpdateProgress(request))) =
        Action::RegisterUpdateCoordinator(1234).wire()
    else {
        panic!("the runtime must register its real coordinator process");
    };
    assert_eq!(request.status, "coordinator");
    assert_eq!(request.detail, "1234");
    assert!(request.target_version.is_empty());
}

#[test]
fn topology_and_status_versions_feed_badges_without_guessing_unknown_peers() {
    let mut vm = view();
    vm.apply(event(Msg::Status(status_with("LOCAL"))));
    let mut doc = two_node_doc(7);
    doc.members
        .iter_mut()
        .find(|member| member.node_id == PEER_NODE)
        .unwrap()
        .app_version = "0.1.5".into();
    vm.apply(event(Msg::TopologyDoc(doc)));
    assert_eq!(vm.node_app_version(PEER_NODE), "0.1.5");
    assert_eq!(
        vm.node_app_version(SELF_NODE),
        protocol::sanitize_app_version(protocol::app_version())
    );
    let mut status = status_with("LOCAL");
    status
        .members
        .iter_mut()
        .find(|member| member.node_id == PEER_NODE)
        .unwrap()
        .app_version = "0.1.7".into();
    vm.apply(event(Msg::Status(status.clone())));
    assert_eq!(vm.node_app_version(PEER_NODE), "0.1.7");
    status
        .members
        .iter_mut()
        .find(|member| member.node_id == PEER_NODE)
        .unwrap()
        .app_version = String::new();
    vm.apply(event(Msg::Status(status)));
    assert!(!vm.node_needs_update(PEER_NODE));
}

#[test]
fn incoming_request_reopens_dock_and_buttons_name_the_exact_requester() {
    let mut vm = view();
    vm.look.dock_open = false;
    vm.modal = Some(Modal::Settings);
    vm.appearance_open = true;
    let prompt = protocol::ipc::IncomingPairingRequest {
        requester_node_id: vec![7; 32],
        display_name: "WORKSTATION".into(),
        fingerprint: "0707".into(),
        seconds_remaining: 60,
        ..Default::default()
    };
    vm.apply(event(Msg::IncomingPairingRequest(prompt.clone())));
    assert!(vm.look.dock_open);
    assert!(vm.modal.is_none());
    assert!(!vm.appearance_open);
    assert_eq!(vm.incoming_pair_request, Some(prompt.clone()));
    for accept in [true, false] {
        let action = Action::AnswerPairingRequest {
            requester_node_id: prompt.requester_node_id.clone(),
            accept,
        };
        let Some((Request::PairingRequest, Msg::AnswerPairingRequest(answer))) = action.wire()
        else {
            panic!("request decision");
        };
        assert_eq!(answer.requester_node_id, prompt.requester_node_id);
        assert_eq!(answer.accept, accept);
    }
    assert!(matches!(
        Action::CancelPairingRequest.wire(),
        Some((_, Msg::CancelPairingRequest(_)))
    ));
    assert!(
        matches!(Action::SetPairingRequests(false).wire(), Some((Request::SetPairingRequests, Msg::SetPairingRequests(set))) if !set.enabled)
    );
    vm.apply(event(Msg::IncomingPairingRequest(Default::default())));
    assert!(vm.incoming_pair_request.is_none());
    assert!(
        vm.pairing.paired.is_none(),
        "prompt-only acceptance never creates trust"
    );
}

#[test]
fn discovery_fallback_and_hidden_mute_are_explicit_wire_actions() {
    let peer = protocol::ipc::DiscoveredPeer {
        node_id: "ab".repeat(32),
        ..Default::default()
    };
    assert!(
        matches!(Action::connect_discovered(&peer).wire(), Some((_, Msg::RequestPairing(request))) if !request.supports_requests)
    );
    assert!(
        matches!(Action::HideDiscovered(peer.node_id.clone()).wire(), Some((_, Msg::MutePairingRequester(request))) if request.node_id == peer.node_id)
    );
}

#[test]
fn pairing_request_setting_uses_saved_config_until_core_status_arrives() {
    let dir = settings_dir("192.168.1.10");
    let paths = paths_of(&dir);
    assert!(lws_gui::view_model::LocalConfig::load(&paths).accept_pairing_requests);
    let mut config = config::Config::load(paths.config_file()).unwrap();
    config.pairing.accept_requests = false;
    config.save(paths.config_file()).unwrap();
    let mut vm = view();
    vm.config = lws_gui::view_model::LocalConfig::load(&paths);
    assert!(!vm.config.accept_pairing_requests);
    let mut status = status_with("LOCAL");
    status.accept_pairing_requests = true;
    vm.apply(event(Msg::Status(status)));
    assert!(vm.config.accept_pairing_requests);
    vm.on_disconnected("test".into());
    assert!(vm.config.accept_pairing_requests);
    std::fs::remove_dir_all(dir).unwrap();
}
