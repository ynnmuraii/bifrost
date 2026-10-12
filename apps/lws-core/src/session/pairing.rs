//! The pairing window and the ceremonies it serves, inside the session loop.
//!
//! `docs/05-protocol.md`, «Сопряжение»: the window is opened by the user for at most 120 s,
//! the listener that serves it is the node's own input/control listener (ALPN
//! `lan-workspace/pair/1`, see [`transport::PairingGate`]), and both sides must confirm the
//! 6-digit code before a `TrustedPeer` is written. This module is the application half: it
//! owns the window, runs each ceremony in its own task so a peer that never answers cannot
//! stall the loop, and reports prompts and outcomes to whoever asked — the GUI
//! (`GuiFrame`) or the CLI's control line, which both drive the same methods here.
//!
//! # Why the ceremony runs in a task
//!
//! `PairingDriver::approve` waits for the *other* side's `PairDecision`, and a 120 s window
//! is far too long to hold a `select!` arm. The task owns the driver and reports back over
//! [`Event`]; the loop keeps serving input, links and the GUI meanwhile. What the loop
//! holds is only the state the user's two answers need.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use identity::{PairedPeer, PairingDriver};
use tokio::sync::{mpsc, oneshot};

/// The `attempt_id` of one ceremony: random bytes16 from the window opener, the exporter
/// context of the code both users compare.
pub type AttemptId = [u8; 16];

/// How long a ceremony may wait for the other side once the local user has answered.
///
/// The window is 120 s (`docs/05-protocol.md`: «окно 120 s»), and past it the ceremony is over
/// whatever the peer does. Without this a peer that approves locally and then vanishes would
/// leave the task — and the caller whose answer waits on it — waiting on a socket forever.
const DECISION_TIMEOUT: Duration = Duration::from_secs(120);

/// Why an operation could not even start: the code the GUI acts on and the sentence a human
/// reads.
pub type Refusal = (String, String);

/// The outcome of one operation, as the loop's two front-ends report it.
pub type Verdict = Result<(), Refusal>;

/// What the GUI shows for one ceremony.
#[derive(Debug, Clone)]
pub struct Prompt {
    /// Which ceremony; `PairingAnswer` names it back.
    pub attempt_id: AttemptId,
    /// The peer's advertised name (non-authoritative).
    pub peer_name: String,
    /// The 6 digits both users must see the same.
    pub sas_code: String,
    /// This node's own `aa:bb:…` fingerprint.
    pub local_fingerprint: String,
    /// The peer's fingerprint, for the "Подробнее" view (ADR-010).
    pub peer_fingerprint: String,
    /// How to reach the peer, when this side learned it (`network.peers` material).
    pub address: Option<String>,
}

/// How a ceremony ended.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// `TrustedPeer` written on this side and confirmed by the other.
    Paired(Box<PairedPeer>),
    /// The user rejected on either side; nothing is written («Rejected --> [*]: audit only»).
    Rejected,
    /// The ceremony failed before the user decided: TLS, protocol, an expired window.
    Failed(String),
}

/// What a ceremony task reports to the loop.
pub enum Event {
    /// A dialer learned the ceremony's `attempt_id` from the opener's first `PairHello`
    /// (`docs/05-protocol.md`): the reserved answer channel belongs to it now.
    Started {
        /// Which ceremony.
        attempt_id: AttemptId,
    },
    /// The dial never reached a ceremony (a refused connection, a TLS failure, a timeout):
    /// whoever asked for the dial is told instead of being left waiting.
    DialFailed {
        /// The code the caller acts on.
        code: String,
        /// The sentence a human reads.
        detail: String,
    },
    /// The code is known and the user must decide.
    Prompt(Box<Prompt>),
    /// The ceremony is over.
    Finished {
        /// Which ceremony; it names the prompt that led here.
        attempt_id: AttemptId,
        /// What happened.
        outcome: Outcome,
    },
}

