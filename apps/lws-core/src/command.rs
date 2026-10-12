//! The small vocabulary the CLI control channel and the session share.

use std::fmt;

use geometry::NodeId;
use identity::TrustStore;

/// A peer named on the command line: a 64-hex node id or a display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRef(String);

impl PeerRef {
    /// Wraps the text the user typed.
    pub fn new(text: impl Into<String>) -> Self {
        PeerRef(text.into())
    }

    /// The text as typed.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PeerRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Resolves a [`PeerRef`] against the trust store.
///
/// A name is deliberately ambiguous-safe: two peers may share a display name
/// (`Hello.display_name` is explicitly non-authoritative), so a name that matches
/// more than one record is refused and the node id is asked for instead.
pub fn resolve_peer(trust: &TrustStore, reference: &PeerRef) -> Result<NodeId, String> {
    let text = reference.as_str();
    if let Some(bytes) = config::topology::node_id_from_hex(text) {
        let node_id = NodeId::new(bytes);
        return if trust.is_trusted(&node_id) {
            Ok(node_id)
        } else {
            Err(format!("{text} is not a trusted peer"))
        };
    }
    let matches: Vec<NodeId> = trust
        .active()
        .filter(|peer| peer.display_name == text)
        .map(|peer| peer.node_id)
        .collect();
    match matches.as_slice() {
        [only] => Ok(*only),
        [] => Err(format!("no trusted peer is named {text:?}")),
        _ => Err(format!(
            "{text:?} names {} peers; use the node id",
            matches.len()
        )),
    }
}

/// The node id in the hex form the CLI prints and accepts.
///
/// Plain lower-case hex (64 characters), the form `[[network.peers]].node_id` uses;
/// `config::topology::peer_reference` is the `peer:<hex>` *reference* form the
/// topology document uses, which is a different thing.
pub fn node_id_hex(node_id: &NodeId) -> String {
    use std::fmt::Write;
    let mut text = String::with_capacity(64);
    for byte in node_id.as_bytes() {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// The hex form of a 16-byte id (`attempt_id`, `edit_id`), for a control line.
pub fn hex16(bytes: &[u8; 16]) -> String {
    use std::fmt::Write;
    let mut text = String::with_capacity(32);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// The 16 bytes a [`hex16`] string names; `None` when it is not one.
pub fn hex16_decode(text: &str) -> Option<[u8; 16]> {
    if text.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

/// What the agent is doing, as the `status` line reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// Never started in this process.
    Starting,
    /// Attached and answering.
    Ready,
    /// Restarted in `RECOVERY` after a crash (rule 7).
    Recovery,
    /// Gone; core is supervising a restart.
    Lost,
}

impl AgentState {
    /// The word used in the `status` line.
    pub const fn as_str(self) -> &'static str {
        match self {
            AgentState::Starting => "starting",
            AgentState::Ready => "ready",
            AgentState::Recovery => "recovery",
            AgentState::Lost => "lost",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use identity::TrustedPeer;

    fn store(peers: &[(&str, [u8; 32])]) -> TrustStore {
        let mut store = TrustStore::new();
        for (name, id) in peers {
            store.trust(TrustedPeer::paired(NodeId::new(*id), *name, None, 0));
        }
        store
    }

    #[test]
    fn a_sixteen_byte_id_round_trips_through_hex() {
        let id = [0xab; 16];
        let text = hex16(&id);
        assert_eq!(text.len(), 32);
        assert_eq!(hex16_decode(&text), Some(id));
        // A control line must not accept a truncated or non-hex id: the ceremony it names
        // would be somebody else's.
        assert_eq!(hex16_decode(""), None);
        assert_eq!(hex16_decode(&text[..31]), None);
        assert_eq!(hex16_decode(&format!("{}zz", &text[..30])), None);
    }

    #[test]
    fn a_node_id_resolves_when_it_is_trusted() {
        let id = [7u8; 32];
        let trust = store(&[("laptop", id)]);
        let hex = node_id_hex(&NodeId::new(id));
        assert_eq!(hex.len(), 64);
        assert_eq!(
            resolve_peer(&trust, &PeerRef::new(hex.clone())),
            Ok(NodeId::new(id))
        );
        assert!(resolve_peer(&trust, &PeerRef::new("00".repeat(32))).is_err());
    }

    #[test]
    fn a_display_name_resolves_but_an_ambiguous_one_does_not() {
        let trust = store(&[("laptop", [1u8; 32])]);
        assert_eq!(
            resolve_peer(&trust, &PeerRef::new("laptop")),
            Ok(NodeId::new([1u8; 32]))
        );

        let ambiguous = store(&[("laptop", [1u8; 32]), ("laptop", [2u8; 32])]);
        let error = resolve_peer(&ambiguous, &PeerRef::new("laptop")).expect_err("ambiguous");
        assert!(error.contains("node id"), "{error}");

        assert!(resolve_peer(&trust, &PeerRef::new("desktop")).is_err());
    }
}
