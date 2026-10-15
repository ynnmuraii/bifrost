//! `GuiFrame` inside the session loop: the same operations the control verbs use, plus the
//! pushed events `docs/03-architecture.md` («GUI ↔ core») promises.
//!
//! This is a child module of [`crate::session`] on purpose: the GUI's requests *are* session
//! operations (`claim`, `abandon`, `reset-portals`, a layout edit), and putting them
//! anywhere else would either duplicate the FSM half of those operations or force every
//! field they touch to become `pub(crate)`. The transport half — the endpoint, the link, the
//! event fan-out — lives in [`crate::gui`], which knows nothing about the FSM.
//!
//! # Pushes
//!
//! Once per turn of the loop, [`Session::publish_gui_state`] compares what the GUI would be
//! told with what it was told last time and publishes only the difference: `Status` (the
//! whole line: focus, ownership, the layout revision, the members, the capabilities this node
//! is missing) and `TopologyDoc` (on every applied revision). The loop turns at most every
//! 50 ms, so an open GUI never shows a stale screen, and a GUI nobody opened costs nothing
//! but the comparison.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use focus_router as fr;
use protocol::ipc::{self as ipc, gui_frame::Msg};
use tokio::sync::{mpsc, oneshot};

use super::{ctl, pairing, Handled, LayoutSource, Session, LAYOUT_VERDICT_TIMEOUT};
use crate::gui::Outgoing;

/// How long the loop waits for one GUI connection to take a frame.
///
/// The connection's queue holds [`crate::gui::OUTBOUND_QUEUE`] frames, so this only triggers
/// for a GUI that is not reading at all. Core must not be stalled by its own GUI («GUI не
/// участвует в горячем пути»), so the frame is dropped with a warning instead of being
/// written into a void.
const ANSWER_TIMEOUT: Duration = Duration::from_millis(500);

impl Session {
    /// Publishes a frame to every connected GUI.
    pub(crate) fn publish(&self, message: Msg) {
        crate::gui::Server::publish(&self.gui_events, message);
    }

    /// This node's own `aa:bb:…` fingerprint, for the pairing prompt's "Подробнее".
    fn local_fingerprint(&self) -> String {
        identity::fingerprint_grouped(&self.node_id)
    }

    /// The status of this node, as `GetStatus` answers it and `Status` pushes it.
    ///
    /// Every field is read from what the loop already knows: nothing here measures anything
    /// on its own, and a value this build does not know is left at its default rather than
    /// guessed — `rtt_p95_us` is the one such field today, because no link measures its RTT
    /// yet and the GUI hides a zero.
    pub(crate) async fn gui_status(&mut self) -> ipc::Status {
        let focus = self.router.focus();
        let lease_peer = match self.router.lease() {
            fr::LeaseSlot::Source(lease) => Some(lease.peer),
            fr::LeaseSlot::Receiver(lease) => Some(lease.source),
            fr::LeaseSlot::Idle => None,
        };
        let owner = self.router.owner().copied();
        let active_peer = lease_peer
            .and_then(|peer| {
                self.trust
                    .get(&peer)
                    .map(|record| record.display_name.clone())
            })
            .unwrap_or_default();
        let online = self.manager.members().await;
        let members = self
            .trust
            .peers
            .iter()
            .filter(|record| record.is_active())
            .map(|record| {
                let connected = online.contains(&record.node_id);
                ipc::MemberStatus {
                    node_id: crate::command::node_id_hex(&record.node_id),
                    display_name: record.display_name.clone(),
                    online: connected,
                    is_owner: owner == Some(record.node_id),
                    link_state: if connected { "READY" } else { "OFFLINE" }.to_owned(),
                }
            })
            .collect();
        let applied = self.layout_state.applied();
        ipc::Status {
            role: if self.router.is_owner() {
                "owner".to_owned()
            } else {
                "follower".to_owned()
            },
            focus: focus_word(focus.as_str()).to_owned(),
            active_peer,
            connected: !online.is_empty(),
            clipboard_enabled: self
                .clipboard
                .as_ref()
                .is_some_and(crate::clipboard::Clipboard::is_enabled),
            emergency_hotkey: self.emergency_hotkey.clone(),
            rtt_p95_us: 0,
            rtt_jitter_us: 0,
            last_error_code: self
                .pairing_error
                .clone()
                .or_else(|| {
                    self.last_layout_error
                        .as_ref()
                        .map(|(code, _)| code.clone())
                })
                .unwrap_or_default(),
            backend: self.backend.clone(),
            capabilities_unavailable: self.unavailable_capabilities(),
            portal_state: self.portal_state.clone(),
            is_owner: self.router.is_owner(),
            owner_node_id: owner
                .map_or_else(String::new, |owner| crate::command::node_id_hex(&owner)),
            owner_serial: self.router.serial(),
            members,
            layout_revision: applied.revision,
            layout_author: applied
                .author
                .map(|author| identity::fingerprint_grouped(&author))
                .unwrap_or_default(),
            layout_pending_edits: u32::from(self.layout_state.pending().is_some()),
            layout_editable: self.layout_editing_disabled.is_none(),
            active_layout: self.active_layout.clone(),
            agent_degraded_reason: self.agent_degraded_reason.clone(),
            agent_restart_circuit_open: self.agent_restart_circuit.is_open(),
            transition_disabled_reason: self.transition_disabled_reason(),
        }
    }

