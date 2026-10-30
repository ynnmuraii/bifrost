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

use std::collections::BTreeMap;

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
    /// Rewrites a document whose display ids are old platform device names to the stable ids
    /// the manifests report now, and reports whether anything moved.
    ///
    /// `aliases` maps a device name Windows currently reports (`\\.\DISPLAY1`) to the stable id
    /// that carries it. Only rows and endpoints on `local_node_id` are rewritten: a peer's ids
    /// are the peer's own and are never touched. A rewrite that would leave two display rows
    /// with the same id on one node keeps the first and drops the rest — the rows are a GUI
    /// snapshot, and the live manifest is the source of truth for what exists.
    ///
    /// Returns `false` (and changes nothing) for an empty alias table.
    pub fn rewrite_display_ids(
        &mut self,
        local_node_id: &[u8; 32],
        aliases: &BTreeMap<String, String>,
    ) -> bool {
        if aliases.is_empty() {
            return false;
        }
        let is_local = |node: &str| {
            parse_node_ref(node, local_node_id).is_ok_and(|id| id.as_bytes() == local_node_id)
                || node_id_from_hex(node).as_ref() == Some(local_node_id)
        };

        let mut changed = false;
        let mut seen: Vec<(String, String)> = Vec::with_capacity(self.displays.len());
        let mut displays = Vec::with_capacity(self.displays.len());
        for display in &self.displays {
            let mut display = display.clone();
            if is_local(&display.node) {
                if let Some(target) = aliases.get(&display.id) {
                    display.id = target.clone();
                    changed = true;
                }
                let key = (display.node.clone(), display.id.clone());
                if seen.contains(&key) {
                    changed = true;
                    continue;
                }
                seen.push(key);
            }
            displays.push(display);
        }
        self.displays = displays;

        for link in &mut self.links {
            for endpoint in [&mut link.from, &mut link.to] {
                if is_local(&endpoint.node) {
                    if let Some(target) = aliases.get(&endpoint.display) {
                        endpoint.display = target.clone();
                        changed = true;
                    }
                }
            }
        }
        for row in &mut self.map.displays {
            if is_local(&row.node_id) {
                if let Some(target) = aliases.get(&row.display_id) {
                    row.display_id = target.clone();
                    changed = true;
                }
            }
        }
        let mut rows = Vec::new();
        self.map.displays.retain(|row| {
            let key = (row.node_id.clone(), row.display_id.clone());
            if rows.contains(&key) {
                changed = true;
                false
            } else {
                rows.push(key);
                true
            }
        });
        for rule in &mut self.map.bridges {
            for end in [&mut rule.a, &mut rule.b] {
                if is_local(&end.node_id) {
                    if let Some(target) = aliases.get(&end.display_id) {
                        end.display_id = target.clone();
                        changed = true;
                    }
                }
            }
        }
        let mut rules: Vec<crate::BridgeRuleConfig> = Vec::new();
        for rule in self.map.bridges.drain(..) {
            if let Some(existing) = rules
                .iter_mut()
                .find(|other| other.matches(&rule.a, &rule.b))
            {
                // A persistent prohibition wins if old aliases collapse onto the same pair.
                if rule.decision == crate::BridgeDecision::Disabled {
                    existing.decision = rule.decision;
                }
                changed = true;
            } else {
                rules.push(rule);
            }
        }
        self.map.bridges = rules;
        changed
    }

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
        let mut links = Vec::new();
        for link in &self.links {
            let source = endpoint_node(&link.from, local_node_id, "topology.links.from")?;
            let target = endpoint_node(&link.to, local_node_id, "topology.links.to")?;
            if self.link_disabled(link, local_node_id) {
                continue;
            }
            links.push(geometry::TransitionLink::new(
                geometry::LinkId::new(link.id.clone()),
                endpoint(&link.from, source),
                endpoint(&link.to, target),
                link.bidirectional,
            ));
        }
        Ok(links)
    }

    /// Explicit pair rules override the default minimum contact on either concrete endpoint.
    pub fn link_disabled(&self, link: &crate::LinkConfig, local: &[u8; 32]) -> bool {
        let same = |a: &crate::BridgeEndpointConfig, b: &EndpointConfig| {
            let node = if a.node_id == "self" {
                Some(*local)
            } else {
                node_id_from_hex(a.node_id.strip_prefix("peer:").unwrap_or(&a.node_id))
            };
            a.display_id == b.display
                && a.edge == b.edge
                && node.is_some_and(|node| {
                    parse_node_ref(&b.node, local).is_ok_and(|id| id.as_bytes() == &node)
                })
        };
        let matching = |rule: &&crate::BridgeRuleConfig| {
            (same(&rule.a, &link.from) && same(&rule.b, &link.to))
                || (same(&rule.b, &link.from) && same(&rule.a, &link.to))
        };
        if self
            .map
            .bridges
            .iter()
            .filter(matching)
            .any(|rule| rule.decision == crate::BridgeDecision::Disabled)
        {
            return true;
        }
        if self
            .map
            .bridges
            .iter()
            .filter(matching)
            .any(|rule| rule.decision == crate::BridgeDecision::Enabled)
        {
            return false;
        }
        [&link.from, &link.to].into_iter().any(|end| {
            let span = end.end - end.start;
            if !span.is_finite() || span <= 0.0 || end.start < 0.0 {
                return false;
            }
            if end.is_auto() {
                return false;
            }
            let Some(display) = self.displays.iter().find(|display| {
                display.id == end.display
                    && parse_node_ref(&display.node, local).ok()
                        == parse_node_ref(&end.node, local).ok()
            }) else {
                return false;
            };
            let extent = match end.edge {
                crate::Edge::Left | crate::Edge::Right => display.logical[3],
                crate::Edge::Top | crate::Edge::Bottom => display.logical[2],
            };
            span + 1e-6 < (0.02 * extent).max(32.0)
        })
    }
}

