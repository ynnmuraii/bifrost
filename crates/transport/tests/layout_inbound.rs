//! The layout half of the wire: `Inbound::Layout` round trips and stays out of the focus mirror.
//!
//! `docs/03-architecture.md` gives the layout document its own messages (`Topology`,
//! `LayoutEdit`, `TopologyRequest`, `TopologyApplied`, `ActiveLayout`) and the refusals that
//! go with them (`LAYOUT_CONFLICT`, `INVALID_TOPOLOGY`, `NO_COORDINATOR`). None of them drives
//! a transition, so none of them belongs in `focus_router::Message`; transport delivers them
//! beside the mirror, and these tests pin that split: every layout payload comes back out of
//! `inbound_from_envelope` as `Inbound::Layout`, a focus payload still comes back as
//! `Inbound::Focus`, and a refusal with a *layout* code is a layout message while any other
//! `ProtocolError` stays with the FSM.

use protocol::wire;
use transport::convert::{
    inbound_from_envelope, is_layout_code, layout_envelope, Inbound, LayoutMessage,
};

const CONNECTION: [u8; 16] = [0x11; 16];

fn round_trip(message: &LayoutMessage) -> LayoutMessage {
    let envelope = layout_envelope(message, 7, &CONNECTION);
    assert_eq!(envelope.seq, 7, "the caller's sequence travels unchanged");
    assert!(
        envelope.lease_id.is_empty(),
        "layout messages belong to the connection, not to a lease"
    );
    match inbound_from_envelope(&envelope).expect("a layout payload must convert") {
        Inbound::Layout(message) => message,
        Inbound::Focus(other) => panic!("expected a layout message, got {other:?}"),
        Inbound::Clipboard { message, .. } => {
            panic!("expected a layout message, got a clipboard one: {message:?}")
        }
    }
}

fn node(byte: u8) -> Vec<u8> {
    vec![byte; 32]
}

fn document() -> wire::Topology {
    wire::Topology {
        revision: 4,
        nodes: vec![wire::NodeDisplays {
            node_id: node(1),
            displays: vec![wire::Display {
                id: "eDP-1".to_owned(),
                logical_bounds: Some(wire::Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 1920.0,
                    height: 1080.0,
                }),
                native_bounds: None,
                rotation_degrees: 0,
                scale_x: 1.0,
                scale_y: 1.0,
                primary: true,
                native_known: false,
            }],
            online: true,
        }],
        links: vec![wire::TransitionLink {
            source_endpoint: Some(wire::Endpoint {
                node_id: node(1),
                display_id: "eDP-1".to_owned(),
                edge: wire::Edge::Right as i32,
                segment_start: 0.0,
                segment_end: 1080.0,
            }),
            target_endpoint: Some(wire::Endpoint {
                node_id: node(2),
                display_id: "HDMI-1".to_owned(),
                edge: wire::Edge::Left as i32,
                segment_start: 0.0,
                segment_end: 1080.0,
            }),
        }],
        author_node_id: node(1),
    }
}

/// Every layout payload survives the trip through an envelope unchanged — including the whole
/// document, which is what makes replication a copy rather than a translation (ADR-013).
#[test]
fn every_layout_payload_round_trips() {
    let topology = document();
    let cases = [
        LayoutMessage::Topology(topology.clone()),
        LayoutMessage::LayoutEdit(wire::LayoutEdit {
            expected_revision: 3,
            expected_author: node(1),
            author_node_id: node(2),
            layout_toml: b"[[topology.links]]".to_vec(),
            request_nonce: 7,
        }),
        LayoutMessage::TopologyRequest,
        LayoutMessage::TopologyApplied {
            revision: 9,
            request_nonce: 7,
        },
        LayoutMessage::ActiveLayout(wire::ActiveLayout {
            layout_id: "us".to_owned(),
            layout_revision: 2,
        }),
        LayoutMessage::Error {
            code: wire::ErrorCode::LayoutConflict,
            request_nonce: 7,
            layout_revision: 5,
        },
    ];
    for case in cases {
        assert_eq!(round_trip(&case), case, "{case:?}");
    }
}

/// The refusals of KVM-072 travel on the layout arm with their own codes; every other
/// `ProtocolError` keeps its place in the focus vocabulary.
#[test]
fn layout_refusal_codes_are_layout_messages() {
    for code in [
        wire::ErrorCode::LayoutConflict,
        wire::ErrorCode::InvalidTopology,
        wire::ErrorCode::NoCoordinator,
    ] {
        assert!(is_layout_code(code), "{code:?} is a layout code");
        assert_eq!(
            round_trip(&LayoutMessage::Error {
                code,
                request_nonce: 0,
                layout_revision: 0,
            }),
            LayoutMessage::Error {
                code,
                request_nonce: 0,
                layout_revision: 0,
            }
        );
    }
    assert!(!is_layout_code(wire::ErrorCode::NotOwner));
    assert!(!is_layout_code(wire::ErrorCode::TopologyChanged));

    let envelope = layout_envelope(
        &LayoutMessage::Error {
            code: wire::ErrorCode::NotOwner,
            request_nonce: 0,
            layout_revision: 0,
        },
        1,
        &CONNECTION,
    );
    // `NotOwner` is an FSM code: it must arrive as a focus message, not as a layout one.
    match inbound_from_envelope(&envelope) {
        Ok(Inbound::Focus(focus_router::Message::ProtocolError { .. })) => {}
        other => panic!("expected a focus protocol error, got {other:?}"),
    }
}

/// A focus message still arrives as a focus message: the split did not move the mirror.
#[test]
fn focus_payloads_stay_on_the_focus_arm() {
    let envelope = layout_envelope(&LayoutMessage::TopologyRequest, 1, &CONNECTION);
    assert!(matches!(
        inbound_from_envelope(&envelope),
        Ok(Inbound::Layout(LayoutMessage::TopologyRequest))
    ));

    let claim = focus_router::Message::OwnerClaim {
        serial: 3,
        reason: focus_router::ClaimReason::Startup,
    };
    let envelope = transport::convert::to_envelope(&claim, 2, &CONNECTION, None, "")
        .expect("a claim converts");
    match inbound_from_envelope(&envelope) {
        Ok(Inbound::Focus(message)) => assert_eq!(message, claim),
        other => panic!("expected the focus mirror, got {other:?}"),
    }
}
