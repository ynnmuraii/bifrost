//! The layout document as core assembles it from three sources.
//!
//! `docs/03-architecture.md`, «Топология и движение»: the document is
//! `Topology{revision, author_node_id, nodes[], links[]}`, the links come from
//! `config.example.toml`'s `[[topology.links]]` (the GUI edits the same document), and
//! the manifests are per node: the local one from the agent's last non-empty `DisplaysReport`
//! (or the `[[topology.displays]]` snapshot before one arrives), each peer's from its last
//! non-empty `Hello`/display report or the configured snapshot.
//!
//! Two rules decide what this module does with bad input:
//!
//! * «Углы, неоднозначные/перекрывающиеся links и сегмент в «дырке» между мониторами не
//!   допускаются валидатором конфигурации» — every assembled document goes through
//!   [`geometry::validate_topology`], and its errors are returned, not repaired.
//! * a display the platform cannot describe (non-positive size, a broken scale) is
//!   dropped instead of being given invented bounds; `geometry` would reject it anyway.
//!
//! The module is pure: it takes the already-parsed configuration and the already-decoded
//! reports, so the whole conversion is unit-testable without a session.

use std::collections::BTreeMap;

use geometry::{Display, DisplayId, NodeDisplays, NodeId, Rect, Rotation, Scale, Topology};
use protocol::ipc;
use protocol::wire;

/// Where the document's parts come from.
pub struct Documents<'a> {
    /// This node's identity, which resolves `"self"` in `[[topology.links]]`.
    pub local_node_id: [u8; 32],
    /// `[topology]` from `config.toml`.
    pub topology: &'a config::TopologyConfig,
    /// The local display list selected by the session: the last non-empty agent report, or
    /// the configured snapshot while no live manifest is available.
    pub local_displays: &'a [ipc::DisplayInfo],
    /// Peer display lists selected by the session, with configured snapshots retained when
    /// a peer's hello/report is empty.
    pub peer_displays: &'a [(NodeId, Vec<wire::Display>)],
}

