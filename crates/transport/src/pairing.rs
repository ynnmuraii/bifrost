//! The pairing window as seen by the link listener: `pair/1` shares `47901`.
//!
//! `docs/05-protocol.md`, «Сопряжение»: «Сопряжение — отдельный bounded state machine на
//! ALPN `lan-workspace/pair/1`; … узел, открывший окно сопряжения, — TLS-сервер и отправляет
//! `PairHello` первым, второй узел дозванивается и отвечает тем же сообщением». Both nodes
//! listen on exactly two ports (`docs/05-protocol.md`: «оба узла слушают `47901`/`47902`»),
//! so the pairing window is *not* a third listener: it is served by the same `47901`
//! listener and told apart by the offered ALPN, exactly as this crate's [`crate::manager`]
//! does.
//!
//! The other documented rules, quoted:
//!
//! * «Пользователь открывает окно сопряжения на узле-сервере на 120 s» — the window is
//!   time-bounded ([`identity::PairingWindow`] carries the deadline);
//! * «не более 3 ожидающих peers, rate limit handshakes» — [`PairingGate::admit`] delegates
//!   the cap to [`identity::PairingWindow::admit`];
//! * `pair/1` is admitted only while a window is open; with no window its ClientHello
//!   is refused before the handshake. `pair-request/1` is a separate prompt-only channel
//!   and cannot become a ceremony, a trusted session, or an input link.
//!   Unknown peers on `input/1` stay refused by the pin.
//!
//! The gate is a shared cell: the application opens the window (via
//! [`PairingGate::open`]) and the manager's accept task reads it. The application keeps a
//! clone so it can [`PairingGate::finish`] a ceremony it ran through [`identity::accept_stream`].

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

/// The pairing window the listener may serve (`docs/05-protocol.md`: `pair/1` shares
/// `47901`).
///
/// Cloning shares one window: the manager holds a clone inside its
/// [`crate::ManagerConfig`] and the application holds another, so opening or closing the
/// window is visible to both.
#[derive(Debug, Clone, Default)]
pub struct PairingGate {
    window: Arc<Mutex<Option<identity::PairingWindow>>>,
}

impl PairingGate {
    /// A gate with no window: every `pair/1` connection is refused.
    pub fn closed() -> Self {
        Self::default()
    }

    /// Opens the window the listener serves; replaces any previous one.
    pub fn open(&self, window: identity::PairingWindow) {
        *self.lock() = Some(window);
    }

    /// Closes the window; from now on `pair/1` is refused again.
    pub fn close(&self) {
        *self.lock() = None;
    }

    /// The window when one is open at `now_ms`; a window past its deadline reports `None`.
    pub fn window(&self, now_ms: u64) -> Option<identity::PairingWindow> {
        self.lock().clone().filter(|window| window.is_open(now_ms))
    }

    /// Admits one dialer (`pending += 1`) and returns the window it belongs to.
    ///
    /// Refuses when no window is open, when it has expired, or when the handshake budget
    /// (`pending + attempts`, «не более 3 ожидающих peers, rate limit handshakes» and «3
    /// попытки на 120 s») is spent — the [`identity::PairingWindow::admit`] rules, reported
    /// as a short reason string.
    pub fn admit(&self, now_ms: u64) -> Result<identity::PairingWindow, String> {
        let mut guard = self.lock();
        let Some(window) = guard.as_mut() else {
            return Err("no pairing window is open".into());
        };
        window
            .admit(now_ms)
            .map_err(|reason| format!("{reason:?}"))?;
        Ok(window.clone())
    }

    /// The RAII slot for one admitted handshake: dropping it releases `pending` unless the
    /// connection was handed to the application, which then owns the slot until `finish`.
    ///
    /// This is what keeps a leaked slot from pinning a pairing window: a TLS accept that
    /// fails, a `Pairing` event whose receiver is gone, or any early return between `admit`
    /// and the hand-off releases the slot instead of counting against the three handshakes.
    ///
    /// Pass the admitted window's `attempt_id`: replacing the window must not let an old
    /// handshake release a slot belonging to the replacement.
    pub fn admit_guard(&self, attempt_id: identity::AttemptId) -> PendingGuard {
        PendingGuard {
            gate: self.clone(),
            attempt_id,
            armed: true,
        }
    }

