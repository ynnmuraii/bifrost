//! The wire contract: a round trip for every focus/lease message, the enum numbers
//! measured against the generated protobuf enums, and the `lease_id` lifting rule.

use focus_router as fr;
use geometry::{NodeId, Position};
use input_model::{HidKey, InputEvent, KeyState, MouseButton};
use protocol::wire;
use transport::convert::{
    batch_display_after, claim_reason_from_i32, error_code_from_i32, from_envelope,
    inject_status_from_i32, lease_id_of, lease_id_of_message, leave_reason_from_i32, to_envelope,
    ConvertError, PeerInfo,
};

const CONNECTION: [u8; 16] = [0xaa; 16];
const LEASE: [u8; 16] = [0xbb; 16];
const EMPTY_LEASE: [u8; 16] = [0u8; 16];

fn envelope_of(message: &fr::Message, seq: u64) -> wire::Envelope {
    let lease = lease_id_of_message(message);
    to_envelope(message, seq, &CONNECTION, lease.as_ref(), "dp1").expect("the message converts")
}

fn round_trip(message: &fr::Message, seq: u64) -> fr::Message {
    from_envelope(&envelope_of(message, seq)).expect("the frame converts back")
}

fn some_owner() -> NodeId {
    NodeId::new([0x11; 32])
}

#[test]
fn every_focus_and_lease_message_round_trips() {
    let events = vec![
        InputEvent::Key {
            key: HidKey::keyboard(0x04),
            state: KeyState::Down,
        },
        InputEvent::Button {
            button: MouseButton::Forward,
            state: KeyState::Up,
        },
        InputEvent::Scroll {
            h120: -120,
            v120: 240,
        },
        InputEvent::PointerAbsolute(input_model::Position::new(4.0, 600.0)),
    ];

    let cases: Vec<(fr::Message, u64)> = vec![
        (
            fr::Message::EnterPrepare {
                lease_id: LEASE,
                topology_revision: 7,
                position: Position::new("dp1", 4.0, 600.0),
                owner_serial: 3,
            },
            1,
        ),
        (
            fr::Message::EnterAck {
                lease_id: LEASE,
                accepted: true,
                reason: None,
            },
            2,
        ),
        (
            fr::Message::EnterAck {
                lease_id: LEASE,
                accepted: false,
                reason: Some(fr::ErrorCode::CaptureFailed),
            },
            3,
        ),
        (fr::Message::EnterCommit { lease_id: LEASE }, 4),
        (
            fr::Message::Leave {
                lease_id: LEASE,
                reason: fr::LeaveReason::Emergency,
            },
            5,
        ),
        (fr::Message::LeaveAck { lease_id: LEASE }, 6),
        (
            fr::Message::InputBatch {
                lease_id: LEASE,
                input_batch_seq: 7,
                events: events.clone(),
            },
            7,
        ),
        (
            fr::Message::InjectAck {
                lease_id: LEASE,
                input_batch_seq: 7,
                applied_count: 2,
                actual_position: Some(Position::new("dp1", 5.0, 601.0)),
                status: fr::InjectStatus::Partial,
            },
            8,
        ),
        (
            fr::Message::LeaseChallenge {
                lease_id: LEASE,
                nonce: [0xcc; 16],
                ttl_ms: 500,
            },
            9,
        ),
        (
            fr::Message::LeaseRenew {
                lease_id: LEASE,
                nonce: [0xcc; 16],
            },
            10,
        ),
        (
            fr::Message::LeaseStatus {
                lease_id: LEASE,
                nonce: [0xcc; 16],
                accepted: true,
                last_input_batch_seq: 7,
            },
            11,
        ),
        (
            fr::Message::OwnerClaim {
                serial: 3,
                reason: fr::ClaimReason::Takeover,
            },
            12,
        ),
        (
            fr::Message::OwnerState {
                serial: 3,
                owner: Some(some_owner()),
                granted: true,
            },
            13,
        ),
        (
            fr::Message::OwnerState {
                serial: 0,
                owner: None,
                granted: false,
            },
            14,
        ),
        (
            fr::Message::ProtocolError {
                code: fr::ErrorCode::NotOwner,
                related_seq: 9,
            },
            15,
        ),
        (
            fr::Message::Shutdown {
                reason: fr::LeaveReason::Shutdown,
            },
            16,
        ),
    ];

    for (message, seq) in cases {
        assert_eq!(
            round_trip(&message, seq),
            message,
            "{message:?} did not round trip"
        );
    }
}

#[test]
fn a_lease_message_with_no_lease_id_travels_without_one() {
    let message = fr::Message::EnterCommit {
        lease_id: EMPTY_LEASE,
    };
    assert_eq!(lease_id_of_message(&message), None);

    let envelope = envelope_of(&message, 1);
    assert!(
        envelope.lease_id.is_empty(),
        "no lease id on the envelope either"
    );
    assert_eq!(round_trip(&message, 1), message);
}

