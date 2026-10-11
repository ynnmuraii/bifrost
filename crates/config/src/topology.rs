//! `[topology]` → `geometry` conversion.
//!
//! `docs/03-architecture.md` («Правила целостности») makes the transition graph
//! the authoritative part of the layout document, so it is the part that has a
//! faithful `geometry` counterpart here. The `[[topology.displays]]` entries are
//! only «снимок для GUI; источник истины — агент узла», so they are *not* turned
//! into [`geometry::Display`] values: a real document gets its displays from each
//! node's agent, and inventing rotation/scale/native bounds from a GUI snapshot
//! would fabricate data.
//!
//! Node references are resolved against the local identity:
//!
//! * `"self"` → the node running this code;
//! * `"peer:<64 hex digits>"` → that `SHA-256(SPKI DER)`;
//!
//! The local node id is a parameter because the configuration does not contain
//! it — identity lives in the trust store.

use crate::error::{ConfigError, ErrorCode};
use crate::schema::{EndpointConfig, TopologyConfig};

/// Parse `64` hexadecimal digits into a 32-byte node id.
pub fn node_id_from_hex(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
        let high = hex_digit(pair[0])?;
        let low = hex_digit(pair[1])?;
        out[index] = (high << 4) | low;
    }
    Some(out)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Render a node id the way `[topology]` writes a peer reference.
pub fn peer_reference(node_id: &[u8; 32]) -> String {
    let mut text = String::with_capacity(5 + 64);
    text.push_str("peer:");
    for byte in node_id {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

/// Resolve a `"self"` / `"peer:<hex>"` reference to a [`geometry::NodeId`].
pub fn parse_node_ref(
    text: &str,
    local_node_id: &[u8; 32],
) -> Result<geometry::NodeId, ConfigError> {
    if text == "self" {
        return Ok(geometry::NodeId::new(*local_node_id));
    }
    if let Some(hex) = text.strip_prefix("peer:") {
        if let Some(bytes) = node_id_from_hex(hex) {
            return Ok(geometry::NodeId::new(bytes));
        }
    }
    Err(ConfigError::coded(
        ErrorCode::Topology,
        "topology.links.node",
        text,
        "\"self\" or \"peer:<64 hexadecimal digits>\"",
    ))
}

impl TopologyConfig {
    /// The transition graph as [`geometry::TransitionLink`] values.
    ///
    /// `local_node_id` resolves `"self"`; `[topology].author`,
    /// `max_seen_*` and `[[topology.displays]]` are not used here (see the
    /// module documentation). Display-level integrity (segments inside their
    /// display, no overlapping segments on one edge, nodes not in a gap) is
    /// checked by [`geometry::validate_topology`] once real displays are known.
    pub fn to_transition_links(
        &self,
        local_node_id: &[u8; 32],
    ) -> Result<Vec<geometry::TransitionLink>, ConfigError> {
        self.links
            .iter()
            .map(|link| {
                let source = endpoint(&link.from, local_node_id, "topology.links.from")?;
                let target = endpoint(&link.to, local_node_id, "topology.links.to")?;
                Ok(geometry::TransitionLink::new(
                    geometry::LinkId::new(link.id.clone()),
                    source,
                    target,
                    link.bidirectional,
                ))
            })
            .collect()
    }
}

fn endpoint(
    config: &EndpointConfig,
    local_node_id: &[u8; 32],
    key: &'static str,
) -> Result<geometry::Endpoint, ConfigError> {
    let node = parse_node_ref(&config.node, local_node_id).map_err(|_| {
        ConfigError::coded(
            ErrorCode::Topology,
            key,
            format!("node = {:?}", config.node),
            "\"self\" or \"peer:<64 hexadecimal digits>\"",
        )
    })?;
    Ok(geometry::Endpoint::new(
        node,
        geometry::DisplayId::new(config.display.clone()),
        config.edge.to_geometry(),
        geometry::Segment::new(config.start, config.end),
    ))
}
