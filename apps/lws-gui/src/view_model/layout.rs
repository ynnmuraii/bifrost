//! The map's model: the applied document turned into something draggable, and back.
//!
//! Drags edit a local placement; only a valid drop queues a document through the CAS path.

use super::{
    endpoint_config, node_id_bytes, node_reference, short_id, short_key, Action, EditorDisplay,
    Request, Save, Sent, Side, ViewModel, HINT_FOR, SAVED_FOR,
};
use crate::map::{self, DocLink, Pt, Rect};
use ui_text::{fill, fill_named, text, Key};

/// How a drawn link is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Up, and input is not going through it now.
    Idle,
    /// Input is going through it now.
    Active,
    /// One of its computers is not in the network.
    Offline,
    /// The core keeps the link but a display it names is not reported.
    Inactive,
}

/// One display of a drawn computer.
#[derive(Debug, Clone, PartialEq)]
pub struct MapMonitor {
    /// Relative to the computer's corner, in stage units.
    pub rect: Rect,
    /// «01».
    pub num: String,
    /// «1920×1080», «≈ 3840×2160», with the rotation when it is not 0.
    pub res: String,
    /// «ввод» on the active display, otherwise empty.
    pub tag: &'static str,
}

/// One computer of the map.
#[derive(Debug, Clone, PartialEq)]
pub struct MapNode {
    /// The member's id.
    pub node_id: String,
    /// Its name.
    pub name: String,
    /// The line under the name.
    pub os: String,
    /// Compact connection/input state shown below the computer.
    pub focus: String,
    /// In the network, as far as the core knows.
    pub online: bool,
    /// Input goes here now.
    pub target: bool,
    /// The size of any of its displays is a guess (Wayland portals).
    pub approx: bool,
    /// Top-left on the stage.
    pub at: Pt,
    /// Box size on the stage.
    pub w: f64,
    /// Box size on the stage.
    pub h: f64,
    /// Its displays.
    pub mons: Vec<MapMonitor>,
}

/// One link between two computers.
#[derive(Debug, Clone, PartialEq)]
pub struct MapLink {
    /// Index of the left/upper computer in [`MapModel::nodes`].
    pub a: usize,
    /// Index of the right/lower computer.
    pub b: usize,
    /// Where it leaves the left/upper computer.
    pub from: Pt,
    /// Where it arrives at the right/lower one.
    pub to: Pt,
    /// Side by side or stacked.
    pub dir: map::Dir,
    /// The shared stretch of the two edges, along the axis of the edge.
    pub span: (f64, f64),
    /// Independent endpoint intervals in world tangent coordinates.
    pub a_span: (f64, f64),
    /// Destination endpoint interval in world tangent coordinates.
    pub b_span: (f64, f64),
    /// Display indices in the respective computer.
    pub a_display: usize,
    /// Destination display index in its computer.
    pub b_display: usize,
    /// Stable configuration link identity.
    pub id: String,
    /// What the link is doing.
    pub state: LinkState,
    /// `true` for an `auto` link: the whole desktop's edge.
    pub whole_desktop: bool,
    /// `true` when input flows from `from` to `to` (only meaningful while active).
    pub forward: bool,
}

/// Everything the map draws.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MapModel {
    /// The computers.
    pub nodes: Vec<MapNode>,
    /// The links.
    pub links: Vec<MapLink>,
    /// How many displays there are in all.
    pub screens: usize,
    /// The map differs from the applied document.
    pub dirty: bool,
}

/// One computer in the list of pairs and in the top bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRow {
    /// The member's id.
    pub node_id: String,
    /// Its name.
    pub name: String,
    /// Two letters for the icon.
    pub initials: String,
    /// «7F3A·91C2».
    pub meta: String,
    /// In the network.
    pub online: bool,
}

