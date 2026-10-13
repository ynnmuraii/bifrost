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

#[cfg(test)]
use focus_router as fr;

use geometry::{
    Display, DisplayId, EndpointSide, LinkId, NodeDisplays, NodeId, Rect, Rotation, Scale, Topology,
};
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

/// A link the live document had to leave out because one of its endpoint displays is not
/// in any member's manifest right now.
///
/// The display is not a *structural* defect: a monitor that is switched off, asleep, or
/// renumbered disappears from `EnumDisplayMonitors`, and the link cannot be resolved until
/// it comes back. The link stays in the authored document and becomes active again on the
/// next rebuild once the display is reported — only the resolver's copy is missing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InactiveLink {
    /// The authored link id ([`geometry::TransitionLink::id`]).
    pub id: LinkId,
    /// The endpoints whose display is currently unknown.
    pub missing: Vec<MissingDisplay>,
}

/// One endpoint of an inactive link: the display the manifests do not describe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingDisplay {
    /// Which end of the link.
    pub side: EndpointSide,
    /// The node the display was expected on.
    pub node: NodeId,
    /// The missing display id.
    pub display: DisplayId,
}

/// The assembled live document plus the links it left out.
///
/// [`build`] returns this instead of a bare [`Topology`] so the caller can log every inactive
/// link and carry the state to the GUI; the resolver only ever sees [`Built::topology`], which
/// contains the links whose endpoints are present.
#[derive(Debug)]
pub struct Built {
    /// The document `geometry` resolves against: only links whose endpoints are present.
    pub topology: Topology,
    /// The configured links left out of `topology`, in document order.
    pub inactive: Vec<InactiveLink>,
}

