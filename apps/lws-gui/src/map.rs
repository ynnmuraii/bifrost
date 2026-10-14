//! The monitor map, as pure geometry: placement, snapping and links.
//!
//! Nothing here knows `egui` or the wire: the functions take rectangles and answer rectangles,
//! so `cargo test -p lws-gui` checks the whole of «Карта мониторов» — where a computer is
//! first drawn, where a drag lands, which edges end up linked — without a window.
//!
//! # Units
//!
//! The map works in *stage units*: the nominal stage is [`STAGE_W`] × [`STAGE_H`] and a
//! display of `logical_w` units is drawn `logical_w * scale` stage units wide, where `scale`
//! is [`BASE_SCALE`] shrunk until every computer fits the stage ([`arrange`]). The gaps and
//! snap distances below are in stage units, so they feel the same whatever the desktop sizes.
//!
//! # What a link is
//!
//! Two computers are linked when their desktops (the bounding box of all their displays) face
//! each other within [`LINK_MAX_GAP`] with at least [`MIN_OVERLAP`] of shared edge — the rule of
//! the mockup, taken at the granularity the document is written in: «Применить» writes one
//! `auto` (whole desktop) link per facing pair, so a drag and a document cannot disagree.
//! Vertically stacked desktops overlap horizontally and join Bottom ↔ Top; side-by-side
//! desktops overlap vertically and join Right ↔ Left.

use crate::view_model::Side;

/// Nominal stage width.
pub const STAGE_W: f64 = 916.0;
/// Nominal stage height.
pub const STAGE_H: f64 = 270.0;
/// The distance a snapped computer keeps from its neighbour.
pub const GAP: f64 = 72.0;
/// How near (stage units) an edge has to be before the drag snaps onto it.
pub const SNAP: f64 = 14.0;
/// The widest gap that still counts as «edge to edge».
pub const LINK_MAX_GAP: f64 = 110.0;
/// The least shared edge a link needs.
pub const MIN_OVERLAP: f64 = 16.0;
/// Stage units per logical unit before shrinking (1920 logical → 144).
pub const BASE_SCALE: f64 = 0.075;
/// Computers that are not linked to anyone are parked this far below the rest.
const PARK_GAP: f64 = 130.0;

/// A point of the stage.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Pt {
    /// Horizontal.
    pub x: f64,
    /// Vertical.
    pub y: f64,
}

impl Pt {
    /// A point.
    pub const fn new(x: f64, y: f64) -> Self {
        Pt { x, y }
    }
}

/// A rectangle of the stage.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    /// Left.
    pub x: f64,
    /// Top.
    pub y: f64,
    /// Width.
    pub w: f64,
    /// Height.
    pub h: f64,
}

impl Rect {
    /// A rectangle.
    pub const fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Rect { x, y, w, h }
    }
}

/// One computer: its displays relative to its own top-left corner, and the box around them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Shape {
    /// The displays, relative to the computer's origin.
    pub mons: Vec<Rect>,
    /// Width of the box around all displays.
    pub w: f64,
    /// Height of the box around all displays.
    pub h: f64,
}

impl Shape {
    /// The shape of `displays` (logical rectangles) drawn at `scale`; the box starts at 0,0.
    pub fn new(displays: &[Rect], scale: f64) -> Self {
        let min_x = displays.iter().map(|d| d.x).fold(f64::INFINITY, f64::min);
        let min_y = displays.iter().map(|d| d.y).fold(f64::INFINITY, f64::min);
        let mons: Vec<Rect> = displays
            .iter()
            .map(|d| {
                Rect::new(
                    (d.x - min_x) * scale,
                    (d.y - min_y) * scale,
                    d.w * scale,
                    d.h * scale,
                )
            })
            .collect();
        let w = mons.iter().map(|m| m.x + m.w).fold(0.0, f64::max);
        let h = mons.iter().map(|m| m.y + m.h).fold(0.0, f64::max);
        Shape { mons, w, h }
    }
}