/// The singular/few/many form of a Russian noun for `n`.
pub fn plural(n: usize, forms: [&'static str; 3]) -> &'static str {
    let (tens, ones) = (n % 100, n % 10);
    if (11..=14).contains(&tens) || ones == 0 || ones >= 5 {
        forms[2]
    } else if ones == 1 {
        forms[0]
    } else {
        forms[1]
    }
}

impl ViewModel {
    fn index_of(&self, node_id: &str) -> Option<usize> {
        self.editor
            .nodes
            .iter()
            .position(|node| node.node_id == node_id)
    }

    /// The applied links as «b is on side s of a» between member indices.
    fn doc_links(&self) -> Vec<DocLink> {
        self.doc_links_from(
            &self
                .applied_links()
                .into_iter()
                .map(|(_, link)| link)
                .collect::<Vec<_>>(),
        )
    }

    fn doc_links_from(&self, links: &[config::LinkConfig]) -> Vec<DocLink> {
        links
            .iter()
            .filter_map(|link| {
                let a = self
                    .editor
                    .nodes
                    .iter()
                    .position(|n| node_reference(&n.node_id) == link.from.node)?;
                let b = self
                    .editor
                    .nodes
                    .iter()
                    .position(|n| node_reference(&n.node_id) == link.to.node)?;
                let side = match link.from.edge {
                    config::Edge::Left => Side::Left,
                    config::Edge::Right => Side::Right,
                    config::Edge::Top => Side::Top,
                    config::Edge::Bottom => Side::Bottom,
                };
                let contact = if link.from.is_auto() && link.to.is_auto() {
                    None
                } else {
                    Some((
                        self.endpoint_segment(a, &link.from, map::BASE_SCALE)?,
                        self.endpoint_segment(b, &link.to, map::BASE_SCALE)?,
                    ))
                };
                (a != b).then_some(DocLink {
                    a,
                    side,
                    b,
                    contact,
                })
            })
            .collect()
    }

    /// A concrete endpoint in the node's normalized local coordinates.
    fn endpoint_segment(
        &self,
        node: usize,
        endpoint: &config::EndpointConfig,
        scale: f64,
    ) -> Option<Rect> {
        let displays = &self.editor.nodes.get(node)?.displays;
        if endpoint.is_auto() {
            let min_x = displays.iter().map(|d| d.x).fold(f64::INFINITY, f64::min);
            let min_y = displays.iter().map(|d| d.y).fold(f64::INFINITY, f64::min);
            let w = displays
                .iter()
                .map(|d| d.x + d.w - min_x)
                .fold(0.0, f64::max)
                * scale;
            let h = displays
                .iter()
                .map(|d| d.y + d.h - min_y)
                .fold(0.0, f64::max)
                * scale;
            return Some(match endpoint.edge {
                config::Edge::Left => Rect::new(0.0, 0.0, 0.0, h),
                config::Edge::Right => Rect::new(w, 0.0, 0.0, h),
                config::Edge::Top => Rect::new(0.0, 0.0, w, 0.0),
                config::Edge::Bottom => Rect::new(0.0, h, w, 0.0),
            });
        }
        let display = displays.iter().find(|d| d.id == endpoint.display)?;
        let x = displays.iter().map(|d| d.x).fold(f64::INFINITY, f64::min);
        let y = displays.iter().map(|d| d.y).fold(f64::INFINITY, f64::min);
        let (x, y) = ((display.x - x) * scale, (display.y - y) * scale);
        let start = endpoint.start * scale;
        let length = (endpoint.end - endpoint.start) * scale;
        Some(match endpoint.edge {
            config::Edge::Left => Rect::new(x, y + start, 0.0, length),
            config::Edge::Right => Rect::new(x + display.w * scale, y + start, 0.0, length),
            config::Edge::Top => Rect::new(x + start, y, length, 0.0),
            config::Edge::Bottom => Rect::new(x + start, y + display.h * scale, length, 0.0),
        })
    }

    /// Draws the applied document afresh: shapes, first placement, no draft.
    pub(crate) fn rebuild_layout(&mut self) {
        let logical: Vec<Vec<Rect>> = self
            .editor
            .nodes
            .iter()
            .map(|node| {
                let rects: Vec<Rect> = node
                    .displays
                    .iter()
                    .map(|d| Rect::new(d.x, d.y, d.w.max(1.0), d.h.max(1.0)))
                    .collect();
                if rects.is_empty() {
                    vec![Rect::new(0.0, 0.0, 1920.0, 1080.0)]
                } else {
                    rects
                }
            })
            .collect();
        let anchor = self
            .self_node()
            .and_then(|id| self.index_of(id))
            .unwrap_or(0);
        let arrangement = map::arrange(&logical, &self.doc_links(), anchor);
        self.editor.scale = arrangement.scale;
        self.editor.shapes = arrangement.shapes;
        self.editor.baseline = arrangement.pos.clone();
        self.editor.placement = arrangement.pos;
        if let Some(wanted) = &self.editor.save.wanted {
            self.editor.placement = map::place_at_scale(
                &self.editor.shapes,
                &self.doc_links_from(wanted),
                anchor,
                self.editor.scale,
            );
            self.editor.save.placement = Some(self.editor.placement.clone());
        }
        // A changed manifest/replacement document invalidates an in-progress drag.
        // Retain only the accepted drop, which editor_links reprojects and clamps.
        self.editor.paddles = self.editor.save.wanted.clone().or_else(|| {
            self.editor
                .save
                .sent
                .as_ref()
                .map(|sent| sent.links.clone())
        });
        if self.editor.save.wanted.is_some() {
            let reconciled = self.editor_links_reconciled(true);
            self.editor.paddles = Some(reconciled.clone());
            self.editor.save.paddles = Some(reconciled.clone());
            self.editor.save.wanted = Some(reconciled);
        }
    }

    pub(crate) fn restore_placement(&mut self) {
        self.editor.placement.clone_from(
            self.editor
                .save
                .placement
                .as_ref()
                .unwrap_or(&self.editor.baseline),
        );
        self.editor.paddles = self
            .editor
            .save
            .wanted
            .clone()
            .or_else(|| {
                self.editor
                    .save
                    .sent
                    .as_ref()
                    .map(|sent| sent.links.clone())
            })
            .or_else(|| self.editor.save.paddles.clone());
    }

    fn drop_is_valid(&self, i: usize) -> bool {
        if !self.connection.is_connected() || !self.editor.has_document() || !self.editor.editable {
            return false;
        }
        let pos = &self.editor.placement;
        let shapes = &self.editor.shapes;
        if pos.len() != shapes.len() || i >= pos.len() {
            return false;
        }
        for a in 0..pos.len() {
            for b in a + 1..pos.len() {
                if shapes[a].mons.iter().any(|ma| {
                    shapes[b].mons.iter().any(|mb| {
                        pos[a].x + ma.x < pos[b].x + mb.x + mb.w - 1e-6
                            && pos[b].x + mb.x < pos[a].x + ma.x + ma.w - 1e-6
                            && pos[a].y + ma.y < pos[b].y + mb.y + mb.h - 1e-6
                            && pos[b].y + mb.y < pos[a].y + ma.y + ma.h - 1e-6
                    })
                }) {
                    return false;
                }
            }
        }
        true
    }

    pub(crate) fn drop_node(&mut self, node_id: &str) {
        let lang = self.look.language;
        let Some(i) = self.index_of(node_id) else {
            return;
        };
        if !self.editor.dirty() {
            return;
        }
        if !self.drop_is_valid(i) {
            // Restore only the invalid move, retaining a previous accepted drop.
            self.restore_placement();
            self.flash(
                text(lang, Key::PutTheComputerNextToANeighbourWithoutOverlaps),
                HINT_FOR,
            );
            return;
        }
        self.editor.save.wanted = Some(self.editor_links());
        self.editor.save.paddles = self.editor.save.wanted.clone();
        self.editor.save.placement = Some(self.editor.placement.clone());
        self.apply_draft();
    }

    /// The single place a dropped draft becomes a wire request.
    pub(crate) fn apply_draft(&mut self) {
        if self.editor.save.sent.is_some()
            || self.editor.save.reloading
            || self.editor.save.awaiting.is_some()
        {
            return;
        }
        let Some(links) = self.editor.save.wanted.clone() else {
            return;
        };
        let applied: Vec<_> = self
            .applied_links()
            .into_iter()
            .map(|(_, link)| link)
            .collect();
        if same_links(&links, &applied) {
            self.editor.save = Save::default();
            self.editor.baseline.clone_from(&self.editor.placement);
            self.editor.paddles = None;
            return;
        }
        let layout_toml = match toml::to_string(&self.topology_with(links.clone())) {
            Ok(text) => text.into_bytes(),
            Err(_) => {
                self.editor.save = Save::default();
                self.editor.paddles = None;
                self.restore_placement();
                return;
            }
        };
        let action = Action::UpdateTopology {
            layout_toml,
            expected_revision: self.editor.revision,
            expected_author: node_id_bytes(&self.editor.author),
        };
        let Some((request, message)) = action.wire() else {
            return;
        };
        let request_id = self.begin_request(request, std::time::Instant::now());
        self.editor.save.sent = Some(Sent {
            request_id,
            base_revision: self.editor.revision,
            links,
        });
        self.outgoing.push((request_id, message));
    }

    pub(crate) fn on_layout_verdict(
        &mut self,
        request_id: u64,
        result: &protocol::ipc::OpResult,
    ) -> bool {
        let lang = self.look.language;
        if !self
            .editor
            .save
            .sent
            .as_ref()
            .is_some_and(|sent| sent.request_id == request_id)
        {
            return false;
        }
        let sent = self.editor.save.sent.take().expect("matched save");
        if result.ok {
            if self
                .editor
                .save
                .wanted
                .as_ref()
                .is_some_and(|wanted| same_links(wanted, &sent.links))
            {
                self.editor.save.wanted = None;
            }
            self.editor.save.awaiting = Some(sent.base_revision);
            self.flash(text(lang, Key::Saved), SAVED_FOR);
            self.continue_save();
            return false;
        }
        if result.error_code == "LAYOUT_CONFLICT" && !self.editor.save.retried {
            self.editor.save.retried = true;
            self.editor.save.reloading = true;
            let id = self.begin_request(Request::GetTopology, std::time::Instant::now());
            self.outgoing.push((
                id,
                protocol::ipc::gui_frame::Msg::GetTopology(protocol::ipc::GetTopology {}),
            ));
            return true;
        }
        self.editor.save = Save::default();
        self.editor.paddles = None;
        self.restore_placement();
        false
    }

    pub(crate) fn continue_save(&mut self) {
        if let Some(base) = self.editor.save.awaiting {
            if self.editor.revision <= base {
                return;
            }
            self.editor.save.awaiting = None;
            if self.editor.save.wanted.is_none() {
                let accepted = self.editor.save.placement.take();
                self.editor.save = Save::default();
                let applied: Vec<_> = self
                    .applied_links()
                    .into_iter()
                    .map(|(_, link)| link)
                    .collect();
                if self
                    .editor
                    .paddles
                    .as_ref()
                    .is_some_and(|draft| same_links(draft, &applied))
                {
                    self.editor.paddles = None;
                }
                if let Some(accepted) = accepted.filter(|p| p.len() == self.editor.nodes.len()) {
                    self.editor.baseline.clone_from(&accepted);
                    self.editor.placement = accepted;
                }
            }
        }
        self.apply_draft();
    }

    /// A free drag: proximity creates links without changing the requested placement.
    pub fn move_node(&mut self, node_id: &str, x: f64, y: f64) {
        self.move_node_with_threshold(node_id, x, y, map::SNAP);
    }

    /// Retains the UI threshold parameter without snapping the computer.
    pub fn move_node_with_threshold(
        &mut self,
        node_id: &str,
        x: f64,
        y: f64,
        _snap_threshold: f64,
    ) {
        let Some(i) = self.index_of(node_id) else {
            return;
        };
        if i >= self.editor.placement.len() || !x.is_finite() || !y.is_finite() {
            return;
        }
        self.editor.placement[i] = Pt::new(x, y);
    }

    /// A keyboard step: no snapping, so a step is always the step that was asked for.
    pub fn nudge_node(&mut self, node_id: &str, dx: f64, dy: f64) {
        let Some(i) = self.index_of(node_id) else {
            return;
        };
        if let Some(at) = self.editor.placement.get_mut(i) {
            at.x = (at.x + dx).clamp(-60.0, map::STAGE_W + 60.0);
            at.y = (at.y + dy).clamp(-70.0, map::STAGE_H + 40.0);
        }
    }
    /// Adjust one endpoint locally in absolute stage tangent coordinates.
    pub fn move_paddle(&mut self, link_id: &str, from: bool, start: f64, end: f64) {
        if !start.is_finite() || !end.is_finite() || !self.editor.editable {
            return;
        }
        let mut links = self.editor_links();
        let Some(doc) = links.iter_mut().find(|doc| doc.id == link_id) else {
            return;
        };
        let Some(a) = self
            .editor
            .nodes
            .iter()
            .position(|n| node_reference(&n.node_id) == doc.from.node)
        else {
            return;
        };
        let Some(b) = self
            .editor
            .nodes
            .iter()
            .position(|n| node_reference(&n.node_id) == doc.to.node)
        else {
            return;
        };
        let Some(facing) = self
            .faced(&self.editor.placement)
            .into_iter()
            .filter(|g| self.geometric_endpoints(doc, g).is_some())
            .max_by(|left, right| {
                let score = |g: &map::GeoLink| {
                    let display = if g.a == a { g.a_display } else { g.b_display };
                    let mon = self.editor.shapes[a].mons[display];
                    let at = self.editor.placement[a];
                    let origin = match g.dir {
                        map::Dir::Horizontal => at.y + mon.y,
                        map::Dir::Vertical => at.x + mon.x,
                    };
                    (g.span.1.min(origin + doc.from.end * self.editor.scale)
                        - g.span.0.max(origin + doc.from.start * self.editor.scale))
                    .max(0.0)
                };
                score(left).total_cmp(&score(right))
            })
        else {
            return;
        };
        let (node, display, endpoint) = if from {
            (
                a,
                if facing.a == a {
                    facing.a_display
                } else {
                    facing.b_display
                },
                &mut doc.from,
            )
        } else {
            (
                b,
                if facing.b == b {
                    facing.b_display
                } else {
                    facing.a_display
                },
                &mut doc.to,
            )
        };
        let mon = self.editor.shapes[node].mons[display];
        let at = self.editor.placement[node];
        let origin = match facing.dir {
            map::Dir::Horizontal => at.y + mon.y,
            map::Dir::Vertical => at.x + mon.x,
        };
        let span = map::clamp_paddle(start, end, facing.span.0, facing.span.1);
        endpoint.start = (span.0 - origin) / self.editor.scale;
        endpoint.end = (span.1 - origin) / self.editor.scale;
        self.editor.paddles = Some(links);
    }

    /// Queue the latest endpoint draft using the existing CAS autosave.
    pub fn drop_paddle(&mut self) {
        if self.editor.paddles.is_none() || !self.connection.is_connected() || !self.editor.editable
        {
            return;
        }
        self.editor.save.wanted = Some(self.editor_links());
        self.editor.save.paddles = self.editor.save.wanted.clone();
        self.editor.save.placement = Some(self.editor.placement.clone());
        self.apply_draft();
    }

    /// The pairs of members that face each other in `placement`, with how they face.
    fn faced(&self, placement: &[Pt]) -> Vec<map::GeoLink> {
        map::links(&self.editor.shapes, placement)
    }

    /// Endpoints in geometric order, and whether the document order is reversed.
    fn geometric_endpoints<'a>(
        &self,
        doc: &'a config::LinkConfig,
        g: &map::GeoLink,
    ) -> Option<(&'a config::EndpointConfig, &'a config::EndpointConfig, bool)> {
        let (ae, be) = g.side_of_b().edges();
        let matches = |a: &config::EndpointConfig, b: &config::EndpointConfig| {
            a.node == node_reference(&self.editor.nodes[g.a].node_id)
                && b.node == node_reference(&self.editor.nodes[g.b].node_id)
                && a.edge == ae
                && b.edge == be
                && self.editor.nodes[g.a]
                    .displays
                    .get(g.a_display)
                    .is_some_and(|d| d.id == a.display)
                && self.editor.nodes[g.b]
                    .displays
                    .get(g.b_display)
                    .is_some_and(|d| d.id == b.display)
        };
        if matches(&doc.from, &doc.to) {
            Some((&doc.from, &doc.to, false))
        } else if matches(&doc.to, &doc.from) {
            Some((&doc.to, &doc.from, true))
        } else {
            None
        }
    }

    /// The applied links, as the configuration schema writes them.
    fn applied_links(&self) -> Vec<(Option<(usize, usize)>, config::LinkConfig)> {
        self.editor
            .links
            .iter()
            .enumerate()
            .filter_map(|(index, link)| {
                let from = link.from.as_ref()?;
                let to = link.to.as_ref()?;
                let ends = self.index_of(&from.node_id).zip(self.index_of(&to.node_id));
                Some((
                    ends,
                    config::LinkConfig {
                        id: if link.id.is_empty() {
                            format!("link-{index}")
                        } else {
                            link.id.clone()
                        },
                        from: endpoint_config(from)?,
                        to: endpoint_config(to)?,
                        bidirectional: link.bidirectional,
                    },
                ))
            })
            .collect()
    }

    /// The links the map draft would save: untouched document pairs as they are, moved pairs
    /// as their exact contacts. The document's own text comes from this.
    pub fn editor_links(&self) -> Vec<config::LinkConfig> {
        self.editor_links_reconciled(false)
    }

    fn editor_links_reconciled(&self, reconcile: bool) -> Vec<config::LinkConfig> {
        let applied = self.editor.paddles.as_ref().map_or_else(
            || self.applied_links(),
            |links| {
                links
                    .iter()
                    .cloned()
                    .map(|link| {
                        let a = self
                            .editor
                            .nodes
                            .iter()
                            .position(|n| node_reference(&n.node_id) == link.from.node);
                        let b = self
                            .editor
                            .nodes
                            .iter()
                            .position(|n| node_reference(&n.node_id) == link.to.node);
                        (a.zip(b), link)
                    })
                    .collect()
            },
        );
        if (!reconcile && !self.editor.dirty())
            || self.editor.placement.len() != self.editor.nodes.len()
        {
            return applied.into_iter().map(|(_, link)| link).collect();
        }
        let now = self.faced(&self.editor.placement);
        let baseline = &self.editor.baseline;
        let moved = |i: usize| reconcile || self.editor.placement.get(i) != baseline.get(i);
        let mut links: Vec<config::LinkConfig> = Vec::new();
        for (ends, link) in &applied {
            let missing = ends.is_none_or(|(i, j)| {
                (!link.from.is_auto()
                    && !self.editor.nodes[i]
                        .displays
                        .iter()
                        .any(|d| d.id == link.from.display))
                    || (!link.to.is_auto()
                        && !self.editor.nodes[j]
                            .displays
                            .iter()
                            .any(|d| d.id == link.to.display))
            });
            if missing || ends.is_some_and(|(i, j)| !moved(i) && !moved(j)) {
                links.push(link.clone());
            }
        }
        for link in &now {
            if !moved(link.a) && !moved(link.b) {
                continue;
            }
            let (a, b) = (
                self.editor.nodes[link.a].node_id.as_str(),
                self.editor.nodes[link.b].node_id.as_str(),
            );
            let (a_edge, b_edge) = link.side_of_b().edges();
            let existing = applied
                .iter()
                .map(|(_, doc)| doc)
                .filter(|doc| {
                    !links.iter().any(|saved| saved.id == doc.id)
                        && self.geometric_endpoints(doc, link).is_some()
                })
                .max_by(|left, right| {
                    let mon = self.editor.shapes[link.a].mons[link.a_display];
                    let at = self.editor.placement[link.a];
                    let origin = match link.dir {
                        map::Dir::Horizontal => at.y + mon.y,
                        map::Dir::Vertical => at.x + mon.x,
                    };
                    let overlap = |doc: &config::LinkConfig| {
                        let (a, _, _) = self
                            .geometric_endpoints(doc, link)
                            .expect("matched endpoints");
                        (link.span.1.min(origin + a.end * self.editor.scale)
                            - link.span.0.max(origin + a.start * self.editor.scale))
                        .max(0.0)
                    };
                    overlap(left).total_cmp(&overlap(right))
                });
            let mut id = existing.map(|doc| doc.id.clone()).unwrap_or_else(|| {
                format!(
                    "gui-{}-{}-{}-{}",
                    short_key(a),
                    short_key(b),
                    link.a_display,
                    link.b_display
                )
            });
            while links.iter().any(|existing| existing.id == id) {
                id.push('x');
            }
            let endpoint = |node: usize,
                            display: usize,
                            edge: config::Edge,
                            previous: Option<&config::EndpointConfig>| {
                let mon = self.editor.shapes[node].mons[display];
                let at = self.editor.placement[node];
                let origin = match link.dir {
                    map::Dir::Horizontal => at.y + mon.y,
                    map::Dir::Vertical => at.x + mon.x,
                };
                let span = previous.map_or(link.span, |e| {
                    map::clamp_paddle(
                        origin + e.start * self.editor.scale,
                        origin + e.end * self.editor.scale,
                        link.span.0,
                        link.span.1,
                    )
                });
                config::EndpointConfig {
                    node: node_reference(&self.editor.nodes[node].node_id),
                    display: self.editor.nodes[node].displays[display].id.clone(),
                    edge,
                    start: (span.0 - origin) / self.editor.scale,
                    end: (span.1 - origin) / self.editor.scale,
                }
            };
            // A guessed desktop has no stable display identity to persist.
            if self.editor.nodes[link.a]
                .displays
                .get(link.a_display)
                .is_none()
                || self.editor.nodes[link.b]
                    .displays
                    .get(link.b_display)
                    .is_none()
            {
                continue;
            }
            let ends = existing.and_then(|doc| self.geometric_endpoints(doc, link));
            let a = endpoint(link.a, link.a_display, a_edge, ends.map(|(a, _, _)| a));
            let b = endpoint(link.b, link.b_display, b_edge, ends.map(|(_, b, _)| b));
            let (from, to) = if ends.is_some_and(|(_, _, reversed)| reversed) {
                (b, a)
            } else {
                (a, b)
            };
            links.push(config::LinkConfig {
                id,
                from,
                to,
                bidirectional: existing.is_none_or(|doc| doc.bidirectional),
            });
        }
        links
    }

    /// The member the input goes to now, as an index: this computer when it is local.
    fn route_target(&self) -> Option<usize> {
        let status = self.status.as_ref()?;
        let this = self.self_node().and_then(|id| self.index_of(id));
        match status.focus.as_str() {
            "REMOTE" | "PREPARING" if !status.active_peer.is_empty() => self
                .editor
                .nodes
                .iter()
                .position(|n| n.display_name == status.active_peer)
                .or(this),
            _ => this,
        }
    }

    /// The name of this computer.
    pub fn this_name(&self) -> String {
        let lang = self.look.language;
        self.self_node()
            .and_then(|id| self.editor.nodes.iter().find(|n| n.node_id == id))
            .map(|n| n.label(lang))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| self.config.display_name.clone())
    }

    /// The headline of the map: «3 компьютера · 4 экрана».
    pub fn map_title(&self) -> String {
        let lang = self.look.language;
        let nodes = self.editor.nodes.len();
        let screens: usize = self.editor.nodes.iter().map(|n| n.displays.len()).sum();
        format!(
            "{nodes} {} · {screens} {}",
            plural(
                nodes,
                [
                    text(lang, Key::Computer),
                    text(lang, Key::ComputersFew),
                    text(lang, Key::ComputersMany)
                ]
            ),
            plural(
                screens,
                [
                    text(lang, Key::Display),
                    text(lang, Key::DisplaysFew),
                    text(lang, Key::DisplaysMany)
                ]
            ),
        )
    }

    /// Everything the map draws, from the draft and the live state.
    pub fn map_model(&self) -> MapModel {
        let lang = self.look.language;
        let editor = &self.editor;
        let ready = editor.shapes.len() == editor.nodes.len()
            && editor.placement.len() == editor.nodes.len();
        if !ready {
            return MapModel::default();
        }
        let this = self.self_node().and_then(|id| self.index_of(id));
        let target = self.route_target();
        let backend = self.backend_label();
        let mut number = 0usize;
        let mut nodes = Vec::with_capacity(editor.nodes.len());
        for (i, node) in editor.nodes.iter().enumerate() {
            let is_target = target == Some(i) && self.status.is_some();
            let single = node.displays.len() == 1;
            let mut mons = Vec::with_capacity(node.displays.len());
            for (k, display) in node.displays.iter().enumerate() {
                number += 1;
                let rect = editor.shapes[i].mons.get(k).copied().unwrap_or_default();
                let tag = if is_target && (display.primary || single) {
                    text(lang, Key::Input)
                } else {
                    ""
                };
                mons.push(MapMonitor {
                    rect,
                    num: format!("{number:02}"),
                    res: resolution(display),
                    tag,
                });
            }
            let os = if this == Some(i) {
                if backend == text(lang, Key::Unknown) {
                    text(lang, Key::ThisComputer).to_owned()
                } else {
                    fill_named(lang, Key::ThisComputerBackend, &[("backend", &backend)])
                }
            } else {
                String::new()
            };
            let focus = if is_target {
                text(lang, Key::InputHere2).to_owned()
            } else if !node.online {
                text(lang, Key::Offline2).to_owned()
            } else {
                text(lang, Key::Online).to_owned()
            };
            nodes.push(MapNode {
                node_id: node.node_id.clone(),
                name: node.label(lang),
                os,
                focus,
                online: node.online,
                target: is_target,
                approx: node.displays.iter().any(|d| !d.native_known),
                at: editor.placement[i],
                w: editor.shapes[i].w,
                h: editor.shapes[i].h,
                mons,
            });
        }
        let dirty = editor.dirty();
        let mut links = Vec::new();
        let facing = self.faced(&editor.placement);
        for doc in self.editor_links() {
            let Some(a) = editor
                .nodes
                .iter()
                .position(|n| node_reference(&n.node_id) == doc.from.node)
            else {
                continue;
            };
            let Some(b) = editor
                .nodes
                .iter()
                .position(|n| node_reference(&n.node_id) == doc.to.node)
            else {
                continue;
            };
            let whole_desktop = doc.from.is_auto() || doc.to.is_auto();
            let mut missing = false;
            let mut endpoint = |node: usize, e: &config::EndpointConfig| {
                let local = self
                    .endpoint_segment(node, e, editor.scale)
                    .unwrap_or_else(|| {
                        missing |= !e.is_auto();
                        let shape = &editor.shapes[node];
                        let start = e.start * editor.scale;
                        let length = (e.end - e.start) * editor.scale;
                        match e.edge {
                            config::Edge::Left => Rect::new(
                                0.0,
                                start,
                                0.0,
                                if e.is_auto() { shape.h } else { length },
                            ),
                            config::Edge::Right => Rect::new(
                                shape.w,
                                start,
                                0.0,
                                if e.is_auto() { shape.h } else { length },
                            ),
                            config::Edge::Top => Rect::new(
                                start,
                                0.0,
                                if e.is_auto() { shape.w } else { length },
                                0.0,
                            ),
                            config::Edge::Bottom => Rect::new(
                                start,
                                shape.h,
                                if e.is_auto() { shape.w } else { length },
                                0.0,
                            ),
                        }
                    });
                let at = editor.placement[node];
                Rect::new(at.x + local.x, at.y + local.y, local.w, local.h)
            };
            let f = endpoint(a, &doc.from);
            let t = endpoint(b, &doc.to);
            let dir = match doc.from.edge {
                config::Edge::Left | config::Edge::Right => map::Dir::Horizontal,
                config::Edge::Top | config::Edge::Bottom => map::Dir::Vertical,
            };
            let a_span = match dir {
                map::Dir::Horizontal => (f.y, f.y + f.h),
                map::Dir::Vertical => (f.x, f.x + f.w),
            };
            let b_span = match dir {
                map::Dir::Horizontal => (t.y, t.y + t.h),
                map::Dir::Vertical => (t.x, t.x + t.w),
            };
            let a_display = editor.nodes[a]
                .displays
                .iter()
                .position(|d| d.id == doc.from.display)
                .unwrap_or(0);
            let b_display = editor.nodes[b]
                .displays
                .iter()
                .position(|d| d.id == doc.to.display)
                .unwrap_or(0);
            let span = facing
                .iter()
                .filter(|g| self.geometric_endpoints(&doc, g).is_some())
                .max_by(|left, right| {
                    let score = |g: &map::GeoLink| {
                        (g.span.1.min(a_span.1) - g.span.0.max(a_span.0)).max(0.0)
                    };
                    score(left).total_cmp(&score(right))
                })
                .map_or((a_span.0.max(b_span.0), a_span.1.min(b_span.1)), |g| g.span);
            let from = Pt::new(f.x + f.w / 2.0, f.y + f.h / 2.0);
            let to = Pt::new(t.x + t.w / 2.0, t.y + t.h / 2.0);
            let documented = editor.links.iter().find(|l| l.id == doc.id);
            let (a_node, b_node) = (&editor.nodes[a], &editor.nodes[b]);
            let offline = !a_node.online || !b_node.online;
            let active = !offline
                && target != this
                && this.is_some()
                && ((this == Some(a) && target == Some(b))
                    || (this == Some(b) && target == Some(a)));
            let state = if missing || documented.is_some_and(|d| !d.active) && !dirty {
                LinkState::Inactive
            } else if offline {
                LinkState::Offline
            } else if active {
                LinkState::Active
            } else {
                LinkState::Idle
            };
            let forward = this == Some(a);
            links.push(MapLink {
                a,
                b,
                from,
                to,
                dir,
                span,
                a_span,
                b_span,
                a_display,
                b_display,
                id: doc.id.clone(),
                state,
                whole_desktop,
                forward,
            });
        }
        MapModel {
            nodes,
            links,
            screens: number,
            dirty,
        }
    }

    /// The other computers: members of `Status` that are not this one.
    pub fn peers(&self) -> Vec<PeerRow> {
        let lang = self.look.language;
        let Some(status) = &self.status else {
            return Vec::new();
        };
        let this = self.self_node().map(str::to_owned);
        status
            .members
            .iter()
            .filter(|m| !self.is_this_computer(m, this.as_deref()))
            .map(|m| {
                let name = if m.display_name.is_empty() {
                    fill(lang, Key::ComputerNamed, &[&short_id(&m.node_id)])
                } else {
                    m.display_name.clone()
                };
                let id = short_id(&m.node_id).to_uppercase();
                let meta = if id.chars().count() == 8 {
                    let (l, r) = id.split_at(4);
                    format!("{l}·{r}")
                } else {
                    id
                };
                PeerRow {
                    node_id: m.node_id.clone(),
                    initials: name
                        .chars()
                        .filter(|c| c.is_alphanumeric())
                        .take(2)
                        .flat_map(char::to_uppercase)
                        .collect(),
                    name,
                    meta,
                    online: m.online,
                }
            })
            .collect()
    }
}