/// The assembled document, or why it cannot be used.
#[derive(Debug)]
pub enum BuildError {
    /// The links themselves are malformed (a `"self"`/`"peer:<hex>"` reference that does
    /// not parse, a segment with `start >= end`): [`config::TopologyConfig`] refuses them.
    Links(config::ConfigError),
    /// The document breaks a *structural* integrity rule of «Доменная модель»: a duplicate
    /// node or link id, a link whose endpoints are on one node, an unknown node, a bad segment
    /// or an overlapping/ambiguous pair of links. A link whose endpoint display is simply not
    /// reported right now is *not* here — it becomes [`Built::inactive`].
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
///
/// A link whose endpoint display is not in the manifests is left out of
/// [`Built::topology`] and reported in [`Built::inactive`] instead of failing the whole
/// document: one monitor that is off, asleep or renumbered must not disable every edge on
/// the displays that *are* present. `geometry::validate_topology` still reports that link as
/// [`geometry::TopologyError::UnknownDisplay`] — this function decides which errors are soft
/// for the *live* document, and leaves every structural rule fatal.
pub fn build(documents: &Documents<'_>) -> Result<Built, BuildError> {
    let mut links = documents
        .topology
        .to_transition_links(&documents.local_node_id)
        .map_err(BuildError::Links)?;

    let local = NodeId::new(documents.local_node_id);
    // A link written before Windows renumbered its displays names the display by an old
    // device name; the manifest says which stable id carries that name now, so the reference
    // is resolved here instead of failing as an unknown display.
    let aliases = display_aliases(documents.local_displays);
    if !aliases.is_empty() {
        for link in &mut links {
            for endpoint in [&mut link.source, &mut link.target] {
                if endpoint.node == local {
                    if let Some(target) = aliases.get(endpoint.display.as_str()) {
                        endpoint.display = DisplayId::new(target.clone());
                    }
                }
            }
        }
    }

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

    // `display = "auto"` names the desktop, not a display: expand every such endpoint against
    // the manifests assembled just above, so the resolver only ever sees concrete display
    // segments and the mapping code stays the one `geometry` already has. A monitor that is
    // switched off, added back or re-arranged changes the manifests and therefore the resolved
    // segments on the next rebuild — no configuration edit.
    let resolution = geometry::resolve_auto_links(&topology);
    for id in &resolution.unresolved {
        tracing::debug!(
            link = %id,
            "auto layout link unresolved: a node it names reports no display on that edge"
        );
    }
    let Topology {
        revision,
        author,
        nodes,
        ..
    } = topology;
    let topology = Topology::new(revision, author, nodes, resolution.links);

    match geometry::validate_topology(&topology) {
        Ok(()) => Ok(Built {
            topology,
            inactive: Vec::new(),
        }),
        Err(errors) => {
            // `UnknownDisplay` is the only soft error: it names a link that cannot resolve
            // because the monitor is not reported, which says nothing about the rest of the
            // document. Every other violation (an unknown node, a duplicate, a bad or
            // overlapping segment) is structural and stays fatal.
            let mut missing: BTreeMap<usize, Vec<MissingDisplay>> = BTreeMap::new();
            let mut fatal = Vec::new();
            for error in errors.into_errors() {
                match error {
                    geometry::TopologyError::UnknownDisplay {
                        link,
                        side,
                        node,
                        display,
                    } => missing.entry(link).or_default().push(MissingDisplay {
                        side,
                        node,
                        display,
                    }),
                    other => fatal.push(other),
                }
            }
            if !fatal.is_empty() {
                return Err(BuildError::Invalid(fatal));
            }

            let inactive: Vec<InactiveLink> = missing
                .iter()
                .map(|(index, missing)| InactiveLink {
                    id: topology.links[*index].id.clone(),
                    missing: missing.clone(),
                })
                .collect();
            let kept = topology
                .links
                .into_iter()
                .enumerate()
                .filter(|(index, _)| !missing.contains_key(index))
                .map(|(_, link)| link)
                .collect();
            let filtered = Topology::new(topology.revision, topology.author, topology.nodes, kept);

            // Removing links can only settle the soft errors; revalidate anyway so a link
            // removal that somehow exposed a structural defect is still refused, not shipped.
            match geometry::validate_topology(&filtered) {
                Ok(()) => Ok(Built {
                    topology: filtered,
                    inactive,
                }),
                Err(errors) => Err(BuildError::Invalid(errors.into_errors())),
            }
        }
    }
}

/// Windows renumbers `\\.\DISPLAYn` when monitors change, so a document edited earlier may
/// name a display by the device name it had then. A manifest entry carries both the stable id
/// and the device name it currently answers at, which is what makes an alias out of that:
/// `device name -> stable id`. Entries without a name, and entries whose name already is the
/// id (the platform had no interface to read), produce no alias.
#[must_use]
pub fn display_aliases(displays: &[ipc::DisplayInfo]) -> BTreeMap<String, String> {
    displays
        .iter()
        .filter(|display| !display.name.is_empty() && display.name != display.id)
        .map(|display| (display.name.clone(), display.id.clone()))
        .collect()
}

/// The `[topology]` document to persist once a stored layout names a display by a device name
/// Windows has since renumbered, or `None` when nothing has to move.
///
/// The rewrite itself is [`config::TopologyConfig::rewrite_display_ids`]; this renders the
/// result as the TOML the ordinary CAS path carries ([`crate::Session::layout_edit_document`]),
/// so the document is written by the coordinator's one writer and never by a side door.
#[must_use]
pub fn alias_layout(
    layout: &config::TopologyConfig,
    local_node_id: &[u8; 32],
    aliases: &BTreeMap<String, String>,
) -> Option<String> {
    let mut rewritten = layout.clone();
    if !rewritten.rewrite_display_ids(local_node_id, aliases) {
        return None;
    }
    match toml::to_string_pretty(&rewritten) {
        Ok(text) => Some(text),
        Err(error) => {
            tracing::warn!(%error, "cannot render the display-id migration for the layout");
            None
        }
    }
}

/// The stable id of the link [`auto_layout`] writes.
pub const DEFAULT_LINK_ID: &str = "auto-right";

/// The layout a fresh pair starts from: one desktop-anchored link to the **right** of this
/// desktop.
///
/// The link is `self right` → `peer left`, bidirectional, and *both* endpoints are
/// `display = "auto"` (`geometry::resolve_auto_links`): the document keeps working when a
/// monitor is switched off, added back or re-arranged, because the core re-resolves the edges
/// against the manifests on the next `DisplaysReport` with no edit.
///
/// `None` — the user's document is never overwritten — unless the applied document has no
/// links *and* nobody has applied a document at all (`revision == 0`: a new pair, or a legacy
/// configuration whose `[topology]` only held display snapshots) or the document is empty
/// (neither links nor displays). A document someone applied is their choice even when it has
/// no links. `None` as well when the workspace does not have exactly one peer: with two or
/// more, where each one goes is the user's call and the GUI is the place to make it.
#[must_use]
pub fn default_layout(
    document: &config::TopologyConfig,
    peers: &[NodeId],
) -> Option<config::TopologyConfig> {
    if !document.links.is_empty() {
        return None;
    }
    if document.revision > 0 && !document.displays.is_empty() {
        return None;
    }
    let mut layout = auto_layout(peers)?;
    layout.displays = document.displays.clone();
    Some(layout)
}

/// [`default_layout`] without the «the applied document is empty» rule, for the explicit
/// `lws-core layout auto` the owner runs once to replace their document.
///
/// `None` unless the workspace has exactly one peer.
#[must_use]
pub fn auto_layout(peers: &[NodeId]) -> Option<config::TopologyConfig> {
    if peers.len() != 1 {
        return None;
    }
    let peer = peers[0];
    Some(config::TopologyConfig {
        displays: Vec::new(),
        links: vec![config::LinkConfig {
            id: DEFAULT_LINK_ID.to_owned(),
            from: config::EndpointConfig {
                node: "self".to_owned(),
                display: config::AUTO_DISPLAY.to_owned(),
                edge: config::Edge::Right,
                start: 0.0,
                end: 0.0,
            },
            to: config::EndpointConfig {
                node: config::peer_reference(peer.as_bytes()),
                display: config::AUTO_DISPLAY.to_owned(),
                edge: config::Edge::Left,
                start: 0.0,
                end: 0.0,
            },
            bidirectional: true,
        }],
        ..config::TopologyConfig::default()
    })
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

/// Resolves a compositor activation through the same topology geometry as pointer samples.
pub fn portal_transition(
    topology: &Topology,
    node: &NodeId,
    activation: &ipc::PortalActivation,
) -> Option<(geometry::Position, geometry::Transition)> {
    let pointer = activation.position.as_ref()?;
    if !pointer.x.is_finite() || !pointer.y.is_finite() {
        return None;
    }
    let edge = match ipc::PortalEdge::try_from(activation.edge).ok()? {
        ipc::PortalEdge::Left => geometry::Edge::Left,
        ipc::PortalEdge::Right => geometry::Edge::Right,
        ipc::PortalEdge::Top => geometry::Edge::Top,
        ipc::PortalEdge::Bottom => geometry::Edge::Bottom,
        ipc::PortalEdge::Unspecified => return None,
    };
    // Activated can report the exclusive right/bottom boundary or its last pixel.
    // Only the named barrier's boundary qualifies; never infer a link from proximity
    // to a different edge, nor from a previously observed pointer.
    let display = topology.node(node)?.displays.iter().find(|display| {
        let b = display.logical_bounds;
        match edge {
            geometry::Edge::Left => {
                (pointer.x - b.x).abs() <= 1.0 && pointer.y >= b.y && pointer.y < b.y + b.height
            }
            geometry::Edge::Right => {
                (pointer.x - b.x - b.width).abs() <= 1.0
                    && pointer.y >= b.y
                    && pointer.y < b.y + b.height
            }
            geometry::Edge::Top => {
                (pointer.y - b.y).abs() <= 1.0 && pointer.x >= b.x && pointer.x < b.x + b.width
            }
            geometry::Edge::Bottom => {
                (pointer.y - b.y - b.height).abs() <= 1.0
                    && pointer.x >= b.x
                    && pointer.x < b.x + b.width
            }
        }
    })?;
    let b = display.logical_bounds;
    let (x, y) = match edge {
        geometry::Edge::Left => (b.x + b.width.min(1.0) / 2.0, pointer.y),
        geometry::Edge::Right => (b.x + b.width - b.width.min(1.0) / 2.0, pointer.y),
        geometry::Edge::Top => (pointer.x, b.y + b.height.min(1.0) / 2.0),
        geometry::Edge::Bottom => (pointer.x, b.y + b.height - b.height.min(1.0) / 2.0),
    };
    let basis = position_on(topology, node, x, y)?;
    let position = geometry::Position::new(basis.display_id, pointer.x - b.x, pointer.y - b.y);
    let motion = match edge {
        geometry::Edge::Left => geometry::Point::new(-position.x - 1.0, 0.0),
        geometry::Edge::Right => geometry::Point::new(b.width - position.x + 1.0, 0.0),
        geometry::Edge::Top => geometry::Point::new(0.0, -position.y - 1.0),
        geometry::Edge::Bottom => geometry::Point::new(0.0, b.height - position.y + 1.0),
    };
    let transition = geometry::resolve_motion(topology, node, &position, motion)?;
    if transition.source.edge != edge {
        return None;
    }
    Some((position, transition))
}

/// Maps a captured source-node point to display-local coordinates.
///
/// Local observation follows physical displays and ignores internal gaps. Remote
/// capture integrates raw deltas into a virtual source point, which may cross any
/// physical edge or gap; retain its original display basis to preserve those deltas.
pub fn capture_position(
    topology: &Topology,
    node: &NodeId,
    x: f64,
    y: f64,
    last: Option<&geometry::Position>,
    remote: bool,
) -> Option<geometry::Position> {
    if remote {
        let last = last?;
        let display = topology.display(node, &last.display_id)?;
        return Some(geometry::Position::new(
            last.display_id.clone(),
            x - display.logical_bounds.x,
            y - display.logical_bounds.y,
        ));
    }
    if let Some(position) = position_on(topology, node, x, y) {
        return Some(position);
    }

    let member = topology.node(node)?;
    if member
        .bounds()
        .is_some_and(|bounds| bounds.contains(geometry::Point::new(x, y)))
    {
        return None;
    }
    let last = last?;
    let display = member.display(&last.display_id)?;
    Some(geometry::Position::new(
        last.display_id.clone(),
        x - display.logical_bounds.x,
        y - display.logical_bounds.y,
    ))
}

/// Advances the receiver estimate and resolves a return/chained edge before clamping.
pub fn remote_step(
    topology: &Topology,
    node: &NodeId,
    position: &geometry::Position,
    motion: geometry::Point,
) -> (geometry::Position, Option<geometry::Transition>) {
    let crossing = geometry::resolve_motion(topology, node, position, motion);
    let mut advanced = geometry::Position::new(
        position.display_id.clone(),
        position.x + motion.x,
        position.y + motion.y,
    );
    if let Some(display) = topology.display(node, &position.display_id) {
        let size = display.logical_size();
        // EIS encodes positions as f32: keep the exclusive edge exclusive after
        // that conversion too, rather than rounding a valid f64 up to the edge.
        let max_x = f64::from((size.width as f32).next_down()).max(0.0);
        let max_y = f64::from((size.height as f32).next_down()).max(0.0);
        advanced.x = advanced.x.clamp(0.0, max_x);
        advanced.y = advanced.y.clamp(0.0, max_y);
    }
    (advanced, crossing)
}

/// The axis of the edge a return came back through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// The horizontal axis.
    X,
    /// The vertical axis.
    Y,
}

/// The band of the edge a pointer came home through: the edge is disarmed until the
/// pointer leaves it.
///
/// `[input] edge_hysteresis_px` was parsed and never read (audit F6), so a pointer that
/// returned sitting *on* the edge resolved a fresh crossing on the very next sample that
/// pushed the same way, and the focus oscillated `Local`/`Remote` once per sample — the
/// owner's cursor jumping back and forth at the boundary. This is that hysteresis: the
/// axis and the sign of the step that carried the pointer out, the edge's own coordinate
/// on that axis, and the band width. It holds while the pointer is still inside the band
/// and lets go as soon as it is clear of it, which is also when the edge may trigger
/// again.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReturnGuard {
    axis: Axis,
    /// `+1.0` for the right/bottom edge, `-1.0` for the left/top one.
    outward: f64,
    /// The edge's coordinate on `axis`: the display's width or height, or `0.0`.
    edge: f64,
    hysteresis: f64,
}

