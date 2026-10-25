//! The mapping between [`focus_router`]'s message vocabulary and `protocol::wire`.
//!
//! `focus-router` owns a **1:1 mirror** of the `docs/protocol.proto` messages it needs
//! (built that way on purpose, see its `wire` module), and a swap to `protocol::wire` is a
//! mechanical change of that module. It is not done here: the committed focus-router test
//! suites (`tests/scenarios.rs`, `tests/wire_numbers.rs`) pin its own vocabulary and its
//! `as_u32()` numbers, so replacing it would rewrite another author's tests. This module is
//! therefore the single conversion point, and it is the only place that knows both shapes —
//! when the mirror is eventually swapped, this file shrinks to nothing.
//!
//! Rules that are *not* a plain field copy:
//!
//! * `lease_id` lives on the `Envelope` on the wire («exactly 16 on lease messages»), while
//!   focus-router carries it inside the message. `to_envelope` therefore takes an explicit
//!   `lease_id`, and `from_envelope` lifts it back out.
//! * `InputBatch.input_batch_seq` **is** the envelope `seq` («Sender's envelope sequence of
//!   this batch»), so the two are kept equal in both directions.
//! * [`focus_router::InputEvent::PointerRelative`] has no wire form: a source integrates
//!   deltas into an absolute position before sending
//!   (`docs/03-architecture.md`: relative-only injection «в протоколе нет и не
//!   добавляется»), so it is a conversion error rather than a silent drop.
//! * `capture_monotonic_us` is diagnostic and set to 0: the normalized event has no
//!   timestamp, and nothing may derive timing from it.

use focus_router as fr;
use geometry::{NodeId, Position};
use input_model::{HidKey, InputEvent, KeyState, MouseButton};
use protocol::wire;

/// A message that cannot be expressed on the wire.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConvertError {
    /// A relative pointer event has no wire representation.
    #[error("relative pointer events are not part of the wire contract (integrate them into an absolute position)")]
    RelativePointerNotOnWire,
    /// A `oneof` arm or enum value the contract does not define.
    #[error("unsupported wire value: {0}")]
    Unsupported(&'static str),
    /// An enum field carried an unknown number.
    #[error("{field} has unknown value {value}")]
    UnknownEnum {
        /// Field name.
        field: &'static str,
        /// The offending number.
        value: i32,
    },
}

fn unknown(field: &'static str, value: i32) -> ConvertError {
    ConvertError::UnknownEnum { field, value }
}

/// `focus_router` message -> wire envelope.
///
/// `seq` is the caller's per-direction sequence; `connection_id` is the 16 bytes the dialer
/// chose; `lease_id` is empty for the messages that are not lease-scoped.
///
/// `batch_display` is the display a `InputBatch`'s absolute positions belong to. **Known
/// model gap**: the wire's `Position` carries `display_id`, while the normalized
/// `input_model::Position` inside [`input_model::InputEvent::PointerAbsolute`] does not, so
/// this layer has nothing to fill it with. The parameter is the seam for the day
/// `input-model` grows the field; until then the transport passes an empty string, which a
/// receiver must treat as "resolve from the zone/geometry you know". `EnterPrepare.position`
/// is unaffected: it is a `geometry::Position` and keeps its own `display_id`.
/// The display an outbound `InputBatch`'s absolute positions belong to.
///
/// A lease is entered on one display of the peer — `EnterPrepare.position` is a
/// `geometry::Position` and carries it — and every absolute position the source sends until
/// the next transition is local to that display. Keeping the id here is what lets
/// [`to_envelope`] stamp it instead of passing an empty string.
pub fn batch_display_after(message: &fr::Message, current: &str) -> String {
    match message {
        fr::Message::EnterPrepare { position, .. } => position.display_id.to_string(),
        _ => current.to_owned(),
    }
}