    /// The capability bits this node does *not* have, by their documented names.
    ///
    /// `docs/03-architecture.md` («GUI (NFR)»): the window shows the active backend *and* its
    /// degradations — «отсутствие аварийной клавиши, отсутствие takeover, недоступный
    /// clipboard, ожидание портального диалога» — so the absent bits travel instead of being
    /// inferred from the backend's name.
    fn unavailable_capabilities(&self) -> Vec<String> {
        let bits = [
            ("CAPTURE", self.capabilities.capture),
            ("INJECT", self.capabilities.inject),
            ("ABSOLUTE_POINTER", self.capabilities.absolute_pointer),
            ("TEXT_CLIPBOARD", self.capabilities.text_clipboard),
            ("EMERGENCY_HOTKEY", self.capabilities.emergency_hotkey),
            ("RECEIVER_TAKEOVER", self.capabilities.receiver_takeover),
        ];
        bits.iter()
            .filter(|(_, present)| !present)
            .map(|(name, _)| (*name).to_owned())
            .collect()
    }

    /// The layout editor's model: the applied revision, every member's displays and links.
    ///
    /// Built from the same [`Session::applied_wire`] the replication sends, so the GUI edits
    /// exactly the document the peers hold.
    pub(crate) async fn gui_topology_doc(&self) -> ipc::TopologyDoc {
        let applied = self.layout_state.applied();
        let online = self.manager.members().await;
        // The screens a desktop-anchored endpoint resolves to right now: «Весь рабочий стол
        // (авто)» is one line in the editor, and this is what it currently means. Empty for a
        // link that names its displays.
        let auto_displays = |endpoint: &protocol::wire::Endpoint| -> Vec<String> {
            if endpoint.display_id != config::AUTO_DISPLAY {
                return Vec::new();
            }
            let edge = match protocol::wire::Edge::try_from(endpoint.edge) {
                Ok(protocol::wire::Edge::Left) => geometry::Edge::Left,
                Ok(protocol::wire::Edge::Right) => geometry::Edge::Right,
                Ok(protocol::wire::Edge::Top) => geometry::Edge::Top,
                Ok(protocol::wire::Edge::Bottom) => geometry::Edge::Bottom,
                _ => return Vec::new(),
            };
            node_id_of(&endpoint.node_id)
                .and_then(|node| {
                    self.topology
                        .as_ref()
                        .and_then(|live| live.node(&node))
                        .map(|member| {
                            geometry::boundary_displays(member, edge)
                                .iter()
                                .map(|display| display.id.as_str().to_owned())
                                .collect::<Vec<String>>()
                        })
                })
                .unwrap_or_default()
        };
        // The display an inactive link is missing, matched straight from the endpoint the
        // document carries rather than by position, so a link's state cannot drift with the
        // order of the two lists. `None` means the endpoint's display is present.
        let missing_display = |side: geometry::EndpointSide,
                               endpoint: &protocol::wire::Endpoint| {
            self.inactive_links.iter().find_map(|link| {
                link.missing
                    .iter()
                    .find(|missing| {
                        missing.side == side
                            && missing.node.as_bytes() == endpoint.node_id.as_slice()
                            && missing.display.as_str() == endpoint.display_id
                    })
                    .map(|missing| missing.display.as_str().to_owned())
            })
        };
        let document = match self.applied_wire.clone() {
            Some(document) => document,
            // Before the first revision is published — a fresh workspace, a node that has just
            // started — the editor gets what this node actually knows: the assembled live
            // document (agent displays, configured links) instead of an empty shell it could
            // not edit.
            None => match &self.topology {
                Some(live) => crate::topology::wire_from_live(
                    live,
                    &self.layout,
                    applied.revision,
                    applied.author.unwrap_or(self.node_id),
                    self.node_id,
                ),
                None => protocol::wire::Topology::default(),
            },
        };
        let mut members: Vec<ipc::MemberTopology> = document
            .nodes
            .iter()
            .map(|node| ipc::MemberTopology {
                node_id: hex(&node.node_id),
                display_name: if node_id_of(&node.node_id) == Some(self.node_id) {
                    self.display_name.clone()
                } else {
                    node_id_of(&node.node_id)
                        .and_then(|node| self.trust.get(&node))
                        .map(|record| record.display_name.clone())
                        .unwrap_or_default()
                },
                displays: node.displays.iter().map(display_view).collect(),
                online: node_id_of(&node.node_id)
                    .is_some_and(|node| node == self.node_id || online.contains(&node)),
            })
            .collect();
        // A peer the user paired with is on the map whether or not its link is up: a computer
        // that stopped answering is shown offline with the screens it last reported, instead of
        // disappearing until it comes back. Only a revoke removes it (then it is not trusted).
        for record in self.trust.peers.iter().filter(|record| record.is_active()) {
            let node_id = crate::command::node_id_hex(&record.node_id);
            if members.iter().any(|member| member.node_id == node_id) {
                continue;
            }
            let last_known = self
                .peer_displays
                .get(&record.node_id)
                .map(|(_, displays)| displays.as_slice())
                .unwrap_or_default();
            members.push(ipc::MemberTopology {
                node_id,
                display_name: record.display_name.clone(),
                displays: last_known.iter().map(display_view).collect(),
                online: false,
            });
        }
        ipc::TopologyDoc {
            revision: applied.revision,
            author_node_id: applied
                .author
                .map(|author| crate::command::node_id_hex(&author))
                .unwrap_or_default(),
            members,
            links: document
                .links
                .iter()
                .enumerate()
                .filter_map(|(index, link)| {
                    let from = link.source_endpoint.as_ref()?;
                    let to = link.target_endpoint.as_ref()?;
                    // A link the last rebuild left out because a monitor is not reported is
                    // carried as-is, marked inactive: the editor still shows the authored link,
                    // and says which display is missing instead of dropping it silently.
                    let missing = missing_display(geometry::EndpointSide::Source, from)
                        .or_else(|| missing_display(geometry::EndpointSide::Target, to));
                    // The whole document is unusable right now (overlapping links, say): no
                    // link works, and the editor must not draw them as if they did.
                    let missing = self.topology_error.clone().or(missing);
                    let mut resolved = auto_displays(from);
                    resolved.extend(auto_displays(to));
                    Some(ipc::LinkView {
                        // The protocol's `TransitionLink` has no id: the document's own order
                        // identifies one, and it is stable for as long as that revision is.
                        id: format!("link-{index}"),
                        from: Some(endpoint_view(from)),
                        to: Some(endpoint_view(to)),
                        // A link's reverse direction is resolved by the same document
                        // (`geometry::resolve_motion`), which is what the editor shows.
                        bidirectional: true,
                        active: missing.is_none(),
                        inactive_reason: missing.unwrap_or_default(),
                        auto_displays: resolved,
                    })
                })
                .collect(),
        }
    }