impl ReturnGuard {
    /// The guard a return through `step` arms, or `None` for a step with no axis.
    ///
    /// `step` is the captured delta that crossed the edge, in the *local* node's frame:
    /// its dominant axis is the edge and its sign says which side of the display it left by.
    #[must_use]
    pub fn after_return(
        step: geometry::Point,
        width: f64,
        height: f64,
        hysteresis: f64,
    ) -> Option<ReturnGuard> {
        let (axis, outward, edge) = if step.x.abs() >= step.y.abs() && step.x != 0.0 {
            (
                Axis::X,
                step.x.signum(),
                if step.x > 0.0 { width } else { 0.0 },
            )
        } else if step.y != 0.0 {
            (
                Axis::Y,
                step.y.signum(),
                if step.y > 0.0 { height } else { 0.0 },
            )
        } else {
            return None;
        };
        Some(ReturnGuard {
            axis,
            outward,
            edge,
            hysteresis,
        })
    }

    /// Whether `position` is still inside the band, i.e. the edge is still disarmed.
    #[must_use]
    pub fn holds(&self, position: &geometry::Position) -> bool {
        let coordinate = match self.axis {
            Axis::X => position.x,
            Axis::Y => position.y,
        };
        (self.edge - coordinate) * self.outward <= self.hysteresis
    }

    /// Where the pointer is placed when it comes home: the edge inset by the band.
    #[must_use]
    pub fn entry(&self, position: &geometry::Position) -> geometry::Position {
        let mut entry = position.clone();
        let inset = self.edge - self.outward * self.hysteresis;
        match self.axis {
            Axis::X => entry.x = inset,
            Axis::Y => entry.y = inset,
        }
        entry
    }

    /// The axis this band runs perpendicular to.
    #[must_use]
    pub fn axis(&self) -> Axis {
        self.axis
    }

    /// The band width in logical units.
    #[must_use]
    pub fn hysteresis(&self) -> f64 {
        self.hysteresis
    }
}