pub fn to_envelope(
    message: &fr::Message,
    seq: u64,
    connection_id: &[u8; 16],
    lease_id: Option<&[u8; 16]>,
    batch_display: &str,
) -> Result<wire::Envelope, ConvertError> {
    use wire::envelope::Payload;

    let mut envelope = wire::Envelope {
        major: protocol::limits::PROTOCOL_MAJOR,
        minor: protocol::limits::PROTOCOL_MINOR,
        connection_id: connection_id.to_vec(),
        seq,
        lease_id: lease_id.map(|id| id.to_vec()).unwrap_or_default(),
        payload: None,
    };
    envelope.payload = Some(match message {
        fr::Message::EnterPrepare {
            topology_revision,
            position,
            owner_serial,
            ..
        } => Payload::EnterPrepare(wire::EnterPrepare {
            topology_revision: *topology_revision,
            position: Some(to_wire_position(position)),
            owner_serial: *owner_serial,
        }),
        fr::Message::EnterAck {
            accepted, reason, ..
        } => Payload::EnterAck(wire::EnterAck {
            accepted: *accepted,
            reason: reason.map(|code| code.as_u32() as i32).unwrap_or(0),
        }),
        fr::Message::EnterCommit { .. } => Payload::EnterCommit(wire::EnterCommit {}),
        fr::Message::Leave { reason, .. } => Payload::Leave(wire::Leave {
            reason: reason.as_u32() as i32,
        }),
        fr::Message::LeaveAck { .. } => Payload::LeaveAck(wire::LeaveAck {}),
        fr::Message::InputBatch { events, .. } => {
            // The transport owns the envelope sequence for every frame of a link, and the
            // wire defines the batch sequence *as* the envelope sequence («Sender's envelope
            // sequence of this batch»), so the message's own field is stamped over rather
            // than trusted: a caller that numbered batches independently would otherwise
            // collide with the control frames' numbering and trip the peer's `SeqValidator`.
            Payload::InputBatch(wire::InputBatch {
                events: events
                    .iter()
                    .map(|event| to_wire_event(event, batch_display))
                    .collect::<Result<_, _>>()?,
            })
        }
        fr::Message::InjectAck {
            input_batch_seq,
            applied_count,
            actual_position,
            status,
            ..
        } => Payload::InjectAck(wire::InjectAck {
            input_batch_seq: *input_batch_seq,
            applied_count: *applied_count,
            actual_position: actual_position.as_ref().map(to_wire_position),
            status: status.as_u32() as i32,
        }),
        fr::Message::LeaseChallenge { nonce, ttl_ms, .. } => {
            Payload::LeaseChallenge(wire::LeaseChallenge {
                nonce: nonce.to_vec(),
                ttl_ms: *ttl_ms,
            })
        }
        fr::Message::LeaseRenew { nonce, .. } => Payload::LeaseRenew(wire::LeaseRenew {
            nonce: nonce.to_vec(),
        }),
        fr::Message::LeaseStatus {
            nonce,
            accepted,
            last_input_batch_seq,
            ..
        } => Payload::LeaseStatus(wire::LeaseStatus {
            nonce: nonce.to_vec(),
            accepted: *accepted,
            last_input_batch_seq: *last_input_batch_seq,
        }),
        fr::Message::OwnerClaim { serial, reason, .. } => Payload::OwnerClaim(wire::OwnerClaim {
            serial: *serial,
            reason: reason.as_u32() as i32,
        }),
        fr::Message::OwnerState {
            serial,
            owner,
            granted,
            ..
        } => Payload::OwnerState(wire::OwnerState {
            serial: *serial,
            owner_node_id: owner.map(|id| id.as_bytes().to_vec()).unwrap_or_default(),
            granted: *granted,
        }),
        fr::Message::ProtocolError {
            code, related_seq, ..
        } => Payload::Error(wire::ProtocolError {
            related_seq: *related_seq,
            code: code.as_u32() as i32,
            // The correlation id and the layout revision belong to the layout arm's
            // `LayoutEdit`; the FSM's errors have no request to name.
            request_nonce: 0,
            layout_revision: 0,
        }),
        fr::Message::Shutdown { reason, .. } => Payload::Shutdown(wire::Shutdown {
            reason: reason.as_u32() as i32,
        }),
    });
    Ok(envelope)
}

/// Wire envelope -> `focus_router` message.
pub fn from_envelope(envelope: &wire::Envelope) -> Result<fr::Message, ConvertError> {
    use wire::envelope::Payload;

    let lease_id = lease_id_of(envelope)?;
    let payload = envelope
        .payload
        .as_ref()
        .ok_or(ConvertError::Unsupported("empty payload"))?;
    Ok(match payload {
        Payload::EnterPrepare(prepare) => fr::Message::EnterPrepare {
            lease_id: [0u8; 16],
            topology_revision: prepare.topology_revision,
            position: from_wire_position(prepare.position.as_ref())?,
            owner_serial: prepare.owner_serial,
        },
        Payload::EnterAck(ack) => fr::Message::EnterAck {
            lease_id: [0u8; 16],
            accepted: ack.accepted,
            reason: if ack.accepted {
                None
            } else {
                Some(error_code_from_i32(ack.reason)?)
            },
        },
        Payload::EnterCommit(_) => fr::Message::EnterCommit {
            lease_id: [0u8; 16],
        },
        Payload::Leave(leave) => fr::Message::Leave {
            lease_id: [0u8; 16],
            reason: leave_reason_from_i32(leave.reason)?,
        },
        Payload::LeaveAck(_) => fr::Message::LeaveAck {
            lease_id: [0u8; 16],
        },
        Payload::InputBatch(batch) => fr::Message::InputBatch {
            lease_id: [0u8; 16],
            input_batch_seq: envelope.seq,
            events: batch
                .events
                .iter()
                .map(from_wire_event)
                .collect::<Result<_, _>>()?,
        },
        Payload::InjectAck(ack) => fr::Message::InjectAck {
            lease_id: [0u8; 16],
            input_batch_seq: ack.input_batch_seq,
            applied_count: ack.applied_count,
            actual_position: ack
                .actual_position
                .as_ref()
                .map(|position| from_wire_position(Some(position)))
                .transpose()?,
            status: inject_status_from_i32(ack.status)?,
        },
        Payload::LeaseChallenge(challenge) => fr::Message::LeaseChallenge {
            lease_id: [0u8; 16],
            nonce: nonce_of(&challenge.nonce)?,
            ttl_ms: challenge.ttl_ms,
        },
        Payload::LeaseRenew(renew) => fr::Message::LeaseRenew {
            lease_id: [0u8; 16],
            nonce: nonce_of(&renew.nonce)?,
        },
        Payload::LeaseStatus(status) => fr::Message::LeaseStatus {
            lease_id: [0u8; 16],
            nonce: nonce_of(&status.nonce)?,
            accepted: status.accepted,
            last_input_batch_seq: status.last_input_batch_seq,
        },
        Payload::OwnerClaim(claim) => fr::Message::OwnerClaim {
            serial: claim.serial,
            reason: claim_reason_from_i32(claim.reason)?,
        },
        Payload::OwnerState(state) => {
            fr::Message::OwnerState {
                serial: state.serial,
                owner: if state.owner_node_id.is_empty() {
                    None
                } else {
                    Some(NodeId::new(
                        state.owner_node_id.as_slice().try_into().map_err(|_| {
                            ConvertError::Unsupported("owner_node_id is not 32 bytes")
                        })?,
                    ))
                },
                granted: state.granted,
            }
        }
        Payload::Error(error) => fr::Message::ProtocolError {
            code: error_code_from_i32(error.code)?,
            related_seq: error.related_seq,
        },
        Payload::Shutdown(shutdown) => fr::Message::Shutdown {
            reason: leave_reason_from_i32(shutdown.reason)?,
        },
        // Hello, topology, clipboard and the rest belong to other layers.
        Payload::Hello(_) => {
            return Err(ConvertError::Unsupported(
                "Hello is handled by the handshake",
            ))
        }
        _ => {
            return Err(ConvertError::Unsupported(
                "payload is not a focus/lease message",
            ))
        }
    })
    .map(|message| attach_lease(message, lease_id))
}