#[test]
fn the_lease_id_is_lifted_out_of_the_envelope_and_put_back() {
    // The wire keeps the lease id on the envelope; the state machine keeps it in the
    // message. Both directions must agree on the same string of 16 bytes.
    let message = fr::Message::LeaveAck { lease_id: LEASE };
    assert_eq!(lease_id_of_message(&message), Some(LEASE));

    let envelope = envelope_of(&message, 1);
    assert_eq!(lease_id_of(&envelope), Ok(Some(LEASE)));
    assert_eq!(round_trip(&message, 1), message);

    // Non-lease messages never carry one, whatever their text.
    for message in [
        fr::Message::OwnerClaim {
            serial: 1,
            reason: fr::ClaimReason::User,
        },
        fr::Message::OwnerState {
            serial: 1,
            owner: None,
            granted: false,
        },
        fr::Message::ProtocolError {
            code: fr::ErrorCode::NotOwner,
            related_seq: 1,
        },
        fr::Message::Shutdown {
            reason: fr::LeaveReason::Shutdown,
        },
    ] {
        assert_eq!(lease_id_of_message(&message), None, "{message:?}");
        assert!(envelope_of(&message, 1).lease_id.is_empty());
    }

    // A malformed lease id is refused instead of being truncated.
    let mut envelope = envelope_of(&fr::Message::LeaveAck { lease_id: LEASE }, 1);
    envelope.lease_id.truncate(7);
    assert_eq!(
        lease_id_of(&envelope),
        Err(ConvertError::Unsupported("lease_id is not 16 bytes"))
    );
}

#[test]
fn the_envelope_carries_the_protocol_version_and_the_callers_identifiers() {
    let message = fr::Message::OwnerClaim {
        serial: 1,
        reason: fr::ClaimReason::Startup,
    };
    let envelope = envelope_of(&message, 4242);

    assert_eq!(envelope.major, protocol::limits::PROTOCOL_MAJOR);
    assert_eq!(envelope.minor, protocol::limits::PROTOCOL_MINOR);
    assert_eq!(envelope.seq, 4242);
    assert_eq!(envelope.connection_id, CONNECTION.to_vec());
}

#[test]
fn the_transport_owns_the_batch_sequence() {
    // The message's own field is stamped over, because the wire defines the batch sequence
    // *as* the envelope sequence: a caller numbering batches independently would collide
    // with the control frames of the same direction.
    let message = fr::Message::InputBatch {
        lease_id: LEASE,
        input_batch_seq: 99,
        events: vec![InputEvent::PointerAbsolute(input_model::Position::new(
            1.0, 2.0,
        ))],
    };
    let converted = round_trip(&message, 7);
    match converted {
        fr::Message::InputBatch {
            input_batch_seq,
            events,
            ..
        } => {
            assert_eq!(input_batch_seq, 7, "the envelope sequence wins");
            assert_eq!(
                events,
                vec![InputEvent::PointerAbsolute(input_model::Position::new(
                    1.0, 2.0
                ))]
            );
        }
        other => panic!("expected an InputBatch, got {other:?}"),
    }
}

#[test]
fn every_focus_message_event_kind_survives_the_round_trip() {
    let events = vec![
        InputEvent::Key {
            key: HidKey::keyboard(0x04),
            state: KeyState::Down,
        },
        InputEvent::Key {
            key: HidKey {
                page: 0x0c,
                usage: 0x38,
            },
            state: KeyState::Up,
        },
        InputEvent::Button {
            button: MouseButton::Back,
            state: KeyState::Down,
        },
        InputEvent::Scroll {
            h120: 120,
            v120: -240,
        },
        InputEvent::PointerAbsolute(input_model::Position::new(-10.5, 20.0)),
    ];
    let message = fr::Message::InputBatch {
        lease_id: LEASE,
        input_batch_seq: 5,
        events: events.clone(),
    };
    assert_eq!(round_trip(&message, 5), message);
}

#[test]
fn every_button_number_matches_the_generated_button_kind() {
    for (button, kind) in [
        (MouseButton::Left, wire::ButtonKind::Left),
        (MouseButton::Right, wire::ButtonKind::Right),
        (MouseButton::Middle, wire::ButtonKind::Middle),
        (MouseButton::Back, wire::ButtonKind::Back),
        (MouseButton::Forward, wire::ButtonKind::Forward),
    ] {
        assert_eq!(
            button as i32, kind as i32,
            "MouseButton and the wire ButtonKind must agree on the numbering"
        );

        // And the number survives the conversion in both directions.
        let message = fr::Message::InputBatch {
            lease_id: LEASE,
            input_batch_seq: 1,
            events: vec![InputEvent::Button {
                button,
                state: KeyState::Down,
            }],
        };
        assert_eq!(round_trip(&message, 1), message);
    }
}