/// The open window as the loop and the GUI see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// The id every `PairingEnvelope` of this window carries.
    pub attempt_id: AttemptId,
    /// What the other computer's user has to type: this node's listen address and port.
    pub address: String,
    /// When the window closes, on the loop's millisecond scale.
    pub deadline_ms: u64,
    /// The window length the user asked for.
    pub seconds: u32,
}

impl Window {
    /// Seconds left of `seconds` at `now_ms`.
    pub fn remaining_s(&self, now_ms: u64) -> u32 {
        u32::try_from(self.deadline_ms.saturating_sub(now_ms).div_ceil(1000)).unwrap_or(0)
    }
}

/// One ceremony the loop follows between its prompt and its outcome.
struct Ceremony {
    /// The user's decision, once somebody answers; `None` after it was sent.
    answer: Option<oneshot::Sender<bool>>,
    /// The request whose reply waits for the outcome.
    reply: Option<oneshot::Sender<Verdict>>,
    /// Where the peer is reachable, for the trust record's `last_address`.
    address: Option<String>,
}

/// Everything the loop knows about pairing.
pub struct Pairing {
    gate: transport::PairingGate,
    /// The window this node opened, until it closes.
    window: Option<Window>,
    /// One entry per ceremony between its prompt and its outcome, keyed by `attempt_id`.
    ceremonies: HashMap<AttemptId, Ceremony>,
    /// A dial that has not produced a prompt yet (`PairWith`).
    dial: Option<oneshot::Sender<Verdict>>,
    /// Who is waiting for the next prompt (the control channel's `pair-prompt`).
    waiting: Vec<oneshot::Sender<Prompt>>,
    /// Answer channels for ceremonies whose `attempt_id` is not known yet (a dialer learns it
    /// from the opener's first `PairHello`), in the order they were reserved.
    reserved: Vec<oneshot::Sender<bool>>,
}

impl Default for Pairing {
    fn default() -> Self {
        Pairing::new(transport::PairingGate::closed())
    }
}

impl Pairing {
    /// Wraps the gate the listener reads.
    pub fn new(gate: transport::PairingGate) -> Self {
        Pairing {
            gate,
            window: None,
            ceremonies: HashMap::new(),
            dial: None,
            waiting: Vec::new(),
            reserved: Vec::new(),
        }
    }

    /// The window, for the status line and the GUI's pairing screen.
    pub fn window(&self) -> Option<&Window> {
        self.window.as_ref()
    }

    /// Opens the window: a fresh `attempt_id`, the gate opened, the address to type.
    ///
    /// `window_s` is the user's choice, clamped to the documented maximum (`ipc.proto`:
    /// «max 120»); the ceremony's own limits (3 pending, 3 attempts, 120 s) live in
    /// `identity::PairingWindow`.
    pub fn open(
        &mut self,
        now_ms: u64,
        window_s: u32,
        listen: SocketAddr,
    ) -> Result<Window, (String, String)> {
        let seconds = window_s.clamp(1, 120);
        if self
            .window
            .as_ref()
            .is_some_and(|window| now_ms < window.deadline_ms)
        {
            // One window at a time: a second `StartPairing` while the first is open would
            // replace an `attempt_id` the other computer may already hold.
            return Err((
                "PAIRING_WINDOW_OPEN".to_owned(),
                "a pairing window is already open".to_owned(),
            ));
        }
        let opened = identity::open_window(now_ms)
            .map_err(|error| ("PAIRING_FAILED".to_owned(), error.to_string()))?;
        let attempt_id = *opened.attempt_id();
        self.gate.open(opened);
        let window = Window {
            attempt_id,
            address: listen.to_string(),
            deadline_ms: now_ms.saturating_add(u64::from(seconds) * 1000),
            seconds,
        };
        self.window = Some(window.clone());
        Ok(window)
    }