/// Where a transition to a peer starts when there is no edge to cross (batch-2 audit).
///
/// A manual `switch <peer>` (and any transition whose document is unusable) has no crossing
/// to derive an entry point from. The anchor is the receiver's **primary** display (its first
/// when none is flagged), and the position is display-local — `protocol.proto` `Position` is
/// «local to display top-left» — at that display's centre.
///
/// The `display_id` is the part that matters beyond this function: `transport` stamps every
/// later `InputBatch` of the lease with the display of the `EnterPrepare` that opened it
/// (`convert::batch_display_after`), so a source that started a lease with an empty id sent
/// the whole pointer stream with an empty one — which a receiver refuses, and which the
/// batch-1 audit then made it drop in silence.
#[must_use]
pub fn start_position(displays: &[wire::Display]) -> Option<geometry::Position> {
    let display = displays
        .iter()
        .find(|display| display.primary)
        .or_else(|| displays.first())?;
    if display.id.is_empty() {
        return None;
    }
    let bounds = display.logical_bounds.as_ref()?;
    if bounds.width <= 0.0 || bounds.height <= 0.0 {
        return None;
    }
    Some(geometry::Position::new(
        display.id.clone(),
        bounds.width / 2.0,
        bounds.height / 2.0,
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
            // A snapshot has no OS device name: it was written by the GUI, not read from the
            // platform, so it can neither be an alias source nor a target.
            name: String::new(),
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

/// The editable `[topology]` view of a wire document, retaining its graph for later rebuilds.
///
/// Node/display manifests are carried on the wire rather than in the configuration schema.
/// They become display snapshots here; the receiver's next live rebuild still uses its agents.
pub fn config_from_wire(
    document: &wire::Topology,
    self_node: NodeId,
) -> Result<config::TopologyConfig, String> {
    let author: [u8; 32] = document
        .author_node_id
        .as_slice()
        .try_into()
        .map_err(|_| "topology author is not 32 bytes".to_owned())?;
    let node_reference = |bytes: &[u8]| -> Result<String, String> {
        let node: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "topology node id is not 32 bytes".to_owned())?;
        if node == *self_node.as_bytes() {
            Ok("self".to_owned())
        } else {
            Ok(format!(
                "peer:{}",
                node.iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            ))
        }
    };
    let endpoint = |endpoint: &wire::Endpoint| -> Result<config::EndpointConfig, String> {
        let edge = match wire::Edge::try_from(endpoint.edge)
            .map_err(|_| "topology link has an unknown edge".to_owned())?
        {
            wire::Edge::Left => config::Edge::Left,
            wire::Edge::Right => config::Edge::Right,
            wire::Edge::Top => config::Edge::Top,
            wire::Edge::Bottom => config::Edge::Bottom,
            wire::Edge::Unspecified => {
                return Err("topology link has an unspecified edge".to_owned());
            }
        };
        Ok(config::EndpointConfig {
            node: node_reference(&endpoint.node_id)?,
            display: endpoint.display_id.clone(),
            edge,
            start: endpoint.segment_start,
            end: endpoint.segment_end,
        })
    };
    let author_hex = author
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mut layout = config::TopologyConfig {
        revision: document.revision,
        author: author_hex.clone(),
        max_seen_revision: document.revision,
        max_seen_author: author_hex,
        ..config::TopologyConfig::default()
    };
    for node in &document.nodes {
        let reference = node_reference(&node.node_id)?;
        for display in &node.displays {
            let Some(bounds) = &display.logical_bounds else {
                continue;
            };
            layout.displays.push(config::DisplayConfig {
                node: reference.clone(),
                id: display.id.clone(),
                logical: [bounds.x, bounds.y, bounds.width, bounds.height],
                primary: display.primary,
            });
        }
    }
    for (index, link) in document.links.iter().enumerate() {
        let from = link
            .source_endpoint
            .as_ref()
            .ok_or_else(|| "topology link has no source endpoint".to_owned())?;
        let to = link
            .target_endpoint
            .as_ref()
            .ok_or_else(|| "topology link has no target endpoint".to_owned())?;
        layout.links.push(config::LinkConfig {
            id: format!("link-{index}"),
            from: endpoint(from)?,
            to: endpoint(to)?,
            bidirectional: true,
        });
    }
    Ok(layout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_activation_maps_the_configured_zone_edge_without_a_previous_sample() {
        let config = topology_config();
        let local = [info(
            "eDP-1",
            [-1920.0, -200.0, 1920.0, 1080.0],
            [0, 0, 1920, 1080],
            true,
        )];
        let peers = [(
            NodeId::new(peer_id()),
            vec![wire_display("DP-1", [0.0, 0.0, 2560.0, 1440.0], true)],
        )];
        let document = build(&Documents {
            local_node_id: local_id(),
            topology: &config,
            local_displays: &local,
            peer_displays: &peers,
        })
        .expect("valid topology")
        .topology;
        // The portal reports the last logical pixel on the right barrier.
        let mut activation = ipc::PortalActivation {
            position: Some(ipc::PointerAbs {
                display_id: String::new(),
                x: -1.0,
                y: 340.0,
            }),
            edge: ipc::PortalEdge::Right as i32,
        };
        let (source, transition) =
            portal_transition(&document, &NodeId::new(local_id()), &activation)
                .expect("activation is an outward crossing, even without prior motion");
        assert_eq!(source.display_id.as_str(), "eDP-1");
        assert_eq!((source.x, source.y), (1919.0, 540.0));
        assert_eq!(transition.target.node, NodeId::new(peer_id()));
        assert_eq!(
            (transition.target_position.x, transition.target_position.y),
            (2.0, 540.0)
        );
        activation.position.as_mut().unwrap().x = 0.0;
        assert!(
            portal_transition(&document, &NodeId::new(local_id()), &activation).is_some(),
            "an exact outer boundary belongs to its portal zone"
        );
        activation.edge = ipc::PortalEdge::Top as i32;
        assert!(
            portal_transition(&document, &NodeId::new(local_id()), &activation).is_none(),
            "a different barrier cannot trigger the right-edge link"
        );
        activation.edge = ipc::PortalEdge::Unspecified as i32;
        assert!(portal_transition(&document, &NodeId::new(local_id()), &activation).is_none());
        activation.position = None;
        assert!(portal_transition(&document, &NodeId::new(local_id()), &activation).is_none());
    }

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
            InputEvent::PointerRelative { dx: -7, dy: 12 },
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
        .expect("two displays and no links")
        .topology;

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
    fn a_pointer_sample_beyond_the_desktop_edge_can_resolve_a_link() {
        let config = topology_config();
        let local = [
            info(
                "eDP-1",
                [0.0, 0.0, 1080.0, 1920.0],
                [0, 0, 1080, 1920],
                true,
            ),
            info(
                "left",
                [-1280.0, 0.0, 1280.0, 1024.0],
                [0, 0, 1280, 1024],
                false,
            ),
        ];
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
        .expect("the linked displays form a valid document")
        .topology;
        let node = NodeId::new(local_id());
        let last = position_on(&document, &node, 1079.0, 959.0).unwrap();

        // SendInput can report its requested point even though Windows clamps the actual
        // cursor to x=1079 on this 1080-wide virtual desktop.
        let current = capture_position(&document, &node, 1087.0, 959.0, Some(&last), false)
            .expect("the overshoot continues on the last display");
        assert_eq!(current, geometry::Position::new("eDP-1", 1087.0, 959.0));
        let motion = geometry::Point::new(current.x - last.x, current.y - last.y);
        let transition =
            geometry::resolve_motion(&document, &node, &current, motion).expect("right edge link");
        assert_eq!(transition.target.node, NodeId::new(peer_id()));

        assert!(capture_position(&document, &node, -100.0, 1500.0, Some(&last), false).is_none());
        assert!(capture_position(&document, &node, 1087.0, 959.0, None, false).is_none());
    }

    #[test]
    fn remote_virtual_source_motion_is_not_lost_in_a_physical_monitor_gap() {
        let config = config::TopologyConfig::default();
        let local = [
            info(
                "primary",
                [0.0, 0.0, 1920.0, 1080.0],
                [0, 0, 1920, 1080],
                true,
            ),
            info(
                "right",
                [3200.0, 0.0, 800.0, 600.0],
                [3200, 0, 800, 600],
                false,
            ),
        ];
        let document = build(&Documents {
            local_node_id: local_id(),
            topology: &config,
            local_displays: &local,
            peer_displays: &[],
        })
        .unwrap()
        .topology;
        let node = NodeId::new(local_id());
        let last = geometry::Position::new("primary", 1919.0, 540.0);
        let current = capture_position(&document, &node, 1931.0, 546.0, Some(&last), true)
            .expect("Remote uses the virtual source point, not physical screen membership");
        assert_eq!(current, geometry::Position::new("primary", 1931.0, 546.0));
        assert_eq!(
            geometry::Point::new(current.x - last.x, current.y - last.y),
            geometry::Point::new(12.0, 6.0)
        );
    }

    /// A pointer that comes home must not re-open the lease by being pushed at the edge it
    /// came home through: that oscillation is the owner's jumping cursor, and
    /// `input.edge_hysteresis_px` is the band that stops it.
    #[test]
    fn a_return_disarms_the_edge_until_the_pointer_leaves_the_band() {
        let (width, height) = (1080.0_f64, 1920.0_f64);
        let guard = ReturnGuard::after_return(geometry::Point::new(100.0, 0.0), width, height, 8.0)
            .expect("a horizontal return arms the guard");
        let home = guard.entry(&geometry::Position::new("eDP-1", width - 1.0, 540.0));
        assert_eq!(home.x, width - 8.0, "the entry point is inside the band");
        for x in [home.x, home.x + 1.0, width - 1.0, width] {
            let sample = geometry::Position::new("eDP-1", x, 540.0);
            assert!(guard.holds(&sample), "a push at {x} is inside the band");
        }
        let clear = geometry::Position::new("eDP-1", home.x - 10.0, 540.0);
        assert!(
            !guard.holds(&clear),
            "ten pixels inward is clear of the band"
        );
        assert_eq!(guard.axis(), Axis::X);
        assert_eq!(guard.hysteresis(), 8.0);
    }

    /// The band is the same on the other three edges.
    #[test]
    fn every_edge_arms_its_own_band() {
        let (width, height) = (1080.0_f64, 1920.0_f64);
        for (step, x, y, inside) in [
            (geometry::Point::new(-100.0, 0.0), 8.0, 540.0, 18.0),
            (geometry::Point::new(0.0, -100.0), 540.0, 8.0, 18.0),
            (
                geometry::Point::new(0.0, 100.0),
                540.0,
                height - 8.0,
                height - 18.0,
            ),
        ] {
            let guard = ReturnGuard::after_return(step, width, height, 8.0)
                .expect("a return with an axis arms the guard");
            let home = guard.entry(&geometry::Position::new("eDP-1", width / 2.0, height / 2.0));
            let held = geometry::Position::new(
                "eDP-1",
                if home.x == width / 2.0 { x } else { home.x },
                if home.y == height / 2.0 { y } else { home.y },
            );
            assert!(guard.holds(&held), "{step:?} is still inside its band");
            let clear = geometry::Position::new(
                "eDP-1",
                if held.x == x { inside } else { held.x },
                if held.y == y { inside } else { held.y },
            );
            assert!(!guard.holds(&clear), "{step:?} is clear of its band");
        }
    }

    /// A coarse return step must cross the entry edge exactly like a fine one.
    ///
    /// `remote_step` resolves the crossing from the *unclamped* motion and only then clamps
    /// the estimate, so the size of a captured delta cannot decide whether the pointer is
    /// allowed to leave the peer's display: a fast flick has to come home. A live probe once
    /// read as «-32 px never returns while -16 px does»; the step was innocent — the probe's
    /// own status polling was aliasing with the boundary ping-pong the same motion produces —
    /// and this pins the property the estimate actually has.
    #[test]
    fn a_coarse_return_step_crosses_like_a_fine_one() {
        let config = topology_config();
        let local = [info(
            "eDP-1",
            [0.0, 0.0, 1920.0, 1080.0],
            [0, 0, 1920, 1080],
            true,
        )];
        let peers = [(
            NodeId::new(peer_id()),
            vec![wire_display("DP-1", [0.0, 0.0, 1745.0, 1091.0], true)],
        )];
        let document = build(&Documents {
            local_node_id: local_id(),
            topology: &config,
            local_displays: &local,
            peer_displays: &peers,
        })
        .unwrap()
        .topology;
        let peer = NodeId::new(peer_id());
        let mut steps = Vec::new();
        for step in [-16.0_f64, -32.0, -100.0, -400.0] {
            let mut position = geometry::Position::new("DP-1", 100.0, 540.0);
            let mut taken = 0;
            loop {
                let (advanced, crossing) =
                    remote_step(&document, &peer, &position, geometry::Point::new(step, 0.0));
                taken += 1;
                match crossing {
                    Some(transition) => {
                        assert_eq!(
                            transition.target.node,
                            NodeId::new(local_id()),
                            "a {step} px step returns to the local node"
                        );
                        break;
                    }
                    None => position = advanced,
                }
                assert!(taken < 100, "a {step} px step never crossed in 100 pushes");
            }
            steps.push(taken);
        }
        assert!(
            steps.windows(2).all(|pair| pair[0] >= pair[1]),
            "crossing must not need more pushes as the step grows: {steps:?}"
        );
    }

    #[test]
    fn remote_motion_accumulates_clamps_and_resolves_the_return_edge() {
        let config = topology_config();
        let local = [info(
            "eDP-1",
            [0.0, 0.0, 1920.0, 1080.0],
            [0, 0, 1920, 1080],
            true,
        )];
        let peers = [(
            NodeId::new(peer_id()),
            vec![wire_display("DP-1", [0.0, 0.0, 1745.0, 1091.0], true)],
        )];
        let document = build(&Documents {
            local_node_id: local_id(),
            topology: &config,
            local_displays: &local,
            peer_displays: &peers,
        })
        .unwrap()
        .topology;
        let peer = NodeId::new(peer_id());
        let mut position = geometry::Position::new("DP-1", 2.0, 540.0);
        let mut source_position = geometry::Position::new("eDP-1", 1919.0, 540.0);
        let captured = ipc::CapturedBatch {
            events: [(1931.0, 546.0), (1943.0, 552.0)]
                .into_iter()
                .map(|(x, y)| ipc::LocalEvent {
                    monotonic_us: 0,
                    ev: Some(ipc::local_event::Ev::Pointer(ipc::PointerAbs {
                        display_id: String::new(),
                        x,
                        y,
                    })),
                })
                .collect(),
            at_edge: false,
            activation: None,
        };
        for (event, (expected_x, expected_y)) in
            captured.events.iter().zip([(14.0, 546.0), (26.0, 552.0)])
        {
            let Some(ipc::local_event::Ev::Pointer(pointer)) = &event.ev else {
                panic!("expected source pointer");
            };
            let current = capture_position(
                &document,
                &NodeId::new(local_id()),
                pointer.x,
                pointer.y,
                Some(&source_position),
                true,
            )
            .unwrap();
            let motion =
                geometry::Point::new(current.x - source_position.x, current.y - source_position.y);
            source_position = current;
            let (advanced, crossing) = remote_step(&document, &peer, &position, motion);
            assert!(crossing.is_none());
            position = advanced;
            let message = focus_router::Message::InputBatch {
                lease_id: [7; 16],
                input_batch_seq: 0,
                events: forward_events(
                    vec![crate::routing::input_event_of(event).unwrap()],
                    Some(&position),
                ),
            };
            let envelope =
                transport::convert::to_envelope(&message, 1, &[2; 16], Some(&[7; 16]), "DP-1")
                    .unwrap();
            let focus_router::Message::InputBatch { events, .. } =
                transport::convert::from_envelope(&envelope).unwrap()
            else {
                panic!("expected received InputBatch");
            };
            assert_eq!(
                events,
                vec![input_model::InputEvent::PointerAbsolute(
                    input_model::Position::new(expected_x, expected_y)
                )]
            );
            assert_eq!(position.x, expected_x);
        }
        assert_eq!(position.y, 552.0);
        let (clamped, crossing) = remote_step(
            &document,
            &peer,
            &position,
            geometry::Point::new(3000.0, 2000.0),
        );
        assert!(crossing.is_none(), "an unlinked edge does not open a lease");
        assert!(clamped.x >= 0.0 && clamped.x < 1745.0);
        assert!(clamped.y >= 0.0 && clamped.y < 1091.0);
        for expected_x in [14.0, 2.0] {
            let (advanced, crossing) = remote_step(
                &document,
                &peer,
                &position,
                geometry::Point::new(-12.0, 0.0),
            );
            assert!(crossing.is_none());
            position = advanced;
            assert_eq!(position.x, expected_x);
        }
        let (_, crossing) =
            remote_step(&document, &peer, &position, geometry::Point::new(-3.0, 0.0));
        assert_eq!(
            crossing
                .expect("accumulated deltas cross the entry edge")
                .target
                .node,
            NodeId::new(local_id())
        );
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
        .expect("the snapshot alone is a complete document")
        .topology;

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
        named_info(id, "", logical, native, primary)
    }

    /// A manifest entry with the OS device name it answers at (`\\.\DISPLAY1` on Windows).
    fn named_info(
        id: &str,
        name: &str,
        logical: [f64; 4],
        native: [i32; 4],
        primary: bool,
    ) -> ipc::DisplayInfo {
        ipc::DisplayInfo {
            id: id.to_owned(),
            name: name.to_owned(),
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

    /// Batch-2 audit: a transition with no edge starts on the receiver's primary display, and
    /// the wire carries that display id — the value every later batch of the lease is stamped
    /// with.
    #[test]
    fn a_transition_without_an_edge_starts_on_the_receivers_primary_display() {
        let displays = vec![
            wire_display("HDMI-1", [0.0, 0.0, 1920.0, 1080.0], false),
            wire_display("eDP-1", [1920.0, 0.0, 2560.0, 1440.0], true),
        ];
        let position = start_position(&displays).expect("a primary display is an anchor");
        assert_eq!(
            position.display_id.as_str(),
            "eDP-1",
            "the primary display anchors"
        );
        assert_eq!(
            (position.x, position.y),
            (1280.0, 720.0),
            "display-local coordinates: the centre of the display"
        );

        // Without a primary flag the first display is used, so a manifest that never set the
        // flag still gives the lease a display.
        let unmarked = vec![wire_display("DP-1", [0.0, 0.0, 800.0, 600.0], false)];
        let first = start_position(&unmarked).expect("the first display is the fallback");
        assert_eq!(first.display_id.as_str(), "DP-1");
        assert_eq!((first.x, first.y), (400.0, 300.0));

        // Nothing to anchor on: the caller falls back to its neutral position and says so.
        assert_eq!(start_position(&[]), None);
        assert_eq!(
            start_position(&[wire_display("", [0.0, 0.0, 100.0, 100.0], true)]),
            None,
            "a display with no id cannot be named on the wire"
        );
    }

    /// Batch-2 audit, the wire half: the entry position the switch seeds becomes the display
    /// every `InputBatch` of that lease carries, and the position inside the batch is inside
    /// the receiver's display rather than the source's node space.
    #[test]
    fn the_first_batch_of_a_manual_switch_carries_the_receivers_display() {
        use transport::convert;

        let displays = vec![wire_display("eDP-1", [1920.0, 0.0, 1600.0, 900.0], true)];
        let entry = start_position(&displays).expect("a display to start on");
        let prepare = fr::Message::EnterPrepare {
            lease_id: [9u8; 16],
            topology_revision: 0,
            position: entry.clone(),
            owner_serial: 1,
        };
        let lease = [9u8; 16];
        // This is exactly what the link does: the batch display comes from the `EnterPrepare`.
        let batch_display = convert::batch_display_after(&prepare, "");
        assert_eq!(
            batch_display, "eDP-1",
            "the lease opens on the receiver's display"
        );
        let batch = fr::Message::InputBatch {
            lease_id: lease,
            input_batch_seq: 0,
            events: vec![input_model::InputEvent::PointerAbsolute(
                input_model::Position::new(entry.x, entry.y),
            )],
        };
        let envelope = convert::to_envelope(&batch, 2, &[1u8; 16], Some(&lease), &batch_display)
            .expect("the batch converts");
        let back = convert::from_envelope(&envelope).expect("the receiver accepts the batch");
        let fr::Message::InputBatch { events, .. } = back else {
            panic!("expected an InputBatch");
        };
        let input_model::InputEvent::PointerAbsolute(position) = events[0] else {
            panic!("expected an absolute position");
        };
        assert!(
            position.x >= 0.0 && position.x <= 1600.0 && position.y >= 0.0 && position.y <= 900.0,
            "the position must be inside the receiver's display, got {position:?}"
        );
        assert_eq!((position.x, position.y), (800.0, 450.0), "the centre");

        // The position `Session::switch_to` used to carry: an empty display id. On the wire it
        // blanks every batch of the lease and the receiver refuses each one, which is the
        // defect this anchor removes.
        let lease_id = [9u8; 16];
        let neutral = fr::Message::EnterPrepare {
            lease_id,
            topology_revision: 0,
            position: geometry::Position::new(String::new(), 0.0, 0.0),
            owner_serial: 1,
        };
        assert_eq!(
            convert::batch_display_after(&neutral, ""),
            "",
            "an empty entry display id is what every later batch is stamped with"
        );
        let blanked = fr::Message::InputBatch {
            lease_id,
            input_batch_seq: 0,
            events: vec![input_model::InputEvent::PointerAbsolute(
                input_model::Position::new(0.0, 0.0),
            )],
        };
        let envelope = convert::to_envelope(&blanked, 2, &[1u8; 16], Some(&lease), "")
            .expect("the source can encode it");
        assert!(
            convert::from_envelope(&envelope).is_err(),
            "and the receiver cannot read it"
        );
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
        .expect("a complete document validates")
        .topology;

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

    /// A Windows monitor's stable id and the device name it currently answers at.
    const STABLE_WINDOWS_ID: &str =
        "\\\\?\\DISPLAY#DEL4098#5&2a4b1c3&0&UID4353#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
    const WINDOWS_DEVICE_NAME: &str = "\\\\.\\DISPLAY1";

    /// A document written before the device names were renumbered: the display row and the link
    /// name the monitor by `\\.\DISPLAY1`, plus a peer row that must never be rewritten.
    fn renumbered_layout() -> config::TopologyConfig {
        let mut config = config::TopologyConfig::default();
        config.displays.push(config::DisplayConfig {
            node: "self".to_owned(),
            id: WINDOWS_DEVICE_NAME.to_owned(),
            logical: [0.0, 0.0, 1920.0, 1080.0],
            primary: true,
        });
        config.displays.push(config::DisplayConfig {
            node: config::peer_reference(&peer_id()),
            id: WINDOWS_DEVICE_NAME.to_owned(),
            logical: [0.0, 0.0, 1920.0, 1080.0],
            primary: true,
        });
        config.links.push(config::LinkConfig {
            id: "self-to-peer".to_owned(),
            from: config::EndpointConfig {
                node: "self".to_owned(),
                display: WINDOWS_DEVICE_NAME.to_owned(),
                edge: config::Edge::Right,
                start: 0.0,
                end: 1080.0,
            },
            to: config::EndpointConfig {
                node: config::peer_reference(&peer_id()),
                display: "DP-1".to_owned(),
                edge: config::Edge::Left,
                start: 0.0,
                end: 1080.0,
            },
            bidirectional: true,
        });
        config
    }

    #[test]
    fn an_alias_table_maps_device_names_and_ignores_the_rest() {
        let displays = [
            named_info(
                STABLE_WINDOWS_ID,
                WINDOWS_DEVICE_NAME,
                [0.0, 0.0, 1920.0, 1080.0],
                [0, 0, 1920, 1080],
                true,
            ),
            // An id that already is the platform name (no interface was read) is no alias.
            named_info(
                "eDP-1",
                "eDP-1",
                [0.0, 0.0, 800.0, 600.0],
                [0, 0, 0, 0],
                false,
            ),
            // A GUI snapshot carries no device name.
            info("snapshot", [0.0, 0.0, 1024.0, 768.0], [0, 0, 0, 0], false),
        ];
        let aliases = display_aliases(&displays);
        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[WINDOWS_DEVICE_NAME], STABLE_WINDOWS_ID);
    }

    #[test]
    fn a_link_naming_a_renumbered_device_name_resolves_through_the_alias() {
        let config = renumbered_layout();
        // The agent now reports the monitor under its stable interface id, carrying the device
        // name it answers at; the stored link still names `\\.\DISPLAY1`.
        let local = [named_info(
            STABLE_WINDOWS_ID,
            WINDOWS_DEVICE_NAME,
            [0.0, 0.0, 1920.0, 1080.0],
            [0, 0, 1920, 1080],
            true,
        )];
        let peers = [(
            NodeId::new(peer_id()),
            vec![wire_display("DP-1", [0.0, 0.0, 2560.0, 1440.0], true)],
        )];

        let built = build(&Documents {
            local_node_id: local_id(),
            topology: &config,
            local_displays: &local,
            peer_displays: &peers,
        })
        .expect("the alias resolves the old device name");

        assert!(built.inactive.is_empty());
        assert_eq!(built.topology.links.len(), 1);
        assert_eq!(
            built.topology.links[0].source.display.as_str(),
            STABLE_WINDOWS_ID,
            "the endpoint moved to the display that answers at the old device name"
        );
        let local_node = built.topology.node(&NodeId::new(local_id())).unwrap();
        assert!(local_node
            .display(&DisplayId::new(STABLE_WINDOWS_ID))
            .is_some());
    }

    #[test]
    fn a_renumbered_layout_renders_a_migration_and_leaves_peers_alone() {
        let aliases =
            BTreeMap::from([(WINDOWS_DEVICE_NAME.to_owned(), STABLE_WINDOWS_ID.to_owned())]);
        let text = alias_layout(&renumbered_layout(), &local_id(), &aliases)
            .expect("the local rows move, so a document is rendered");
        let parsed: config::TopologyConfig =
            toml::from_str(&text).expect("the migration renders a readable layout document");

        assert_eq!(parsed.displays[0].id, STABLE_WINDOWS_ID);
        assert_eq!(
            parsed.displays[1].id, WINDOWS_DEVICE_NAME,
            "a peer id is its own"
        );
        assert_eq!(parsed.links[0].from.display, STABLE_WINDOWS_ID);
        assert_eq!(parsed.links[0].to.display, "DP-1");

        // Nothing to move: no alias, or an alias the document does not name.
        assert!(alias_layout(&renumbered_layout(), &local_id(), &BTreeMap::new()).is_none());
        let unrelated =
            BTreeMap::from([("\\\\.\\DISPLAY9".to_owned(), STABLE_WINDOWS_ID.to_owned())]);
        assert!(alias_layout(&renumbered_layout(), &local_id(), &unrelated).is_none());
    }

    #[test]
    fn a_migration_that_would_duplicate_a_display_keeps_the_stable_row() {
        let mut config = renumbered_layout();
        // The document already carries the stable id as well (the GUI saved the live editor):
        // rewriting the old row must not leave two rows with the same id.
        config.displays.insert(
            1,
            config::DisplayConfig {
                node: "self".to_owned(),
                id: STABLE_WINDOWS_ID.to_owned(),
                logical: [0.0, 0.0, 1920.0, 1080.0],
                primary: true,
            },
        );
        let aliases =
            BTreeMap::from([(WINDOWS_DEVICE_NAME.to_owned(), STABLE_WINDOWS_ID.to_owned())]);
        let text = alias_layout(&config, &local_id(), &aliases).expect("the old row moves");
        let parsed: config::TopologyConfig = toml::from_str(&text).expect("readable document");
        let self_rows: Vec<&str> = parsed
            .displays
            .iter()
            .filter(|display| display.node == "self")
            .map(|display| display.id.as_str())
            .collect();
        assert_eq!(self_rows, vec![STABLE_WINDOWS_ID]);
    }

    #[test]
    fn a_document_with_no_alias_keeps_the_fatal_missing_display() {
        // No manifest name matches `\\.\DISPLAY1`, so there is nothing to alias and the link
        // is still inactive (2b558c1 behaviour), not resolved to some other display.
        let config = renumbered_layout();
        let local = [info(
            "eDP-1",
            [0.0, 0.0, 1920.0, 1080.0],
            [0, 0, 1920, 1080],
            true,
        )];
        let peers = [(
            NodeId::new(peer_id()),
            vec![wire_display("DP-1", [0.0, 0.0, 2560.0, 1440.0], true)],
        )];
        let built = build(&Documents {
            local_node_id: local_id(),
            topology: &config,
            local_displays: &local,
            peer_displays: &peers,
        })
        .expect("an unknown display is inactive, not fatal");
        assert!(built.topology.links.is_empty());
        assert_eq!(built.inactive.len(), 1);
        assert_eq!(
            built.inactive[0].missing[0].display.as_str(),
            WINDOWS_DEVICE_NAME
        );
    }

    /// Two links, one per half of a local display's right edge and of the peer's left edge.
    ///
    /// The second link names `DISPLAY1` on this node, so a caller that omits that display
    /// from its manifest exercises exactly the "monitor is off/asleep" case.
    fn two_link_config() -> config::TopologyConfig {
        let mut config = topology_config();
        config.links[0].from.end = 540.0;
        config.links[0].to.end = 540.0;
        config.links.push(config::LinkConfig {
            id: "b-right-to-a-left".to_owned(),
            from: config::EndpointConfig {
                node: "self".to_owned(),
                display: "DISPLAY1".to_owned(),
                edge: config::Edge::Right,
                start: 540.0,
                end: 1080.0,
            },
            to: config::EndpointConfig {
                node: config::peer_reference(&peer_id()),
                display: "DP-1".to_owned(),
                edge: config::Edge::Left,
                start: 540.0,
                end: 1080.0,
            },
            bidirectional: true,
        });
        config
    }

    #[test]
    fn a_link_to_a_display_nobody_reports_is_inactive_not_fatal() {
        let config = two_link_config();
        let local = [info(
            "eDP-1",
            [0.0, 0.0, 1920.0, 1080.0],
            [0, 0, 1920, 1080],
            true,
        )];
        let peers = [(
            NodeId::new(peer_id()),
            vec![wire_display("DP-1", [0.0, 0.0, 2560.0, 1440.0], true)],
        )];

        let built = build(&Documents {
            local_node_id: local_id(),
            topology: &config,
            local_displays: &local,
            peer_displays: &peers,
        })
        .expect("one missing display must not disable the whole document");

        assert_eq!(
            built.topology.links.len(),
            1,
            "only the resolvable link reaches the resolver"
        );
        assert_eq!(
            built.topology.links[0].id,
            geometry::LinkId::new("a-right-to-b-left")
        );
        assert_eq!(built.inactive.len(), 1);
        assert_eq!(
            built.inactive[0].id,
            geometry::LinkId::new("b-right-to-a-left")
        );
        assert_eq!(built.inactive[0].missing.len(), 1);
        assert_eq!(
            built.inactive[0].missing[0].side,
            geometry::EndpointSide::Source
        );
        assert_eq!(built.inactive[0].missing[0].display.as_str(), "DISPLAY1");
        assert_eq!(built.inactive[0].missing[0].node, NodeId::new(local_id()));

        // The surviving link still resolves on this node: eDP-1's right edge is an exit.
        let transition = geometry::resolve_motion(
            &built.topology,
            &NodeId::new(local_id()),
            &geometry::Position::new("eDP-1", 1919.0, 300.0),
            geometry::Point::new(4.0, 0.0),
        )
        .expect("the active link still resolves");
        assert_eq!(transition.target.node, NodeId::new(peer_id()));
    }

    #[test]
    fn the_link_returns_when_the_display_is_reported_again() {
        let config = two_link_config();
        let local = [
            info(
                "eDP-1",
                [0.0, 0.0, 1920.0, 1080.0],
                [0, 0, 1920, 1080],
                true,
            ),
            info(
                "DISPLAY1",
                [4000.0, 3000.0, 1920.0, 1080.0],
                [0, 0, 1920, 1080],
                false,
            ),
        ];
        let peers = [(
            NodeId::new(peer_id()),
            vec![wire_display("DP-1", [0.0, 0.0, 2560.0, 1440.0], true)],
        )];

        let built = build(&Documents {
            local_node_id: local_id(),
            topology: &config,
            local_displays: &local,
            peer_displays: &peers,
        })
        .expect("both displays are present");

        assert_eq!(built.topology.links.len(), 2);
        assert!(built.inactive.is_empty());
    }

    #[test]
    fn overlapping_links_stay_fatal_even_with_every_display_present() {
        let mut config = two_link_config();
        // The second link now leaves over the whole of eDP-1's right edge, which the first
        // one already covers: an overlap, not a missing display.
        config.links[1].from.display = "eDP-1".to_owned();
        config.links[1].from.start = 0.0;
        config.links[1].from.end = 1080.0;
        let local = [
            info(
                "eDP-1",
                [0.0, 0.0, 1920.0, 1080.0],
                [0, 0, 1920, 1080],
                true,
            ),
            info(
                "DISPLAY1",
                [4000.0, 3000.0, 1920.0, 1080.0],
                [0, 0, 1920, 1080],
                false,
            ),
        ];
        let peers = [(
            NodeId::new(peer_id()),
            vec![wire_display("DP-1", [0.0, 0.0, 2560.0, 1440.0], true)],
        )];
        let error = build(&Documents {
            local_node_id: local_id(),
            topology: &config,
            local_displays: &local,
            peer_displays: &peers,
        })
        .expect_err("overlapping segments are structural");
        match error {
            BuildError::Invalid(errors) => assert!(
                errors
                    .iter()
                    .any(|error| matches!(error, geometry::TopologyError::SegmentOverlap { .. })),
                "{errors:?}"
            ),
            other => panic!("expected a document error, got {other}"),
        }
    }

    #[test]
    fn a_link_to_a_node_that_is_not_a_peer_stays_fatal() {
        let config = topology_config();
        // No peer manifest declares the node the link names: that is a structural error, not
        // a display that happens to be off.
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
        .expect_err("an unknown node is structural");
        assert!(
            matches!(
                &error,
                BuildError::Invalid(errors)
                    if errors
                        .iter()
                        .any(|error| matches!(error, geometry::TopologyError::UnknownNode { .. }))
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
        .expect("empty display manifests still form a document")
        .topology;

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
        .expect("a document with no links is valid")
        .topology;
        assert!(document.links.is_empty());
    }

    /// (d) An empty document with exactly one peer becomes one desktop-anchored link to the
    /// right; a document with links, or two peers, becomes nothing.
    #[test]
    fn the_default_layout_is_one_auto_link_to_the_right_of_one_peer() {
        let peer = NodeId::new(peer_id());
        let empty = config::TopologyConfig::default();

        let layout = default_layout(&empty, &[peer]).expect("one peer is enough");
        assert_eq!(layout.links.len(), 1);
        let link = &layout.links[0];
        assert_eq!(link.id, DEFAULT_LINK_ID);
        assert_eq!(link.from.node, "self");
        assert!(link.from.is_auto());
        assert_eq!(link.from.edge, config::Edge::Right);
        assert_eq!(link.to.node, config::peer_reference(&peer_id()));
        assert!(link.to.is_auto());
        assert_eq!(link.to.edge, config::Edge::Left);
        assert!(link.bidirectional);

        // It is a valid document the ordinary CAS path can apply.
        let mut whole = config::Config::default();
        whole.network.listen_address = "192.168.1.10".to_owned();
        whole.topology = layout.clone();
        whole.validate().expect("the default layout is valid");

        assert!(
            default_layout(&layout, &[peer]).is_none(),
            "a document with links is never overwritten"
        );
        // A document someone applied is theirs even when it has no links: only a pair nobody
        // has applied to, or a truly empty document, gets the default.
        let applied = config::TopologyConfig {
            revision: 2,
            displays: vec![config::DisplayConfig {
                node: "self".to_owned(),
                id: "eDP-1".to_owned(),
                logical: [0.0, 0.0, 1920.0, 1080.0],
                primary: true,
            }],
            ..config::TopologyConfig::default()
        };
        assert!(
            default_layout(&applied, &[peer]).is_none(),
            "an applied document without links is the user's choice"
        );
        assert!(default_layout(&empty, &[]).is_none(), "no peer, no default");
        assert!(
            default_layout(&empty, &[peer, NodeId::new([0xBB; 32])]).is_none(),
            "with two peers the placement is the user's call"
        );
    }

    /// A document whose links are desktop anchors resolves against the live manifests, and a
    /// monitor that disappears changes the resolved edge with no configuration edit.
    #[test]
    fn an_auto_document_follows_the_live_manifests() {
        let document = config::TopologyConfig {
            links: auto_layout(&[NodeId::new(peer_id())])
                .expect("one peer")
                .links,
            ..config::TopologyConfig::default()
        };

        let two = vec![
            info("left", [0.0, 0.0, 1920.0, 1080.0], [0, 0, 1920, 1080], true),
            named_info(
                "right",
                "\\\\.\\DISPLAY2",
                [1920.0, 0.0, 1920.0, 1080.0],
                [1920, 0, 1920, 1080],
                false,
            ),
        ];
        let peer_displays = vec![(
            NodeId::new(peer_id()),
            vec![wire_display("DP-1", [0.0, 0.0, 1920.0, 1080.0], false)],
        )];

        let built = build(&Documents {
            local_node_id: local_id(),
            topology: &document,
            local_displays: &two,
            peer_displays: &peer_displays,
        })
        .expect("the auto document is valid");
        assert!(built.inactive.is_empty());
        assert_eq!(built.topology.links.len(), 1);
        assert_eq!(
            built.topology.links[0].source.display.as_str(),
            "right",
            "the right-most monitor owns the right edge"
        );

        // The second monitor is switched off: the same document resolves to what is left.
        let one = vec![info(
            "left",
            [0.0, 0.0, 1920.0, 1080.0],
            [0, 0, 1920, 1080],
            true,
        )];
        let built = build(&Documents {
            local_node_id: local_id(),
            topology: &document,
            local_displays: &one,
            peer_displays: &peer_displays,
        })
        .expect("a missing monitor is not an error for an auto link");
        assert!(built.inactive.is_empty());
        assert_eq!(built.topology.links.len(), 1);
        assert_eq!(built.topology.links[0].source.display.as_str(), "left");
        assert_eq!(
            built.topology.links[0].source.segment,
            geometry::Segment::new(0.0, 1080.0)
        );

        // A step from just inside its right edge leaves the node, and comes back in reverse.
        let position = geometry::Position::new("left", 1910.0, 540.0);
        let out = geometry::resolve_motion(
            &built.topology,
            &NodeId::new(local_id()),
            &position,
            geometry::Point::new(20.0, 0.0),
        )
        .expect("the auto right edge is an exit");
        assert_eq!(out.target.display.as_str(), "DP-1");
    }

    /// (f) The `auto` anchor is the display id itself, so it survives the wire document.
    #[test]
    fn an_auto_anchor_survives_the_wire_round_trip() {
        let layout = auto_layout(&[NodeId::new(peer_id())]).expect("one peer");
        let document = wire_document(&layout, 0, NodeId::new(local_id()), NodeId::new(local_id()));
        let endpoint = document.links[0]
            .source_endpoint
            .as_ref()
            .expect("the source endpoint");
        assert_eq!(endpoint.display_id, config::AUTO_DISPLAY);

        let back = config_from_wire(&document, NodeId::new(local_id())).expect("it converts back");
        assert_eq!(back.links.len(), 1);
        assert!(back.links[0].from.is_auto());
        assert_eq!(back.links[0].from.edge, config::Edge::Right);
        assert!(back.links[0].to.is_auto());
        assert_eq!(back.links[0].to.edge, config::Edge::Left);
    }
}