/// The assembled document, or why it cannot be used.
#[derive(Debug)]
pub enum BuildError {
    /// The links themselves are malformed (a `"self"`/`"peer:<hex>"` reference that does
    /// not parse, a segment with `start >= end`): [`config::TopologyConfig`] refuses them.
    Links(config::ConfigError),
    /// The document assembled, but breaks the integrity rules of «Доменная модель».
    Invalid(Vec<geometry::TopologyError>),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::Links(error) => write!(f, "the configured links are invalid: {error}"),
            BuildError::Invalid(errors) => {
                write!(
                    f,
                    "the layout document is invalid ({} problems):",
                    errors.len()
                )?;
                for error in errors {
                    write!(f, " {error:?};")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for BuildError {}

/// Assembles and validates the document.
///
/// `revision` is the applied revision from `[topology]`, and the author is the node that
/// authored it (`[topology].author`, resolved as a node id) — the pair core publishes in
/// `Hello.topology_revision`/`topology_author` and checks in `EnterPrepare`.
pub fn build(documents: &Documents<'_>) -> Result<Topology, BuildError> {
    let links = documents
        .topology
        .to_transition_links(&documents.local_node_id)
        .map_err(BuildError::Links)?;

    let local = NodeId::new(documents.local_node_id);
    let mut nodes: BTreeMap<NodeId, Vec<Display>> = BTreeMap::new();
    let mut local_named = Vec::new();
    for info in documents.local_displays {
        if let Some(display) = display_from_ipc(info) {
            local_named.push(display);
        }
    }
    nodes.insert(local, local_named);
    for (peer, displays) in documents.peer_displays {
        let converted: Vec<Display> = displays.iter().filter_map(display_from_wire).collect();
        nodes.insert(*peer, converted);
    }

    let revision = documents
        .topology
        .max_seen_revision
        .max(documents.topology.revision);
    let author = author_of(documents).unwrap_or(local);
    let topology = Topology::new(
        revision,
        author,
        nodes
            .into_iter()
            .map(|(node, displays)| NodeDisplays::new(node, displays, true))
            .collect(),
        links,
    );

    match geometry::validate_topology(&topology) {
        Ok(()) => Ok(topology),
        Err(errors) => Err(BuildError::Invalid(errors.errors().to_vec())),
    }
}

/// The author of the applied revision, when the configuration names one that parses.
fn author_of(documents: &Documents<'_>) -> Option<NodeId> {
    let name = if documents.topology.author.is_empty() {
        documents.topology.max_seen_author.as_str()
    } else {
        documents.topology.author.as_str()
    };
    if name == "self" {
        return Some(NodeId::new(documents.local_node_id));
    }
    let hex = name.strip_prefix("peer:").unwrap_or(name);
    config::node_id_from_hex(hex).map(NodeId::new)
}

/// A captured position in the node's logical space → a display-local
/// [`geometry::Position`].
///
/// The capture path carries no display id: `input-model`'s `Position` is two numbers and
/// the agent sends `PointerAbs.display_id` empty on purpose («the receiver resolves the
/// display from its own zones/geometry»), because what the OS reports is a position in
/// the *node's* space (the Windows virtual desktop, the X11 root window). Resolving an
/// edge needs the display-local form, and this is that conversion: find the display whose
/// rectangle contains the point. A point outside every display (a gap between monitors,
/// which the documents allow) has no local position and cannot open a transition.
pub fn position_on(
    topology: &Topology,
    node: &NodeId,
    x: f64,
    y: f64,
) -> Option<geometry::Position> {
    let member = topology.node(node)?;
    let point = geometry::Point::new(x, y);
    let display = member
        .displays
        .iter()
        .find(|display| display.logical_bounds.contains(point))?;
    Some(geometry::Position::new(
        display.id.clone(),
        x - display.logical_bounds.x,
        y - display.logical_bounds.y,
    ))
}

/// The events a peer receives while a source lease is remote.
///
/// `protocol.proto` `Position` is «local to display top-left, logical units», and the
/// peer's display is the *target* one, so an absolute position the source captured in its
/// own node space must be replaced by the virtual position on the target display (which is
/// where the deltas have already carried it). Sending the node-space position instead was
/// the bug this function exists for: the receiver would inject at whatever its own
/// geometry made of those numbers.
///
/// Without a virtual position (no resolved transition yet) the events are passed through
/// unchanged: there is nothing better to send, and dropping a key edge would be worse.
pub fn forward_events(
    events: Vec<input_model::InputEvent>,
    virtual_position: Option<&geometry::Position>,
) -> Vec<input_model::InputEvent> {
    let Some(position) = virtual_position else {
        return events;
    };
    events
        .into_iter()
        .map(|event| match event {
            input_model::InputEvent::PointerAbsolute(_) => {
                input_model::InputEvent::PointerAbsolute(input_model::Position::new(
                    position.x, position.y,
                ))
            }
            other => other,
        })
        .collect()
}

/// `DisplayInfo` (IPC) → [`Display`].
///
/// The IPC message has no `native_known` flag, so a non-positive native width or height
/// means "not known" — which is how the portal backend reports its zones
/// (see `apps/lws-agent/src/displays.rs`). An unusable rectangle or scale returns `None`
/// rather than inventing bounds.
pub fn display_from_ipc(info: &ipc::DisplayInfo) -> Option<Display> {
    let logical = Rect {
        x: info.lx,
        y: info.ly,
        width: info.lw,
        height: info.lh,
    };
    if !logical.is_finite() || !logical.has_positive_area() {
        return None;
    }
    let native = if info.nw > 0 && info.nh > 0 {
        Some(Rect {
            x: f64::from(info.nx),
            y: f64::from(info.ny),
            width: f64::from(info.nw),
            height: f64::from(info.nh),
        })
    } else {
        None
    };
    let scale = Scale::uniform(if info.scale.is_finite() && info.scale > 0.0 {
        info.scale
    } else {
        1.0
    });
    if !scale.is_valid() {
        return None;
    }
    Some(Display {
        id: DisplayId::new(info.id.clone()),
        logical_bounds: logical,
        native_bounds: native,
        rotation: Rotation::from_degrees(u16::try_from(info.rotation).unwrap_or(0))
            .unwrap_or(Rotation::Deg0),
        scale,
        primary: info.primary,
    })
}

/// `DisplayInfo` → the member manifest carried by `Hello.displays` and `DisplaysChanged`.
pub fn display_to_wire(info: &ipc::DisplayInfo) -> Option<wire::Display> {
    let display = display_from_ipc(info)?;
    let rect = |rect: Rect| wire::Rect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    };
    Some(wire::Display {
        id: display.id.as_str().to_owned(),
        logical_bounds: Some(rect(display.logical_bounds)),
        native_bounds: display.native_bounds.map(rect),
        rotation_degrees: match display.rotation {
            Rotation::Deg0 => 0,
            Rotation::Deg90 => 90,
            Rotation::Deg180 => 180,
            Rotation::Deg270 => 270,
        },
        scale_x: display.scale.x,
        scale_y: display.scale.y,
        primary: display.primary,
        native_known: display.native_bounds.is_some(),
    })
}

/// `Display` (wire `Hello.displays`) → [`Display`], the same way.
pub fn display_from_wire(display: &wire::Display) -> Option<Display> {
    let logical = rect_of(display.logical_bounds.as_ref()?);
    if !logical.is_finite() || !logical.has_positive_area() {
        return None;
    }
    let native = if display.native_known {
        display.native_bounds.as_ref().map(rect_of)
    } else {
        None
    };
    let scale = Scale {
        x: display.scale_x,
        y: display.scale_y,
    };
    if !scale.is_valid() {
        return None;
    }
    Some(Display {
        id: DisplayId::new(display.id.clone()),
        logical_bounds: logical,
        native_bounds: native,
        rotation: Rotation::from_degrees(u16::try_from(display.rotation_degrees).unwrap_or(0))
            .unwrap_or(Rotation::Deg0),
        scale,
        primary: display.primary,
    })
}

fn rect_of(rect: &protocol::wire::Rect) -> Rect {
    Rect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

/// The `[[topology.displays]]` snapshot of every *other* node, as wire displays.
///
/// A `[[topology.links]]` endpoint names a peer display, so the snapshot already declares
/// that it exists; until the peer sends a non-empty manifest (or when a dry session never
/// sends one), this is what keeps the document complete instead of failing validation.
/// Rows whose `node` reference does not parse are skipped with a log, because the example
/// file carries truncated placeholders.
pub fn snapshot_peers(
    topology: &config::TopologyConfig,
    local_node_id: &[u8; 32],
) -> Vec<(NodeId, Vec<wire::Display>)> {
    let local = config::peer_reference(local_node_id);
    let mut by_node: BTreeMap<NodeId, Vec<wire::Display>> = BTreeMap::new();
    for row in &topology.displays {
        if row.node == "self" || row.node == local {
            continue;
        }
        let Some(bytes) = node_reference(&row.node) else {
            tracing::warn!(
                node = row.node.as_str(),
                "a [[topology.displays]] row names a node that is not a hex id; skipped"
            );
            continue;
        };
        by_node
            .entry(NodeId::new(bytes))
            .or_default()
            .push(wire::Display {
                id: row.id.clone(),
                logical_bounds: Some(wire::Rect {
                    x: row.logical[0],
                    y: row.logical[1],
                    width: row.logical[2],
                    height: row.logical[3],
                }),
                native_bounds: None,
                rotation_degrees: 0,
                scale_x: 1.0,
                scale_y: 1.0,
                primary: row.primary,
                native_known: false,
            });
    }
    by_node.into_iter().collect()
}

/// A `"self"`/`"peer:<hex>"`/bare-hex reference as 32 bytes.
fn node_reference(reference: &str) -> Option<[u8; 32]> {
    let hex = reference.strip_prefix("peer:").unwrap_or(reference);
    config::node_id_from_hex(hex)
}

/// The `[[topology.displays]]` snapshot of this node as `DisplayInfo` values.
///
/// Used before a non-empty agent report arrives (dry mode, or a session without an OS input
/// path): the snapshot is what the GUI edited, and the documents call it «снимок для GUI;
/// источник истины — агент узла» — a snapshot is exactly what a fallback needs.
pub fn snapshot_displays(
    topology: &config::TopologyConfig,
    local_node_id: &[u8; 32],
) -> Vec<ipc::DisplayInfo> {
    let self_node = "self";
    let local = config::peer_reference(local_node_id);
    topology
        .displays
        .iter()
        .filter(|display| display.node == self_node || display.node == local)
        .map(|display| ipc::DisplayInfo {
            id: display.id.clone(),
            lx: display.logical[0],
            ly: display.logical[1],
            lw: display.logical[2],
            lh: display.logical[3],
            nx: 0,
            ny: 0,
            nw: 0,
            nh: 0,
            rotation: 0,
            scale: 1.0,
            primary: display.primary,
        })
        .collect()
}

/// The `(revision, author)` pair a wire document carries (KVM-072).
///
/// `None` when the author field is not 32 bytes: a document whose identity cannot be read is
/// not a document, and the caller refuses it instead of guessing an author.
#[must_use]
pub fn wire_pair(document: &wire::Topology) -> Option<crate::layout::Pair> {
    let bytes: [u8; 32] = document.author_node_id.as_slice().try_into().ok()?;
    Some(crate::layout::Pair::new(
        document.revision,
        NodeId::new(bytes),
    ))
}

/// Revalidates a document that arrived from a peer.
///
/// ADR-013 rejected merging rows for exactly this reason: the invariants are global. The
/// checks here are the ones a document must satisfy to be *usable* — a node list with no
/// duplicates, and links whose endpoints name nodes the document declares — while the
/// configuration validator keeps owning the geometric rules at edit time.
pub fn validate_wire(document: &wire::Topology) -> Result<(), String> {
    if document.nodes.is_empty() {
        return Err("topology with no nodes".to_owned());
    }
    let mut seen: Vec<&[u8]> = Vec::with_capacity(document.nodes.len());
    for node in &document.nodes {
        if node.node_id.len() != 32 {
            return Err("topology node_id is not 32 bytes".to_owned());
        }
        if seen.contains(&node.node_id.as_slice()) {
            return Err("topology names the same node twice".to_owned());
        }
        seen.push(node.node_id.as_slice());
    }
    for link in &document.links {
        for (side, endpoint) in [
            ("source", link.source_endpoint.as_ref()),
            ("target", link.target_endpoint.as_ref()),
        ] {
            let Some(endpoint) = endpoint else {
                return Err(format!("link {side} endpoint is missing"));
            };
            if !seen.contains(&endpoint.node_id.as_slice()) {
                return Err(format!("link {side} endpoint names an unknown node"));
            }
            if endpoint.display_id.is_empty() {
                return Err(format!("link {side} endpoint has no display"));
            }
        }
    }
    Ok(())
}

/// Builds the wire document of the *live* layout: nodes from the member manifests, links from
/// the configuration (KVM-072).
///
/// `docs/05-protocol.md` gives `Topology.nodes[]` as «members and their displays», and
/// `config.example.toml` says the same about `[[topology.displays]]`: «снимок для GUI; источник
/// истины — агент узла». The live document is therefore the source for the nodes — it is
/// assembled from the agents' `DisplaysReport`s and the peers' `Hello.displays`, exactly what
/// the wire carries — while the *links* come from the layout configuration, which is what an
/// edit carries.
#[must_use]
pub fn wire_from_live(
    live: &geometry::Topology,
    layout: &config::TopologyConfig,
    revision: u64,
    author: NodeId,
    self_node: NodeId,
) -> wire::Topology {
    let rect = |rect: geometry::Rect| wire::Rect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    };
    let nodes: Vec<wire::NodeDisplays> = live
        .nodes
        .iter()
        .map(|node| wire::NodeDisplays {
            node_id: node.node.as_bytes().to_vec(),
            displays: node
                .displays
                .iter()
                .map(|display| wire::Display {
                    id: display.id.as_str().to_owned(),
                    logical_bounds: Some(rect(display.logical_bounds)),
                    native_bounds: display.native_bounds.map(rect),
                    rotation_degrees: match display.rotation {
                        geometry::Rotation::Deg0 => 0,
                        geometry::Rotation::Deg90 => 90,
                        geometry::Rotation::Deg180 => 180,
                        geometry::Rotation::Deg270 => 270,
                    },
                    scale_x: display.scale.x,
                    scale_y: display.scale.y,
                    primary: display.primary,
                    native_known: display.native_bounds.is_some(),
                })
                .collect(),
            online: node.online,
        })
        .collect();
    let mut document = wire_document(layout, revision, author, self_node);
    if nodes.is_empty() {
        // A document with no nodes could not be revalidated; build declares local identity
        // even before the agent reports any displays.
        return document;
    }
    document.nodes = nodes;
    document
}

/// Builds the wire document of a `[topology]` section (KVM-072).
///
/// This is the coordinator's publish step: the applied revision and its author travel with the
/// document, and `self_node` is what `"self"` in the configuration means. Everything the
/// configuration does not carry — native bounds, rotation, scale — is reported as the logical
/// bounds with `native_known = false`, which is exactly the claim the portals make.
#[must_use]
pub fn wire_document(
    layout: &config::TopologyConfig,
    revision: u64,
    author: NodeId,
    self_node: NodeId,
) -> wire::Topology {
    let node_of = |text: &str| -> Option<[u8; 32]> {
        if text == "self" {
            return Some(*self_node.as_bytes());
        }
        let hex = text.strip_prefix("peer:")?;
        let mut bytes = [0u8; 32];
        if hex.len() != 64 {
            return None;
        }
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
        }
        Some(bytes)
    };
    let rect = |logical: [f64; 4]| wire::Rect {
        x: logical[0],
        y: logical[1],
        width: logical[2],
        height: logical[3],
    };
    let edge = |edge: config::Edge| match edge {
        config::Edge::Left => wire::Edge::Left,
        config::Edge::Right => wire::Edge::Right,
        config::Edge::Top => wire::Edge::Top,
        config::Edge::Bottom => wire::Edge::Bottom,
    };
    let endpoint = |endpoint: &config::EndpointConfig| wire::Endpoint {
        node_id: node_of(&endpoint.node).unwrap_or_default().to_vec(),
        display_id: endpoint.display.clone(),
        edge: edge(endpoint.edge) as i32,
        segment_start: endpoint.start,
        segment_end: endpoint.end,
    };

    let mut nodes: Vec<wire::NodeDisplays> = Vec::new();
    for display in &layout.displays {
        let Some(node) = node_of(&display.node) else {
            continue;
        };
        let entry = match nodes.iter_mut().find(|entry| entry.node_id == node) {
            Some(entry) => entry,
            None => {
                nodes.push(wire::NodeDisplays {
                    node_id: node.to_vec(),
                    displays: Vec::new(),
                    online: true,
                });
                nodes.last_mut().expect("just pushed")
            }
        };
        entry.displays.push(wire::Display {
            id: display.id.clone(),
            logical_bounds: Some(rect(display.logical)),
            native_bounds: Some(rect(display.logical)),
            rotation_degrees: 0,
            scale_x: 1.0,
            scale_y: 1.0,
            primary: display.primary,
            native_known: false,
        });
    }
    wire::Topology {
        revision,
        nodes,
        links: layout
            .links
            .iter()
            .map(|link| wire::TransitionLink {
                source_endpoint: Some(endpoint(&link.from)),
                target_endpoint: Some(endpoint(&link.to)),
            })
            .collect(),
        author_node_id: author.as_bytes().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_forwarded_batch_carries_the_target_display_position() {
        use input_model::{InputEvent, KeyState, MouseButton, Position};

        let virtual_position = geometry::Position::new("peer-display", 640.0, 360.0);
        let events = vec![
            InputEvent::PointerAbsolute(Position::new(1919.0, 540.0)),
            InputEvent::Key {
                key: input_model::HidKey::keyboard(0x04),
                state: KeyState::Down,
            },
            InputEvent::Button {
                button: MouseButton::Left,
                state: KeyState::Down,
            },
            InputEvent::Scroll { h120: 0, v120: 120 },
        ];
        let forwarded = forward_events(events.clone(), Some(&virtual_position));
        assert_eq!(
            forwarded[0],
            InputEvent::PointerAbsolute(Position::new(640.0, 360.0)),
            "the source's own coordinates must not reach the peer"
        );
        // Everything that is not a position is passed through untouched.
        assert_eq!(&forwarded[1..], &events[1..]);

        // A batch with no resolved position is not silently emptied.
        assert_eq!(forward_events(events.clone(), None), events);
    }

    #[test]
    fn a_node_space_point_becomes_a_display_local_one() {
        let config = config::TopologyConfig::default();
        let displays = vec![
            info(
                "primary",
                [0.0, 0.0, 1920.0, 1080.0],
                [0, 0, 1920, 1080],
                true,
            ),
            info(
                "left",
                [-1280.0, 0.0, 1280.0, 1024.0],
                [0, 0, 1280, 1024],
                false,
            ),
        ];
        let document = build(&Documents {
            local_node_id: local_id(),
            topology: &config,
            local_displays: &displays,
            peer_displays: &[],
        })
        .expect("two displays and no links");

        // Inside the primary: display-local means subtracting its origin.
        let local = position_on(&document, &NodeId::new(local_id()), 1919.0, 540.0).unwrap();
        assert_eq!(local.display_id.as_str(), "primary");
        assert_eq!((local.x, local.y), (1919.0, 540.0));

        // On the display to the left, with its negative origin.
        let left = position_on(&document, &NodeId::new(local_id()), -1279.0, 512.0).unwrap();
        assert_eq!(left.display_id.as_str(), "left");
        assert_eq!((left.x, left.y), (1.0, 512.0));

        // A point in the gap between monitors belongs to no display.
        assert!(position_on(&document, &NodeId::new(local_id()), -2000.0, 512.0).is_none());
    }

    #[test]
    fn the_snapshot_of_other_nodes_becomes_their_manifest() {
        let text = format!(
            r#"
config_version = 2
[network]
listen_address = "192.168.1.10"
[topology]
revision = 1
author = "self"
[[topology.displays]]
node = "self"
id = "edid:self"
logical = [0, 0, 1920, 1080]
primary = true
[[topology.displays]]
node = "{peer}"
id = "DP-1"
logical = [0, 0, 2560, 1440]
primary = true
[[topology.displays]]
node = "peer:not-hex"
id = "ignored"
logical = [0, 0, 800, 600]
"#,
            peer = config::peer_reference(&peer_id())
        );
        let config = config::Config::from_str(&text).expect("parses").topology;
        let peers = snapshot_peers(&config, &local_id());
        assert_eq!(
            peers.len(),
            1,
            "the placeholder row is skipped, not guessed"
        );
        assert_eq!(peers[0].0, NodeId::new(peer_id()));
        assert_eq!(peers[0].1.len(), 1);
        assert_eq!(peers[0].1[0].id, "DP-1");
        assert_eq!(peers[0].1[0].logical_bounds.as_ref().unwrap().width, 2560.0);
        assert!(
            !peers[0].1[0].native_known,
            "a snapshot has no physical size"
        );
    }

    #[test]
    fn a_dry_document_needs_only_the_configuration() {
        // The smoke case: the agent reports nothing, so both manifests come from the
        // snapshot and the link still resolves.
        let text = format!(
            r#"
config_version = 2
[network]
listen_address = "192.168.1.10"
[topology]
revision = 1
author = "self"
[[topology.displays]]
node = "self"
id = "dry-0"
logical = [0, 0, 1920, 1080]
primary = true
[[topology.displays]]
node = "{peer}"
id = "dry-0"
logical = [0, 0, 1920, 1080]
primary = true
[[topology.links]]
id = "win-right-to-b-left"
from = {{ node = "self", display = "dry-0", edge = "right", start = 0, end = 1080 }}
to = {{ node = "{peer}", display = "dry-0", edge = "left", start = 0, end = 1080 }}
bidirectional = true
"#,
            peer = config::peer_reference(&peer_id())
        );
        let config = config::Config::from_str(&text).expect("parses").topology;
        let self_displays = snapshot_displays(&config, &local_id());
        let peers = snapshot_peers(&config, &local_id());
        let document = build(&Documents {
            local_node_id: local_id(),
            topology: &config,
            local_displays: &self_displays,
            peer_displays: &peers,
        })
        .expect("the snapshot alone is a complete document");

        // A step from just inside the right edge leaves the node.
        let position = geometry::Position::new("dry-0", 1910.0, 540.0);
        let motion = geometry::Point::new(20.0, 0.0);
        let out = geometry::resolve_motion(&document, &NodeId::new(local_id()), &position, motion)
            .expect("right edge");
        assert_eq!(out.target.node, NodeId::new(peer_id()));
        assert!(
            out.target_position.x >= 0.0 && out.target_position.x < 8.0,
            "2 px inside the target"
        );

        // And from 2 px inside the peer's left edge, a step back leaves it in reverse.
        let back = geometry::resolve_motion(
            &document,
            &NodeId::new(peer_id()),
            &out.target_position,
            geometry::Point::new(-20.0, 0.0),
        )
        .expect("the reverse direction of a bidirectional link");
        assert_eq!(back.target.node, NodeId::new(local_id()));
    }

    fn local_id() -> [u8; 32] {
        [0x11; 32]
    }

    fn peer_id() -> [u8; 32] {
        [0xAA; 32]
    }

    fn topology_config() -> config::TopologyConfig {
        let text = format!(
            r#"
config_version = 2
[network]
listen_address = "192.168.1.10"
[topology]
revision = 3
author = "self"
max_seen_revision = 3
max_seen_author = "self"
[[topology.links]]
id = "a-right-to-b-left"
from = {{ node = "self", display = "eDP-1", edge = "right", start = 0, end = 1080 }}
to = {{ node = "{peer}", display = "DP-1", edge = "left", start = 0, end = 1080 }}
bidirectional = true
"#,
            peer = config::peer_reference(&peer_id())
        );
        config::Config::from_str(&text)
            .expect("the test document parses")
            .topology
    }

    fn info(id: &str, logical: [f64; 4], native: [i32; 4], primary: bool) -> ipc::DisplayInfo {
        ipc::DisplayInfo {
            id: id.to_owned(),
            lx: logical[0],
            ly: logical[1],
            lw: logical[2],
            lh: logical[3],
            nx: native[0],
            ny: native[1],
            nw: native[2],
            nh: native[3],
            rotation: 0,
            scale: 1.0,
            primary,
        }
    }

    fn wire_display(id: &str, logical: [f64; 4], primary: bool) -> wire::Display {
        wire::Display {
            id: id.to_owned(),
            logical_bounds: Some(wire::Rect {
                x: logical[0],
                y: logical[1],
                width: logical[2],
                height: logical[3],
            }),
            native_bounds: None,
            rotation_degrees: 0,
            scale_x: 1.0,
            scale_y: 1.0,
            primary,
            native_known: false,
        }
    }

    #[test]
    fn a_document_with_a_local_and_a_peer_manifest_validates() {
        let config = topology_config();
        let local = [info(
            "eDP-1",
            [0.0, 0.0, 1920.0, 1080.0],
            [0, 0, 1920, 1080],
            true,
        )];
        let peers = vec![(
            NodeId::new(peer_id()),
            vec![wire_display("DP-1", [0.0, 0.0, 2560.0, 1440.0], true)],
        )];
        let document = build(&Documents {
            local_node_id: local_id(),
            topology: &config,
            local_displays: &local,
            peer_displays: &peers,
        })
        .expect("a complete document validates");

        assert_eq!(document.revision, 3);
        assert_eq!(document.author, NodeId::new(local_id()), "author = self");
        assert_eq!(document.links.len(), 1);
        assert_eq!(
            document.links[0].id,
            geometry::LinkId::new("a-right-to-b-left")
        );
        assert_eq!(
            document
                .node(&NodeId::new(local_id()))
                .unwrap()
                .displays
                .len(),
            1
        );
        assert_eq!(
            document
                .node(&NodeId::new(peer_id()))
                .unwrap()
                .displays
                .len(),
            1
        );

        // The document the resolver needs is now usable: the local right edge is an exit.
        let position = geometry::Position::new("eDP-1", 1920.0, 540.0);
        let motion = geometry::Point::new(4.0, 0.0);
        let transition =
            geometry::resolve_motion(&document, &NodeId::new(local_id()), &position, motion)
                .expect("the right edge is a link");
        assert_eq!(transition.target.node, NodeId::new(peer_id()));
        assert_eq!(transition.target.display.as_str(), "DP-1");
    }

    #[test]
    fn a_link_to_a_node_without_displays_is_rejected() {
        let config = topology_config();
        let local = [info(
            "eDP-1",
            [0.0, 0.0, 1920.0, 1080.0],
            [0, 0, 1920, 1080],
            true,
        )];
        let error = build(&Documents {
            local_node_id: local_id(),
            topology: &config,
            local_displays: &local,
            peer_displays: &[(NodeId::new(peer_id()), Vec::new())],
        })
        .expect_err("a link endpoint with no display cannot resolve");
        assert!(
            matches!(
                &error,
                BuildError::Invalid(errors)
                    if errors.iter().any(|error| matches!(
                        error,
                        geometry::TopologyError::UnknownDisplay {
                            side: geometry::EndpointSide::Target,
                            ..
                        }
                    ))
            ),
            "{error}"
        );
    }

    #[test]
    fn an_empty_manifest_still_declares_both_members_for_the_editor() {
        let topology = build(&Documents {
            local_node_id: local_id(),
            topology: &config::TopologyConfig::default(),
            local_displays: &[],
            peer_displays: &[(NodeId::new([2; 32]), Vec::new())],
        })
        .expect("empty display manifests still form a document");

        assert_eq!(topology.nodes.len(), 2);
        assert!(topology.nodes.iter().all(|node| node.displays.is_empty()));
    }

    #[test]
    fn a_segment_with_no_length_is_a_document_error_not_a_link_error() {
        // `Config::validate` rejects `start >= end` when the file is *loaded*; a caller
        // that hands over an already-parsed structure bypassing validation still cannot
        // get a usable document, because `validate_topology` is the second gate.
        let mut config = topology_config();
        config.links[0].from.start = 500.0;
        config.links[0].from.end = 100.0;
        let error = build(&Documents {
            local_node_id: local_id(),
            topology: &config,
            local_displays: &[info(
                "eDP-1",
                [0.0, 0.0, 1920.0, 1080.0],
                [0, 0, 1920, 1080],
                true,
            )],
            peer_displays: &[],
        })
        .expect_err("start > end is not a segment");
        match error {
            BuildError::Invalid(errors) => assert!(
                errors
                    .iter()
                    .any(|error| matches!(error, geometry::TopologyError::InvalidSegment { .. })),
                "{errors:?}"
            ),
            other => panic!("expected a document error, got {other}"),
        }
    }

    #[test]
    fn a_node_reference_that_is_not_self_or_hex_is_a_link_error() {
        let mut config = topology_config();
        config.links[0].to.node = "peer:not-hex".to_owned();
        let error = build(&Documents {
            local_node_id: local_id(),
            topology: &config,
            local_displays: &[info(
                "eDP-1",
                [0.0, 0.0, 1920.0, 1080.0],
                [0, 0, 1920, 1080],
                true,
            )],
            peer_displays: &[],
        })
        .expect_err("a peer reference must be hex");
        assert!(matches!(error, BuildError::Links(_)), "{error}");
        assert!(error.to_string().contains("hexadecimal"), "{error}");
    }

    #[test]
    fn the_native_unknown_convention_and_bad_rectangles() {
        let known = display_from_ipc(&info(
            "a",
            [0.0, 0.0, 1920.0, 1080.0],
            [-1920, 0, 1920, 1080],
            true,
        ))
        .expect("usable");
        assert_eq!(known.native_bounds.map(|rect| rect.x), Some(-1920.0));
        assert!(known.primary);

        // Zero native size means "the platform does not expose physical bounds".
        let portal = display_from_ipc(&info(
            "zone-0",
            [0.0, 0.0, 800.0, 600.0],
            [0, 0, 0, 0],
            false,
        ))
        .expect("usable");
        assert_eq!(portal.native_bounds, None);

        // A rectangle with no area, a non-finite one, or a broken scale is not a display.
        assert!(
            display_from_ipc(&info("empty", [0.0, 0.0, 0.0, 600.0], [0, 0, 0, 0], false)).is_none()
        );
        assert!(display_from_ipc(&info(
            "nan",
            [f64::NAN, 0.0, 800.0, 600.0],
            [0, 0, 0, 0],
            false
        ))
        .is_none());
        let mut negative_scale = info("scale", [0.0, 0.0, 800.0, 600.0], [0, 0, 0, 0], false);
        negative_scale.scale = -2.0;
        let clamped = display_from_ipc(&negative_scale).expect("a broken scale falls back to 1.0");
        assert_eq!(clamped.scale, Scale::uniform(1.0));

        // The wire form uses the explicit flag instead of the zero convention.
        let mut wire = wire_display("peer-screen", [0.0, 0.0, 1024.0, 768.0], false);
        assert_eq!(display_from_wire(&wire).unwrap().native_bounds, None);
        wire.native_known = true;
        wire.native_bounds = Some(wire::Rect {
            x: 0.0,
            y: 0.0,
            width: 2048.0,
            height: 1536.0,
        });
        assert_eq!(
            display_from_wire(&wire)
                .unwrap()
                .native_bounds
                .unwrap()
                .width,
            2048.0
        );
        assert!(display_from_wire(&wire::Display::default()).is_none());
    }

    #[test]
    fn the_config_snapshot_fills_in_for_an_agent_that_reports_nothing() {
        let text = format!(
            r#"
config_version = 2
[network]
listen_address = "192.168.1.10"
[topology]
revision = 1
author = "self"
[[topology.displays]]
node = "self"
id = "edid:snapshot"
logical = [0, 0, 2560, 1440]
primary = true
[[topology.displays]]
node = "{peer}"
id = "other"
logical = [0, 0, 800, 600]
"#,
            peer = config::peer_reference(&peer_id())
        );
        let config = config::Config::from_str(&text).expect("parses").topology;
        let snapshot = snapshot_displays(&config, &local_id());
        assert_eq!(snapshot.len(), 1, "only this node's snapshot is local");
        assert_eq!(snapshot[0].id, "edid:snapshot");
        assert_eq!(snapshot[0].lw, 2560.0);
        assert!(snapshot[0].primary);
        assert_eq!(
            (snapshot[0].nw, snapshot[0].nh),
            (0, 0),
            "the snapshot has no native size"
        );

        // And it is enough to build a document when there are no links to resolve.
        let empty = config::TopologyConfig::default();
        let document = build(&Documents {
            local_node_id: local_id(),
            topology: &empty,
            local_displays: &snapshot,
            peer_displays: &[],
        })
        .expect("a document with no links is valid");
        assert!(document.links.is_empty());
    }
}
