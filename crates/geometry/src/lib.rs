#![forbid(unsafe_code)]
//! Pure geometry for LAN Workspace: displays, edges, transition links, topology
//! validation and edge-crossing transitions.
//!
//! The crate is a plain data model plus pure functions: no I/O, no async, no
//! allocation-heavy paths, no `unsafe`, no dependencies outside `core`/`std`.
//!
//! Sources (all in `docs/`):
//!
//! * [`docs/protocol.proto`] — `Display`, `Edge`, `Endpoint`, `TransitionLink`,
//!   `Topology`, `NodeDisplays`, `Position`.
//! * `docs/03-architecture.md`, section «Координаты» and «Топология и движение» —
//!   node logical space, edge/segment parameterisation, transition formula,
//!   integrity rules.
//! * `docs/config.example.toml`, section `[topology]` — on-the-wire shape of a
//!   layout document.
//! * `docs/06-development-plan.md` rows KVM-021 / KVM-040 / KVM-072.
//!
//! # Model in one paragraph
//!
//! A [`Display`] occupies a rectangle in its node's *logical space*
//! (`logical_bounds`), optionally carries diagnostic native bounds and a
//! logical → native [`Scale`], plus a [`Rotation`]. Coordinates may be negative
//! and a node's desktop is not a bounding box: gaps between local monitors are
//! possible. A [`TransitionLink`] connects a [`Segment`] of one display edge on
//! node A to a [`Segment`] of a display edge on node B; it carries the authored
//! `id` and `bidirectional` flag (`LinkView` / `TRANSITION_LINK.link_id`), and a
//! bidirectional link is an exit on both of its segments. When the owner-local
//! pointer leaves its display, [`resolve_transition`] (or [`resolve_motion`] for
//! a motion step that may jump over an edge) finds the matching link and maps the
//! crossing proportionally onto the target segment, `2` logical units inside the
//! target display ([`ENTRY_INSET_LOGICAL`]); the result reports the
//! [`LinkDirection`] that was taken.
//!
//! # Decisions taken where the documents are ambiguous
//!
//! * `protocol.proto TransitionLink` carries only `source_endpoint`/`target_endpoint`,
//!   while the authored document (`LinkView` in `ipc.proto`, the `TRANSITION_LINK`
//!   ERD row, `config.example.toml`) also has `id` and `bidirectional`. The model
//!   mirrors the authored document: a wire conversion expands `bidirectional`
//!   into two directed links and restores `id` from the document.
//! * `EDGE_UNSPECIFIED` (`protocol.proto`) has **no model representation**: a
//!   wire-to-model conversion must reject it, because an endpoint without an edge
//!   cannot participate in a transition. [`Edge`] therefore has four variants.
//! * «Углы … не допускаются валидатором конфигурации» (03-architecture.md, line
//!   777) is **not** read as "segments may not start or end at a corner" — the
//!   shipped example in `config.example.toml` uses `start = 0, end = 1080` on a
//!   1080-long edge, i.e. a full edge. It is read as *corner ambiguity*: two links
//!   that attach to the two edges meeting at one corner **and** both include that
//!   corner are rejected (`TopologyError::AmbiguousCorner`).
//! * «сегмент в «дырке» между мониторами» (03-architecture.md, line 777) is
//!   defined precisely as: the space directly outside the edge, at that distance
//!   along the edge, is empty while another display **of the same node** lies
//!   beyond it at the same perpendicular coordinate. That is a gap the local
//!   pointer cannot cross, so a link there would be ambiguous
//!   (`TopologyError::SegmentInHole`).
//! * The backend note «на X11 и Windows link допустим на любом внешнем ребре
//!   дисплея, на Wayland — только на внешней границе зоны» (03-architecture.md,
//!   line 777) collapses into a single invariant, because on Wayland a zone *is*
//!   the display rectangle: the segment must lie on the outer boundary of the
//!   node's display union (`TopologyError::SegmentNotOuter`). No backend
//!   parameter is needed until the two notions actually diverge.
//! * Every conversion through [`Scale`] is explicit and `None`-able; per
//!   «Sender никогда не пересчитывает координаты через `scale_*`»
//!   (03-architecture.md, line 361) the transition math itself stays in logical
//!   units and only the receiver may ask for native pixels.

pub mod display;
pub mod topology;
pub mod transition;
pub mod validate;

pub use display::{Display, DisplayId, Edge, NodeId, Point, Rect, Rotation, Scale, Size};
pub use topology::{Endpoint, LinkId, NodeDisplays, Position, Segment, Topology, TransitionLink};
pub use transition::{
    crossing, crossing_with_motion, map_crossing, map_endpoints, resolve_motion,
    resolve_transition, EdgeCrossing, LinkDirection, Transition, ENTRY_INSET_LOGICAL,
};
pub use validate::{
    validate_topology, Corner, DisplayDefect, DisplayRef, EdgeRef, EndpointSide, TopologyError,
    ValidationErrors,
};
