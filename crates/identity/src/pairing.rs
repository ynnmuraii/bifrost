//! The pairing ceremony: a bounded, sans-IO state machine (KVM-030).
//!
//! Spec: `docs/05-protocol.md`, «Сопряжение», with the state diagram that this module
//! implements literally:
//!
//! ```text
//! Off --> Listening: user opens the pairing window, 120 s
//! Off --> Dialing: user enters the other node address
//! Dialing --> Handshaking
//! Listening --> Handshaking: TLS with pair ALPN, max 3 pending
//! Handshaking --> Comparing: PairHello both ways, node_id equals SPKI hash
//! Comparing --> LocalApproved: user confirms code on this device
//! Comparing --> Rejected: user rejects or mismatch
//! LocalApproved --> Committed: peer PairDecision approved
//! LocalApproved --> Rejected: peer rejected
//! Comparing --> Expired: 120 s
//! LocalApproved --> Expired: 120 s
//! Committed --> [*]: write TrustedPeer atomically, close channel
//! Rejected --> [*]: audit only
//! Expired --> [*]: audit only
//! ```
//!
//! Constants, quoted:
//!
//! * window «120 s» — [`WINDOW_MS`];
//! * «не более 3 ожидающих peers, rate limit handshakes» — [`MAX_PENDING_PEERS`] names the
//!   concurrent part of the budget;
//! * «при лимите 3 попытки на 120 s» — [`MAX_ATTEMPTS`] caps the whole budget
//!   (`pending + attempts`), so the window never admits more than three handshakes;
//! * «Пока ключ неизвестен, allowlist разрешает только `PairHello` и статус» — the only
//!   outputs a session in `Handshaking`/`Comparing` can produce are `PairHello` and
//!   `PairDecision`, so a pairing connection can never carry runtime payload.
//!
//! No I/O and no clock reads: every input carries `now_ms`, and the attempt id / addresses
//! are injected. The async driver in [`crate::drive`] owns the sockets and the exporter.

use geometry::NodeId;

use crate::sas::SasCode;
use crate::spki::fingerprint_grouped;

/// The pairing window: «Пользователь открывает окно сопряжения на узле-сервере на 120 s».
pub const WINDOW_MS: u64 = 120_000;

/// Concurrent pending peers: «не более 3 ожидающих peers».
pub const MAX_PENDING_PEERS: u32 = 3;

/// Comparison attempts inside one window: «при лимите 3 попытки на 120 s».
pub const MAX_ATTEMPTS: u32 = 3;

/// `attempt_id`: random bytes16 from the window opener, carried by every
/// `PairingEnvelope` and used as the TLS exporter context.
pub type AttemptId = [u8; 16];

/// Which end of the ceremony this session is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingRole {
    /// The node that opened the pairing window: the TLS server, and it sends `PairHello`
    /// first («узел, открывший окно сопряжения, — TLS-сервер и отправляет `PairHello`
    /// первым»).
    WindowOpener,
    /// The node that dialled the address.
    Dialer,
}

impl PairingRole {
    /// The name used in logs and in the GUI.
    pub const fn as_str(self) -> &'static str {
        match self {
            PairingRole::WindowOpener => "window-opener",
            PairingRole::Dialer => "dialer",
        }
    }
}

/// The ceremony states of the diagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PairingState {
    /// Nothing started.
    Off,
    /// The window is open, waiting for a dialer.
    Listening,
    /// We dialled an address.
    Dialing,
    /// TLS is up; `PairHello` is being exchanged.
    Handshaking,
    /// Both `PairHello`s matched; the users compare the code.
    Comparing,
    /// This device's user confirmed the code.
    LocalApproved,
    /// The trust record was committed on both sides.
    Committed,
    /// The ceremony failed.
    Rejected,
    /// The 120 s window ran out.
    Expired,
}

impl PairingState {
    /// The name used in logs and in the GUI.
    pub const fn as_str(self) -> &'static str {
        match self {
            PairingState::Off => "Off",
            PairingState::Listening => "Listening",
            PairingState::Dialing => "Dialing",
            PairingState::Handshaking => "Handshaking",
            PairingState::Comparing => "Comparing",
            PairingState::LocalApproved => "LocalApproved",
            PairingState::Committed => "Committed",
            PairingState::Rejected => "Rejected",
            PairingState::Expired => "Expired",
        }
    }

    /// A terminal state.
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            PairingState::Committed | PairingState::Rejected | PairingState::Expired
        )
    }
}

