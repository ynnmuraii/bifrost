//! The map's model: the applied document turned into something draggable, and back.
//!
//! The geometry is [`crate::map`]; this is the part that knows the wire. A drag changes only
//! [`LayoutEditor::placement`] (a draft, never sent); «Применить» is the one place a draft
//! becomes a document ([`ViewModel::layout_action`] → `UpdateTopology`, the existing CAS path),
//! and «Авто» is `LayoutAuto`. The functions of this file are pure in the view model, so the
//! drag → document path is a unit test and not a click.

use super::{
    endpoint_config, node_id_bytes, node_reference, short_id, short_key, Action, EditorDisplay,
    Side, ViewModel,
};
use crate::map::{self, DocLink, Pt, Rect};

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
    /// «ввод», «осн.», «офлайн» or empty.
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
    /// «● клавиатура и мышь здесь», «○ не в сети» or empty.
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
    /// The text on the link.
    pub label: String,
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

/// What «Передать ввод» does right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    /// The name after the arrow.
    pub target: String,
    /// The `switch` argument; `None` when there is nothing to switch to.
    pub peer: Option<String>,
    /// `false` while the core would refuse.
    pub enabled: bool,
    /// Why not, in a sentence.
    pub why: Option<String>,
}

fn side_of_edge(edge: &str) -> Option<Side> {
    Some(match config::Edge::parse(edge).ok()? {
        config::Edge::Left => Side::Left,
        config::Edge::Right => Side::Right,
        config::Edge::Top => Side::Top,
        config::Edge::Bottom => Side::Bottom,
    })
}