    /// The divergent local edits kept after a partition: zero or one, with its id.
    pub(crate) fn gui_pending_edits(&self) -> ipc::PendingEdits {
        let applied = self.layout_state.applied();
        let edits = match self.layout_state.pending_with_id() {
            Some((edit_id, layout_toml)) => {
                let base = self.layout_state.pending_base().unwrap_or(applied);
                vec![ipc::PendingEdit {
                    edit_id: edit_id.to_vec(),
                    base_revision: base.revision,
                    base_author: base
                        .author
                        .map(|author| crate::command::node_id_hex(&author))
                        .unwrap_or_default(),
                    author_node_id: crate::command::node_id_hex(&self.node_id),
                    layout_toml: layout_toml.into_bytes(),
                }]
            }
            None => Vec::new(),
        };
        ipc::PendingEdits { edits }
    }

    /// One `GuiFrame` request, answered on the connection it came from.
    pub(crate) async fn on_gui_request(&mut self, request: crate::gui::Request) {
        let crate::gui::Request {
            request_id,
            connection_id,
            message,
            out,
        } = request;
        let source = LayoutSource::local("gui", connection_id);
        match message {
            Msg::GetStatus(_) => {
                let status = self.gui_status().await;
                self.gui_status = Some(status.clone());
                answer(&out, request_id, Msg::Status(status)).await;
                // Broadcast events are not history: a second/reconnected GUI needs the
                // current window and unanswered ceremony as well as the workspace status.
                if let Some(window) = self.pairing.window() {
                    answer(
                        &out,
                        0,
                        Msg::PairingWindow(ipc::PairingWindow {
                            open: true,
                            address: window.address.clone(),
                            seconds_remaining: window.remaining_s(self.now_ms()),
                            attempt_id: window.attempt_id.to_vec(),
                        }),
                    )
                    .await;
                }
                if let Some(prompt) = self.pairing.pending_prompt() {
                    answer(
                        &out,
                        0,
                        Msg::PairingPrompt(ipc::PairingPrompt {
                            attempt_id: prompt.ceremony_id.to_vec(),
                            peer_name: prompt.peer_name.clone(),
                            sas_code: prompt.sas_code.clone(),
                            local_fingerprint: prompt.local_fingerprint.clone(),
                            peer_fingerprint: prompt.peer_fingerprint.clone(),
                        }),
                    )
                    .await;
                }
            }
            Msg::GetTopology(_) => {
                self.query_agent_displays().await;
                let doc = self.gui_topology_doc().await;
                self.gui_topology = Some(doc.clone());
                answer(&out, request_id, Msg::TopologyDoc(doc)).await;
            }
            Msg::ListPendingEdits(_) => {
                let edits = self.gui_pending_edits();
                answer(&out, request_id, Msg::PendingEdits(edits)).await;
            }
            Msg::ClaimOwnership(_) => {
                let response = self.claim().await;
                answer(&out, request_id, Msg::OpResult(op_result(&response))).await;
            }
            Msg::AbandonOwnership(_) => {
                let response = self.abandon().await;
                answer(&out, request_id, Msg::OpResult(op_result(&response))).await;
            }
            Msg::RetryAgent(_) => {
                let result = self.retry_agent().await;
                answer(&out, request_id, Msg::OpResult(result)).await;
            }
            Msg::ResetPortalGrants(_) => {
                let response = self.reset_portals().await;
                answer(&out, request_id, Msg::OpResult(op_result(&response))).await;
            }
            Msg::Shutdown(_) => {
                answer(&out, request_id, Msg::OpResult(ok())).await;
                self.shutdown_agent().await;
            }
            Msg::SetClipboard(set) => {
                let result = self.set_clipboard(set.enabled);
                answer(&out, request_id, Msg::OpResult(result)).await;
            }
            Msg::RevokePeer(revoke) => {
                let result = self.revoke_peer(&revoke.peer_node_id).await;
                answer(&out, request_id, Msg::OpResult(result)).await;
            }
            Msg::ExportDiagnostics(export) => {
                let result = self.export_diagnostics(&export.target_path).await;
                answer(&out, request_id, Msg::OpResult(result)).await;
            }
            Msg::UpdateTopology(update) => {
                let text = match String::from_utf8(update.layout_toml) {
                    Ok(text) => text,
                    Err(_) => {
                        return answer(&out, request_id, Msg::OpResult(bad_request())).await;
                    }
                };
                let expected = layout_base(update.expected_revision, &update.expected_author);
                let handled = self.layout_edit_document(text, expected, source).await;
                self.reply_handled(request_id, out, handled).await;
            }
            Msg::SwitchTo(switch) => {
                // The same path as `lws-core switch <peer>`: one FSM transition, one verdict.
                let response = self
                    .switch_to(&crate::command::PeerRef::new(switch.peer))
                    .await;
                answer(&out, request_id, Msg::OpResult(op_result(&response))).await;
            }
            Msg::LayoutAuto(_) => {
                // The same path the `layout auto` control command takes: the GUI button and
                // the CLI cannot disagree about what "automatic" means.
                let handled = self.layout_auto(source).await;
                self.reply_handled(request_id, out, handled).await;
            }
            Msg::ApplyPendingEdit(apply) => {
                let matches = matches!(
                    self.layout_state.pending_with_id(),
                    Some((id, _)) if id.as_slice() == apply.edit_id.as_slice()
                );
                if !matches {
                    return answer(
                        &out,
                        request_id,
                        Msg::OpResult(ipc::OpResult {
                            ok: false,
                            error_code: "UNKNOWN_EDIT".to_owned(),
                        }),
                    )
                    .await;
                }
                let handled = self.layout_apply(source).await;
                self.reply_handled(request_id, out, handled).await;
            }
            Msg::DiscardPendingEdit(discard) => {
                let matched = matches!(
                    self.layout_state.pending_with_id(),
                    Some((id, _)) if id.as_slice() == discard.edit_id.as_slice()
                );
                let dropped = matched && self.release_layout_pending();
                answer(
                    &out,
                    request_id,
                    Msg::OpResult(ipc::OpResult {
                        ok: dropped,
                        error_code: if dropped {
                            String::new()
                        } else {
                            "UNKNOWN_EDIT".to_owned()
                        },
                    }),
                )
                .await;
            }
            Msg::StartPairing(start) => {
                let window_s = if start.window_s == 0 {
                    120
                } else {
                    start.window_s
                };
                let result = match self.open_pairing_window(window_s) {
                    Ok(_) => ok(),
                    Err((code, _detail)) => ipc::OpResult {
                        ok: false,
                        error_code: code,
                    },
                };
                answer(&out, request_id, Msg::OpResult(result)).await;
            }
            Msg::PairWith(dial) => {
                let wait = self.pairing.dial_started();
                self.spawn_pair_dial(dial.address);
                spawn_op_result(request_id, out, wait).await;
            }
            Msg::PairingAnswer(peer_answer) => {
                let ceremony: pairing::CeremonyId =
                    match peer_answer.attempt_id.as_slice().try_into() {
                        Ok(ceremony) => ceremony,
                        Err(_) => {
                            return answer(&out, request_id, Msg::OpResult(bad_request())).await
                        }
                    };
                match self.pairing.answer(&ceremony, peer_answer.approve) {
                    Ok(wait) => spawn_op_result(request_id, out, wait).await,
                    Err((code, _detail)) => {
                        answer(
                            &out,
                            request_id,
                            Msg::OpResult(ipc::OpResult {
                                ok: false,
                                error_code: code,
                            }),
                        )
                        .await;
                    }
                }
            }
            // A GUI never sends the pushed half of the contract.
            other => {
                tracing::warn!(
                    message = ?std::mem::discriminant(&other),
                    "a GUI sent a message only core sends"
                );
            }
        }
    }