#[test]
fn key_states_use_the_generated_numbers() {
    for (state, wire_state) in [
        (KeyState::Down, wire::KeyState::Down),
        (KeyState::Up, wire::KeyState::Up),
    ] {
        let message = fr::Message::InputBatch {
            lease_id: LEASE,
            input_batch_seq: 1,
            events: vec![InputEvent::Key {
                key: HidKey::keyboard(0x04),
                state,
            }],
        };
        let envelope = envelope_of(&message, 1);
        let Some(wire::envelope::Payload::InputBatch(batch)) = envelope.payload else {
            panic!("expected an InputBatch payload");
        };
        let Some(wire::input_event::Event::Key(key)) = batch.events[0].event.as_ref() else {
            panic!("expected a Key event");
        };
        assert_eq!(key.state, wire_state as i32);
        assert_eq!(key.usage_page, 0x07);
        assert_eq!(key.usage, 0x04);

        assert_eq!(round_trip(&message, 1), message);
    }
}

/// The wire number of an `ErrorCode` variant, taken from the generated enum.
fn wire_error_code(code: fr::ErrorCode) -> wire::ErrorCode {
    use fr::ErrorCode as E;
    match code {
        E::UnsupportedSession => wire::ErrorCode::UnsupportedSession,
        E::CaptureFailed => wire::ErrorCode::CaptureFailed,
        E::TopologyChanged => wire::ErrorCode::TopologyChanged,
        E::InvalidFrame => wire::ErrorCode::InvalidFrame,
        E::InvalidSequence => wire::ErrorCode::InvalidSequence,
        E::LeaseMismatch => wire::ErrorCode::LeaseMismatch,
        E::InjectionFailed => wire::ErrorCode::InjectionFailed,
        E::LeaseExpired => wire::ErrorCode::LeaseExpired,
        E::AgentLost => wire::ErrorCode::AgentLost,
        E::NotOwner => wire::ErrorCode::NotOwner,
        E::StaleOwnerSerial => wire::ErrorCode::StaleOwnerSerial,
        E::CannotOwn => wire::ErrorCode::CannotOwn,
        E::MembershipFull => wire::ErrorCode::MembershipFull,
    }
}

#[test]
fn every_error_code_number_matches_the_generated_enum() {
    use fr::ErrorCode as E;
    for code in [
        E::UnsupportedSession,
        E::CaptureFailed,
        E::TopologyChanged,
        E::InvalidFrame,
        E::InvalidSequence,
        E::LeaseMismatch,
        E::InjectionFailed,
        E::LeaseExpired,
        E::AgentLost,
        E::NotOwner,
        E::StaleOwnerSerial,
        E::CannotOwn,
        E::MembershipFull,
    ] {
        assert_eq!(
            code.as_u32() as i32,
            wire_error_code(code) as i32,
            "{code:?} disagrees with the generated ErrorCode"
        );
    }
}

#[test]
fn every_leave_claim_and_inject_number_matches_the_generated_enum() {
    use fr::{ClaimReason, InjectStatus, LeaveReason};

    for (reason, generated) in [
        (LeaveReason::ReturnEdge, wire::LeaveReason::ReturnEdge),
        (LeaveReason::Hotkey, wire::LeaveReason::Hotkey),
        (LeaveReason::Emergency, wire::LeaveReason::Emergency),
        (
            LeaveReason::ReceiverLocalInput,
            wire::LeaveReason::ReceiverLocalInput,
        ),
        (LeaveReason::AgentLost, wire::LeaveReason::AgentLost),
        (
            LeaveReason::TopologyChanged,
            wire::LeaveReason::TopologyChanged,
        ),
        (LeaveReason::SuspendOrLock, wire::LeaveReason::SuspendOrLock),
        (LeaveReason::Shutdown, wire::LeaveReason::Shutdown),
        (LeaveReason::OwnershipLost, wire::LeaveReason::OwnershipLost),
    ] {
        assert_eq!(reason.as_u32() as i32, generated as i32, "{reason:?}");
    }

    for (reason, generated) in [
        (ClaimReason::Startup, wire::ClaimReason::Startup),
        (ClaimReason::User, wire::ClaimReason::User),
        (ClaimReason::Takeover, wire::ClaimReason::Takeover),
    ] {
        assert_eq!(reason.as_u32() as i32, generated as i32, "{reason:?}");
    }

    for (status, generated) in [
        (InjectStatus::Ok, wire::InjectStatus::Ok),
        (InjectStatus::Partial, wire::InjectStatus::Partial),
        (InjectStatus::Failed, wire::InjectStatus::Failed),
    ] {
        assert_eq!(status.as_u32() as i32, generated as i32, "{status:?}");
    }
}

