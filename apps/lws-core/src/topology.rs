//! The layout document as core assembles it from three sources.
//!
//! `docs/03-architecture.md`, «Топология и движение»: the document is
//! `Topology{revision, author_node_id, nodes[], links[]}`, the links come from
//! `config.example.toml`'s `[[topology.links]]` (the GUI edits the same document), and
//! the manifests are per node: the local one from the agent's `DisplaysReport` (or the
//! `[[topology.displays]]` snapshot when the agent has not answered), each peer's from
//! `Hello.displays`.
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
    /// The local display report: the agent's `DisplaysReport`, or the
    /// `[[topology.displays]]` snapshot when the agent reported nothing.
    pub local_displays: &'a [ipc::DisplayInfo],
    /// One entry per peer whose `Hello` carried displays.
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
    /// The node has no display at all, so no transition can be resolved.
    NoLocalDisplays,
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
            BuildError::NoLocalDisplays => {
                f.write_str("this node reported no displays, so no edge can be resolved")
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
    if local_named.is_empty() {
        return Err(BuildError::NoLocalDisplays);
    }
    nodes.insert(local, local_named);
    for (peer, displays) in documents.peer_displays {
        let converted: Vec<Display> = displays.iter().filter_map(display_from_wire).collect();
        if !converted.is_empty() {
            nodes.insert(*peer, converted);
        }
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

/// The `[[topology.displays]]` snapshot of this node as `DisplayInfo` values.
///
/// Used when the agent has not reported anything yet (dry mode, or a session without an
/// OS input path): the snapshot is what the GUI edited, and the documents call it «снимок
/// для GUI; источник истины — агент узла» — a snapshot is exactly what a fallback needs.
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

#[cfg(test)]
mod tests {
    use super::*;

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
            peer_displays: &[],
        })
        .expect_err("a link endpoint with no display cannot resolve");
        match error {
            // A peer that reported nothing is not added as a node at all, so the link's
            // target is an unknown *node* rather than an unknown display.
            BuildError::Invalid(errors) => assert!(
                errors.iter().any(|error| matches!(
                    error,
                    geometry::TopologyError::UnknownNode {
                        side: geometry::EndpointSide::Target,
                        ..
                    }
                )),
                "{errors:?}"
            ),
            other => panic!("expected a validation failure, got {other}"),
        }
    }

    #[test]
    fn no_local_displays_is_its_own_error() {
        let config = topology_config();
        let error = build(&Documents {
            local_node_id: local_id(),
            topology: &config,
            local_displays: &[],
            peer_displays: &[],
        })
        .expect_err("without a local display nothing can be resolved");
        assert!(matches!(error, BuildError::NoLocalDisplays), "{error}");
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