/// Why a ceremony was rejected. The names match the `PAIRING_AUDIT.outcome` values of
/// `docs/03-architecture.md` where they overlap (`MISMATCH`, `REJECTED`, `EXPIRED`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionReason {
    /// `PairHello.node_id` did not match the SPKI hash of the presented certificate.
    NodeIdMismatch,
    /// The remote `attempt_id` was not the one this window opened.
    AttemptMismatch,
    /// The peer does not support the comparison code, so it cannot be authenticated.
    SasUnsupported,
    /// The other side rejected the code (or rejected us).
    PeerRejected,
    /// This device's user rejected the code.
    UserRejected,
    /// Too many attempts or too many pending peers for this window.
    TooManyAttempts,
    /// The window ran out.
    Expired,
    /// The ceremony was aborted locally.
    Aborted,
}

impl RejectionReason {
    /// The `PAIRING_AUDIT.outcome`-style name.
    pub const fn as_str(self) -> &'static str {
        match self {
            RejectionReason::NodeIdMismatch => "MISMATCH",
            RejectionReason::AttemptMismatch => "MISMATCH",
            RejectionReason::SasUnsupported => "REJECTED",
            RejectionReason::PeerRejected => "REJECTED",
            RejectionReason::UserRejected => "REJECTED",
            RejectionReason::TooManyAttempts => "REJECTED",
            RejectionReason::Expired => "EXPIRED",
            RejectionReason::Aborted => "REJECTED",
        }
    }
}

/// Everything that can happen to a ceremony.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingInput {
    /// The user opened the pairing window (window opener only).
    WindowOpened {
        /// Monotonic milliseconds.
        now_ms: u64,
    },
    /// The user dialled an address (dialer only).
    DialStarted {
        /// Monotonic milliseconds.
        now_ms: u64,
    },
    /// A TLS connection for this session was admitted (window opener only), subject to
    /// [`MAX_PENDING_PEERS`].
    PeerAdmitted,
    /// The TLS handshake finished; `peer_node_id` comes from the presented certificate.
    HandshakeEstablished {
        /// `SHA-256(SPKI DER)` of the peer's certificate.
        peer_node_id: NodeId,
    },
    /// The comparison code computed from the TLS exporter for this session.
    AttemptMaterial {
        /// The 6-digit code.
        sas: SasCode,
    },
    /// A `PairHello` arrived.
    RemoteHello {
        /// The `attempt_id` from the envelope (the dialer adopts it).
        attempt_id: AttemptId,
        /// `PairHello.node_id`.
        node_id: NodeId,
        /// `PairHello.display_name`.
        display_name: String,
        /// `PairHello.supports_sas`.
        supports_sas: bool,
    },
    /// A `PairDecision` arrived.
    RemoteDecision {
        /// `PairDecision.approved`.
        approved: bool,
        /// `PairDecision.peer_node_id`.
        peer_node_id: NodeId,
    },
    /// This device's user answered.
    LocalDecision {
        /// `true` to commit the trust record.
        approved: bool,
    },
    /// Time passed; the 120 s window is checked here.
    Tick {
        /// Monotonic milliseconds.
        now_ms: u64,
    },
    /// The local side gives up (user cancelled, connection failed).
    Abort,
}

/// What the ceremony wants the caller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingOutput {
    /// Send `PairHello` (the window opener sends it first).
    SendHello {
        /// `PairHello.node_id`.
        node_id: NodeId,
        /// `PairHello.display_name`.
        display_name: String,
        /// `PairHello.supports_sas`: always `true` here, because the code is the
        /// authentication step.
        supports_sas: bool,
    },
    /// Send `PairDecision`.
    SendDecision {
        /// `PairDecision.approved`.
        approved: bool,
        /// `PairDecision.peer_node_id`.
        peer_node_id: NodeId,
    },
    /// Show the comparison code to the local user (and the full fingerprint behind a
    /// "Подробнее" button).
    ShowSas {
        /// The 6-digit code from the TLS exporter.
        sas: SasCode,
        /// The peer identity the code authenticates.
        peer_node_id: NodeId,
    },
    /// Commit the trust record **atomically**; the caller owns the file write and the
    /// audit entry.
    Commit {
        /// The peer identity to store.
        peer_node_id: NodeId,
        /// `Hello.display_name` of the peer.
        display_name: String,
        /// The address used for this ceremony, for `TRUSTED_PEER.last_address`.
        address: Option<String>,
    },
    /// The ceremony failed; nothing is written.
    Rejected {
        /// Why.
        reason: RejectionReason,
    },
    /// The 120 s window ran out.
    Expired,
}