#[test]
fn the_error_code_reader_accepts_exactly_its_documented_domain() {
    use fr::ErrorCode as E;

    // Every code this layer reads back round trips through its own wire number.
    for code in [
        E::UnsupportedSession,
        E::CaptureFailed,
        E::TopologyChanged,
        E::InvalidFrame,
        E::InvalidSequence,
        E::LeaseMismatch,
        E::InjectionFailed,
        E::LeaseExpired,
        E::AgentLost,
        E::NotOwner,
        E::StaleOwnerSerial,
        E::CannotOwn,
        // The link manager emits this one and the refused peer has to classify it, or a
        // full workspace is indistinguishable from a protocol failure.
        E::MembershipFull,
    ] {
        assert_eq!(error_code_from_i32(code.as_u32() as i32), Ok(code));
    }

    // Codes the contract defines but `focus_router` has no variant for stay refused:
    // accepting them would mean silently relabelling them. `0` is the wire's
    // "unspecified" (an absent reason, not a code).
    for value in [
        0, 7,  // InvalidTopology
        15, // ClipboardTooLarge
        18, // RateLimited
        22, // LayoutConflict
        24, // NoCoordinator
        26, -1,
    ] {
        assert_eq!(
            error_code_from_i32(value),
            Err(ConvertError::UnknownEnum {
                field: "ErrorCode",
                value
            }),
            "value {value} must be refused"
        );
    }

    // And the round trip through a real envelope uses the same reader.
    let message = fr::Message::ProtocolError {
        code: E::StaleOwnerSerial,
        related_seq: 3,
    };
    assert_eq!(round_trip(&message, 3), message);
}

#[test]
fn the_leave_claim_and_inject_readers_refuse_unknown_numbers() {
    use fr::{ClaimReason, InjectStatus, LeaveReason};

    for (reason, value) in [
        (LeaveReason::ReturnEdge, 1),
        (LeaveReason::OwnershipLost, 9),
    ] {
        assert_eq!(leave_reason_from_i32(value), Ok(reason));
    }
    for value in [0, 10, -1] {
        assert_eq!(
            leave_reason_from_i32(value),
            Err(ConvertError::UnknownEnum {
                field: "LeaveReason",
                value
            })
        );
    }

    for (reason, value) in [(ClaimReason::Startup, 1), (ClaimReason::Takeover, 3)] {
        assert_eq!(claim_reason_from_i32(value), Ok(reason));
    }
    for value in [0, 4] {
        assert_eq!(
            claim_reason_from_i32(value),
            Err(ConvertError::UnknownEnum {
                field: "ClaimReason",
                value
            })
        );
    }

    for (status, value) in [(InjectStatus::Ok, 1), (InjectStatus::Failed, 3)] {
        assert_eq!(inject_status_from_i32(value), Ok(status));
    }
    for value in [0, 4] {
        assert_eq!(
            inject_status_from_i32(value),
            Err(ConvertError::UnknownEnum {
                field: "InjectStatus",
                value
            })
        );
    }
}

#[test]
fn relative_pointer_events_have_no_wire_representation() {
    // Relative-only injection is excluded by the docs, so a relative event must be
    // integrated into an absolute position before it is sent — not silently dropped.
    let message = fr::Message::InputBatch {
        lease_id: LEASE,
        input_batch_seq: 1,
        events: vec![
            InputEvent::PointerAbsolute(input_model::Position::new(1.0, 1.0)),
            InputEvent::PointerRelative { dx: 3, dy: 4 },
        ],
    };
    assert_eq!(
        to_envelope(&message, 1, &CONNECTION, Some(&LEASE), "dp1"),
        Err(ConvertError::RelativePointerNotOnWire)
    );
}

#[test]
fn absolute_positions_in_a_batch_take_the_batch_display() {
    let message = fr::Message::InputBatch {
        lease_id: LEASE,
        input_batch_seq: 1,
        events: vec![InputEvent::PointerAbsolute(input_model::Position::new(
            960.0, 540.0,
        ))],
    };
    let envelope = to_envelope(&message, 1, &CONNECTION, Some(&LEASE), "eDP-1").expect("convert");
    let Some(wire::envelope::Payload::InputBatch(batch)) = envelope.payload else {
        panic!("expected an InputBatch payload");
    };
    let Some(wire::input_event::Event::PointerAbsolute(position)) = batch.events[0].event.as_ref()
    else {
        panic!("expected a PointerAbsolute event");
    };
    assert_eq!(position.display_id, "eDP-1");
    assert_eq!((position.x, position.y), (960.0, 540.0));

    // The receiver refuses a position without a display id: it has nothing to resolve it
    // into, and guessing a display would inject into the wrong place.
    let empty_display = wire::Envelope {
        payload: Some(wire::envelope::Payload::InputBatch(wire::InputBatch {
            events: vec![wire::InputEvent {
                capture_monotonic_us: 0,
                event: Some(wire::input_event::Event::PointerAbsolute(wire::Position {
                    display_id: String::new(),
                    x: 1.0,
                    y: 2.0,
                })),
            }],
        })),
        ..envelope
    };
    assert_eq!(
        from_envelope(&empty_display),
        Err(ConvertError::Unsupported(
            "PointerAbsolute without display_id"
        ))
    );
}