    /// Closes the window; returns it when one was open.
    pub fn close(&mut self) -> Option<Window> {
        let window = self.window.take()?;
        self.gate.close();
        // A ceremony of the closed window has nobody left to admit: tell each waiting
        // caller instead of leaving it until its own timeout.
        for ceremony in self.ceremonies.values_mut() {
            if let Some(answer) = ceremony.answer.take() {
                let _ = answer.send(false);
            }
        }
        Some(window)
    }

    /// Closes the window if its deadline passed. Returns it when it did.
    pub fn expire(&mut self, now_ms: u64) -> Option<Window> {
        match &self.window {
            Some(window) if now_ms >= window.deadline_ms => self.close(),
            _ => None,
        }
    }

    /// One ceremony is over: the window counts the attempt, and a spent window closes
    /// (`identity::PairingWindow::finish` — «3 попытки на 120 s»).
    pub fn finish(&self, now_ms: u64) {
        self.gate.finish(now_ms);
    }

    /// Registers a ceremony the loop will follow, and hands back the answer channel.
    ///
    /// `opener` selects which address the trust record keeps: the dialer typed the opener's
    /// address, while the opener only ever sees the dialer's source address and has to take
    /// the port from `PairHello.listen_port` ([`peer_address`]). The window opener knows its
    /// `attempt_id` (it generated it); a dialer does not, and uses [`Pairing::reserve`].
    pub fn begin(&mut self, attempt_id: AttemptId) -> oneshot::Receiver<bool> {
        let (answer, receiver) = oneshot::channel();
        self.ceremonies.insert(
            attempt_id,
            Ceremony {
                answer: Some(answer),
                reply: None,
                address: None,
            },
        );
        receiver
    }

    /// Reserves an answer channel for a ceremony whose `attempt_id` is not known yet.
    ///
    /// The dialer learns it from the opener's first `PairHello`, so the task announces it
    /// ([`Event::Started`]) and [`Pairing::register`] pairs the two. Only one dial is in
    /// flight at a time (`Pairing::dial`), so the first reserved slot is the right one.
    pub fn reserve(&mut self) -> oneshot::Receiver<bool> {
        let (answer, receiver) = oneshot::channel();
        self.reserved.push(answer);
        receiver
    }

    /// Names a reserved ceremony; the dialer's side of [`Pairing::begin`].
    pub fn register(&mut self, attempt_id: AttemptId) -> bool {
        let Some(answer) = self.reserved.pop() else {
            tracing::warn!("a ceremony announced an attempt id with nothing reserved for it");
            return false;
        };
        self.ceremonies.insert(
            attempt_id,
            Ceremony {
                answer: Some(answer),
                reply: None,
                address: None,
            },
        );
        true
    }

    /// The user's decision for one ceremony.
    ///
    /// The answer to the caller comes later — when the other side confirms — so the channel
    /// it arrives on is returned here and resolved in [`Pairing::on_event`]. An error is
    /// final: nobody is waiting for an answer this ceremony can give.
    pub fn answer(
        &mut self,
        attempt_id: &AttemptId,
        approve: bool,
    ) -> Result<oneshot::Receiver<Verdict>, Refusal> {
        let Some(ceremony) = self.ceremonies.get_mut(attempt_id) else {
            return Err((
                "UNKNOWN_ATTEMPT".to_owned(),
                "no ceremony is waiting for an answer with that attempt id".to_owned(),
            ));
        };
        let Some(answer) = ceremony.answer.take() else {
            return Err((
                "ALREADY_ANSWERED".to_owned(),
                "this ceremony was already answered".to_owned(),
            ));
        };
        let (sender, receiver) = oneshot::channel();
        ceremony.reply = Some(sender);
        if answer.send(approve).is_err() {
            // The task is gone (the window closed, the ceremony failed): resolve the caller
            // here so it is never left waiting for an outcome that cannot come.
            if let Some(ceremony) = self.ceremonies.remove(attempt_id) {
                if let Some(reply) = ceremony.reply {
                    let _ = reply.send(Err((
                        "PAIRING_GONE".to_owned(),
                        "the ceremony ended before the answer".to_owned(),
                    )));
                }
            }
        }
        Ok(receiver)
    }