/// A message from a peer, split by layer.
///
/// The focus/lease vocabulary is a mirror of `focus_router::Message`, which the FSM owns. The
/// layout document is not a focus message at all — `Topology`, `LayoutEdit`, `TopologyRequest`,
/// `TopologyApplied` and `ActiveLayout` never drive a transition — so it travels beside the
/// mirror instead of being forced into it. Core is the only consumer of both.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    /// Replacement advertised capability set; unknown optional enum values remain raw.
    CapabilitiesChanged(Vec<i32>),
    /// A remote update request, resolved against the receiver's trusted release source.
    UpdateRequest(wire::UpdateRequest),
    /// Remote update progress or refusal.
    UpdateResult(wire::UpdateResult),
    /// Backend clipboard application result, scoped by origin version and hash.
    ClipboardApplied(wire::ClipboardApplied),
    /// Metadata-only clipboard routing control.
    ClipboardRoute(wire::ClipboardRoute),
    /// A focus/lease message, already in the FSM's vocabulary.
    Focus(fr::Message),
    /// A layout-document message, in the wire types core works with.
    Layout(LayoutMessage),
    /// A clipboard message that arrived on the input link.
    ///
    /// `docs/05-protocol.md` («Clipboard канал»): the peer that holds content hands every member
    /// a `ClipboardGrant` *before* the channel it authorises exists, so the grant is the one
    /// clipboard message that travels on the input link. Every later clipboard message belongs
    /// to the channel itself. `connection_id` is the envelope's, which is the parent the bind
    /// echoes back.
    Clipboard {
        /// The message.
        message: crate::clipboard::ClipboardMessage,
        /// The parent connection the channel will be bound to.
        connection_id: [u8; 16],
    },
}

/// Whether a payload belongs to the clipboard channel's vocabulary.
fn is_clipboard(payload: &wire::envelope::Payload) -> bool {
    use wire::envelope::Payload;
    matches!(
        payload,
        Payload::ClipboardGrant(_)
            | Payload::ClipboardBind(_)
            | Payload::ClipboardOffer(_)
            | Payload::ClipboardRequest(_)
            | Payload::ClipboardData(_)
            | Payload::ClipboardComplete(_)
            | Payload::ClipboardCancel(_)
    )
}

/// The layout half of the wire: whole-document replication and its CAS.
///
/// The payloads are `protocol::wire` types rather than a second mirror: the document is what
/// is replicated and revalidated as a whole (ADR-013), and a lossy translation layer would be
/// one more thing to keep in step with the schema.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutMessage {
    /// `Topology{revision, author_node_id, nodes, links}` — the document.
    Topology(wire::Topology),
    /// A member's refreshed display manifest (`DisplaysChanged`).
    DisplaysChanged {
        /// The member-local display revision.
        revision: u64,
        /// The current display set.
        displays: Vec<wire::Display>,
    },
    /// `LayoutEdit{expected_revision, expected_author, author_node_id, layout_toml}`.
    LayoutEdit(wire::LayoutEdit),
    /// `TopologyRequest{}` — a member asking the coordinator, or the coordinator asking the
    /// author of a higher stored revision.
    TopologyRequest,
    /// `TopologyApplied{revision, request_nonce}` — a receiver confirming it applied the
    /// document, or the coordinator's verdict on an edit it applied.
    TopologyApplied {
        /// The revision that was applied.
        revision: u64,
        /// The `LayoutEdit.request_nonce` this confirms, or 0 for a confirmation nobody
        /// asked for by id (a document pushed by the coordinator).
        request_nonce: u64,
    },
    /// `ActiveLayout` — the receiver's keyboard layout, for the owner's GUI.
    ActiveLayout(wire::ActiveLayout),
    /// `ProtocolError` with a layout code: the refusal of an edit or a document
    /// (`LAYOUT_CONFLICT`, `INVALID_TOPOLOGY`, `NO_COORDINATOR`).
    ///
    /// It travels on this arm and not in the focus mirror: those codes never drive a
    /// transition, and `focus_router::ErrorCode` deliberately models only the codes the FSM
    /// emits.
    Error {
        /// The wire code, as the contract defines it.
        code: wire::ErrorCode,
        /// The `LayoutEdit.request_nonce` this refuses, or 0 when the refusal answers a
        /// document instead of an edit.
        request_nonce: u64,
        /// The applied layout revision this refuses an edit against (`LAYOUT_CONFLICT` names
        /// what the editor must reload to), or 0 when there is none to name.
        layout_revision: u64,
    },
}