/// The terminal outcome, for the audit log (`PAIRING_AUDIT.outcome`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingOutcome {
    /// Still running.
    InProgress,
    /// Trust established.
    Committed,
    /// Failed with this reason.
    Rejected(RejectionReason),
    /// The window ran out.
    Expired,
}

impl PairingOutcome {
    /// The `PAIRING_AUDIT.outcome`-style name.
    pub const fn as_str(self) -> &'static str {
        match self {
            PairingOutcome::InProgress => "IN_PROGRESS",
            PairingOutcome::Committed => "COMMITTED",
            PairingOutcome::Rejected(reason) => reason.as_str(),
            PairingOutcome::Expired => "EXPIRED",
        }
    }
}

/// One pairing ceremony.
#[derive(Debug, Clone)]
pub struct PairingFsm {
    role: PairingRole,
    local_node_id: NodeId,
    local_display_name: String,
    state: PairingState,
    attempt_id: AttemptId,
    address: Option<String>,
    deadline_ms: Option<u64>,
    peer_node_id: Option<NodeId>,
    peer_display_name: Option<String>,
    peer_supports_sas: bool,
    sas: Option<SasCode>,
    local_decision: Option<bool>,
    remote_decision: Option<bool>,
    rejection: Option<RejectionReason>,
    hello_sent: bool,
}

impl PairingFsm {
    /// A window opener's session (TLS server; sends `PairHello` first).
    pub fn window_opener(
        local_node_id: NodeId,
        local_display_name: impl Into<String>,
        attempt_id: AttemptId,
    ) -> Self {
        Self::new(
            PairingRole::WindowOpener,
            local_node_id,
            local_display_name,
            attempt_id,
        )
    }

    /// A dialer's session.
    ///
    /// The `attempt_id` is a placeholder until the window opener's `PairHello` arrives:
    /// the dialer cannot know it before the handshake, which is exactly why the code is
    /// computed only after `RemoteHello`.
    pub fn dialer(local_node_id: NodeId, local_display_name: impl Into<String>) -> Self {
        Self::new(
            PairingRole::Dialer,
            local_node_id,
            local_display_name,
            [0u8; 16],
        )
    }

    fn new(
        role: PairingRole,
        local_node_id: NodeId,
        local_display_name: impl Into<String>,
        attempt_id: AttemptId,
    ) -> Self {
        Self {
            role,
            local_node_id,
            local_display_name: local_display_name.into(),
            state: PairingState::Off,
            attempt_id,
            address: None,
            deadline_ms: None,
            peer_node_id: None,
            peer_display_name: None,
            peer_supports_sas: false,
            sas: None,
            local_decision: None,
            remote_decision: None,
            rejection: None,
            hello_sent: false,
        }
    }

    /// The ceremony state.
    pub const fn state(&self) -> PairingState {
        self.state
    }

    /// The `attempt_id` (the dialer's own until the window opener's hello arrives).
    pub const fn attempt_id(&self) -> &AttemptId {
        &self.attempt_id
    }

    /// The role of this session.
    pub const fn role(&self) -> PairingRole {
        self.role
    }

    /// The peer identity, once known.
    pub const fn peer_node_id(&self) -> Option<&NodeId> {
        self.peer_node_id.as_ref()
    }

    /// The comparison code, once computed.
    pub const fn sas(&self) -> Option<SasCode> {
        self.sas.as_ref().copied()
    }

