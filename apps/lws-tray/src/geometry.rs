//! The GUI baseline arrangement, reduced to bounding boxes and neighbour detection.
//! Keep constants/placement in sync with lws-gui/src/map.rs; no rendering or drag state.
use protocol::ipc::TopologyDoc;

#[derive(Clone, Copy, Default)]
struct Box2 {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}
#[derive(Clone, Copy)]
enum Side {
    Left,
    Right,
    Top,
    Bottom,
}
impl Side {
    fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
        }
    }
    fn parse(edge: &str) -> Option<Self> {
        match edge {
            "LEFT" => Some(Self::Left),
            "RIGHT" => Some(Self::Right),
            "TOP" => Some(Self::Top),
            "BOTTOM" => Some(Self::Bottom),
            _ => None,
        }
    }
}
fn bounds(boxes: &[Box2]) -> (f64, f64, f64, f64) {
    if boxes.is_empty() {
        return (0.0, 0.0, 0.0, 0.0);
    }
    boxes.iter().fold(
        (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ),
        |b, s| {
            (
                b.0.min(s.x),
                b.1.min(s.y),
                b.2.max(s.x + s.w),
                b.3.max(s.y + s.h),
            )
        },
    )
}

pub(super) fn neighbours(doc: &TopologyDoc, this: &str) -> Vec<String> {
    let Some(anchor) = doc.members.iter().position(|m| m.node_id == this) else {
        return Vec::new();
    };
    let index = |id: &str| doc.members.iter().position(|m| m.node_id == id);
    let links: Vec<_> = doc
        .links
        .iter()
        .filter_map(|link| {
            let (a, b) = (link.from.as_ref()?, link.to.as_ref()?);
            let (a_index, b_index) = (index(&a.node_id)?, index(&b.node_id)?);
            (a_index != b_index).then_some((a_index, b_index, Side::parse(&a.edge)?))
        })
        .collect();
    let finite = |v: f64, fallback: f64| if v.is_finite() { v } else { fallback };
    let logical: Vec<_> = doc
        .members
        .iter()
        .map(|m| {
            let displays: Vec<_> = m
                .displays
                .iter()
                .map(|d| Box2 {
                    x: finite(d.logical_x, 0.0),
                    y: finite(d.logical_y, 0.0),
                    w: finite(d.logical_w, 0.0).max(1.0),
                    h: finite(d.logical_h, 0.0).max(1.0),
                })
                .collect();
            if displays.is_empty() {
                Box2 {
                    w: 1920.0,
                    h: 1080.0,
                    ..Default::default()
                }
            } else {
                let (x, y, r, b) = bounds(&displays);
                Box2 {
                    w: r - x,
                    h: b - y,
                    ..Default::default()
                }
            }
        })
        .collect();
    let mut scale = 0.075;
    let mut boxes = Vec::new();
    for _ in 0..10 {
        boxes = logical
            .iter()
            .map(|s| Box2 {
                w: s.w * scale,
                h: s.h * scale,
                ..Default::default()
            })
            .collect();
        let n = boxes.len();
        let mut positions = vec![None; n];
        let mut queue = std::collections::VecDeque::new();
        let mut seeds = std::iter::once(anchor).chain(0..n);
        let mut park_x = None;
        while let Some(seed) = seeds.find(|seed| positions[*seed].is_none()) {
            let at = if positions.iter().all(Option::is_none) {
                (0.0, 0.0)
            } else {
                let placed: Vec<_> = boxes
                    .iter()
                    .zip(&positions)
                    .filter_map(|(b, p)| p.map(|(x, y)| Box2 { x, y, ..*b }))
                    .collect();
                let (min_x, _, _, max_y) = bounds(&placed);
                let x = park_x.unwrap_or(min_x);
                park_x = Some(x + boxes[seed].w + 130.0);
                (x, max_y + 130.0)
            };
            positions[seed] = Some(at);
            queue.push_back(seed);
            while let Some(i) = queue.pop_front() {
                let Some((x, y)) = positions[i] else {
                    continue;
                };
                for &(a, b, side) in &links {
                    let (j, side) = if a == i {
                        (b, side)
                    } else if b == i {
                        (a, side.opposite())
                    } else {
                        continue;
                    };
                    if positions[j].is_some() {
                        continue;
                    }
                    let (me, other) = (boxes[i], boxes[j]);
                    positions[j] = Some(match side {
                        Side::Right => (x + me.w + 72.0, y + (me.h - other.h) / 2.0),
                        Side::Left => (x - 72.0 - other.w, y + (me.h - other.h) / 2.0),
                        Side::Bottom => (x + (me.w - other.w) / 2.0, y + me.h + 72.0),
                        Side::Top => (x + (me.w - other.w) / 2.0, y - 72.0 - other.h),
                    });
                    queue.push_back(j);
                }
            }
        }
        for (b, p) in boxes.iter_mut().zip(positions) {
            let (x, y) = p.unwrap_or_default();
            b.x = x;
            b.y = y;
        }
        let (min_x, min_y, max_x, max_y) = bounds(&boxes);
        let (dx, dy) = (
            916.0 / 2.0 - (min_x + max_x) / 2.0,
            270.0 / 2.0 - (min_y + max_y) / 2.0,
        );
        for b in &mut boxes {
            b.x = (b.x + dx).round();
            b.y = (b.y + dy).round();
        }
        let (x, y, r, b) = bounds(&boxes);
        let fit = (916.0 * 0.96 / (r - x).max(1.0)).min(270.0 * 0.96 / (b - y).max(1.0));
        if fit >= 1.0 {
            break;
        }
        scale *= fit * 0.97;
    }
    let me = boxes[anchor];
    boxes
        .iter()
        .enumerate()
        .filter(|(i, other)| {
            if *i == anchor {
                return false;
            }
            [(me, **other), (**other, me)].into_iter().any(|(a, b)| {
                let horizontal = b.x - (a.x + a.w);
                let vertical = b.y - (a.y + a.h);
                ((-2.0..=110.0).contains(&horizontal)
                    && (a.y + a.h).min(b.y + b.h) - a.y.max(b.y) >= 16.0)
                    || ((-2.0..=110.0).contains(&vertical)
                        && (a.x + a.w).min(b.x + b.w) - a.x.max(b.x) >= 16.0)
            })
        })
        .map(|(i, _)| doc.members[i].node_id.clone())
        .collect()
}