/// Whether a `ProtocolError` code belongs to the layout half.
#[must_use]
pub fn is_layout_code(code: wire::ErrorCode) -> bool {
    matches!(
        code,
        wire::ErrorCode::LayoutConflict
            | wire::ErrorCode::InvalidTopology
            | wire::ErrorCode::NoCoordinator
    )
}

/// Reads one envelope into the layer that owns its payload.
///
/// `Hello` stays with the handshake and clipboard stays with its channel, as before; a payload
/// that belongs to neither layer is an error, exactly as in [`from_envelope`].
pub fn inbound_from_envelope(envelope: &wire::Envelope) -> Result<Inbound, ConvertError> {
    use wire::envelope::Payload;

    match envelope.payload.as_ref() {
        Some(Payload::ClipboardApplied(applied)) => {
            validate_clipboard_applied(applied)?;
            if !envelope.lease_id.is_empty() {
                return Err(ConvertError::Unsupported(
                    "clipboard acknowledgement carries a lease",
                ));
            }
            Ok(Inbound::ClipboardApplied(applied.clone()))
        }
        Some(Payload::ClipboardRoute(route)) => {
            validate_clipboard_route(route)?;
            if !envelope.lease_id.is_empty() {
                return Err(ConvertError::Unsupported("clipboard route carries a lease"));
            }
            Ok(Inbound::ClipboardRoute(route.clone()))
        }
        Some(Payload::UpdateRequest(request)) => {
            validate_update_version(&request.target_version)?;
            if !envelope.lease_id.is_empty() {
                return Err(ConvertError::Unsupported("update message carries a lease"));
            }
            Ok(Inbound::UpdateRequest(request.clone()))
        }
        Some(Payload::UpdateResult(result)) => {
            validate_update_result(result)?;
            if !envelope.lease_id.is_empty() {
                return Err(ConvertError::Unsupported("update message carries a lease"));
            }
            Ok(Inbound::UpdateResult(result.clone()))
        }
        Some(Payload::CapabilitiesChanged(changed)) => {
            Ok(Inbound::CapabilitiesChanged(changed.capabilities.clone()))
        }
        Some(Payload::Topology(topology)) => {
            Ok(Inbound::Layout(LayoutMessage::Topology(topology.clone())))
        }
        Some(Payload::LayoutEdit(edit)) => {
            Ok(Inbound::Layout(LayoutMessage::LayoutEdit(edit.clone())))
        }
        Some(Payload::DisplaysChanged(changed)) => {
            Ok(Inbound::Layout(LayoutMessage::DisplaysChanged {
                revision: changed.displays_revision,
                displays: changed.displays.clone(),
            }))
        }
        Some(Payload::TopologyRequest(_)) => Ok(Inbound::Layout(LayoutMessage::TopologyRequest)),
        Some(Payload::TopologyApplied(applied)) => {
            Ok(Inbound::Layout(LayoutMessage::TopologyApplied {
                revision: applied.revision,
                request_nonce: applied.request_nonce,
            }))
        }
        Some(Payload::ActiveLayout(layout)) => {
            Ok(Inbound::Layout(LayoutMessage::ActiveLayout(layout.clone())))
        }
        Some(Payload::Error(error)) => match wire::ErrorCode::try_from(error.code) {
            // A layout refusal travels on the layout arm; every other `ProtocolError` keeps
            // its place in the focus vocabulary (`focus_router::ErrorCode` models the codes
            // the FSM emits, and these are not among them).
            Ok(code) if is_layout_code(code) => Ok(Inbound::Layout(LayoutMessage::Error {
                code,
                request_nonce: error.request_nonce,
                layout_revision: error.layout_revision,
            })),
            _ => from_envelope(envelope).map(Inbound::Focus),
        },
        Some(payload) if is_clipboard(payload) => {
            let message = crate::clipboard::ClipboardMessage::from_envelope(envelope)?;
            let connection_id: [u8; 16] = envelope
                .connection_id
                .as_slice()
                .try_into()
                .map_err(|_| ConvertError::Unsupported("connection_id is not 16 bytes"))?;
            Ok(Inbound::Clipboard {
                message,
                connection_id,
            })
        }
        None => Err(ConvertError::Unsupported("empty payload")),
        Some(_) => from_envelope(envelope).map(Inbound::Focus),
    }
}

/// Checks an update target without accepting URLs, partial versions or unknown metadata.
pub fn validate_update_version(version: &str) -> Result<(), ConvertError> {
    if !protocol::is_valid_app_version(version) {
        return Err(ConvertError::Unsupported("invalid update target version"));
    }
    Ok(())
}

/// Checks the remote update progress vocabulary at the transport boundary.
pub fn validate_update_result(result: &wire::UpdateResult) -> Result<(), ConvertError> {
    validate_update_version(&result.target_version)?;
    if !matches!(
        result.status.as_str(),
        "pending" | "downloading" | "restarting" | "done" | "refused" | "unreachable"
    ) {
        return Err(ConvertError::Unsupported("unknown update status"));
    }
    Ok(())
}