    /// The terminal outcome.
    pub fn outcome(&self) -> PairingOutcome {
        match self.state {
            PairingState::Committed => PairingOutcome::Committed,
            PairingState::Expired => PairingOutcome::Expired,
            PairingState::Rejected => {
                PairingOutcome::Rejected(self.rejection.unwrap_or(RejectionReason::Aborted))
            }
            _ => PairingOutcome::InProgress,
        }
    }

    /// Why the ceremony failed, when it failed.
    pub const fn rejection(&self) -> Option<RejectionReason> {
        self.rejection
    }

    /// Processes one input.
    pub fn handle(&mut self, input: PairingInput) -> Vec<PairingOutput> {
        if self.state.is_terminal() {
            return Vec::new();
        }
        // The window is the deadline for every non-terminal state: «Comparing --> Expired:
        // 120 s», «LocalApproved --> Expired: 120 s».
        if let PairingInput::Tick { now_ms } = input {
            if self.deadline_ms.is_some_and(|deadline| now_ms >= deadline) {
                return self.expire();
            }
            return Vec::new();
        }
        match input {
            PairingInput::WindowOpened { now_ms } => self.on_window_opened(now_ms),
            PairingInput::DialStarted { now_ms } => self.on_dial_started(now_ms),
            PairingInput::PeerAdmitted => self.on_peer_admitted(),
            PairingInput::HandshakeEstablished { peer_node_id } => {
                self.on_handshake_established(peer_node_id)
            }
            PairingInput::AttemptMaterial { sas } => self.on_attempt_material(sas),
            PairingInput::RemoteHello {
                attempt_id,
                node_id,
                display_name,
                supports_sas,
            } => self.on_remote_hello(attempt_id, node_id, display_name, supports_sas),
            PairingInput::RemoteDecision {
                approved,
                peer_node_id,
            } => self.on_remote_decision(approved, peer_node_id),
            PairingInput::LocalDecision { approved } => self.on_local_decision(approved),
            PairingInput::Abort => self.reject(RejectionReason::Aborted),
            PairingInput::Tick { .. } => unreachable!("handled above"),
        }
    }

    fn on_window_opened(&mut self, now_ms: u64) -> Vec<PairingOutput> {
        if self.role != PairingRole::WindowOpener || self.state != PairingState::Off {
            return Vec::new();
        }
        self.deadline_ms = Some(now_ms.saturating_add(WINDOW_MS));
        self.state = PairingState::Listening;
        Vec::new()
    }

    fn on_dial_started(&mut self, now_ms: u64) -> Vec<PairingOutput> {
        if self.role != PairingRole::Dialer || self.state != PairingState::Off {
            return Vec::new();
        }
        self.deadline_ms = Some(now_ms.saturating_add(WINDOW_MS));
        self.state = PairingState::Dialing;
        Vec::new()
    }

    fn on_peer_admitted(&mut self) -> Vec<PairingOutput> {
        if self.state != PairingState::Listening {
            return Vec::new();
        }
        self.state = PairingState::Handshaking;
        Vec::new()
    }

    fn on_handshake_established(&mut self, peer_node_id: NodeId) -> Vec<PairingOutput> {
        if !matches!(
            self.state,
            PairingState::Handshaking | PairingState::Dialing
        ) {
            return Vec::new();
        }
        self.peer_node_id = Some(peer_node_id);
        self.state = PairingState::Handshaking;
        // The window opener sends PairHello first; the dialer replies with its own after
        // it has seen the opener's attempt id.
        if self.role == PairingRole::WindowOpener {
            return vec![self.hello_output()];
        }
        Vec::new()
    }

    fn on_attempt_material(&mut self, sas: SasCode) -> Vec<PairingOutput> {
        if self.state != PairingState::Handshaking {
            return Vec::new();
        }
        self.sas = Some(sas);
        self.maybe_compare()
    }

