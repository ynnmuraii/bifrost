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
                (a != b).then_some(DocLink { a, side, b })
            })
            .collect()
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
            self.editor.placement =
                map::place(&self.editor.shapes, &self.doc_links_from(wanted), anchor);
            self.editor.save.placement = Some(self.editor.placement.clone());
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
    }

    fn drop_is_valid(&self, i: usize) -> bool {
        let lang = self.look.language;
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
                if pos[a].x < pos[b].x + shapes[b].w
                    && pos[b].x < pos[a].x + shapes[a].w
                    && pos[a].y < pos[b].y + shapes[b].h
                    && pos[b].y < pos[a].y + shapes[a].h
                {
                    return false;
                }
            }
        }
        self.faced(pos)
            .iter()
            .any(|link| link.a == i || link.b == i)
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
            self.restore_placement();
            return;
        }
        let layout_toml = match toml::to_string(&self.topology_with(links.clone())) {
            Ok(text) => text.into_bytes(),
            Err(_) => {
                self.editor.save = Save::default();
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
                self.editor.save = Save::default();
                self.rebuild_layout();
            }
        }
        self.apply_draft();
    }

    /// A drag: the computer follows the pointer and snaps to the edge it comes near.
    pub fn move_node(&mut self, node_id: &str, x: f64, y: f64) {
        let Some(i) = self.index_of(node_id) else {
            return;
        };
        if i >= self.editor.placement.len() || !x.is_finite() || !y.is_finite() {
            return;
        }
        self.editor.placement[i] = map::snap(&self.editor.shapes, &self.editor.placement, i, x, y);
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

    /// The pairs of members that face each other in `placement`, with how they face.
    fn faced(&self, placement: &[Pt]) -> Vec<map::GeoLink> {
        map::links(&self.editor.shapes, placement)
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

    /// The links of the edited document.
    ///
    /// With no draft they are the applied links, untouched. With one, a link survives when the
    /// two computers it joins still face each other the way they did (or neither moved); a pair
    /// that now faces differently, or newly, gets one `auto` link — the whole desktop's edge,
    /// which then follows the monitors with no further edit — and a pair that no longer faces
    /// loses its link. Stacked desktops write Bottom ↔ Top; side-by-side desktops write
    /// Right ↔ Left, with the upper/left desktop first.
    pub(crate) fn editor_links(&self) -> Vec<config::LinkConfig> {
        let applied = self.applied_links();
        if !self.editor.dirty() || self.editor.placement.len() != self.editor.nodes.len() {
            return applied.into_iter().map(|(_, link)| link).collect();
        }
        let now = self.faced(&self.editor.placement);
        let before = self.faced(&self.editor.baseline);
        let find = |set: &[map::GeoLink], i: usize, j: usize| {
            set.iter()
                .find(|l| (l.a, l.b) == (i, j) || (l.a, l.b) == (j, i))
                .map(|l| (l.a, l.b, l.dir))
        };
        let moved = |i: usize| self.editor.placement[i] != self.editor.baseline[i];
        let mut links: Vec<config::LinkConfig> = Vec::new();
        for (ends, link) in applied {
            let keep = match ends {
                None => true,
                Some((i, j)) => {
                    (!moved(i) && !moved(j))
                        || (find(&now, i, j).is_some() && find(&now, i, j) == find(&before, i, j))
                }
            };
            if keep {
                links.push(link);
            }
        }
        for link in &now {
            if find(&before, link.a, link.b) == find(&now, link.a, link.b) {
                continue;
            }
            let (a, b) = (
                self.editor.nodes[link.a].node_id.as_str(),
                self.editor.nodes[link.b].node_id.as_str(),
            );
            let (a_edge, b_edge) = link.side_of_b().edges();
            let mut id = format!("gui-{}-{}", short_key(a), short_key(b));
            while links.iter().any(|existing| existing.id == id) {
                id.push('x');
            }
            links.push(config::LinkConfig {
                id,
                from: config::EndpointConfig {
                    node: node_reference(a),
                    display: config::AUTO_DISPLAY.to_owned(),
                    edge: a_edge,
                    start: 0.0,
                    end: 0.0,
                },
                to: config::EndpointConfig {
                    node: node_reference(b),
                    display: config::AUTO_DISPLAY.to_owned(),
                    edge: b_edge,
                    start: 0.0,
                    end: 0.0,
                },
                bidirectional: true,
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
        for link in self.faced(&editor.placement) {
            let (a, b) = (&editor.nodes[link.a], &editor.nodes[link.b]);
            let documented = self.editor.links.iter().find(|l| {
                let (Some(f), Some(t)) = (l.from.as_ref(), l.to.as_ref()) else {
                    return false;
                };
                (f.node_id == a.node_id && t.node_id == b.node_id)
                    || (f.node_id == b.node_id && t.node_id == a.node_id)
            });
            let whole_desktop = match documented {
                Some(doc) if !dirty => [doc.from.as_ref(), doc.to.as_ref()]
                    .into_iter()
                    .flatten()
                    .any(|e| e.display_id == config::AUTO_DISPLAY),
                _ => true,
            };
            let offline = !a.online || !b.online;
            let active = !offline
                && target != this
                && this.is_some()
                && ((this == Some(link.a) && target == Some(link.b))
                    || (this == Some(link.b) && target == Some(link.a)));
            let state = if offline {
                LinkState::Offline
            } else if documented.is_some_and(|d| !d.active) && !dirty {
                LinkState::Inactive
            } else if active {
                LinkState::Active
            } else {
                LinkState::Idle
            };
            let forward = this == Some(link.a);
            links.push(MapLink {
                a: link.a,
                b: link.b,
                from: link.from,
                to: link.to,
                dir: link.dir,
                span: link.span,
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