/// «Computer `b` is on side `side` of computer `a`» — one link of the applied document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocLink {
    /// The computer the link leaves.
    pub a: usize,
    /// Which side of `a` the link is on.
    pub side: Side,
    /// The computer it arrives at.
    pub b: usize,
}

/// The direction two linked computers face each other in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    /// Side by side: `a` is left of `b`.
    Horizontal,
    /// Stacked: `a` is above `b`.
    Vertical,
}

/// Two computers that face each other.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeoLink {
    /// The left (or upper) computer.
    pub a: usize,
    /// The right (or lower) computer.
    pub b: usize,
    /// How they face.
    pub dir: Dir,
    /// The empty space between the two edges (may be slightly negative).
    pub gap: f64,
    /// Where the link leaves `a`'s edge.
    pub from: Pt,
    /// Where it arrives at `b`'s edge.
    pub to: Pt,
    /// The shared stretch of the two edges, along the edge's own axis.
    pub span: (f64, f64),
}

impl GeoLink {
    /// Which side of `a` computer `b` is on.
    pub const fn side_of_b(&self) -> Side {
        match self.dir {
            Dir::Horizontal => Side::Right,
            Dir::Vertical => Side::Bottom,
        }
    }
}

/// The computers drawn on the stage: their shapes, positions and the scale they were drawn at.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Arrangement {
    /// Stage units per logical unit.
    pub scale: f64,
    /// One shape per computer.
    pub shapes: Vec<Shape>,
    /// One position per computer.
    pub pos: Vec<Pt>,
}

fn opposite(side: Side) -> Side {
    match side {
        Side::Left => Side::Right,
        Side::Right => Side::Left,
        Side::Top => Side::Bottom,
        Side::Bottom => Side::Top,
    }
}

/// Where `other` sits when it is put on `side` of `me`, centred on the shared edge and `GAP`
/// away from it.
pub fn adjacent(me: &Shape, me_at: Pt, other: &Shape, side: Side) -> Pt {
    match side {
        Side::Right => Pt::new(me_at.x + me.w + GAP, me_at.y + (me.h - other.h) / 2.0),
        Side::Left => Pt::new(me_at.x - GAP - other.w, me_at.y + (me.h - other.h) / 2.0),
        Side::Bottom => Pt::new(me_at.x + (me.w - other.w) / 2.0, me_at.y + me.h + GAP),
        Side::Top => Pt::new(me_at.x + (me.w - other.w) / 2.0, me_at.y - GAP - other.h),
    }
}

/// The corners of the box around every computer: `(min_x, min_y, max_x, max_y)`.
pub fn bounds(shapes: &[Shape], pos: &[Pt]) -> (f64, f64, f64, f64) {
    let mut b = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for (shape, at) in shapes.iter().zip(pos) {
        b = (
            b.0.min(at.x),
            b.1.min(at.y),
            b.2.max(at.x + shape.w),
            b.3.max(at.y + shape.h),
        );
    }
    if b.0.is_finite() {
        b
    } else {
        (0.0, 0.0, 0.0, 0.0)
    }
}

