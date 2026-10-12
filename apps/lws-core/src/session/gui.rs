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

use std::sync::Arc;
use std::time::Duration;

use focus_router as fr;
use protocol::ipc::{self as ipc, gui_frame::Msg};
use tokio::sync::{mpsc, oneshot};

use super::{ctl, pairing, Handled, Session, LAYOUT_VERDICT_TIMEOUT};
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
        let members = self
            .manager
            .members()
            .await
            .into_iter()
            .map(|node| ipc::MemberStatus {
                node_id: crate::command::node_id_hex(&node),
                display_name: self
                    .trust
                    .get(&node)
                    .map(|record| record.display_name.clone())
                    .unwrap_or_default(),
                // `members()` is the set of *established* links, so a member it names is
                // online by construction; `link_state` is the protocol's own progression and a
                // live link is past it.
                online: true,
                is_owner: owner == Some(node),
                link_state: "READY".to_owned(),
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
            connected: self.manager.member_count().await > 0,
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
            agent_restart_circuit_open: self.agent_restart_circuit_open,
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
    pub(crate) fn gui_topology_doc(&self) -> ipc::TopologyDoc {
        let applied = self.layout_state.applied();
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
        ipc::TopologyDoc {
            revision: applied.revision,
            author_node_id: applied
                .author
                .map(|author| crate::command::node_id_hex(&author))
                .unwrap_or_default(),
            members: document
                .nodes
                .iter()
                .map(|node| ipc::MemberTopology {
                    node_id: hex(&node.node_id),
                    display_name: node_id_of(&node.node_id)
                        .and_then(|node| self.trust.get(&node))
                        .map(|record| record.display_name.clone())
                        .unwrap_or_default(),
                    online: node.online,
                    displays: node.displays.iter().map(display_view).collect(),
                })
                .collect(),
            links: document
                .links
                .iter()
                .enumerate()
                .filter_map(|(index, link)| {
                    let from = link.source_endpoint.as_ref()?;
                    let to = link.target_endpoint.as_ref()?;
                    Some(ipc::LinkView {
                        // The protocol's `TransitionLink` has no id: the document's own order
                        // identifies one, and it is stable for as long as that revision is.
                        id: format!("link-{index}"),
                        from: Some(endpoint_view(from)),
                        to: Some(endpoint_view(to)),
                        // A link's reverse direction is resolved by the same document
                        // (`geometry::resolve_motion`), which is what the editor shows.
                        bidirectional: true,
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
            message,
            out,
        } = request;
        match message {
            Msg::GetStatus(_) => {
                let status = self.gui_status().await;
                self.gui_status = Some(status.clone());
                answer(&out, request_id, Msg::Status(status)).await;
            }
            Msg::GetTopology(_) => {
                let doc = self.gui_topology_doc();
                self.gui_revision = Some(doc.revision);
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
                let handled = self.layout_edit_document(text, expected).await;
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
                let handled = self.layout_apply().await;
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
                let attempt: pairing::AttemptId = match peer_answer.attempt_id.as_slice().try_into()
                {
                    Ok(attempt) => attempt,
                    Err(_) => return answer(&out, request_id, Msg::OpResult(bad_request())).await,
                };
                match self.pairing.answer(&attempt, peer_answer.approve) {
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
        let revision = self.layout_state.applied().revision;
        if self.gui_revision != Some(revision) {
            self.gui_revision = Some(revision);
            self.publish(Msg::TopologyDoc(self.gui_topology_doc()));
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
                    attempt_id: prompt.attempt_id.to_vec(),
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
                        self.trust.trust(record);
                        self.pairing_error = None;
                        // The verdict is sent only after this: a window told "paired" can rely
                        // on the record being readable, and a write that failed is reported
                        // instead of being hidden behind a success.
                        match self.trust.save(&self.paths.trust_file()) {
                            Ok(_) => Ok(()),
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
        let parsed: std::net::SocketAddr = match address.parse() {
            Ok(address) => address,
            Err(error) => {
                self.pairing
                    .dial_failed("BAD_ADDRESS", format!("{address:?}: {error}"));
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
        let answer = self.pairing.reserve();
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
                    pairing::run(driver, answer, false, local_fingerprint, events).await;
                }
                Err(error) => {
                    // No ceremony exists yet, so this is the dial's own failure: the loop
                    // resolves the `PairWith` reply with it.
                    let _ = events
                        .send(pairing::Event::DialFailed {
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
        // The window opener generated the `attempt_id`, so its ceremony is registered before
        // the task starts and `PairingAnswer` can name it at once.
        let attempt_id = *connection.window.attempt_id();
        let answer = self.pairing.begin(attempt_id);
        let identity = Arc::clone(&self.identity);
        let events = self.pairing_events_tx.clone();
        let local_fingerprint = self.local_fingerprint();
        let display_name = self.display_name.clone();
        let listen_port = self.listen.port();
        let window = connection.window.clone();
        let address = connection.address.to_string();
        tokio::spawn(async move {
            match identity::accept_stream(
                connection.stream,
                &identity,
                &window,
                Some(address),
                listen_port,
                &display_name,
            )
            .await
            {
                Ok(driver) => {
                    pairing::run(driver, answer, true, local_fingerprint, events).await;
                }
                Err(error) => {
                    let _ = events
                        .send(pairing::Event::Finished {
                            attempt_id,
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

    /// `RevokePeer`: forget the peer and close the links to it.
    ///
    /// The link manager keeps its own copy of the trust store, so a revoked peer's link would
    /// otherwise stay open until it reconnects: the links are closed here instead. With three
    /// nodes that is heavier than it needs to be (a per-peer close is the next step); with the
    /// two the MVP supports it is exactly right.
    async fn revoke_peer(&mut self, peer: &[u8]) -> ipc::OpResult {
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
        tracing::info!(
            peer = crate::command::node_id_hex(&node).as_str(),
            "peer revoked"
        );
        self.manager.kill_all().await;
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