    /// Waits for the next prompt (the control channel's `pair-prompt`).
    ///
    /// The GUI is told through `PairingPrompt`; this is for the one caller that has no event
    /// stream of its own, and it is resolved by the same [`Event::Prompt`].
    pub fn wait_for_prompt(&mut self) -> oneshot::Receiver<Prompt> {
        let (sender, receiver) = oneshot::channel();
        self.waiting.push(sender);
        receiver
    }

    /// A dial has started: the reply waits for the driver (or the failure).
    pub fn dial_started(&mut self) -> oneshot::Receiver<Verdict> {
        let (sender, receiver) = oneshot::channel();
        if let Some(previous) = self.dial.replace(sender) {
            let _ = previous.send(Err((
                "PAIRING_BUSY".to_owned(),
                "another dial was already in flight".to_owned(),
            )));
        }
        receiver
    }

    /// The dial did not reach a ceremony: an address that is not `ip:port`, a peer that
    /// refused the connection, a TLS failure. The reserved answer channel goes with it.
    pub fn dial_failed(&mut self, code: &str, detail: String) {
        self.reserved.pop();
        if let Some(dial) = self.dial.take() {
            let _ = dial.send(Err((code.to_owned(), detail)));
        }
    }

    /// Folds one report from a ceremony task into the loop's state.
    ///
    /// The dialer's own reply (`PairWith`) is resolved here, the moment the prompt exists —
    /// that is the answer `PairWith` promises. The ceremony's verdict is **not**: it is handed
    /// to the caller in [`PublishedEvent::Finished`], because a window told "paired" must be
    /// able to rely on the `TRUSTED_PEER` record being on disk, and only the caller writes it.
    pub fn on_event(&mut self, event: Event, now_ms: u64) -> Option<PublishedEvent> {
        match event {
            Event::DialFailed { code, detail } => {
                self.dial_failed(&code, detail);
                None
            }
            Event::Started { attempt_id } => {
                // The dialer's `attempt_id` comes from the opener's first `PairHello`, so the
                // window's own admission limit is the opener's business; here the reserved
                // answer channel only finds its ceremony.
                if !self.register(attempt_id) {
                    return None;
                }
                None
            }
            Event::Prompt(prompt) => {
                let prompt = *prompt;
                if let Some(ceremony) = self.ceremonies.get_mut(&prompt.attempt_id) {
                    ceremony.address = prompt.address.clone();
                }
                // A waiter whose receiver is gone has nobody to tell; `send` consumes it.
                for waiter in std::mem::take(&mut self.waiting) {
                    let _ = waiter.send(prompt.clone());
                }
                if let Some(dial) = self.dial.take() {
                    let _ = dial.send(Ok(()));
                }
                let _ = now_ms;
                Some(PublishedEvent::Prompt(prompt))
            }
            Event::Finished {
                attempt_id,
                outcome,
            } => {
                let ceremony = self.ceremonies.remove(&attempt_id);
                // The window counts this ceremony whichever way it ended, and closes itself
                // once its three attempts are spent.
                self.finish(now_ms);
                let window_closed = match &self.window {
                    Some(window) if window.attempt_id == attempt_id => self.close(),
                    _ => None,
                };
                let status = match &outcome {
                    Outcome::Paired(peer) => format!("paired with {}", peer.display_name),
                    Outcome::Rejected => "pairing rejected".to_owned(),
                    Outcome::Failed(detail) => format!("pairing failed: {detail}"),
                };
                let address = ceremony.as_ref().and_then(|c| c.address.clone());
                // Who waits for the verdict, if anybody: the caller resolves it *after* it
                // made the outcome durable (see the method's own comment).
                let reply = ceremony.and_then(|ceremony| ceremony.reply);
                Some(PublishedEvent::Finished {
                    outcome,
                    window_closed,
                    address,
                    status,
                    reply,
                })
            }
        }
    }
}

