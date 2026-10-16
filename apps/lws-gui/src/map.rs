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
//! is [`BASE_SCALE`] shrunk until every computer fits the stage ([`arrange`]).
//! The UI converts its screen-pixel magnet threshold into stage units.
//!
//! Links created by the map are exact overlaps of touching display edges, not
//! bounding boxes or whole-desktop silhouette anchors. Legacy documents still
//! determine their initial placement.

use crate::view_model::Side;

/// Nominal stage width.
pub const STAGE_W: f64 = 916.0;
/// Nominal stage height.
pub const STAGE_H: f64 = 270.0;
/// Initial spacing for legacy whole-desktop links, never a drag snap target.
pub const GAP: f64 = 72.0;
/// How near (stage units) an edge has to be before the drag snaps onto it.
pub const SNAP: f64 = 12.0;
/// Floating-point tolerance for touching edges.
pub const LINK_MAX_GAP: f64 = 1e-6;
/// Positive overlap required for a contact.
pub const MIN_OVERLAP: f64 = 1e-6;
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
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DocLink {
    /// The computer the link leaves.
    pub a: usize,
    /// Which side of `a` the link is on.
    pub side: Side,
    /// The computer it arrives at.
    pub b: usize,
    /// Concrete endpoint segment bounds in node-local units at [`BASE_SCALE`].
    pub contact: Option<(Rect, Rect)>,
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
    /// Display index on the left/upper computer.
    pub a_display: usize,
    /// Display index on the right/lower computer.
    pub b_display: usize,
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
    place_at_scale(shapes, doc_links, anchor, BASE_SCALE)
}