/// The first placement: the applied links decide where each computer goes.
///
/// `anchor` (this computer) is the root; whatever is linked to it is placed on the side the
/// link names, `GAP` away; computers with no link at all are parked below the rest, far enough
/// that [`links`] cannot mistake them for neighbours. The result is centred on the stage.
pub fn place(shapes: &[Shape], doc_links: &[DocLink], anchor: usize) -> Vec<Pt> {
    let n = shapes.len();
    let mut pos: Vec<Option<Pt>> = vec![None; n];
    let mut queue = std::collections::VecDeque::new();
    let mut seeds = std::iter::once(anchor).chain(0..n).filter(|i| *i < n);
    let mut park_x: Option<f64> = None;
    while let Some(seed) = seeds.find(|seed| pos[*seed].is_none()) {
        let at = if queue.is_empty() && pos.iter().all(Option::is_none) {
            Pt::new(0.0, 0.0)
        } else {
            let placed: Vec<(Shape, Pt)> = pos
                .iter()
                .zip(shapes)
                .filter_map(|(p, s)| p.map(|p| (s.clone(), p)))
                .collect();
            let (shapes_p, pos_p): (Vec<Shape>, Vec<Pt>) = placed.into_iter().unzip();
            let (min_x, _, _, max_y) = bounds(&shapes_p, &pos_p);
            let x = park_x.unwrap_or(min_x);
            park_x = Some(x + shapes[seed].w + PARK_GAP);
            Pt::new(x, max_y + PARK_GAP)
        };
        pos[seed] = Some(at);
        queue.push_back(seed);
        while let Some(i) = queue.pop_front() {
            let Some(at_i) = pos[i] else { continue };
            for link in doc_links {
                let (j, side) = if link.a == i {
                    (link.b, link.side)
                } else if link.b == i {
                    (link.a, opposite(link.side))
                } else {
                    continue;
                };
                if j >= n || pos[j].is_some() {
                    continue;
                }
                pos[j] = Some(adjacent(&shapes[i], at_i, &shapes[j], side));
                queue.push_back(j);
            }
        }
    }
    let mut pos: Vec<Pt> = pos.into_iter().map(|p| p.unwrap_or_default()).collect();
    let (min_x, min_y, max_x, max_y) = bounds(shapes, &pos);
    let (dx, dy) = (
        STAGE_W / 2.0 - (min_x + max_x) / 2.0,
        STAGE_H / 2.0 - (min_y + max_y) / 2.0,
    );
    for p in &mut pos {
        p.x = (p.x + dx).round();
        p.y = (p.y + dy).round();
    }
    pos
}

/// Shapes and first placement for `nodes` (each the logical rectangles of one computer),
/// shrunk until the whole arrangement fits the stage.
pub fn arrange(nodes: &[Vec<Rect>], doc_links: &[DocLink], anchor: usize) -> Arrangement {
    let mut scale = BASE_SCALE;
    let mut result = Arrangement::default();
    for _ in 0..10 {
        let shapes: Vec<Shape> = nodes.iter().map(|n| Shape::new(n, scale)).collect();
        let pos = place(&shapes, doc_links, anchor);
        let (min_x, min_y, max_x, max_y) = bounds(&shapes, &pos);
        let (w, h) = (max_x - min_x, max_y - min_y);
        result = Arrangement { scale, shapes, pos };
        let fit = (STAGE_W * 0.96 / w.max(1.0)).min(STAGE_H * 0.96 / h.max(1.0));
        if fit >= 1.0 {
            break;
        }
        scale *= fit * 0.97;
    }
    result
}

/// Where a dragged computer lands: the pointer position `(x, y)` pulled onto the nearest edge
/// of another computer (side by side at [`GAP`], stacked at [`GAP`], or aligned) when one is
/// within [`SNAP`]. Unsnapped coordinates are kept near the stage; a snapped edge can extend
/// beyond it, otherwise the stage boundary would break top/bottom adjacency.
pub fn snap(shapes: &[Shape], pos: &[Pt], me: usize, x: f64, y: f64) -> Pt {
    let Some(mine) = shapes.get(me) else {
        return Pt::new(x, y);
    };
    let (mut bx, mut by, mut dx, mut dy) = (x, y, SNAP, SNAP);
    for (o, (other, at)) in shapes.iter().zip(pos).enumerate() {
        if o == me {
            continue;
        }
        let xs = [
            at.x + other.w + GAP,
            at.x - GAP - mine.w,
            at.x,
            at.x + other.w - mine.w,
            at.x + (other.w - mine.w) / 2.0,
        ];
        for c in xs {
            let d = (x - c).abs();
            if d < dx {
                dx = d;
                bx = c;
            }
        }
        let ys = [
            at.y,
            at.y + other.h - mine.h,
            at.y + (other.h - mine.h) / 2.0,
            at.y + other.h + GAP,
            at.y - GAP - mine.h,
        ];
        for c in ys {
            let d = (y - c).abs();
            if d < dy {
                dy = d;
                by = c;
            }
        }
    }
    Pt::new(
        bx.clamp(-60.0, (STAGE_W - mine.w + 60.0).max(-60.0))
            .round(),
        if dy < SNAP {
            by.round()
        } else {
            by.clamp(-70.0, (STAGE_H - mine.h + 40.0).max(-70.0))
                .round()
        },
    )
}