/// What the loop has to publish after folding one event in.
pub enum PublishedEvent {
    /// A code for the user: push `PairingPrompt`.
    Prompt(Prompt),
    /// A ceremony ended: push the closed window (when it was this node's), report the
    /// outcome in the status line, and let the loop persist a fresh `TrustedPeer`.
    Finished {
        /// How it ended.
        outcome: Outcome,
        /// The window that just closed, if this node opened it.
        window_closed: Option<Window>,
        /// The address to remember for the peer.
        address: Option<String>,
        /// One line for `Status`/`status` and the log.
        status: String,
        /// Whoever is waiting for the verdict (`PairingAnswer`, the CLI's `pair-answer`).
        ///
        /// The loop resolves it after persisting the outcome, so an `Ok` here means the
        /// `TRUSTED_PEER` record is on disk, not that it is about to be written.
        reply: Option<oneshot::Sender<Verdict>>,
    },
}

/// The address to remember for a ceremony, from the two sides' points of view.
///
/// The dialer typed the opener's address, so the address it observed *is* the peer's input
/// endpoint and its port is real. The opener only sees the dialer's *source* address, whose
/// port is ephemeral, so it takes the IP from the socket and the port from
/// `PairHello.listen_port` — the port the dialer actually listens on, or the documented
/// default when it sent `0` (`docs/05-protocol.md`). `crate::pair` does the same for the
/// CLI's own ceremony, and this delegates to that one implementation.
pub fn peer_address(opener: bool, observed: Option<&str>, listen_port: u16) -> Option<String> {
    crate::pair::peer_address(opener, observed, listen_port)
}

/// Runs one ceremony from an existing driver and reports through `events`.
///
/// The task owns the driver, so a peer that stalls delays one ceremony and nothing else:
/// not the loop, and not the other candidates the window may admit.
pub async fn run(
    driver: PairingDriver,
    answer: oneshot::Receiver<bool>,
    opener: bool,
    local_fingerprint: String,
    events: mpsc::Sender<Event>,
) {
    let attempt_id = *driver.attempt_id();
    let prompt = Prompt {
        attempt_id,
        peer_name: driver.peer_display_name().to_owned(),
        sas_code: driver.sas().text(),
        local_fingerprint,
        peer_fingerprint: driver.peer_fingerprint(),
        address: peer_address(opener, driver.address(), driver.peer_listen_port()),
    };
    if !opener {
        // A dialer learns the `attempt_id` from the opener's first `PairHello`, so its
        // reserved answer channel is paired with the ceremony only now.
        if events.send(Event::Started { attempt_id }).await.is_err() {
            return;
        }
    }
    if events.send(Event::Prompt(Box::new(prompt))).await.is_err() {
        return;
    }
    // A window that closes, or a GUI that goes away without answering, means "no": the
    // ceremony must not hold a TLS connection open for its own sake.
    let approve = answer.await.unwrap_or(false);
    let outcome = if approve {
        match tokio::time::timeout(DECISION_TIMEOUT, driver.approve()).await {
            Ok(Ok(peer)) => Outcome::Paired(Box::new(peer)),
            Ok(Err(error)) => Outcome::Failed(error.to_string()),
            Err(_) => Outcome::Failed("the other computer did not confirm in time".to_owned()),
        }
    } else {
        match tokio::time::timeout(DECISION_TIMEOUT, driver.reject()).await {
            Ok(Ok(())) => Outcome::Rejected,
            Ok(Err(error)) => Outcome::Failed(error.to_string()),
            Err(_) => Outcome::Failed("the other computer did not answer in time".to_owned()),
        }
    };
    let _ = events
        .send(Event::Finished {
            attempt_id,
            outcome,
        })
        .await;
}