/// Checks the identity of a completed backend clipboard application.
pub fn validate_clipboard_applied(applied: &wire::ClipboardApplied) -> Result<(), ConvertError> {
    if applied.sha256.len() != 32 {
        return Err(ConvertError::Unsupported(
            "invalid clipboard acknowledgement hash",
        ));
    }
    Ok(())
}

/// Checks metadata-only routing before it reaches core's capability and trust policy.
pub fn validate_clipboard_route(route: &wire::ClipboardRoute) -> Result<(), ConvertError> {
    if route.origin_node_id.len() != 32 {
        return Err(ConvertError::Unsupported("invalid clipboard route origin"));
    }
    let valid = match route.action.as_str() {
        "carry" => route.sha256.len() == 32 && route.target_node_id.is_empty(),
        "clear" => {
            (route.sha256.is_empty() || route.sha256.len() == 32) && route.target_node_id.is_empty()
        }
        "deliver" | "pending" | "failed" => {
            route.sha256.len() == 32 && route.target_node_id.len() == 32
        }
        "addressed" => {
            route.sha256.len() == 32
                && (route.target_node_id.is_empty() || route.target_node_id.len() == 32)
        }
        _ => false,
    };
    if !valid {
        return Err(ConvertError::Unsupported(
            "invalid clipboard route action, hash or target",
        ));
    }
    Ok(())
}

/// Builds the envelope of a layout message.
///
/// No lease id: these messages belong to the connection, not to a lease (`docs/05-protocol.md`
/// gives every one of them a `connection_id`-scoped meaning).
pub fn layout_envelope(
    message: &LayoutMessage,
    seq: u64,
    connection_id: &[u8; 16],
) -> wire::Envelope {
    use wire::envelope::Payload;

    let payload = match message {
        LayoutMessage::DisplaysChanged { revision, displays } => {
            Payload::DisplaysChanged(wire::DisplaysChanged {
                displays_revision: *revision,
                displays: displays.clone(),
            })
        }
        LayoutMessage::Topology(topology) => Payload::Topology(topology.clone()),
        LayoutMessage::LayoutEdit(edit) => Payload::LayoutEdit(edit.clone()),
        LayoutMessage::TopologyRequest => Payload::TopologyRequest(wire::TopologyRequest {}),
        LayoutMessage::TopologyApplied {
            revision,
            request_nonce,
        } => Payload::TopologyApplied(wire::TopologyApplied {
            revision: *revision,
            request_nonce: *request_nonce,
        }),
        LayoutMessage::ActiveLayout(layout) => Payload::ActiveLayout(layout.clone()),
        LayoutMessage::Error {
            code,
            request_nonce,
            layout_revision,
        } => Payload::Error(wire::ProtocolError {
            related_seq: 0,
            code: *code as i32,
            request_nonce: *request_nonce,
            layout_revision: *layout_revision,
        }),
    };
    wire::Envelope {
        major: protocol::limits::PROTOCOL_MAJOR,
        minor: protocol::limits::PROTOCOL_MINOR,
        connection_id: connection_id.to_vec(),
        seq,
        lease_id: Vec::new(),
        payload: Some(payload),
    }
}

/// The lease id of an envelope, when it carries one.
pub fn lease_id_of(envelope: &wire::Envelope) -> Result<Option<[u8; 16]>, ConvertError> {
    if envelope.lease_id.is_empty() {
        return Ok(None);
    }
    envelope
        .lease_id
        .as_slice()
        .try_into()
        .map(Some)
        .map_err(|_| ConvertError::Unsupported("lease_id is not 16 bytes"))
}

fn attach_lease(message: fr::Message, lease_id: Option<[u8; 16]>) -> fr::Message {
    match message {
        fr::Message::EnterPrepare {
            topology_revision,
            position,
            owner_serial,
            ..
        } => fr::Message::EnterPrepare {
            lease_id: lease_id.unwrap_or_default(),
            topology_revision,
            position,
            owner_serial,
        },
        fr::Message::EnterAck {
            accepted, reason, ..
        } => fr::Message::EnterAck {
            lease_id: lease_id.unwrap_or_default(),
            accepted,
            reason,
        },
        fr::Message::EnterCommit { .. } => fr::Message::EnterCommit {
            lease_id: lease_id.unwrap_or_default(),
        },
        fr::Message::Leave { reason, .. } => fr::Message::Leave {
            lease_id: lease_id.unwrap_or_default(),
            reason,
        },
        fr::Message::LeaveAck { .. } => fr::Message::LeaveAck {
            lease_id: lease_id.unwrap_or_default(),
        },
        fr::Message::InputBatch {
            input_batch_seq,
            events,
            ..
        } => fr::Message::InputBatch {
            lease_id: lease_id.unwrap_or_default(),
            input_batch_seq,
            events,
        },
        fr::Message::InjectAck {
            input_batch_seq,
            applied_count,
            actual_position,
            status,
            ..
        } => fr::Message::InjectAck {
            lease_id: lease_id.unwrap_or_default(),
            input_batch_seq,
            applied_count,
            actual_position,
            status,
        },
        fr::Message::LeaseChallenge { nonce, ttl_ms, .. } => fr::Message::LeaseChallenge {
            lease_id: lease_id.unwrap_or_default(),
            nonce,
            ttl_ms,
        },
        fr::Message::LeaseRenew { nonce, .. } => fr::Message::LeaseRenew {
            lease_id: lease_id.unwrap_or_default(),
            nonce,
        },
        fr::Message::LeaseStatus {
            nonce,
            accepted,
            last_input_batch_seq,
            ..
        } => fr::Message::LeaseStatus {
            lease_id: lease_id.unwrap_or_default(),
            nonce,
            accepted,
            last_input_batch_seq,
        },
        other => other,
    }
}