    /// Publishes `Status` and `TopologyDoc` when they changed since the last publish.
    pub(crate) async fn publish_gui_state(&mut self) {
        if self.gui_events.receiver_count() == 0 {
            // Nobody is listening: computing the status would cost the loop for nothing. The
            // next GUI asks for it, and its own `GetStatus` sets `gui_status` as it answers.
            return;
        }
        let status = self.gui_status().await;
        if self.gui_status.as_ref() != Some(&status) {
            self.gui_status = Some(status.clone());
            self.publish(Msg::Status(status));
        }
        let document = self.gui_topology_doc().await;
        if self.gui_topology.as_ref() != Some(&document) {
            self.gui_topology = Some(document.clone());
            self.publish(Msg::TopologyDoc(document));
        }
    }

    /// Folds one report from a ceremony task into the loop, and teaches the GUI.
    pub(crate) async fn on_pairing_event(&mut self, event: pairing::Event) {
        let now = self.now_ms();
        let Some(published) = self.pairing.on_event(event, now) else {
            return;
        };
        match published {
            pairing::PublishedEvent::Prompt(prompt) => {
                tracing::info!(
                    peer = prompt.peer_name.as_str(),
                    "pairing: the six digits are on both screens"
                );
                self.publish(Msg::PairingPrompt(ipc::PairingPrompt {
                    attempt_id: prompt.ceremony_id.to_vec(),
                    peer_name: prompt.peer_name,
                    sas_code: prompt.sas_code,
                    local_fingerprint: prompt.local_fingerprint,
                    peer_fingerprint: prompt.peer_fingerprint,
                }));
                self.publish_gui_state().await;
            }
            pairing::PublishedEvent::Finished {
                outcome,
                window_closed,
                address,
                status,
                reply,
            } => {
                if let Some(window) = window_closed {
                    self.publish(Msg::PairingWindow(ipc::PairingWindow {
                        open: false,
                        address: String::new(),
                        seconds_remaining: 0,
                        attempt_id: window.attempt_id.to_vec(),
                    }));
                }
                let verdict: pairing::Verdict = match outcome {
                    pairing::Outcome::Paired(peer) => {
                        // The same write the CLI's ceremony does: one `TrustedPeer`, atomically,
                        // with the address this side learned (`pairing::peer_address`).
                        let record = peer.trusted_peer(identity::now_unix_ms(), address);
                        tracing::info!(
                            peer = crate::command::node_id_hex(&record.node_id).as_str(),
                            "pairing committed"
                        );
                        self.trust.trust(record.clone());
                        self.pairing_error = None;
                        // The verdict follows both durable trust and the live manager update:
                        // success means the other core can connect without a restart.
                        match self.trust.save(&self.paths.trust_file()) {
                            Ok(_) => match self.manager.add_peer(record).await {
                                Ok(()) => Ok(()),
                                Err(error) => {
                                    tracing::warn!(%error, "cannot start the paired peer link");
                                    self.pairing_error = Some("LINK_START".to_owned());
                                    Err(("LINK_START".to_owned(), error.to_string()))
                                }
                            },
                            Err(error) => {
                                tracing::warn!(%error, "cannot write the trust record");
                                self.pairing_error = Some("TRUST_WRITE".to_owned());
                                Err(("TRUST_WRITE".to_owned(), error.to_string()))
                            }
                        }
                    }
                    pairing::Outcome::Rejected => {
                        self.pairing_error = None;
                        tracing::info!("pairing rejected; nothing was written");
                        Ok(())
                    }
                    pairing::Outcome::Failed(detail) => {
                        tracing::warn!(detail = detail.as_str(), "pairing failed");
                        self.pairing_error = Some("PAIRING_FAILED".to_owned());
                        Err(("PAIRING_FAILED".to_owned(), detail))
                    }
                };
                if let Some(reply) = reply {
                    let _ = reply.send(verdict);
                }
                self.refusal = Some(status);
                self.publish_gui_state().await;
            }
        }
    }