    fn on_remote_hello(
        &mut self,
        attempt_id: AttemptId,
        node_id: NodeId,
        display_name: String,
        supports_sas: bool,
    ) -> Vec<PairingOutput> {
        if self.state != PairingState::Handshaking {
            return Vec::new();
        }
        // The identity check of the diagram: «PairHello both ways, node_id equals SPKI
        // hash». The SPKI-derived id came from the handshake, so an attacker cannot claim
        // someone else's node_id.
        let Some(expected) = self.peer_node_id else {
            return self.reject(RejectionReason::NodeIdMismatch);
        };
        if node_id != expected {
            return self.reject(RejectionReason::NodeIdMismatch);
        }
        // The attempt id binds the exporter context to this window.
        match self.role {
            PairingRole::WindowOpener => {
                if attempt_id != self.attempt_id {
                    return self.reject(RejectionReason::AttemptMismatch);
                }
            }
            PairingRole::Dialer => {
                self.attempt_id = attempt_id;
            }
        }
        self.peer_display_name = Some(display_name);
        self.peer_supports_sas = supports_sas;
        let mut outputs = Vec::new();
        if !supports_sas {
            // The 6-digit code is the only authentication in this ceremony; a peer that
            // cannot compute it is not pairable.
            outputs.extend(self.reject(RejectionReason::SasUnsupported));
            return outputs;
        }
        if self.role == PairingRole::Dialer {
            outputs.push(self.hello_output());
        }
        outputs.extend(self.maybe_compare());
        outputs
    }

    fn on_remote_decision(&mut self, approved: bool, peer_node_id: NodeId) -> Vec<PairingOutput> {
        // **Resolved ambiguity**: `protocol.proto` does not say whether
        // `PairDecision.peer_node_id` names the sender or the addressee. This treats it as
        // the *addressee* (`self.local_node_id`), so a decision is only accepted when it
        // is addressed to us, and this side sends the peer's id in the same field.
        if !matches!(
            self.state,
            PairingState::Comparing | PairingState::LocalApproved
        ) {
            return Vec::new();
        }
        if peer_node_id != self.local_node_id {
            return self.reject(RejectionReason::NodeIdMismatch);
        }
        self.remote_decision = Some(approved);
        if !approved {
            return self.reject(RejectionReason::PeerRejected);
        }
        if self.local_decision == Some(true) {
            self.state = PairingState::LocalApproved;
            return self.commit();
        }
        Vec::new()
    }

    fn on_local_decision(&mut self, approved: bool) -> Vec<PairingOutput> {
        if self.state != PairingState::Comparing {
            return Vec::new();
        }
        self.local_decision = Some(approved);
        if !approved {
            let mut outputs = self.reject(RejectionReason::UserRejected);
            if let Some(peer) = self.peer_node_id {
                outputs.push(PairingOutput::SendDecision {
                    approved: false,
                    peer_node_id: peer,
                });
            }
            return outputs;
        }
        self.state = PairingState::LocalApproved;
        let decisions = match self.peer_node_id {
            Some(peer) => vec![PairingOutput::SendDecision {
                approved: true,
                peer_node_id: peer,
            }],
            None => Vec::new(),
        };
        // A peer that already approved (possible when the dialer answered first) commits
        // the moment we do.
        if self.remote_decision == Some(true) {
            let mut outputs = decisions;
            outputs.extend(self.commit());
            return outputs;
        }
        decisions
    }

    /// Both `PairHello`s matched and a code is available: show it.
    fn maybe_compare(&mut self) -> Vec<PairingOutput> {
        if self.state != PairingState::Handshaking {
            return Vec::new();
        }
        if self.peer_display_name.is_none() || !self.peer_supports_sas {
            return Vec::new();
        }
        let (Some(sas), Some(peer_node_id)) = (self.sas, self.peer_node_id) else {
            return Vec::new();
        };
        self.state = PairingState::Comparing;
        vec![PairingOutput::ShowSas { sas, peer_node_id }]
    }

    fn commit(&mut self) -> Vec<PairingOutput> {
        let (Some(peer_node_id), Some(display_name)) =
            (self.peer_node_id, self.peer_display_name.clone())
        else {
            return self.reject(RejectionReason::Aborted);
        };
        self.state = PairingState::Committed;
        vec![PairingOutput::Commit {
            peer_node_id,
            display_name,
            address: self.address.clone(),
        }]
    }

    fn reject(&mut self, reason: RejectionReason) -> Vec<PairingOutput> {
        self.state = PairingState::Rejected;
        self.rejection = Some(reason);
        vec![PairingOutput::Rejected { reason }]
    }

    fn expire(&mut self) -> Vec<PairingOutput> {
        self.state = PairingState::Expired;
        self.rejection = Some(RejectionReason::Expired);
        vec![PairingOutput::Expired]
    }