/// The lease id a `focus_router` message must travel with.
pub fn lease_id_of_message(message: &fr::Message) -> Option<[u8; 16]> {
    let empty = [0u8; 16];
    match message {
        fr::Message::EnterPrepare { lease_id, .. }
        | fr::Message::EnterAck { lease_id, .. }
        | fr::Message::EnterCommit { lease_id }
        | fr::Message::Leave { lease_id, .. }
        | fr::Message::LeaveAck { lease_id }
        | fr::Message::InputBatch { lease_id, .. }
        | fr::Message::InjectAck { lease_id, .. }
        | fr::Message::LeaseChallenge { lease_id, .. }
        | fr::Message::LeaseRenew { lease_id, .. }
        | fr::Message::LeaseStatus { lease_id, .. } => {
            if *lease_id == empty {
                None
            } else {
                Some(*lease_id)
            }
        }
        _ => None,
    }
}

fn to_wire_position(position: &Position) -> wire::Position {
    wire::Position {
        display_id: position.display_id.as_str().to_owned(),
        x: position.x,
        y: position.y,
    }
}

/// An absolute pointer position from a batch, which carries no display id.
fn to_wire_event_position(position: &input_model::Position, display_id: &str) -> wire::Position {
    wire::Position {
        display_id: display_id.to_owned(),
        x: position.x,
        y: position.y,
    }
}

fn from_wire_position(position: Option<&wire::Position>) -> Result<Position, ConvertError> {
    let position = position.ok_or(ConvertError::Unsupported("missing Position"))?;
    Ok(Position::new(
        position.display_id.clone(),
        position.x,
        position.y,
    ))
}

fn to_wire_event(
    event: &InputEvent,
    batch_display: &str,
) -> Result<wire::InputEvent, ConvertError> {
    use wire::input_event::Event;
    let arm = match event {
        InputEvent::PointerAbsolute(position) => {
            Event::PointerAbsolute(to_wire_event_position(position, batch_display))
        }
        InputEvent::PointerRelative { .. } => return Err(ConvertError::RelativePointerNotOnWire),
        InputEvent::Key { key, state } => Event::Key(wire::Key {
            usage_page: u32::from(key.page),
            usage: u32::from(key.usage),
            state: key_state_to_i32(*state),
        }),
        InputEvent::Button { button, state } => Event::Button(wire::Button {
            button: *button as i32,
            state: key_state_to_i32(*state),
        }),
        InputEvent::Scroll { h120, v120 } => Event::Scroll(wire::Scroll {
            horizontal_units120: *h120,
            vertical_units120: *v120,
        }),
    };
    Ok(wire::InputEvent {
        // Diagnostic field; the normalized event carries no timestamp and nothing may
        // derive timing from this.
        capture_monotonic_us: 0,
        event: Some(arm),
    })
}

fn from_wire_event(event: &wire::InputEvent) -> Result<InputEvent, ConvertError> {
    use wire::input_event::Event;
    match event.event.as_ref() {
        Some(Event::PointerAbsolute(position)) => {
            // The display id is part of the wire contract; the normalized event has no field
            // for it (see `to_envelope`), so it is checked here and then dropped. The check
            // stays strict on purpose (the receiver has nothing to resolve an empty id into),
            // but it is no longer a link teardown: an `InputBatch` that cannot be decoded
            // drops the frame and the link lives on (audit F4).
            if position.display_id.is_empty() {
                return Err(ConvertError::Unsupported(
                    "PointerAbsolute without display_id",
                ));
            }
            Ok(InputEvent::PointerAbsolute(input_model::Position::new(
                position.x, position.y,
            )))
        }
        Some(Event::Key(key)) => {
            let state = match key.state {
                1 => KeyState::Down,
                2 => KeyState::Up,
                other => return Err(unknown("Key.state", other)),
            };
            Ok(InputEvent::Key {
                key: HidKey {
                    page: u16::try_from(key.usage_page)
                        .map_err(|_| ConvertError::Unsupported("usage_page does not fit u16"))?,
                    usage: u16::try_from(key.usage)
                        .map_err(|_| ConvertError::Unsupported("usage does not fit u16"))?,
                },
                state,
            })
        }
        Some(Event::Button(button)) => {
            let state = match button.state {
                1 => KeyState::Down,
                2 => KeyState::Up,
                other => return Err(unknown("Button.state", other)),
            };
            // The wire numbering is the `ButtonKind` enum (LEFT = 1 .. FORWARD = 5), which is
            // also `MouseButton`'s discriminant order.
            let kind = match button.button {
                1 => MouseButton::Left,
                2 => MouseButton::Right,
                3 => MouseButton::Middle,
                4 => MouseButton::Back,
                5 => MouseButton::Forward,
                other => return Err(unknown("Button.button", other)),
            };
            Ok(InputEvent::Button {
                button: kind,
                state,
            })
        }
        Some(Event::Scroll(scroll)) => Ok(InputEvent::Scroll {
            h120: scroll.horizontal_units120,
            v120: scroll.vertical_units120,
        }),
        None => Err(ConvertError::Unsupported("empty InputEvent")),
    }
}