    /// Opens the pairing window and tells every GUI what the other node has to type.
    ///
    /// The control channel's `pair-open` calls this too: one window, one implementation, and
    /// both surfaces get the same address and `attempt_id`.
    pub(super) fn open_pairing_window(
        &mut self,
        window_s: u32,
    ) -> Result<pairing::Window, pairing::Refusal> {
        let now = self.now_ms();
        match self.pairing.open(now, window_s, self.listen) {
            Ok(window) => {
                tracing::info!(
                    address = window.address.as_str(),
                    seconds = window.seconds,
                    "pairing window open"
                );
                self.pairing_error = None;
                let remaining = window.remaining_s(now);
                self.publish(Msg::PairingWindow(ipc::PairingWindow {
                    open: true,
                    address: window.address.clone(),
                    seconds_remaining: remaining,
                    attempt_id: window.attempt_id.to_vec(),
                }));
                Ok(window)
            }
            Err((code, detail)) => {
                tracing::warn!(
                    code = code.as_str(),
                    detail = detail.as_str(),
                    "pairing window"
                );
                self.pairing_error = Some(code.clone());
                Err((code, detail))
            }
        }
    }

    /// Closes the window when its deadline passes (checked once per loop turn).
    pub(crate) fn expire_pairing_window(&mut self) {
        if let Some(window) = self.pairing.expire(self.now_ms()) {
            tracing::info!(address = window.address.as_str(), "pairing window closed");
            self.publish(Msg::PairingWindow(ipc::PairingWindow {
                open: false,
                address: String::new(),
                seconds_remaining: 0,
                attempt_id: window.attempt_id.to_vec(),
            }));
        }
    }