#[test]
fn payloads_this_layer_does_not_own_are_refused() {
    let base = envelope_of(
        &fr::Message::OwnerClaim {
            serial: 1,
            reason: fr::ClaimReason::User,
        },
        1,
    );

    let empty = wire::Envelope {
        payload: None,
        ..base.clone()
    };
    assert_eq!(
        from_envelope(&empty),
        Err(ConvertError::Unsupported("empty payload"))
    );

    let hello = wire::Envelope {
        payload: Some(wire::envelope::Payload::Hello(wire::Hello::default())),
        ..base.clone()
    };
    assert_eq!(
        from_envelope(&hello),
        Err(ConvertError::Unsupported(
            "Hello is handled by the handshake"
        ))
    );

    let foreign = wire::Envelope {
        payload: Some(wire::envelope::Payload::Topology(wire::Topology::default())),
        ..base
    };
    assert_eq!(
        from_envelope(&foreign),
        Err(ConvertError::Unsupported(
            "payload is not a focus/lease message"
        ))
    );
}

#[test]
fn malformed_identifier_lengths_are_refused() {
    // A nonce that is not 16 bytes.
    let mut challenge = envelope_of(
        &fr::Message::LeaseChallenge {
            lease_id: LEASE,
            nonce: [1u8; 16],
            ttl_ms: 500,
        },
        1,
    );
    if let Some(wire::envelope::Payload::LeaseChallenge(payload)) = challenge.payload.as_mut() {
        payload.nonce.truncate(3);
    }
    assert_eq!(
        from_envelope(&challenge),
        Err(ConvertError::Unsupported("nonce is not 16 bytes"))
    );

    // An owner that is not 32 bytes.
    let mut owner_state = envelope_of(
        &fr::Message::OwnerState {
            serial: 1,
            owner: Some(some_owner()),
            granted: true,
        },
        1,
    );
    if let Some(wire::envelope::Payload::OwnerState(payload)) = owner_state.payload.as_mut() {
        payload.owner_node_id.truncate(5);
    }
    assert_eq!(
        from_envelope(&owner_state),
        Err(ConvertError::Unsupported("owner_node_id is not 32 bytes"))
    );

    // A missing `EnterPrepare.position`.
    let mut prepare = envelope_of(
        &fr::Message::EnterPrepare {
            lease_id: LEASE,
            topology_revision: 1,
            position: Position::new("dp1", 1.0, 1.0),
            owner_serial: 1,
        },
        1,
    );
    if let Some(wire::envelope::Payload::EnterPrepare(payload)) = prepare.payload.as_mut() {
        payload.position = None;
    }
    assert_eq!(
        from_envelope(&prepare),
        Err(ConvertError::Unsupported("missing Position"))
    );
}

#[test]
fn malformed_input_events_are_refused_by_name() {
    let base = envelope_of(
        &fr::Message::InputBatch {
            lease_id: LEASE,
            input_batch_seq: 1,
            events: vec![InputEvent::PointerAbsolute(input_model::Position::new(
                1.0, 1.0,
            ))],
        },
        1,
    );

    let with_event = |event: wire::input_event::Event| wire::Envelope {
        payload: Some(wire::envelope::Payload::InputBatch(wire::InputBatch {
            events: vec![wire::InputEvent {
                capture_monotonic_us: 0,
                event: Some(event),
            }],
        })),
        ..base.clone()
    };

    assert_eq!(
        from_envelope(&wire::Envelope {
            payload: Some(wire::envelope::Payload::InputBatch(wire::InputBatch {
                events: vec![wire::InputEvent {
                    capture_monotonic_us: 0,
                    event: None,
                }],
            })),
            ..base.clone()
        }),
        Err(ConvertError::Unsupported("empty InputEvent"))
    );

    assert_eq!(
        from_envelope(&with_event(wire::input_event::Event::Key(wire::Key {
            usage_page: 0x07,
            usage: 0x04,
            state: 3,
        }))),
        Err(ConvertError::UnknownEnum {
            field: "Key.state",
            value: 3
        })
    );

    assert_eq!(
        from_envelope(&with_event(wire::input_event::Event::Button(
            wire::Button {
                button: 1,
                state: 0,
            }
        ))),
        Err(ConvertError::UnknownEnum {
            field: "Button.state",
            value: 0
        })
    );

    assert_eq!(
        from_envelope(&with_event(wire::input_event::Event::Button(
            wire::Button {
                button: 6,
                state: 1,
            }
        ))),
        Err(ConvertError::UnknownEnum {
            field: "Button.button",
            value: 6
        })
    );

    // A usage page that does not fit the normalized model is refused rather than folded.
    assert_eq!(
        from_envelope(&with_event(wire::input_event::Event::Key(wire::Key {
            usage_page: 0x1_0000,
            usage: 0x04,
            state: 1,
        }))),
        Err(ConvertError::Unsupported("usage_page does not fit u16"))
    );
}