    /// `pending -= 1` without counting an attempt: the handshake never became a ceremony.
    fn release(&self, attempt_id: &identity::AttemptId) {
        if let Some(window) = self.lock().as_mut() {
            if window.attempt_id() == attempt_id {
                window.release_pending();
            }
        }
    }

    /// `pending -= 1`, `attempts += 1`; closes a window that is spent or expired.
    pub fn finish(&self, now_ms: u64) {
        let mut guard = self.lock();
        if let Some(window) = guard.as_mut() {
            window.finish();
            if !window.is_open(now_ms) {
                *guard = None;
            }
        }
    }

    /// Locks the cell, tolerating a poisoned mutex (the value is an `Option`, never a
    /// half-updated invariant).
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<identity::PairingWindow>> {
        self.window.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One admitted pairing handshake's slot, released on drop until it is disarmed.
#[derive(Debug)]
pub struct PendingGuard {
    gate: PairingGate,
    attempt_id: identity::AttemptId,
    armed: bool,
}

impl PendingGuard {
    /// The connection reached the application: the ceremony now owns the slot and will call
    /// [`PairingGate::finish`].
    pub fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if self.armed {
            self.gate.release(&self.attempt_id);
        }
    }
}

/// A certificate-authenticated but untrusted, prompt-only channel.
#[derive(Debug)]
pub struct RequestConnection {
    pub stream: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    pub address: SocketAddr,
    pub peer: geometry::NodeId,
}

/// Dials the request-only channel and checks the selected discovery identity.
/// Only [`request_alpn_unsupported`] permits falling back to the manual pairing flow.
pub async fn dial_request(
    identity: &identity::NodeIdentity,
    address: SocketAddr,
    expected: geometry::NodeId,
) -> std::io::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
    let config =
        identity::pairing_request_client_config(identity).map_err(std::io::Error::other)?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(address).await?;
    let stream = connector
        .connect(identity::server_name(address.ip()), tcp)
        .await?;
    let actual = identity::peer_node_id(stream.get_ref().1.peer_certificates().unwrap_or_default())
        .map_err(std::io::Error::other)?;
    if actual != expected {
        return Err(std::io::Error::other(
            "pairing request target certificate mismatch",
        ));
    }
    Ok(stream)
}

/// Recognizes an explicit TLS ALPN refusal without downgrading network or pin failures.
pub fn request_alpn_unsupported(error: &std::io::Error) -> bool {
    error
        .get_ref()
        .and_then(|cause| cause.downcast_ref::<rustls::Error>())
        .is_some_and(|cause| {
            matches!(
                cause,
                rustls::Error::NoApplicationProtocol
                    | rustls::Error::AlertReceived(rustls::AlertDescription::NoApplicationProtocol)
            )
        })
}

/// An accepted `pair/1` connection handed to the application, which runs the ceremony
/// with [`identity::accept_stream`].
///
/// The manager has already admitted the dialer through [`PairingGate::admit`], so the
/// [`window`](Self::window) is the one the ceremony's `attempt_id` and comparison code
/// belong to; the application calls [`PairingGate::finish`] once it has run the ceremony.
pub struct PairingConnection {
    /// The handshaked TLS stream (server side, unknown peer).
    pub stream: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    /// The dialer's address on this connection.
    pub address: SocketAddr,
    /// The pairing window this connection was admitted into.
    pub window: identity::PairingWindow,
}