    /// `PairWith`: dial the address the user typed and run the dialer's half.
    pub(super) fn spawn_pair_dial(&mut self, address: String) {
        // The dialer's ceremony exists before the dial, under its own id: a dial that never
        // reaches the peer is reported back against that id, and two dials can never share one.
        let (ceremony_id, answer) = self.pairing.begin();
        let parsed: std::net::SocketAddr = match address.parse() {
            Ok(address) => address,
            Err(error) => {
                self.pairing.dial_failed(
                    &ceremony_id,
                    "BAD_ADDRESS",
                    format!("{address:?}: {error}"),
                );
                return;
            }
        };
        let identity = Arc::clone(&self.identity);
        let events = self.pairing_events_tx.clone();
        let local_fingerprint = self.local_fingerprint();
        // What the other computer's prompt and trust record will show for this node.
        let display_name = self.display_name.clone();
        // The port this node listens on, which the opener records as *this* node's address
        // (`PairHello.listen_port`): the dialer's source port is ephemeral.
        let listen_port = self.listen.port();
        tokio::spawn(async move {
            match identity::dial(
                parsed,
                &identity,
                crate::pair::WINDOW,
                listen_port,
                &display_name,
            )
            .await
            {
                Ok(driver) => {
                    pairing::run(
                        driver,
                        ceremony_id,
                        answer,
                        false,
                        local_fingerprint,
                        events,
                    )
                    .await;
                }
                Err(error) => {
                    // The dial never reached a ceremony: the loop drops the registered
                    // ceremony and resolves the `PairWith` reply with this failure.
                    let _ = events
                        .send(pairing::Event::DialFailed {
                            ceremony_id,
                            code: "PAIR_FAILED".to_owned(),
                            detail: error.to_string(),
                        })
                        .await;
                }
            }
        });
    }

    /// One accepted `pair/1` connection: the opener's half of the ceremony.
    pub(crate) fn spawn_pair_accept(&mut self, connection: transport::PairingConnection) {
        // Each connection gets its own ceremony id, so a second concurrent dialer of the same
        // window cannot overwrite the first (`docs/05-protocol.md` admits up to three).
        let (ceremony_id, answer) = self.pairing.begin();
        let identity = Arc::clone(&self.identity);
        let events = self.pairing_events_tx.clone();
        let local_fingerprint = self.local_fingerprint();
        let display_name = self.display_name.clone();
        let listen_port = self.listen.port();
        let window = connection.window.clone();
        let address = connection.address.to_string();
        let hello_timeout = opener_hello_timeout(&window, self.now_ms());
        tokio::spawn(async move {
            match within(
                hello_timeout,
                identity::accept_stream(
                    connection.stream,
                    &identity,
                    &window,
                    Some(address),
                    listen_port,
                    &display_name,
                ),
            )
            .await
            {
                Ok(driver) => {
                    pairing::run(driver, ceremony_id, answer, true, local_fingerprint, events)
                        .await;
                }
                Err(error) => {
                    // A dialer that completes TLS and then says nothing must not hold its slot:
                    // the failure releases the ceremony and the window's pending count.
                    let _ = events
                        .send(pairing::Event::Finished {
                            ceremony_id,
                            outcome: pairing::Outcome::Failed(error.to_string()),
                        })
                        .await;
                }
            }
        });
    }

    /// `SetClipboard`: the GUI's toggle, on the same policy the configuration seeds.
    ///
    /// Without `[clipboard].enabled` there is no runtime at all (ADR-008: the channel is
    /// opt-in), and the honest answer is a refusal the GUI shows instead of a toggle it
    /// pretends to move.
    fn set_clipboard(&mut self, enabled: bool) -> ipc::OpResult {
        let Some(clipboard) = self.clipboard.as_mut() else {
            return ipc::OpResult {
                ok: false,
                error_code: "CLIPBOARD_DISABLED".to_owned(),
            };
        };
        clipboard.set_enabled(enabled);
        tracing::info!(enabled, "clipboard toggled from the GUI");
        ok()
    }

    /// `RevokePeer`: remove trust from new handshakes, then close current links.
    ///
    /// The manager shares this pin set with its live TLS verifiers.
    pub(crate) async fn revoke_peer(&mut self, peer: &[u8]) -> ipc::OpResult {
        let Some(node) = node_id_of(peer) else {
            return bad_request();
        };
        if !self.trust.revoke(&node, identity::now_unix_ms()) {
            return ipc::OpResult {
                ok: false,
                error_code: "UNKNOWN_PEER".to_owned(),
            };
        }
        if let Err(error) = self.trust.save(&self.paths.trust_file()) {
            tracing::warn!(%error, "cannot write the trust record");
            return ipc::OpResult {
                ok: false,
                error_code: "TRUST_WRITE".to_owned(),
            };
        }
        if let Err(error) = self.manager.remove_peer(&node).await {
            tracing::warn!(%error, "cannot remove the live peer pin");
        }
        if let Some(clipboard) = self.clipboard.as_mut() {
            clipboard.on_link_down(node).await;
        }
        // A revoke is the one case where the last known manifest goes away with the peer:
        // a link drop keeps it, so the map still shows the computer offline.
        self.peer_displays.remove(&node);
        self.rebuild_topology();
        tracing::info!(
            peer = crate::command::node_id_hex(&node).as_str(),
            "peer revoked"
        );
        ok()
    }