    fn hello_output(&mut self) -> PairingOutput {
        self.hello_sent = true;
        PairingOutput::SendHello {
            node_id: self.local_node_id,
            display_name: self.local_display_name.clone(),
            supports_sas: true,
        }
    }

    /// Records the address used for this ceremony (goes into `last_address`).
    pub fn set_address(&mut self, address: impl Into<String>) {
        self.address = Some(address.into());
    }

    /// Whether `PairHello` was already sent.
    pub const fn hello_sent(&self) -> bool {
        self.hello_sent
    }

    /// The peer's display name, once its hello arrived.
    pub fn peer_display_name(&self) -> Option<&str> {
        self.peer_display_name.as_deref()
    }

    /// The peer's fingerprint for the "Подробнее" view.
    pub fn peer_fingerprint(&self) -> Option<String> {
        self.peer_node_id
            .map(|node_id| fingerprint_grouped(&node_id))
    }
}

/// The window opener's admission control for one 120 s window.
///
/// The diagram's «max 3 pending», «rate limit handshakes» and «3 попытки на 120 s» live
/// here rather than in [`PairingFsm`], because they are properties of the window, not of
/// one connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingWindow {
    attempt_id: AttemptId,
    opened_ms: u64,
    deadline_ms: u64,
    pending: u32,
    attempts: u32,
    requester: Option<NodeId>,
}

impl PairingWindow {
    /// Opens a window with a fresh `attempt_id` (the window opener generates it).
    pub fn open(now_ms: u64, attempt_id: AttemptId) -> Self {
        Self {
            attempt_id,
            opened_ms: now_ms,
            deadline_ms: now_ms.saturating_add(WINDOW_MS),
            pending: 0,
            attempts: 0,
            requester: None,
        }
    }

    /// Restricts this window to the certificate identity that requested it.
    pub fn restrict_to(&mut self, requester: NodeId) {
        self.requester = Some(requester);
    }

    /// The only admissible TLS peer, or `None` for a manually opened window.
    pub const fn requester(&self) -> Option<NodeId> {
        self.requester
    }

    /// The `attempt_id` every `PairingEnvelope` of this window carries.
    pub const fn attempt_id(&self) -> &AttemptId {
        &self.attempt_id
    }

    /// When the window was opened.
    pub const fn opened_ms(&self) -> u64 {
        self.opened_ms
    }

    /// When the window closes.
    pub const fn deadline_ms(&self) -> u64 {
        self.deadline_ms
    }

    /// Whether the window is still usable at `now_ms`.
    pub const fn is_open(&self, now_ms: u64) -> bool {
        now_ms < self.deadline_ms && self.attempts < MAX_ATTEMPTS
    }

    /// Pending connections.
    pub const fn pending(&self) -> u32 {
        self.pending
    }

    /// Concluded ceremonies in this window.
    pub const fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Admits a dialer, enforcing «не более 3 ожидающих peers, rate limit handshakes».
    ///
    /// The two quoted limits are one budget: every admitted handshake occupies a slot until
    /// it concludes (`finish`) or is abandoned (`release_pending`), and «3 попытки на 120 s»
    /// caps how many handshakes the window ever admits — `pending + attempts` — so a peer
    /// cannot keep opening new connections while earlier ones conclude.
    pub fn admit(&mut self, now_ms: u64) -> Result<(), RejectionReason> {
        if !self.is_open(now_ms) {
            return Err(RejectionReason::Expired);
        }
        if self.pending + self.attempts >= MAX_ATTEMPTS {
            return Err(RejectionReason::TooManyAttempts);
        }
        self.pending += 1;
        Ok(())
    }

    /// Records that a ceremony concluded (`attempts` counts against the 3-attempt limit).
    pub fn finish(&mut self) {
        self.pending = self.pending.saturating_sub(1);
        self.attempts = self.attempts.saturating_add(1);
    }

    /// Releases a slot whose handshake never became a ceremony (a failed TLS accept, a
    /// dropped connection): the handshake is not an attempt, so only `pending` moves.
    pub fn release_pending(&mut self) {
        self.pending = self.pending.saturating_sub(1);
    }
}