impl std::fmt::Debug for PairingConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingConnection")
            .field("address", &self.address)
            .field("window", &self.window)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use identity::{PairingWindow, MAX_ATTEMPTS, MAX_PENDING_PEERS, WINDOW_MS};

    fn window_at(attempt_id: [u8; 16]) -> PairingWindow {
        PairingWindow::open(0, attempt_id)
    }

    #[test]
    fn a_closed_gate_refuses_every_dialer() {
        let gate = PairingGate::closed();
        assert!(gate.window(0).is_none());
        assert!(gate.admit(0).is_err());
        // Finishing without a window is a no-op, not a panic.
        gate.finish(0);
    }

    #[test]
    fn admit_refuses_past_max_pending_peers() {
        let gate = PairingGate::closed();
        gate.open(window_at([9u8; 16]));
        for _ in 0..MAX_PENDING_PEERS {
            assert!(gate.admit(0).is_ok());
        }
        let refused = gate.admit(0);
        assert!(refused.is_err(), "a fourth pending peer must be refused");
        assert_eq!(gate.window(0).map(|w| w.pending()), Some(MAX_PENDING_PEERS));
    }

    #[test]
    fn a_dropped_admit_guard_releases_the_slot_without_counting_an_attempt() {
        let gate = PairingGate::closed();
        gate.open(window_at([3u8; 16]));
        // Three abandoned handshakes must not pin the window: each drops its guard.
        for _ in 0..MAX_ATTEMPTS {
            let window = gate.admit(0).expect("admitted");
            let guard = gate.admit_guard(*window.attempt_id());
            drop(guard);
        }
        let window = gate.window(0).expect("the window must stay open");
        assert_eq!(window.pending(), 0, "a dropped guard releases its slot");
        assert_eq!(
            window.attempts(),
            0,
            "an abandoned handshake is not an attempt"
        );
        // The budget is whole again.
        assert!(gate.admit(0).is_ok());
    }

    #[test]
    fn an_old_guard_does_not_release_a_new_windows_pending_slot() {
        let gate = PairingGate::closed();
        gate.open(window_at([1u8; 16]));
        let admitted = gate.admit(0).expect("old window admits");
        let old_guard = gate.admit_guard(*admitted.attempt_id());

        gate.open(window_at([2u8; 16]));
        gate.admit(0).expect("new window admits");
        drop(old_guard);

        let window = gate.window(0).expect("new window stays open");
        assert_eq!(window.pending(), 1);
        assert_eq!(window.attempts(), 0);
    }

    #[test]
    fn a_guard_created_after_replacement_still_belongs_to_the_admitted_window() {
        let gate = PairingGate::closed();
        gate.open(window_at([1u8; 16]));
        let admitted = gate.admit(0).expect("old window admits");

        gate.open(window_at([2u8; 16]));
        gate.admit(0).expect("new window admits");
        drop(gate.admit_guard(*admitted.attempt_id()));

        let window = gate.window(0).expect("new window stays open");
        assert_eq!(window.pending(), 1);
        assert_eq!(window.attempts(), 0);
    }

    #[test]
    fn a_disarmed_guard_leaves_the_slot_to_finish() {
        let gate = PairingGate::closed();
        gate.open(window_at([4u8; 16]));
        let window = gate.admit(0).expect("admitted");
        gate.admit_guard(*window.attempt_id()).disarm();
        assert_eq!(gate.window(0).map(|w| w.pending()), Some(1));
        gate.finish(0);
        assert_eq!(gate.window(0).map(|w| w.pending()), Some(0));
    }

    #[test]
    fn finish_counts_an_attempt_and_closes_a_spent_window() {
        let gate = PairingGate::closed();
        gate.open(window_at([1u8; 16]));
        let admitted = gate.admit(0).expect("admitted");
        assert_eq!(admitted.attempt_id(), &[1u8; 16]);
        gate.finish(0);
        let window = gate.window(0).expect("still open");
        assert_eq!(window.pending(), 0);
        assert_eq!(window.attempts(), 1);
        // Spend the rest of the attempts; the window closes when the last one finishes.
        for attempt in 1..MAX_ATTEMPTS {
            gate.admit(0).expect("admitted");
            gate.finish(0);
            let _ = attempt;
        }
        assert!(gate.window(0).is_none(), "a spent window must close");
        assert!(gate.admit(0).is_err());
    }

    #[test]
    fn a_window_past_its_deadline_reports_none() {
        let gate = PairingGate::closed();
        gate.open(window_at([3u8; 16]));
        assert!(gate.window(WINDOW_MS).is_none());
        assert!(gate.admit(WINDOW_MS).is_err());
        // `finish` on an expired window clears it rather than resurrecting it.
        gate.finish(WINDOW_MS);
        assert!(gate.window(0).is_none());
    }

    #[test]
    fn open_replaces_a_previous_window() {
        let gate = PairingGate::closed();
        gate.open(window_at([1u8; 16]));
        gate.open(window_at([2u8; 16]));
        assert_eq!(gate.window(0).expect("open").attempt_id(), &[2u8; 16]);
        gate.close();
        assert!(gate.window(0).is_none());
    }
}