#[test]
fn an_enter_ack_rejection_must_carry_a_code() {
    // `accepted = false` with no reason writes the wire's "unspecified" 0, which the
    // reader cannot turn back into a code: a rejection has to name its reason.
    let message = fr::Message::EnterAck {
        lease_id: LEASE,
        accepted: false,
        reason: None,
    };
    let envelope = envelope_of(&message, 1);
    let Some(wire::envelope::Payload::EnterAck(ack)) = envelope.payload.as_ref() else {
        panic!("expected an EnterAck payload");
    };
    assert!(!ack.accepted);
    assert_eq!(ack.reason, 0);
    assert_eq!(
        from_envelope(&envelope),
        Err(ConvertError::UnknownEnum {
            field: "ErrorCode",
            value: 0
        })
    );

    // With a reason it round trips, and an accepted ack never carries one.
    let rejected = fr::Message::EnterAck {
        lease_id: LEASE,
        accepted: false,
        reason: Some(fr::ErrorCode::LeaseExpired),
    };
    assert_eq!(round_trip(&rejected, 1), rejected);

    let accepted = fr::Message::EnterAck {
        lease_id: LEASE,
        accepted: true,
        reason: None,
    };
    assert_eq!(round_trip(&accepted, 1), accepted);
}

#[test]
fn peer_info_reads_the_hello_fields_it_needs() {
    let hello = wire::Hello {
        node_id: some_owner().as_bytes().to_vec(),
        display_name: "laptop-a".to_owned(),
        can_own: true,
        owner_serial: 4,
        owner_node_id: NodeId::new([0x22; 32]).as_bytes().to_vec(),
        ..Default::default()
    };
    let info = PeerInfo::from_hello(&hello).expect("valid hello");
    assert_eq!(info.node_id, some_owner());
    assert_eq!(info.display_name, "laptop-a");
    assert!(info.can_own);
    assert_eq!(info.owner_serial, 4);
    assert_eq!(info.owner, Some(NodeId::new([0x22; 32])));

    // An empty owner field means "no owner", not a zero identity.
    let no_owner = wire::Hello {
        owner_node_id: Vec::new(),
        ..hello.clone()
    };
    assert_eq!(PeerInfo::from_hello(&no_owner).expect("valid").owner, None);

    // Wrong lengths are refused.
    let short_id = wire::Hello {
        node_id: vec![0u8; 8],
        ..hello.clone()
    };
    assert_eq!(
        PeerInfo::from_hello(&short_id),
        Err(ConvertError::Unsupported("Hello.node_id is not 32 bytes"))
    );

    let short_owner = wire::Hello {
        owner_node_id: vec![0u8; 8],
        ..hello
    };
    assert_eq!(
        PeerInfo::from_hello(&short_owner),
        Err(ConvertError::Unsupported("owner_node_id is not 32 bytes"))
    );
}

#[test]
fn the_entry_display_of_a_lease_becomes_the_batch_display() {
    // `docs/03-architecture.md`, «Топология и движение»: the source integrates the deltas
    // and sends an absolute position *on the target display*, and `protocol.proto`
    // `Position` is «local to display top-left». The lease names that display once, in
    // `EnterPrepare.position`, so the link remembers it for every later batch.
    let prepare = fr::Message::EnterPrepare {
        lease_id: LEASE,
        topology_revision: 3,
        position: Position::new("eDP-1", 10.0, 20.0),
        owner_serial: 1,
    };
    let display = batch_display_after(&prepare, "");
    assert_eq!(display, "eDP-1", "the entry display is the batch display");

    // Nothing else changes it: a batch keeps whatever display the link already has, so the
    // id stays pinned to the lease's entry point until the next `EnterPrepare`.
    let batch = fr::Message::InputBatch {
        lease_id: LEASE,
        input_batch_seq: 1,
        events: vec![InputEvent::PointerAbsolute(input_model::Position::new(
            640.0, 360.0,
        ))],
    };
    assert_eq!(batch_display_after(&batch, &display), "eDP-1");
    assert_eq!(batch_display_after(&batch, ""), "");

    // And a batch built with that display carries it, with the target-local coordinates
    // the source resolved.
    let envelope = to_envelope(&batch, 2, &CONNECTION, Some(&LEASE), &display).expect("convert");
    let Some(wire::envelope::Payload::InputBatch(wire_batch)) = envelope.payload else {
        panic!("expected an InputBatch payload");
    };
    let Some(wire::input_event::Event::PointerAbsolute(position)) =
        wire_batch.events[0].event.as_ref()
    else {
        panic!("expected a PointerAbsolute event");
    };
    assert_eq!(position.display_id, "eDP-1");
    assert_eq!((position.x, position.y), (640.0, 360.0));
}