    /// `ExportDiagnostics`: a report a human can attach to a bug.
    ///
    /// `docs/03-architecture.md` («Логи и diagnostic export»): never key presses, never
    /// clipboard content, never a private key or a portal restore token. Everything written
    /// here is the status line plus the paths and versions around it.
    async fn export_diagnostics(&mut self, target: &str) -> ipc::OpResult {
        let path = if target.trim().is_empty() {
            self.paths
                .state_dir()
                .join(format!("diagnostics-{}.txt", identity::now_unix_ms()))
        } else {
            std::path::PathBuf::from(target)
        };
        let status = self.gui_status().await;
        let window = self.pairing.window().map(|window| {
            format!(
                "open, {}s left, {}",
                window.remaining_s(self.now_ms()),
                window.address
            )
        });
        let mut report = String::new();
        let mut line = |text: String| {
            report.push_str(&text);
            report.push('\n');
        };
        line("lan-workspace diagnostics".to_owned());
        line(format!("generated_at_unix_ms: {}", identity::now_unix_ms()));
        line(format!(
            "node_id: {}",
            crate::command::node_id_hex(&self.node_id)
        ));
        line(format!("backend: {}", self.backend));
        line(format!("focus: {} role: {}", status.focus, status.role));
        line(format!("owner: {}", status.owner_node_id));
        line(format!("owner_serial: {}", status.owner_serial));
        line(format!("layout_revision: {}", status.layout_revision));
        line(format!("layout_author: {}", status.layout_author));
        line(format!(
            "layout_pending_edits: {}",
            status.layout_pending_edits
        ));
        line(format!("layout_editable: {}", status.layout_editable));
        line(format!("clipboard_enabled: {}", status.clipboard_enabled));
        line(format!("portal_state: {}", status.portal_state));
        line(format!("emergency_hotkey: {}", status.emergency_hotkey));
        line(format!(
            "capabilities_unavailable: {}",
            status.capabilities_unavailable.join(",")
        ));
        line(format!(
            "pairing_window: {}",
            window.unwrap_or_else(|| "closed".to_owned())
        ));
        line(format!(
            "refusal: {}",
            self.refusal.clone().unwrap_or_default()
        ));
        line(format!("config: {}", self.paths.config_file().display()));
        line(format!("state: {}", self.paths.state_dir().display()));
        line(format!("logs: {}", self.paths.logs_dir().display()));
        line("members:".to_owned());
        for member in &status.members {
            line(format!(
                "  {} {} owner={} link={}",
                member.node_id, member.display_name, member.is_owner, member.link_state
            ));
        }
        for peer in self.trust.active() {
            line(format!(
                "trusted: {} {}",
                crate::command::node_id_hex(&peer.node_id),
                peer.display_name
            ));
        }
        match config::save::save_atomic(&path, &report) {
            Ok(()) => {
                tracing::info!(path = %path.display(), "diagnostics exported");
                ipc::OpResult {
                    ok: true,
                    error_code: String::new(),
                }
            }
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "cannot write diagnostics");
                ipc::OpResult {
                    ok: false,
                    error_code: "EXPORT_FAILED".to_owned(),
                }
            }
        }
    }

    /// Sends an answer now, or hands it to a task that waits for a verdict.
    async fn reply_handled(
        &mut self,
        request_id: u64,
        out: mpsc::Sender<Outgoing>,
        handled: Handled,
    ) {
        match handled {
            Handled::Reply(response) => {
                answer(&out, request_id, Msg::OpResult(op_result(&response))).await;
            }
            Handled::Deferred(verdict) => {
                // The same two seconds the control channel gives the coordinator: past that,
                // "the verdict has not arrived" is the answer (`PENDING_VERDICT`), and the edit
                // stays pending.
                tokio::spawn(async move {
                    let result = match tokio::time::timeout(LAYOUT_VERDICT_TIMEOUT, verdict).await {
                        Ok(Ok(response)) => op_result(&response),
                        Ok(Err(_)) | Err(_) => ipc::OpResult {
                            ok: false,
                            error_code: "PENDING_VERDICT".to_owned(),
                        },
                    };
                    answer(&out, request_id, Msg::OpResult(result)).await;
                });
            }
        }
    }
}

/// Answers a request whose reply arrives when the coordinator (or the other user) does.
async fn spawn_op_result(
    request_id: u64,
    out: mpsc::Sender<Outgoing>,
    wait: oneshot::Receiver<pairing::Verdict>,
) {
    tokio::spawn(async move {
        let result = match wait.await {
            Ok(Ok(())) => ok(),
            Ok(Err((code, _detail))) => ipc::OpResult {
                ok: false,
                error_code: code,
            },
            Err(_) => ipc::OpResult {
                ok: false,
                error_code: "PAIRING_GONE".to_owned(),
            },
        };
        answer(&out, request_id, Msg::OpResult(result)).await;
    });
}

/// Sends one frame, bounded so a wedged GUI cannot stall the loop.
async fn answer(out: &mpsc::Sender<Outgoing>, request_id: u64, message: Msg) {
    match tokio::time::timeout(ANSWER_TIMEOUT, out.send((request_id, message))).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => tracing::debug!("a GUI connection went away before its answer"),
        Err(_) => tracing::warn!("a GUI did not take its answer; dropping it"),
    }
}

/// The focus state in the words `docs/ipc.proto` fixes for `Status.focus` («LOCAL / PREPARING
/// / REMOTE / BLOCKED»).
///
/// The FSM's own `as_str` is the log's spelling (`Local`), and the control line has always
/// printed that; the GUI's contract is the uppercase enum, so the translation lives here
/// instead of changing the words a log reader is used to.
fn focus_word(state: &str) -> &'static str {
    match state {
        "Local" => "LOCAL",
        "Preparing" => "PREPARING",
        "Remote" => "REMOTE",
        "Releasing" => "RELEASING",
        "Blocked" => "BLOCKED",
        // A state this build does not know is not invented: the empty string is the contract's
        // "nothing to say", and the GUI shows no claim rather than a wrong one.
        _ => "",
    }
}