/// The events of a batch that this layer can put on the wire, and how many it cannot.
///
/// [`to_wire_event`] refuses [`InputEvent::PointerRelative`]: `protocol.proto`'s `InputEvent`
/// has no relative member, and relative-only *injection* does not exist. One such event must
/// not be able to fail a whole `InputBatch` — and, through `write_message`, it must not be
/// able to end the link — so the batch keeps the rest of its events (a key or button edge in
/// it still travels) and the caller logs the count (audit F4).
///
/// The `the_encodable_filter_agrees_with_the_encoder` test pins this list to what
/// [`to_wire_event`] accepts, so the cheap check here cannot drift from the encoder.
#[must_use]
pub fn split_encodable(events: &[InputEvent]) -> (Vec<InputEvent>, usize) {
    let mut kept = Vec::with_capacity(events.len());
    let mut dropped = 0usize;
    for event in events {
        if matches!(event, InputEvent::PointerRelative { .. }) {
            dropped += 1;
        } else {
            kept.push(*event);
        }
    }
    (kept, dropped)
}

fn key_state_to_i32(state: KeyState) -> i32 {
    match state {
        KeyState::Down => 1,
        KeyState::Up => 2,
    }
}

fn nonce_of(bytes: &[u8]) -> Result<[u8; 16], ConvertError> {
    bytes
        .try_into()
        .map_err(|_| ConvertError::Unsupported("nonce is not 16 bytes"))
}

/// `ErrorCode` numbers (`docs/protocol.proto`).
///
/// The number check comes from the generated enum
/// (`protocol::wire::ErrorCode::try_from`), and the match below is exhaustive over that
/// enum, so a code added to the `.proto` cannot slip through unnoticed: the build fails
/// until its mapping is decided. `MembershipFull` is a code the FSM carries on purpose
/// (`fr::ErrorCode::MembershipFull` exists so a refused peer can be *told* why), while
/// the codes `focus_router` has no variant for are refused explicitly — mapping them to
/// some other code would mislabel them.
pub fn error_code_from_i32(value: i32) -> Result<fr::ErrorCode, ConvertError> {
    use protocol::wire::ErrorCode as WireCode;
    let code = WireCode::try_from(value).map_err(|_| unknown("ErrorCode", value))?;
    let mapped = match code {
        WireCode::UnsupportedSession => Some(fr::ErrorCode::UnsupportedSession),
        WireCode::CaptureFailed => Some(fr::ErrorCode::CaptureFailed),
        WireCode::TopologyChanged => Some(fr::ErrorCode::TopologyChanged),
        WireCode::InvalidFrame => Some(fr::ErrorCode::InvalidFrame),
        WireCode::InvalidSequence => Some(fr::ErrorCode::InvalidSequence),
        WireCode::LeaseMismatch => Some(fr::ErrorCode::LeaseMismatch),
        WireCode::InjectionFailed => Some(fr::ErrorCode::InjectionFailed),
        WireCode::LeaseExpired => Some(fr::ErrorCode::LeaseExpired),
        WireCode::AgentLost => Some(fr::ErrorCode::AgentLost),
        WireCode::NotOwner => Some(fr::ErrorCode::NotOwner),
        WireCode::StaleOwnerSerial => Some(fr::ErrorCode::StaleOwnerSerial),
        WireCode::MembershipFull => Some(fr::ErrorCode::MembershipFull),
        WireCode::CannotOwn => Some(fr::ErrorCode::CannotOwn),
        WireCode::Unspecified
        | WireCode::UnsupportedVersion
        | WireCode::WrongRole
        | WireCode::UnknownPeer
        | WireCode::ChangedKey
        | WireCode::InvalidTopology
        | WireCode::ClipboardTooLarge
        | WireCode::ClipboardDisabled
        | WireCode::ClipboardSuperseded
        | WireCode::UnsupportedKey
        | WireCode::RateLimited
        | WireCode::PortalDenied
        | WireCode::LayoutConflict
        | WireCode::NoCoordinator => None,
    };
    mapped.ok_or_else(|| unknown("ErrorCode", value))
}

/// `LeaveReason` numbers.
pub fn leave_reason_from_i32(value: i32) -> Result<fr::LeaveReason, ConvertError> {
    Ok(match value {
        1 => fr::LeaveReason::ReturnEdge,
        2 => fr::LeaveReason::Hotkey,
        3 => fr::LeaveReason::Emergency,
        4 => fr::LeaveReason::ReceiverLocalInput,
        5 => fr::LeaveReason::AgentLost,
        6 => fr::LeaveReason::TopologyChanged,
        7 => fr::LeaveReason::SuspendOrLock,
        8 => fr::LeaveReason::Shutdown,
        9 => fr::LeaveReason::OwnershipLost,
        other => return Err(unknown("LeaveReason", other)),
    })
}

/// `ClaimReason` numbers.
pub fn claim_reason_from_i32(value: i32) -> Result<fr::ClaimReason, ConvertError> {
    Ok(match value {
        1 => fr::ClaimReason::Startup,
        2 => fr::ClaimReason::User,
        3 => fr::ClaimReason::Takeover,
        other => return Err(unknown("ClaimReason", other)),
    })
}