fn opposite(side: Side) -> Side {
    match side {
        Side::Left => Side::Right,
        Side::Right => Side::Left,
        Side::Top => Side::Bottom,
        Side::Bottom => Side::Top,
    }
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
        self.editor
            .links
            .iter()
            .filter_map(|link| {
                let (from, to) = (link.from.as_ref()?, link.to.as_ref()?);
                let (a, b) = (self.index_of(&from.node_id)?, self.index_of(&to.node_id)?);
                (a != b).then_some(())?;
                Some(DocLink {
                    a,
                    side: side_of_edge(&from.edge)?,
                    b,
                })
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
        self.sync_side();
    }

    /// Throws the draft away: the map goes back to the applied document.
    pub fn discard_draft(&mut self) {
        self.editor.placement = self.editor.baseline.clone();
        self.sync_side();
    }

    /// The indices of this computer and the other one of the pair, when both are known.
    fn pair(&self) -> Option<(usize, usize)> {
        let this = self.index_of(self.self_node()?)?;
        let other = self.index_of(self.other_node()?)?;
        (this != other).then_some((this, other))
    }

    /// Reads the side of the pair off the map, when the two face each other.
    fn sync_side(&mut self) {
        let Some((this, other)) = self.pair() else {
            return;
        };
        let found = map::links(&self.editor.shapes, &self.editor.placement)
            .into_iter()
            .find(|l| (l.a, l.b) == (this, other) || (l.a, l.b) == (other, this));
        if let Some(link) = found {
            self.editor.side = if link.a == this {
                link.side_of_b()
            } else {
                opposite(link.side_of_b())
            };
        }
    }

    /// «Сторона»: the other computer goes to that side of this one, `GAP` away.
    pub fn set_side(&mut self, side: Side) {
        self.editor.side = side;
        let Some((this, other)) = self.pair() else {
            return;
        };
        if self.editor.shapes.len() != self.editor.nodes.len() {
            return;
        }
        let at = map::adjacent(
            &self.editor.shapes[this],
            self.editor.placement[this],
            &self.editor.shapes[other],
            side,
        );
        self.editor.placement[other] = Pt::new(at.x.round(), at.y.round());
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
        self.sync_side();
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
        self.sync_side();
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
                let ends = self
                    .index_of(&from.node_id)
                    .zip(self.index_of(&to.node_id));
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
    /// loses its link.
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

    /// «Применить»: the `UpdateTopology` of the edited document, against the revision it was
    /// edited from. `None` while there is nothing to apply.
    pub fn layout_action(&self) -> Option<Action> {
        if !self.editor.has_document() || !self.editor.dirty() {
            return None;
        }
        Some(Action::UpdateTopology {
            layout_toml: self.layout_toml().into_bytes(),
            expected_revision: self.editor.revision,
            expected_author: node_id_bytes(&self.editor.author),
        })
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

    /// «Ввод <…>» of the top bar.
    pub fn route_text(&self) -> String {
        let Some(status) = &self.status else {
            return "—".to_owned();
        };
        let this = self.this_name();
        match status.focus.as_str() {
            "LOCAL" => "этот компьютер".to_owned(),
            "REMOTE" if !status.active_peer.is_empty() => {
                format!("{this} → {}", status.active_peer)
            }
            "REMOTE" => format!("{this} → другой компьютер"),
            "PREPARING" if !status.active_peer.is_empty() => {
                format!("{this} → {} …", status.active_peer)
            }
            "PREPARING" => "переход…".to_owned(),
            "BLOCKED" => "остановлен из-за ошибки".to_owned(),
            _ => "—".to_owned(),
        }
    }

    /// The name of this computer.
    pub fn this_name(&self) -> String {
        self.self_node()
            .and_then(|id| self.editor.nodes.iter().find(|n| n.node_id == id))
            .map(|n| n.label())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| self.config.display_name.clone())
    }

    /// The headline of the map: «3 компьютера · 4 экрана».
    pub fn map_title(&self) -> String {
        let nodes = self.editor.nodes.len();
        let screens: usize = self.editor.nodes.iter().map(|n| n.displays.len()).sum();
        format!(
            "{nodes} {} · {screens} {}",
            plural(nodes, ["компьютер", "компьютера", "компьютеров"]),
            plural(screens, ["экран", "экрана", "экранов"]),
        )
    }

    /// «ревизия 14 · координатор WORKSTATION», or «черновик · не применён».
    pub fn revision_text(&self) -> String {
        if self.editor.dirty() {
            return "черновик · не применён".to_owned();
        }
        let Some(status) = &self.status else {
            return format!("ревизия {}", self.editor.revision);
        };
        let coordinator = if status.is_owner {
            Some(self.this_name())
        } else {
            status
                .members
                .iter()
                .find(|m| m.is_owner)
                .map(|m| m.display_name.clone())
                .filter(|n| !n.is_empty())
        };
        match coordinator {
            Some(name) => format!("ревизия {} · координатор {name}", self.editor.revision),
            None => format!("ревизия {}", self.editor.revision),
        }
    }

    /// Everything the map draws, from the draft and the live state.
    pub fn map_model(&self) -> MapModel {
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
                let rect = editor.shapes[i]
                    .mons
                    .get(k)
                    .copied()
                    .unwrap_or_default();
                let tag = if is_target && (display.primary || single) {
                    "ввод"
                } else if !node.online {
                    "офлайн"
                } else if display.primary && !single {
                    "осн."
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
                if backend == "неизвестно" {
                    "этот компьютер".to_owned()
                } else {
                    format!("этот компьютер · {backend}")
                }
            } else {
                String::new()
            };
            let focus = if is_target {
                "● клавиатура и мышь здесь".to_owned()
            } else if !node.online {
                "○ не в сети".to_owned()
            } else {
                String::new()
            };
            nodes.push(MapNode {
                node_id: node.node_id.clone(),
                name: node.label(),
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
            let horizontal = link.dir == map::Dir::Horizontal;
            let label = match state {
                LinkState::Offline => "офлайн".to_owned(),
                LinkState::Inactive => "неактивна".to_owned(),
                LinkState::Active => {
                    let to_b = target == Some(link.b);
                    match (horizontal, to_b) {
                        (true, true) => "ввод →",
                        (true, false) => "← ввод",
                        (false, true) => "ввод ↓",
                        (false, false) => "ввод ↑",
                    }
                    .to_owned()
                }
                LinkState::Idle if whole_desktop => "весь стол".to_owned(),
                LinkState::Idle => "край".to_owned(),
            };
            links.push(MapLink {
                a: link.a,
                b: link.b,
                from: link.from,
                to: link.to,
                dir: link.dir,
                span: link.span,
                state,
                whole_desktop,
                label,
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
                    format!("компьютер {}", short_id(&m.node_id))
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

    /// What «Передать ввод» does now, and whether the core would let it.
    pub fn transfer(&self) -> Transfer {
        let none = |why: &str| Transfer {
            target: String::new(),
            peer: None,
            enabled: false,
            why: Some(why.to_owned()),
        };
        let Some(status) = &self.status else {
            return none("нет данных от службы");
        };
        if !self.connection.is_connected() {
            return none("служба не запущена");
        }
        let peers = self.peers();
        if status.focus == "REMOTE" {
            let back = peers
                .iter()
                .find(|p| p.name == status.active_peer)
                .or_else(|| peers.first());
            return Transfer {
                target: self.this_name(),
                peer: back.map(|p| p.node_id.clone()),
                enabled: back.is_some() && status.transition_disabled_reason.is_empty(),
                why: Some(status.transition_disabled_reason.clone()).filter(|w| !w.is_empty()),
            };
        }
        // The neighbours on the map first, then anyone who is in the network.
        let neighbours: Vec<String> = match self.self_node().and_then(|id| self.index_of(id)) {
            Some(this) => self
                .faced(&self.editor.baseline)
                .iter()
                .filter_map(|l| {
                    let other = if l.a == this {
                        l.b
                    } else if l.b == this {
                        l.a
                    } else {
                        return None;
                    };
                    self.editor.nodes.get(other).map(|n| n.node_id.clone())
                })
                .collect(),
            None => Vec::new(),
        };
        let next = peers
            .iter()
            .filter(|p| p.online)
            .min_by_key(|p| !neighbours.contains(&p.node_id));
        let Some(next) = next else {
            return none("нет компьютеров в сети");
        };
        let reason = if status.focus != "LOCAL" {
            Some("переход уже идёт или остановлен".to_owned())
        } else {
            Some(status.transition_disabled_reason.clone()).filter(|w| !w.is_empty())
        };
        Transfer {
            target: next.name.clone(),
            peer: Some(next.node_id.clone()),
            enabled: reason.is_none(),
            why: reason,
        }
    }

    /// The member input is with now, as its node id: this computer while it is local.
    pub fn route_node_id(&self) -> String {
        self.route_target()
            .and_then(|i| self.editor.nodes.get(i))
            .map(|n| n.node_id.clone())
            .unwrap_or_default()
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