/// The cheap encodability filter `link::write_message` uses must agree exactly with what
/// [`to_envelope`] accepts, or a future event kind would slip past it and end the link again
/// (audit F4).
#[test]
fn the_encodable_filter_agrees_with_the_encoder() {
    let events = [
        InputEvent::PointerAbsolute(input_model::Position::new(1.0, 2.0)),
        InputEvent::PointerRelative { dx: 1, dy: 2 },
        InputEvent::Key {
            key: HidKey::keyboard(0x04),
            state: KeyState::Down,
        },
        InputEvent::Button {
            button: MouseButton::Left,
            state: KeyState::Up,
        },
        InputEvent::Scroll {
            h120: 120,
            v120: -120,
        },
    ];
    for event in events {
        let batch = fr::Message::InputBatch {
            lease_id: LEASE,
            input_batch_seq: 1,
            events: vec![event],
        };
        let encodable = to_envelope(&batch, 1, &CONNECTION, Some(&LEASE), "").is_ok();
        let kept = transport::convert::split_encodable(&[event]).0.len() == 1;
        assert_eq!(
            kept, encodable,
            "the filter disagrees with the encoder for {event:?}"
        );
    }
}

#[test]
fn peer_versions_are_sanitized_without_refusing_handshake_metadata() {
    for (version, expected) in [
        ("1.2.3", "1.2.3"),
        ("1.2.3-rc.1+smoke", "1.2.3-rc.1+smoke"),
        ("", ""),
        ("v1.2.3", ""),
        ("1.2", ""),
        ("1.2.3\n", ""),
    ] {
        let hello = wire::Hello {
            node_id: some_owner().as_bytes().to_vec(),
            app_version: version.into(),
            capabilities: vec![wire::Capability::RemoteUpdate as i32],
            ..Default::default()
        };
        assert!(protocol::validate::hello(&hello).is_ok());
        let info = PeerInfo::from_hello(&hello).unwrap();
        assert_eq!(info.app_version, expected);
        assert!(info.remote_update);
    }
    let old = wire::Hello {
        node_id: some_owner().as_bytes().to_vec(),
        ..Default::default()
    };
    let info = PeerInfo::from_hello(&old).unwrap();
    assert!(info.app_version.is_empty());
    assert!(!info.remote_update);
}

#[test]
fn update_payload_conversion_validates_version_status_and_lease_scope() {
    use transport::convert::{inbound_from_envelope, Inbound};
    use wire::envelope::Payload;
    let mut envelope = wire::Envelope {
        connection_id: vec![1; 16],
        payload: Some(Payload::UpdateRequest(wire::UpdateRequest {
            target_version: "1.2.3".into(),
        })),
        ..Default::default()
    };
    assert!(matches!(
        inbound_from_envelope(&envelope),
        Ok(Inbound::UpdateRequest(_))
    ));
    for invalid in ["", "1.2", "v1.2.3", "https://example.org/release"] {
        envelope.payload = Some(Payload::UpdateRequest(wire::UpdateRequest {
            target_version: invalid.into(),
        }));
        assert!(inbound_from_envelope(&envelope).is_err());
    }
    for status in [
        "pending",
        "downloading",
        "restarting",
        "done",
        "refused",
        "unreachable",
    ] {
        let result = wire::UpdateResult {
            target_version: "1.2.3".into(),
            status: status.into(),
            detail: String::new(),
        };
        envelope.payload = Some(Payload::UpdateResult(result.clone()));
        assert_eq!(
            inbound_from_envelope(&envelope).unwrap(),
            Inbound::UpdateResult(result)
        );
    }
    envelope.lease_id = vec![0; 16];
    assert!(inbound_from_envelope(&envelope).is_err());
    envelope.lease_id.clear();
    envelope.payload = Some(Payload::UpdateResult(wire::UpdateResult {
        target_version: "1.2.3".into(),
        status: "unexpected".into(),
        detail: String::new(),
    }));
    assert!(inbound_from_envelope(&envelope).is_err());
}

#[test]
fn clipboard_routing_capability_is_optional_in_hello() {
    let mut hello = wire::Hello {
        node_id: some_owner().as_bytes().to_vec(),
        ..Default::default()
    };
    assert!(!PeerInfo::from_hello(&hello).unwrap().clipboard_apply_ack);
    hello
        .capabilities
        .push(wire::Capability::ClipboardApplyAck as i32);
    assert!(PeerInfo::from_hello(&hello).unwrap().clipboard_apply_ack);
}