/// `(direction, gap, from, to, swapped, shared span)` of two facing boxes.
type Face = (Dir, f64, Pt, Pt, bool, (f64, f64));

/// The facing of two boxes, when they face at all.
fn facing(a: (&Shape, Pt), b: (&Shape, Pt)) -> Option<Face> {
    let mut best: Option<Face> = None;
    let mut consider = |cand: Face| {
        if best.as_ref().is_none_or(|b| cand.1 < b.1) {
            best = Some(cand);
        }
    };
    // `swapped` records that the pair was tried as (b, a): the caller wants `a` to be first.
    for (l, r, swapped) in [(a, b, false), (b, a, true)] {
        let gap = r.1.x - (l.1.x + l.0.w);
        let (o0, o1) = (l.1.y.max(r.1.y), (l.1.y + l.0.h).min(r.1.y + r.0.h));
        if (-2.0..=LINK_MAX_GAP).contains(&gap) && o1 - o0 >= MIN_OVERLAP {
            let mid = (o0 + o1) / 2.0;
            consider((
                Dir::Horizontal,
                gap,
                Pt::new(l.1.x + l.0.w, mid),
                Pt::new(r.1.x, mid),
                swapped,
                (o0, o1),
            ));
        }
    }
    for (t, d, swapped) in [(a, b, false), (b, a, true)] {
        let gap = d.1.y - (t.1.y + t.0.h);
        let (o0, o1) = (t.1.x.max(d.1.x), (t.1.x + t.0.w).min(d.1.x + d.0.w));
        if (-2.0..=LINK_MAX_GAP).contains(&gap) && o1 - o0 >= MIN_OVERLAP {
            let mid = (o0 + o1) / 2.0;
            consider((
                Dir::Vertical,
                gap,
                Pt::new(mid, t.1.y + t.0.h),
                Pt::new(mid, d.1.y),
                swapped,
                (o0, o1),
            ));
        }
    }
    best
}