/// Reconstruct concrete contacts at the scale used by the current shapes.
pub fn place_at_scale(
    shapes: &[Shape], doc_links: &[DocLink], anchor: usize, scale: f64,
) -> Vec<Pt> {
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
                pos[j] = Some(if let Some((from, to)) = link.contact {
                    let (from, to) = if link.a == i { (from, to) } else { (to, from) };
                    let factor = scale / BASE_SCALE;
                    Pt::new(
                        at_i.x + (from.x - to.x) * factor,
                        at_i.y + (from.y - to.y) * factor,
                    )
                } else {
                    adjacent(&shapes[i], at_i, &shapes[j], side)
                });
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
        p.x += dx;
        p.y += dy;
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
        let pos = place_at_scale(&shapes, doc_links, anchor, scale);
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

/// Snap perpendicular to the nearest real display edge, preserving tangent offset.
pub fn snap(shapes: &[Shape], pos: &[Pt], me: usize, x: f64, y: f64) -> Pt {
    snap_with_threshold(shapes, pos, me, x, y, SNAP)
}

/// Screen-aware variant, with the pixel threshold converted to stage units.
pub fn snap_with_threshold(
    shapes: &[Shape], pos: &[Pt], me: usize, x: f64, y: f64, threshold: f64,
) -> Pt {
    let Some(mine) = shapes.get(me) else { return Pt::new(x, y) };
    let mut best = threshold;
    let mut landed = Pt::new(x, y);
    for (index, (other, at)) in shapes.iter().zip(pos).enumerate() {
        if index == me { continue; }
        for m in &mine.mons {
            for o in &other.mons {
                let (mx, my) = (x + m.x, y + m.y);
                let (ox, oy) = (at.x + o.x, at.y + o.y);
                if (my + m.h).min(oy + o.h) - my.max(oy) > MIN_OVERLAP {
                    for delta in [ox + o.w - mx, ox - (mx + m.w)] {
                        if delta.abs() < best {
                            best = delta.abs();
                            landed = Pt::new(x + delta, y);
                        }
                    }
                }
                if (mx + m.w).min(ox + o.w) - mx.max(ox) > MIN_OVERLAP {
                    for delta in [oy + o.h - my, oy - (my + m.h)] {
                        if delta.abs() < best {
                            best = delta.abs();
                            landed = Pt::new(x, y + delta);
                        }
                    }
                }
            }
        }
    }
    landed
}

/// Every real display-edge contact; touching two monitors produces two links.
pub fn links(shapes: &[Shape], pos: &[Pt]) -> Vec<GeoLink> {
    let mut out = Vec::new();
    for i in 0..shapes.len().min(pos.len()) {
        for j in i + 1..shapes.len().min(pos.len()) {
            for (mi, m) in shapes[i].mons.iter().enumerate() {
                for (mj, n) in shapes[j].mons.iter().enumerate() {
                    for (a, am, ar, ap, b, bm, br, bp) in [
                        (i, mi, m, pos[i], j, mj, n, pos[j]),
                        (j, mj, n, pos[j], i, mi, m, pos[i]),
                    ] {
                        let (ax, ay) = (ap.x + ar.x, ap.y + ar.y);
                        let (bx, by) = (bp.x + br.x, bp.y + br.y);
                        for (dir, gap, start, end, edge) in [
                            (Dir::Horizontal, bx - (ax + ar.w), ay.max(by),
                                (ay + ar.h).min(by + br.h), ax + ar.w),
                            (Dir::Vertical, by - (ay + ar.h), ax.max(bx),
                                (ax + ar.w).min(bx + br.w), ay + ar.h),
                        ] {
                            if gap.abs() > LINK_MAX_GAP || end - start <= MIN_OVERLAP { continue; }
                            let mid = (start + end) / 2.0;
                            let (from, to) = match dir {
                                Dir::Horizontal => (Pt::new(edge, mid), Pt::new(bx, mid)),
                                Dir::Vertical => (Pt::new(mid, edge), Pt::new(mid, by)),
                            };
                            out.push(GeoLink {
                                a, b, a_display: am, b_display: bm, dir, gap, from, to,
                                span: (start, end),
                            });
                        }
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(w: f64, h: f64) -> Shape {
        Shape { mons: vec![Rect::new(0.0, 0.0, w, h)], w, h }
    }

    #[test]
    fn real_display_coordinates_are_normalized_without_rearranging() {
        let shape = Shape::new(&[
            Rect::new(-1920.0, 100.0, 1920.0, 1080.0),
            Rect::new(0.0, 0.0, 1080.0, 1920.0),
        ], 0.1);
        assert_eq!(shape.mons, vec![
            Rect::new(0.0, 10.0, 192.0, 108.0),
            Rect::new(192.0, 0.0, 108.0, 192.0),
        ]);
    }

    #[test]
    fn four_sides_snap_flush_and_preserve_the_tangent_offset() {
        let shapes = [boxed(144.0, 190.0), boxed(100.0, 80.0)];
        let pos = [Pt::new(300.0, 60.0), Pt::default()];
        for (drop, expected, dir) in [
            (Pt::new(450.0, 157.5), Pt::new(444.0, 157.5), Dir::Horizontal),
            (Pt::new(194.0, 157.5), Pt::new(200.0, 157.5), Dir::Horizontal),
            (Pt::new(327.5, 256.0), Pt::new(327.5, 250.0), Dir::Vertical),
            (Pt::new(327.5, -26.0), Pt::new(327.5, -20.0), Dir::Vertical),
        ] {
            let landed = snap(&shapes, &pos, 1, drop.x, drop.y);
            assert_eq!(landed, expected);
            let contacts = links(&shapes, &[pos[0], landed]);
            assert_eq!(contacts.len(), 1);
            assert_eq!(contacts[0].dir, dir);
            assert_eq!(contacts[0].gap, 0.0);
        }
    }

    #[test]
    fn free_placement_is_not_clamped_or_linked() {
        let shapes = [boxed(144.0, 90.0), boxed(100.0, 80.0)];
        let pos = [Pt::default(), Pt::default()];
        let landed = snap(&shapes, &pos, 1, -5000.5, 9000.25);
        assert_eq!(landed, Pt::new(-5000.5, 9000.25));
        assert!(links(&shapes, &[pos[0], landed]).is_empty());
        assert!(links(&shapes, &[pos[0], Pt::new(150.0, 0.0)]).is_empty());
    }

    #[test]
    fn lower_half_of_portrait_monitor_is_the_only_contact() {
        let desktop = Shape {
            mons: vec![Rect::new(0.0, 20.0, 192.0, 108.0),
                       Rect::new(192.0, 0.0, 108.0, 192.0)],
            w: 300.0, h: 192.0,
        };
        let shapes = [desktop, boxed(174.5, 96.0)];
        let pos = [Pt::default(), Pt::new(307.0, 96.0)];
        let laptop = snap(&shapes, &pos, 1, pos[1].x, pos[1].y);
        assert_eq!(laptop, Pt::new(300.0, 96.0));
        let contacts = links(&shapes, &[pos[0], laptop]);
        assert_eq!(contacts.len(), 1);
        assert_eq!((contacts[0].a_display, contacts[0].b_display), (1, 0));
        assert_eq!(contacts[0].span, (96.0, 192.0));
        assert_eq!(contacts[0].from, Pt::new(300.0, 144.0));
    }

    #[test]
    fn touching_two_displays_produces_two_exact_segments() {
        let desktop = Shape {
            mons: vec![Rect::new(0.0, 0.0, 100.0, 80.0),
                       Rect::new(0.0, 80.0, 100.0, 120.0)],
            w: 100.0, h: 200.0,
        };
        let contacts = links(&[desktop, boxed(100.0, 120.0)],
            &[Pt::default(), Pt::new(100.0, 40.0)]);
        assert_eq!(contacts.len(), 2);
        assert_eq!((contacts[0].a_display, contacts[0].span), (0, (40.0, 80.0)));
        assert_eq!((contacts[1].a_display, contacts[1].span), (1, (80.0, 160.0)));
    }

    #[test]
    fn horizontal_contacts_use_real_edges_of_unequal_height_displays() {
        let desktop = Shape {
            mons: vec![Rect::new(0.0, 20.0, 100.0, 80.0),
                       Rect::new(100.0, 0.0, 80.0, 100.0)],
            w: 180.0, h: 100.0,
        };
        let shapes = [desktop, boxed(120.0, 90.0)];
        let pos = [Pt::default(), Pt::new(40.25, 106.0)];
        let landed = snap(&shapes, &pos, 1, pos[1].x, pos[1].y);
        assert_eq!(landed, Pt::new(40.25, 100.0));
        let contacts = links(&shapes, &[pos[0], landed]);
        assert_eq!(contacts.len(), 2);
        assert_eq!(contacts[0].span, (40.25, 100.0));
        assert_eq!(contacts[1].span, (100.0, 160.25));
        assert!(contacts.iter().all(|link| link.dir == Dir::Vertical));
    }

    #[test]
    fn screen_threshold_is_not_a_long_distance_magnet() {
        let shapes = [boxed(100.0, 100.0), boxed(80.0, 80.0)];
        let pos = [Pt::default(), Pt::default()];
        assert_eq!(snap_with_threshold(&shapes, &pos, 1, 107.0, 30.25, 6.0),
                   Pt::new(107.0, 30.25));
        assert_eq!(snap_with_threshold(&shapes, &pos, 1, 105.0, 30.25, 6.0),
                   Pt::new(100.0, 30.25));
    }

    #[test]
    fn legacy_links_load_and_disconnected_nodes_are_parked() {
        let nodes = vec![vec![Rect::new(0.0, 0.0, 1920.0, 1080.0)]; 3];
        let doc = [DocLink { a: 0, b: 1, side: Side::Right, contact: None }];
        let arrangement = arrange(&nodes, &doc, 0);
        assert!((arrangement.pos[1].x - arrangement.pos[0].x
            - arrangement.shapes[0].w - GAP).abs() < 1e-6);
        assert!(arrangement.pos[2].y > arrangement.pos[0].y + arrangement.shapes[0].h);
    }

    #[test]
    fn concrete_contacts_reconstruct_at_any_scale_without_rounding() {
        for scale in [BASE_SCALE, BASE_SCALE / 3.0] {
            let shapes = vec![Shape::new(&[Rect::new(0.0, 0.0, 1920.0, 1080.0)], scale); 2];
            let doc = [DocLink {
                a: 0, b: 1, side: Side::Right,
                contact: Some((Rect::new(144.0, 40.125, 0.0, 20.0),
                               Rect::new(0.0, 0.0, 0.0, 20.0))),
            }];
            let pos = place_at_scale(&shapes, &doc, 0, scale);
            assert!((pos[1].x - pos[0].x - shapes[0].w).abs() < 1e-9);
            assert!((pos[1].y - pos[0].y - 40.125 * scale / BASE_SCALE).abs() < 1e-9);
            assert_eq!(links(&shapes, &pos).len(), 1);
        }
    }
}