/// The success of an operation that has nothing else to report.
fn ok() -> ipc::OpResult {
    ipc::OpResult {
        ok: true,
        error_code: String::new(),
    }
}

/// The refusal of a request whose arguments cannot be read.
fn bad_request() -> ipc::OpResult {
    ipc::OpResult {
        ok: false,
        error_code: "BAD_REQUEST".to_owned(),
    }
}

/// A `ctl::Response` in `GuiFrame` terms: the code is the only thing a GUI acts on.
fn op_result(response: &ctl::Response) -> ipc::OpResult {
    match response {
        ctl::Response::Error { code, .. } => ipc::OpResult {
            ok: false,
            error_code: code.clone(),
        },
        _ => ok(),
    }
}

/// The `(revision, author)` base an `UpdateTopology` names.
///
/// An empty author is [`crate::layout::Pair::NONE`]: the document the GUI edited was the
/// empty first draft, which is what a fresh workspace starts from.
fn layout_base(revision: u64, author: &[u8]) -> crate::layout::Pair {
    match <[u8; 32]>::try_from(author) {
        Ok(bytes) => crate::layout::Pair::new(revision, geometry::NodeId::new(bytes)),
        Err(_) => crate::layout::Pair::NONE,
    }
}

/// One display of a member, as the editor sees it.
fn display_view(display: &protocol::wire::Display) -> ipc::DisplayView {
    let bounds = display.logical_bounds.unwrap_or_default();
    ipc::DisplayView {
        id: display.id.clone(),
        logical_x: bounds.x,
        logical_y: bounds.y,
        logical_w: bounds.width,
        logical_h: bounds.height,
        rotation_degrees: display.rotation_degrees,
        primary: display.primary,
        native_known: display.native_known,
        // `scale_x` is «positive finite logical -> native scale»; a zero would divide by zero in
        // the editor, so an unknown scale is 1.
        scale: if display.scale_x > 0.0 {
            display.scale_x
        } else {
            1.0
        },
    }
}

/// One link endpoint, as the editor sees it.
fn endpoint_view(endpoint: &protocol::wire::Endpoint) -> ipc::EndpointView {
    ipc::EndpointView {
        node_id: hex(&endpoint.node_id),
        display_id: endpoint.display_id.clone(),
        edge: match protocol::wire::Edge::try_from(endpoint.edge) {
            Ok(protocol::wire::Edge::Left) => "LEFT",
            Ok(protocol::wire::Edge::Right) => "RIGHT",
            Ok(protocol::wire::Edge::Top) => "TOP",
            Ok(protocol::wire::Edge::Bottom) => "BOTTOM",
            _ => "",
        }
        .to_owned(),
        segment_start: endpoint.segment_start,
        segment_end: endpoint.segment_end,
    }
}

/// Lower-case hex of a wire node id.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// The `NodeId` a 32-byte wire id names, when it is one.
fn node_id_of(bytes: &[u8]) -> Option<geometry::NodeId> {
    <[u8; 32]>::try_from(bytes).ok().map(geometry::NodeId::new)
}

/// How long an accepted `pair/1` connection may take to deliver its `PairHello`. The window
/// itself lasts two minutes; a dialer that completes TLS and then goes silent would otherwise
/// hold one of the three pending slots, a task and a socket until the window closes.
const OPENER_HELLO_TIMEOUT: Duration = Duration::from_secs(15);

fn opener_hello_timeout(window: &identity::PairingWindow, now_ms: u64) -> Duration {
    OPENER_HELLO_TIMEOUT.min(Duration::from_millis(
        window.deadline_ms().saturating_sub(now_ms),
    ))
}

/// `future` under `limit`; a timeout is an error like any other failure of the ceremony.
async fn within<T, E: std::fmt::Display>(
    limit: Duration,
    future: impl Future<Output = Result<T, E>>,
) -> Result<T, String> {
    match tokio::time::timeout(limit, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => {
            Err("the other computer did not complete the pairing handshake in time".to_owned())
        }
    }
}

#[cfg(test)]
mod opener_tests {
    use super::*;

    #[test]
    fn handshake_deadline_never_outlives_its_pairing_window() {
        let window = identity::PairingWindow::open(0, [1; 16]);
        assert_eq!(opener_hello_timeout(&window, 0), OPENER_HELLO_TIMEOUT);
        assert_eq!(
            opener_hello_timeout(&window, window.deadline_ms() - 1),
            Duration::from_millis(1)
        );
        assert_eq!(
            opener_hello_timeout(&window, window.deadline_ms()),
            Duration::ZERO
        );
    }

    /// Review R5': a silent dialer must fail the ceremony instead of holding its slot.
    #[tokio::test]
    async fn a_silent_dialer_times_out_instead_of_holding_its_slot() {
        let error = within::<(), String>(
            Duration::from_millis(50),
            std::future::pending::<Result<(), String>>(),
        )
        .await
        .expect_err("a handshake that never finishes is a failure");
        assert!(error.contains("in time"), "{error}");
        assert_eq!(
            within::<u8, String>(Duration::from_secs(1), async { Ok(7) }).await,
            Ok(7)
        );
    }
}