/// `InjectStatus` numbers.
pub fn inject_status_from_i32(value: i32) -> Result<fr::InjectStatus, ConvertError> {
    Ok(match value {
        1 => fr::InjectStatus::Ok,
        2 => fr::InjectStatus::Partial,
        3 => fr::InjectStatus::Failed,
        other => return Err(unknown("InjectStatus", other)),
    })
}

/// The `Hello` fields this layer needs to identify a peer and gate membership.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerInfo {
    /// `Hello.node_id`, checked against the certificate's SPKI hash.
    pub node_id: NodeId,
    /// `Hello.display_name` (non-authoritative).
    pub display_name: String,
    /// Sanitized optional OS name/version.
    pub system_name: String,
    /// Canonical application SemVer, or empty when absent/invalid.
    pub app_version: String,
    /// Whether the peer advertises support for remote update messages.
    pub remote_update: bool,
    /// Whether the peer supports addressed routing and backend clipboard application acks.
    pub clipboard_apply_ack: bool,
    /// `Hello.can_own`.
    pub can_own: bool,
    /// `Hello.owner_serial`.
    pub owner_serial: u64,
    /// `Hello.owner_node_id`.
    pub owner: Option<NodeId>,
    /// `Hello.topology_revision`: the layout revision this node has *stored* (KVM-072).
    pub topology_revision: u64,
    /// `Hello.topology_author`: the author applied together with that revision.
    pub topology_author: Option<NodeId>,
    /// `Hello.displays_revision`: the peer's current local display-set revision.
    pub displays_revision: u64,
    /// `Hello.displays`: the peer's current local display set.
    pub displays: Vec<wire::Display>,
    /// The capability bits `Hello.capabilities`, in the four flags `focus-router` knows.
    pub capabilities: fr::Capabilities,
    /// `CAPABILITY_TEXT_CLIPBOARD`.
    ///
    /// Deliberately *not* part of [`fr::Capabilities`]: the focus FSM has no opinion on the
    /// clipboard, but the clipboard channel does — ADR-008 makes it opt-in on both sides, so a
    /// node whose peer did not claim this bit never offers it anything.
    pub text_clipboard: bool,
}

impl PeerInfo {
    /// Extracts the fields from a validated `Hello`.
    pub fn from_hello(hello: &wire::Hello) -> Result<Self, ConvertError> {
        let node_id: [u8; 32] = hello
            .node_id
            .as_slice()
            .try_into()
            .map_err(|_| ConvertError::Unsupported("Hello.node_id is not 32 bytes"))?;
        Ok(Self {
            node_id: NodeId::new(node_id),
            display_name: hello.display_name.clone(),
            system_name: protocol::sanitize_system_name(&hello.system_name),
            app_version: protocol::sanitize_app_version(&hello.app_version),
            remote_update: hello
                .capabilities
                .contains(&(wire::Capability::RemoteUpdate as i32)),
            clipboard_apply_ack: hello
                .capabilities
                .contains(&(wire::Capability::ClipboardApplyAck as i32)),
            can_own: hello.can_own,
            owner_serial: hello.owner_serial,
            owner: if hello.owner_node_id.is_empty() {
                None
            } else {
                Some(NodeId::new(
                    hello
                        .owner_node_id
                        .as_slice()
                        .try_into()
                        .map_err(|_| ConvertError::Unsupported("owner_node_id is not 32 bytes"))?,
                ))
            },
            topology_revision: hello.topology_revision,
            topology_author: if hello.topology_author.is_empty() {
                None
            } else {
                Some(NodeId::new(
                    hello.topology_author.as_slice().try_into().map_err(|_| {
                        ConvertError::Unsupported("topology_author is not 32 bytes")
                    })?,
                ))
            },
            displays_revision: hello.displays_revision,
            displays: hello.displays.clone(),
            capabilities: capabilities_from_wire(&hello.capabilities),
            text_clipboard: hello
                .capabilities
                .contains(&(wire::Capability::TextClipboard as i32)),
        })
    }
}

/// The four `focus_router::Capabilities` flags carried by `Hello.capabilities`.
///
/// The wire enum has ten bits; the FSM only models `CAPTURE`, `INJECT`, `ABSOLUTE_POINTER`
/// and `EMERGENCY_HOTKEY`, so the other bits are informational for this layer and are
/// dropped rather than guessed at. The numbers are read through the generated
/// `protocol::wire::Capability`, never as bare literals, and the match below is exhaustive
/// over that enum: a bit added to the `.proto` fails the build until its mapping is
/// decided.
pub fn capabilities_from_wire(bits: &[i32]) -> fr::Capabilities {
    use protocol::wire::Capability as WireCapability;
    let mut out = fr::Capabilities::default();
    for bit in bits {
        let Ok(capability) = WireCapability::try_from(*bit) else {
            // A number the contract does not define is ignored: this layer has no flag for
            // it and must not invent one.
            continue;
        };
        match capability {
            WireCapability::Capture => out.capture = true,
            WireCapability::Inject => out.inject = true,
            WireCapability::AbsolutePointer => out.absolute_pointer = true,
            WireCapability::EmergencyHotkey => out.emergency_hotkey = true,
            WireCapability::Unspecified
            | WireCapability::PhysicalHidKeys
            | WireCapability::TextClipboard
            | WireCapability::NativeRepeat
            | WireCapability::SoftwareRepeat
            | WireCapability::ReceiverTakeover
            | WireCapability::DisplaysChanged
            | WireCapability::RemoteUpdate
            | WireCapability::ClipboardApplyAck => {}
        }
    }
    out
}