fn endpoint_node(
    config: &EndpointConfig,
    local_node_id: &[u8; 32],
    key: &'static str,
) -> Result<geometry::NodeId, ConfigError> {
    parse_node_ref(&config.node, local_node_id).map_err(|_| {
        ConfigError::coded(
            ErrorCode::Topology,
            key,
            format!("node = {:?}", config.node),
            "\"self\" or \"peer:<64 hexadecimal digits>\"",
        )
    })
}

fn endpoint(config: &EndpointConfig, node: geometry::NodeId) -> geometry::Endpoint {
    if config.is_auto() {
        // `display = "auto"`: the edge of the whole desktop. The segment is derived from the
        // live manifests by `geometry::resolve_auto_links`, so nothing else is read here.
        return geometry::Endpoint::auto(node, config.edge.to_geometry());
    }
    geometry::Endpoint::new(
        node,
        geometry::DisplayId::new(config.display.clone()),
        config.edge.to_geometry(),
        geometry::Segment::new(config.start, config.end),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{DisplayConfig, Edge, EndpointConfig, LinkConfig};

    fn local() -> [u8; 32] {
        [0x11; 32]
    }

    fn document() -> TopologyConfig {
        let mut config = TopologyConfig::default();
        config.displays.push(DisplayConfig {
            node: "self".to_owned(),
            id: "\\\\.\\DISPLAY1".to_owned(),
            logical: [0.0, 0.0, 1920.0, 1080.0],
            primary: true,
        });
        config.displays.push(DisplayConfig {
            node: peer_reference(&[0xAA; 32]),
            id: "\\\\.\\DISPLAY1".to_owned(),
            logical: [0.0, 0.0, 1920.0, 1080.0],
            primary: true,
        });
        config.links.push(LinkConfig {
            id: "self-to-peer".to_owned(),
            from: EndpointConfig {
                node: "self".to_owned(),
                display: "\\\\.\\DISPLAY1".to_owned(),
                edge: Edge::Right,
                start: 0.0,
                end: 1080.0,
            },
            to: EndpointConfig {
                node: peer_reference(&[0xAA; 32]),
                display: "\\\\.\\DISPLAY1".to_owned(),
                edge: Edge::Left,
                start: 0.0,
                end: 1080.0,
            },
            bidirectional: true,
        });
        config
    }

    #[test]
    fn an_auto_anchor_round_trips_through_toml_and_the_wire_shape() {
        let peer = peer_reference(&[0xAA; 32]);
        let text = format!(
            r#"
config_version = 2
[network]
listen_address = "192.168.1.10"

[topology]
[[topology.links]]
id = "auto-right"
from = {{ node = "self", display = "auto", edge = "right" }}
to = {{ node = "{peer}", display = "auto", edge = "left" }}
bidirectional = true
"#
        );
        let config = crate::Config::from_str(&text).expect("the document parses");
        config
            .validate()
            .expect("an auto anchor is a valid endpoint");
        assert!(config.topology.links[0].from.is_auto());

        // The segment is not part of an auto anchor: it is neither written nor missed.
        let rendered = toml::to_string_pretty(&config.topology).expect("the document renders");
        assert!(
            !rendered.contains("start"),
            "an auto endpoint carries no segment: {rendered}"
        );
        let back: TopologyConfig = toml::from_str(&rendered).expect("the document reloads");
        assert_eq!(back, config.topology);

        let links = back
            .to_transition_links(&local())
            .expect("the links convert");
        assert!(links[0].source.is_auto());
        assert_eq!(links[0].source.display.as_str(), crate::AUTO_DISPLAY);
        assert_eq!(links[0].source.edge, geometry::Edge::Right);
        assert!(links[0].target.is_auto());
    }

    #[test]
    fn a_concrete_endpoint_still_needs_an_ordered_segment() {
        let mut config = TopologyConfig::default();
        config.links.push(LinkConfig {
            id: "hand-written".to_owned(),
            from: EndpointConfig {
                node: "self".to_owned(),
                display: "\\\\.\\DISPLAY1".to_owned(),
                edge: Edge::Right,
                start: 0.0,
                end: 0.0,
            },
            to: EndpointConfig {
                node: peer_reference(&[0xAA; 32]),
                display: "\\\\.\\DISPLAY1".to_owned(),
                edge: Edge::Left,
                start: 0.0,
                end: 1080.0,
            },
            bidirectional: true,
        });
        let text = toml::to_string_pretty(&config).expect("the document renders");
        let back: TopologyConfig = toml::from_str(&text).expect("the document reloads");
        let mut whole = crate::Config::default();
        whole.network.listen_address = "192.168.1.10".to_owned();
        whole.topology = back;
        assert!(
            whole.validate().is_err(),
            "a display endpoint without an extent is still refused"
        );
    }

    #[test]
    fn an_alias_rewrites_the_local_rows_and_leaves_the_peer_alone() {
        let mut config = document();
        let aliases = BTreeMap::from([(
            "\\\\.\\DISPLAY1".to_owned(),
            "display-interface-path".to_owned(),
        )]);
        assert!(config.rewrite_display_ids(&local(), &aliases));

        assert_eq!(config.displays[0].id, "display-interface-path");
        assert_eq!(
            config.displays[1].id, "\\\\.\\DISPLAY1",
            "a peer's display id belongs to the peer"
        );
        assert_eq!(config.links[0].from.display, "display-interface-path");
        assert_eq!(config.links[0].to.display, "\\\\.\\DISPLAY1");
    }

    #[test]
    fn an_empty_or_unmatched_alias_table_changes_nothing() {
        let mut config = document();
        assert!(!config.rewrite_display_ids(&local(), &BTreeMap::new()));
        let unrelated = BTreeMap::from([("\\\\.\\DISPLAY9".to_owned(), "other".to_owned())]);
        assert!(!config.rewrite_display_ids(&local(), &unrelated));
        assert_eq!(config, document());
    }

    #[test]
    fn a_rewrite_that_would_duplicate_a_display_keeps_one_row() {
        let mut config = document();
        config.displays.insert(
            1,
            DisplayConfig {
                node: "self".to_owned(),
                id: "display-interface-path".to_owned(),
                logical: [0.0, 0.0, 1920.0, 1080.0],
                primary: true,
            },
        );
        let aliases = BTreeMap::from([(
            "\\\\.\\DISPLAY1".to_owned(),
            "display-interface-path".to_owned(),
        )]);
        assert!(config.rewrite_display_ids(&local(), &aliases));

        let self_rows: Vec<&str> = config
            .displays
            .iter()
            .filter(|display| display.node == "self")
            .map(|display| display.id.as_str())
            .collect();
        assert_eq!(self_rows, vec!["display-interface-path"]);
    }

    #[test]
    fn map_rules_reverse_filter_and_alias_without_losing_authored_links() {
        let mut config = document();
        let end = |end: &EndpointConfig| crate::BridgeEndpointConfig {
            node_id: end.node.clone(),
            display_id: end.display.clone(),
            edge: end.edge,
        };
        let a = end(&config.links[0].from);
        let b = end(&config.links[0].to);
        config.map.bridges.push(crate::BridgeRuleConfig {
            a: b.clone(),
            b: a.clone(),
            decision: crate::BridgeDecision::Disabled,
        });
        assert!(config.map.bridges[0].matches(&a, &b));
        assert!(config.to_transition_links(&local()).unwrap().is_empty());
        assert_eq!(config.links.len(), 1);
        config.map.displays.push(crate::DisplayMapConfig {
            node_id: "self".into(),
            display_id: a.display_id.clone(),
            rect: [-10.0, 20.0, 144.0, 81.0],
            size_mode: crate::SizeMode::Manual,
            diagonal_inches: Some(24.0),
        });
        let aliases = BTreeMap::from([(a.display_id, "stable-panel".to_owned())]);
        assert!(config.rewrite_display_ids(&local(), &aliases));
        assert_eq!(config.map.displays[0].display_id, "stable-panel");
        assert_eq!(config.map.bridges[0].b.display_id, "stable-panel");
        assert!(config.to_transition_links(&local()).unwrap().is_empty());
        let text = toml::to_string(&config).unwrap();
        let restored: TopologyConfig = toml::from_str(&text).unwrap();
        assert_eq!(restored, config);
    }

    #[test]
    fn old_map_defaults_and_strict_new_rows_validate() {
        let old: TopologyConfig = toml::from_str("").unwrap();
        assert_eq!(old.map, crate::MapConfig::default());
        assert!(toml::from_str::<TopologyConfig>("[map]\nextra = true").is_err());
        let mut config = crate::Config::default();
        config.topology.map.displays.push(crate::DisplayMapConfig {
            node_id: "self".into(),
            display_id: "panel".into(),
            rect: [0.0, 0.0, 1.0, 1.0],
            size_mode: crate::SizeMode::Legacy,
            diagonal_inches: None,
        });
        assert!(config.topology.validate().is_ok());
        config.topology.map.displays[0].rect[2] = 0.0;
        assert!(config.topology.validate().is_err());
    }

    #[test]
    fn short_default_checks_both_ends_and_explicit_enabled_overrides() {
        let mut config = document();
        config.displays[1].logical[3] = 2160.0;
        config.links[0].from.end = 100.0;
        config.links[0].to.end = 40.0;
        assert!(config.link_disabled(&config.links[0], &local()));
        config.links[0].to.end = 43.2;
        assert!(!config.link_disabled(&config.links[0], &local()));
        config.links[0].from.end = 31.999;
        assert!(config.link_disabled(&config.links[0], &local()));
        config.links[0].from.end = 32.0;
        assert!(!config.link_disabled(&config.links[0], &local()));
        let endpoint = |end: &EndpointConfig| crate::BridgeEndpointConfig {
            node_id: end.node.clone(),
            display_id: end.display.clone(),
            edge: end.edge,
        };
        config.map.bridges.push(crate::BridgeRuleConfig {
            a: endpoint(&config.links[0].to),
            b: endpoint(&config.links[0].from),
            decision: crate::BridgeDecision::Enabled,
        });
        config.links[0].from.end = 1.0;
        assert!(!config.link_disabled(&config.links[0], &local()));
        config.map.bridges[0].decision = crate::BridgeDecision::Disabled;
        config.links[0].from.end = 100.0;
        assert!(config.link_disabled(&config.links[0], &local()));
    }

    #[test]
    fn missing_short_endpoint_is_not_hidden_before_live_resolution() {
        let mut config = document();
        config.displays.clear();
        config.links[0].from.end = 10.0;
        config.links[0].to.end = 10.0;
        assert!(!config.link_disabled(&config.links[0], &local()));
        assert_eq!(config.to_transition_links(&local()).unwrap().len(), 1);
        config.links[0].from.node = "bogus".into();
        let error = config.to_transition_links(&local()).unwrap_err();
        assert_eq!(error.code(), Some(ErrorCode::Topology));
        assert!(error.to_string().contains("topology.links.from"));
    }
}