/// Every pair of computers whose edges face each other. `a` of a link is the left/upper one.
pub fn links(shapes: &[Shape], pos: &[Pt]) -> Vec<GeoLink> {
    let mut out = Vec::new();
    for i in 0..shapes.len().min(pos.len()) {
        for j in i + 1..shapes.len().min(pos.len()) {
            if let Some((dir, gap, from, to, swapped, span)) =
                facing((&shapes[i], pos[i]), (&shapes[j], pos[j]))
            {
                let (a, b) = if swapped { (j, i) } else { (i, j) };
                out.push(GeoLink {
                    a,
                    b,
                    dir,
                    gap,
                    from,
                    to,
                    span,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(w: f64, h: f64) -> Shape {
        Shape::new(
            &[Rect::new(0.0, 0.0, w / BASE_SCALE, h / BASE_SCALE)],
            BASE_SCALE,
        )
    }

    #[test]
    fn a_shape_is_normalised_to_its_own_corner() {
        let shape = Shape::new(
            &[
                Rect::new(-1920.0, 100.0, 1920.0, 1080.0),
                Rect::new(0.0, 0.0, 2560.0, 1440.0),
            ],
            0.1,
        );
        assert!((shape.mons[0].y - 10.0).abs() < 1e-9 && shape.mons[0].x.abs() < 1e-9);
        assert!((shape.mons[0].w - 192.0).abs() < 1e-9 && (shape.mons[0].h - 108.0).abs() < 1e-9);
        assert!((shape.w - 448.0).abs() < 1e-9 && (shape.h - 144.0).abs() < 1e-9);
    }

    #[test]
    fn a_drag_near_a_neighbour_snaps_to_the_gap_and_aligns_the_top() {
        let shapes = [boxed(144.0, 90.0), boxed(192.0, 108.0)];
        let pos = [Pt::new(100.0, 60.0), Pt::new(0.0, 0.0)];
        // Dragging node 1 to just right of node 0, a few units off.
        let landed = snap(&shapes, &pos, 1, 100.0 + 144.0 + GAP + 9.0, 60.0 + 5.0);
        assert_eq!(landed, Pt::new(316.0, 60.0));
    }

    #[test]
    fn a_drag_far_from_everything_stays_where_it_was_put() {
        let shapes = [boxed(144.0, 90.0), boxed(144.0, 90.0)];
        let pos = [Pt::new(0.0, 0.0), Pt::new(0.0, 0.0)];
        assert_eq!(snap(&shapes, &pos, 1, 500.0, 200.0), Pt::new(500.0, 200.0));
    }

    #[test]
    fn a_drag_below_a_neighbour_snaps_to_a_stack() {
        let shapes = [boxed(144.0, 90.0), boxed(144.0, 90.0)];
        let pos = [Pt::new(300.0, 10.0), Pt::new(0.0, 0.0)];
        let landed = snap(&shapes, &pos, 1, 305.0, 10.0 + 90.0 + GAP - 6.0);
        assert_eq!(landed, Pt::new(300.0, 172.0));
    }

    #[test]
    fn vertical_snaps_survive_the_stage_boundary_in_both_directions() {
        let shapes = [boxed(144.0, 150.0), boxed(192.0, 150.0)];
        let pos = [Pt::new(300.0, 60.0), Pt::default()];
        for (y, expected, first, second) in [
            (60.0 + 150.0 + GAP - 6.0, 282.0, 0, 1),
            (60.0 - GAP - 150.0 + 6.0, -162.0, 1, 0),
        ] {
            let landed = snap(&shapes, &pos, 1, 305.0, y);
            assert_eq!(landed, Pt::new(300.0, expected));
            let found = links(&shapes, &[pos[0], landed]);
            assert_eq!(found.len(), 1);
            assert_eq!(
                (found[0].a, found[0].b, found[0].dir),
                (first, second, Dir::Vertical)
            );
            assert_eq!(found[0].gap, GAP);
            assert_eq!(found[0].side_of_b(), Side::Bottom);
        }
    }

    #[test]
    fn a_drag_is_kept_near_the_stage() {
        let shapes = [boxed(144.0, 90.0)];
        let landed = snap(&shapes, &[Pt::new(0.0, 0.0)], 0, -5000.0, 9000.0);
        assert_eq!(landed, Pt::new(-60.0, STAGE_H - 90.0 + 40.0));
    }

    #[test]
    fn side_by_side_computers_are_linked_with_the_left_one_first() {
        let shapes = [boxed(144.0, 90.0), boxed(192.0, 108.0)];
        // Node 1 is to the LEFT of node 0, 72 units away.
        let pos = [Pt::new(300.0, 100.0), Pt::new(300.0 - GAP - 192.0, 90.0)];
        let found = links(&shapes, &pos);
        assert_eq!(found.len(), 1);
        let link = found[0];
        assert_eq!((link.a, link.b, link.dir), (1, 0, Dir::Horizontal));
        assert_eq!(link.side_of_b(), Side::Right);
        assert!((link.gap - GAP).abs() < 1e-6);
        assert!((link.from.x - (300.0 - GAP)).abs() < 1e-6);
        assert_eq!(link.to.x, 300.0);
        // The link runs through the middle of the shared edge: y 100..190 ∩ 90..198 → 145.
        assert!((link.from.y - 145.0).abs() < 1e-6);
    }

    #[test]
    fn stacked_computers_are_linked_vertically() {
        let shapes = [boxed(144.0, 90.0), boxed(144.0, 90.0)];
        let pos = [Pt::new(0.0, 100.0), Pt::new(20.0, 0.0)];
        let found = links(&shapes, &pos);
        assert_eq!(
            (found[0].a, found[0].b, found[0].dir),
            (1, 0, Dir::Vertical)
        );
        assert_eq!(found[0].side_of_b(), Side::Bottom);
    }

    #[test]
    fn far_or_barely_touching_computers_are_not_linked() {
        let shapes = [boxed(144.0, 90.0), boxed(144.0, 90.0)];
        assert!(links(&shapes, &[Pt::new(0.0, 0.0), Pt::new(144.0 + 111.0, 0.0)]).is_empty());
        // 10 units of shared edge is less than MIN_OVERLAP.
        assert!(links(&shapes, &[Pt::new(0.0, 0.0), Pt::new(144.0 + 50.0, 80.0)]).is_empty());
    }

    #[test]
    fn the_first_placement_follows_the_applied_links_and_reproduces_them() {
        let nodes = vec![
            vec![Rect::new(0.0, 0.0, 1920.0, 1200.0)],
            vec![
                Rect::new(0.0, 0.0, 1920.0, 1080.0),
                Rect::new(1920.0, 0.0, 2560.0, 1440.0),
            ],
            vec![Rect::new(0.0, 0.0, 3840.0, 2160.0)],
        ];
        // Node 1 is this computer: node 0 on its left, node 2 on its right.
        let doc = [
            DocLink {
                a: 1,
                side: Side::Left,
                b: 0,
            },
            DocLink {
                a: 1,
                side: Side::Right,
                b: 2,
            },
        ];
        let arrangement = arrange(&nodes, &doc, 1);
        let found = links(&arrangement.shapes, &arrangement.pos);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().any(|l| (l.a, l.b) == (0, 1)));
        assert!(found.iter().any(|l| (l.a, l.b) == (1, 2)));
        let (min_x, _, max_x, _) = bounds(&arrangement.shapes, &arrangement.pos);
        assert!(max_x - min_x <= STAGE_W, "{min_x}..{max_x}");
    }

    #[test]
    fn a_computer_with_no_link_is_parked_where_it_links_to_nobody() {
        let nodes = vec![
            vec![Rect::new(0.0, 0.0, 1920.0, 1080.0)],
            vec![Rect::new(0.0, 0.0, 1920.0, 1080.0)],
            vec![Rect::new(0.0, 0.0, 1920.0, 1080.0)],
        ];
        let doc = [DocLink {
            a: 0,
            side: Side::Right,
            b: 1,
        }];
        let arrangement = arrange(&nodes, &doc, 0);
        let found = links(&arrangement.shapes, &arrangement.pos);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].a, found[0].b), (0, 1));
    }

    #[test]
    fn a_huge_desktop_is_shrunk_to_fit_the_stage() {
        let nodes = vec![
            vec![Rect::new(0.0, 0.0, 7680.0, 4320.0)],
            vec![Rect::new(0.0, 0.0, 7680.0, 4320.0)],
        ];
        let doc = [DocLink {
            a: 0,
            side: Side::Right,
            b: 1,
        }];
        let arrangement = arrange(&nodes, &doc, 0);
        assert!(arrangement.scale < BASE_SCALE);
        let (min_x, min_y, max_x, max_y) = bounds(&arrangement.shapes, &arrangement.pos);
        assert!(max_x - min_x <= STAGE_W && max_y - min_y <= STAGE_H);
        assert_eq!(links(&arrangement.shapes, &arrangement.pos).len(), 1);
    }

    #[test]
    fn adjacent_puts_the_other_computer_on_the_named_side() {
        let me = boxed(144.0, 90.0);
        let other = boxed(100.0, 50.0);
        let at = Pt::new(200.0, 100.0);
        let near = |p: Pt, x: f64, y: f64| (p.x - x).abs() < 1e-6 && (p.y - y).abs() < 1e-6;
        assert!(near(adjacent(&me, at, &other, Side::Right), 416.0, 120.0));
        assert!(near(adjacent(&me, at, &other, Side::Left), 28.0, 120.0));
        assert!(near(adjacent(&me, at, &other, Side::Bottom), 222.0, 262.0));
        assert!(near(adjacent(&me, at, &other, Side::Top), 222.0, -22.0));
        for side in Side::ALL {
            let pos = [at, adjacent(&me, at, &other, side)];
            assert_eq!(
                links(&[me.clone(), other.clone()], &pos).len(),
                1,
                "{side:?}"
            );
        }
    }
}