#[test]
fn image_clipboard_capability_is_independent_and_unknown_bits_are_ignored() {
    let mut hello = wire::Hello {
        node_id: some_owner().as_bytes().to_vec(),
        ..Default::default()
    };
    let legacy = PeerInfo::from_hello(&hello).unwrap();
    assert!(!legacy.image_clipboard);
    hello.capabilities = vec![15, 999_999];
    let image_only = PeerInfo::from_hello(&hello).unwrap();
    assert!(image_only.image_clipboard);
    assert!(!image_only.text_clipboard);
    assert_eq!(
        transport::convert::capabilities_from_wire(&hello.capabilities),
        focus_router::Capabilities::default(),
        "image support and unknown values cannot alter input routing"
    );
    hello.capabilities.clear();
    assert!(!PeerInfo::from_hello(&hello).unwrap().image_clipboard);
}

#[test]
fn clipboard_session_epoch_is_capability_gated_and_exactly_sixteen_bytes() {
    let mut hello = wire::Hello {
        node_id: some_owner().as_bytes().to_vec(),
        clipboard_epoch: vec![7; 16],
        ..Default::default()
    };
    assert_eq!(PeerInfo::from_hello(&hello).unwrap().clipboard_epoch, None);
    hello
        .capabilities
        .push(wire::Capability::ClipboardSession as i32);
    assert_eq!(
        PeerInfo::from_hello(&hello).unwrap().clipboard_epoch,
        Some([7; 16])
    );
    for size in [0, 15, 17] {
        hello.clipboard_epoch = vec![7; size];
        assert!(PeerInfo::from_hello(&hello).is_err());
    }
}

#[test]
fn clipboard_routing_conversion_validates_metadata_and_lease_scope() {
    use transport::convert::{inbound_from_envelope, Inbound};
    use wire::envelope::Payload;

    let mut envelope = wire::Envelope {
        connection_id: vec![1; 16],
        ..Default::default()
    };
    let mut applied = wire::ClipboardApplied {
        origin_version: 7,
        sha256: vec![2; 32],
        ok: true,
        detail: String::new(),
    };
    for ok in [true, false] {
        applied.ok = ok;
        envelope.payload = Some(Payload::ClipboardApplied(applied.clone()));
        assert_eq!(
            inbound_from_envelope(&envelope).unwrap(),
            Inbound::ClipboardApplied(applied.clone())
        );
    }
    applied.sha256.pop();
    envelope.payload = Some(Payload::ClipboardApplied(applied));
    assert!(inbound_from_envelope(&envelope).is_err());

    for (action, hash_len, target_len, valid) in [
        ("carry", 32, 0, true),
        ("clear", 0, 0, true),
        ("clear", 32, 0, true),
        ("deliver", 32, 32, true),
        ("pending", 32, 32, true),
        ("failed", 32, 32, true),
        ("addressed", 32, 0, true),
        ("addressed", 32, 32, true),
        ("unknown", 32, 0, false),
        ("carry", 0, 0, false),
        ("carry", 32, 32, false),
        ("clear", 0, 32, false),
        ("deliver", 32, 0, false),
        ("pending", 32, 0, false),
        ("pending", 31, 32, false),
        ("failed", 32, 0, false),
        ("failed", 31, 32, false),
        ("addressed", 31, 0, false),
        ("addressed", 32, 31, false),
    ] {
        let route = wire::ClipboardRoute {
            action: action.into(),
            origin_node_id: vec![3; 32],
            origin_version: 7,
            sha256: vec![2; hash_len],
            target_node_id: vec![4; target_len],
        };
        envelope.payload = Some(Payload::ClipboardRoute(route.clone()));
        if valid {
            assert_eq!(
                inbound_from_envelope(&envelope).unwrap(),
                Inbound::ClipboardRoute(route)
            );
        } else {
            assert!(inbound_from_envelope(&envelope).is_err());
        }
    }
    let mut route = wire::ClipboardRoute {
        action: "carry".into(),
        origin_node_id: vec![3; 31],
        sha256: vec![2; 32],
        ..Default::default()
    };
    envelope.payload = Some(Payload::ClipboardRoute(route.clone()));
    assert!(inbound_from_envelope(&envelope).is_err());
    route.origin_node_id.push(3);
    envelope.payload = Some(Payload::ClipboardRoute(route));
    envelope.lease_id = vec![0; 16];
    assert!(inbound_from_envelope(&envelope).is_err());
    envelope.payload = Some(Payload::ClipboardApplied(wire::ClipboardApplied {
        sha256: vec![2; 32],
        ..Default::default()
    }));
    assert!(inbound_from_envelope(&envelope).is_err());
}