/// «1920×1080», «≈ 3840×2160», and «· 90°» for a rotated display.
fn resolution(display: &EditorDisplay) -> String {
    let approx = if display.native_known { "" } else { "≈ " };
    let rotation = if display.rotation == 0 {
        String::new()
    } else {
        format!(" · {}°", display.rotation)
    };
    format!("{approx}{:.0}×{:.0}{rotation}", display.w, display.h)
}

fn same_links(a: &[config::LinkConfig], b: &[config::LinkConfig]) -> bool {
    a.len() == b.len() && a.iter().all(|link| b.contains(link))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view_model::{CoreEndpoint, EditorNode, LocalConfig};

    #[test]
    fn cancelled_paddle_drag_restores_last_drop_without_changing_the_queue() {
        let mut vm = paddles();
        let link = vm.map_model().links.remove(0);
        vm.move_paddle(&link.id, true, link.span.0 + 10.0, link.span.0 + 30.0);
        vm.drop_paddle();
        let dropped = vm.editor_links();
        let save = vm.editor.save.clone();
        vm.move_paddle(&link.id, true, link.span.0 + 40.0, link.span.0 + 60.0);
        assert_ne!(vm.editor_links(), dropped);
        vm.restore_placement();
        assert_eq!(vm.editor_links(), dropped);
        assert_eq!(vm.editor.save, save);
    }

    #[test]
    fn geometry_rebuild_discards_undropped_paddle_adjustment() {
        let mut vm = paddles();
        let link = vm.map_model().links.remove(0);
        vm.move_paddle(&link.id, true, link.span.0 + 10.0, link.span.0 + 30.0);
        assert!(vm.editor.paddles.is_some());
        vm.editor.nodes[0].displays[0].h = 720.0;
        vm.rebuild_layout();
        assert!(vm.editor.paddles.is_none());
    }

    #[test]
    fn reversed_horizontal_and_vertical_links_keep_endpoint_ownership_on_edit_and_move() {
        for vertical in [false, true] {
            let mut vm = paddles();
            if vertical {
                let at = vm.editor.placement[0];
                vm.editor.placement[1] = Pt::new(at.x, at.y + vm.editor.shapes[0].h + 40.0);
            }
            let mut docs = vm.editor_links();
            std::mem::swap(&mut docs[0].from, &mut docs[0].to);
            docs[0].id = "reversed".into();
            vm.editor.paddles = Some(docs.clone());
            let link = vm.map_model().links.remove(0);
            vm.move_paddle(&link.id, true, link.span.0 + 15.0, link.span.0 + 35.0);
            let adjusted = vm.editor_links().remove(0);
            assert_eq!(adjusted.id, "reversed");
            assert_eq!(adjusted.from.node, docs[0].from.node);
            assert_eq!(adjusted.to, docs[0].to);
            assert_ne!(adjusted.from.start, docs[0].from.start);
            vm.move_paddle(&link.id, false, link.span.0 + 10.0, link.span.0 + 50.0);
            let adjusted = vm.editor_links().remove(0);
            let id = vm.editor.nodes[1].node_id.clone();
            let at = vm.editor.placement[1];
            vm.move_node(&id, at.x + 5.0, at.y + 5.0);
            let moved = vm.editor_links().remove(0);
            assert_eq!(moved.id, adjusted.id);
            assert_eq!(moved.from, adjusted.from);
            assert_eq!(moved.to.node, adjusted.to.node);
            assert_eq!(moved.to.start, adjusted.to.start);
        }
    }

    #[test]
    fn resized_displays_reconcile_queued_segments_before_cas_submission() {
        let mut vm = paddles();
        let link = vm.map_model().links.remove(0);
        vm.move_paddle(&link.id, true, link.span.0 + 40.0, link.span.0 + 70.0);
        vm.move_paddle(&link.id, false, link.span.0 + 40.0, link.span.0 + 70.0);
        vm.drop_paddle();
        let first = vm.editor.save.sent.clone().unwrap();
        vm.take_outgoing();
        vm.move_paddle(&link.id, true, link.span.0 + 50.0, link.span.0 + 75.0);
        vm.move_paddle(&link.id, false, link.span.0 + 50.0, link.span.0 + 75.0);
        vm.drop_paddle();
        for node in &mut vm.editor.nodes {
            node.displays[0].h = 400.0;
        }
        vm.rebuild_layout();
        let reconciled = vm.editor.save.wanted.clone().unwrap();
        assert_eq!(vm.editor_links(), reconciled);
        for doc in &reconciled {
            assert!(doc.from.end <= 400.0 + 1e-6);
            assert!(doc.to.end <= 400.0 + 1e-6);
        }
        assert_eq!(vm.editor.save.sent.as_ref(), Some(&first));
        vm.on_layout_verdict(
            first.request_id,
            &protocol::ipc::OpResult {
                ok: true,
                error_code: String::new(),
            },
        );
        vm.editor.revision += 1;
        vm.continue_save();
        assert_eq!(vm.editor.save.sent.as_ref().unwrap().links, reconciled);
    }

    fn paddles() -> ViewModel {
        let mut vm = ViewModel::new(
            CoreEndpoint::Ready {
                description: "paddle-test".into(),
            },
            LocalConfig::default(),
        );
        vm.on_connected("paddle-test");
        vm.editor.editable = true;
        vm.editor.nodes = ["aa".repeat(32), "bb".repeat(32)]
            .into_iter()
            .map(|id| EditorNode {
                node_id: id,
                display_name: String::new(),
                online: true,
                displays: vec![EditorDisplay {
                    id: "stable-display".into(),
                    x: 0.0,
                    y: 0.0,
                    w: 1920.0,
                    h: 1080.0,
                    primary: true,
                    native_known: true,
                    rotation: 0,
                }],
            })
            .collect();
        vm.rebuild_layout();
        let a = vm.editor.placement[0];
        vm.editor.placement[1] = Pt::new(a.x + vm.editor.shapes[0].w + 40.0, a.y);
        vm
    }

    #[test]
    fn independent_paddles_round_trip_without_moving_nodes() {
        let mut vm = paddles();
        let link = vm.map_model().links.remove(0);
        let positions = vm.editor.placement.clone();
        vm.move_paddle(&link.id, true, link.span.0 + 12.0, link.span.0 + 30.0);
        vm.move_paddle(&link.id, false, link.span.0 + 36.0, link.span.0 + 60.0);
        assert_eq!(vm.editor.placement, positions);
        assert!(vm.take_outgoing().is_empty());
        let links = vm.editor_links();
        assert_ne!(links[0].from.start, links[0].to.start);
        assert_eq!(links[0].from.display, "stable-display");
        let topology = vm.topology_with(links.clone());
        let text = toml::to_string(&topology).unwrap();
        let restored: config::TopologyConfig = toml::from_str(&text).unwrap();
        assert_eq!(restored.links, topology.links);
        vm.drop_paddle();
        assert!(vm.editor.save.sent.is_some());
    }

    #[test]
    fn paddles_clamp_and_disappear_when_displays_stop_facing() {
        let mut vm = paddles();
        let link = vm.map_model().links.remove(0);
        vm.move_paddle(&link.id, true, link.span.0 - 100.0, link.span.1 + 100.0);
        let adjusted = vm.map_model().links.remove(0);
        assert_eq!(adjusted.a_span, link.span);
        let id = vm.editor.nodes[1].node_id.clone();
        let at = vm.editor.placement[1];
        vm.move_node(&id, at.x, at.y + 500.0);
        assert!(vm.editor_links().is_empty());
    }

    #[test]
    fn paddle_drop_queue_is_latest_wins() {
        let mut vm = paddles();
        let link = vm.map_model().links.remove(0);
        vm.move_paddle(&link.id, true, link.span.0, link.span.0 + 20.0);
        vm.drop_paddle();
        let first = vm.editor.save.sent.clone().unwrap();
        vm.take_outgoing();
        vm.move_paddle(&link.id, false, link.span.0 + 30.0, link.span.0 + 50.0);
        vm.drop_paddle();
        let latest = vm.editor_links();
        assert_eq!(vm.editor.save.wanted.as_ref(), Some(&latest));
        assert_eq!(vm.editor.save.sent.as_ref(), Some(&first));
        assert!(vm.take_outgoing().is_empty());
        vm.on_layout_verdict(
            first.request_id,
            &protocol::ipc::OpResult {
                ok: true,
                error_code: String::new(),
            },
        );
        assert!(vm.take_outgoing().is_empty());
        vm.editor.revision += 1;
        vm.continue_save();
        assert_eq!(vm.editor.save.sent.as_ref().unwrap().links, latest);
    }

    #[test]
    fn legacy_whole_desktop_and_contact_documents_stay_untouched() {
        let mut vm = paddles();
        vm.editor.baseline = vm.editor.placement.clone();
        let endpoint =
            |node: &str, display: &str, edge: &str, start, end| protocol::ipc::EndpointView {
                node_id: node.into(),
                display_id: display.into(),
                edge: edge.into(),
                segment_start: start,
                segment_end: end,
            };
        for display in ["auto", "stable-display"] {
            vm.editor.links = vec![protocol::ipc::LinkView {
                id: "legacy".into(),
                from: Some(endpoint(
                    &vm.editor.nodes[0].node_id,
                    display,
                    "right",
                    100.0,
                    500.0,
                )),
                to: Some(endpoint(
                    &vm.editor.nodes[1].node_id,
                    display,
                    "left",
                    200.0,
                    600.0,
                )),
                bidirectional: true,
                active: true,
                ..Default::default()
            }];
            let links = vm.editor_links();
            assert_eq!(links.len(), 1);
            assert_eq!(links[0].id, "legacy");
            assert_eq!(links[0].from.display, display);
            assert_eq!(links[0].to.display, display);
            assert_eq!(vm.map_model().links.len(), 1);
        }
    }

    #[test]
    fn moving_nodes_preserves_offsets_then_clamps_to_smaller_overlap() {
        let mut vm = paddles();
        let link = vm.map_model().links.remove(0);
        vm.move_paddle(&link.id, false, link.span.0 + 20.0, link.span.0 + 50.0);
        let before = vm.editor_links()[0].to.clone();
        let id = vm.editor.nodes[1].node_id.clone();
        let at = vm.editor.placement[1];
        vm.move_node(&id, at.x + 10.0, at.y + 5.0);
        assert_eq!(vm.editor_links()[0].to, before);
        vm.move_node(&id, at.x + 10.0, at.y + 70.0);
        let clamped = vm.map_model().links.remove(0);
        assert!(clamped.b_span.0 >= clamped.span.0);
        assert!(clamped.b_span.1 <= clamped.span.1);
    }
}
