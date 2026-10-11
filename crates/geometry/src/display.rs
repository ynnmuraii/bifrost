//! Display model: rectangles, rotation, per-display scale and edge helpers.
//!
//! Mirrors `Display` / `Edge` from `docs/protocol.proto` and the coordinate rules
//! of `docs/03-architecture.md` (section «Координаты»):
//!
//! * node logical space has its origin at the top-left corner of the primary
//!   display; coordinates may be negative;
//! * `logical_bounds` is authoritative for topology and transitions,
//!   `native_bounds` is diagnostic and absent when the platform exposes logical
//!   bounds only (`native_known = false`, Wayland portals);
//! * `scale_x` / `scale_y` are positive finite logical → native ratios and are
//!   never applied implicitly.

use core::fmt;

/// Tolerance for geometric comparisons, in logical units.
pub const GEOMETRY_EPSILON: f64 = 1e-6;

/// Node identity: `SHA-256(SPKI DER)`, exactly 32 bytes (`Hello.node_id`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct NodeId([u8; 32]);

impl NodeId {
    /// Wraps the 32 identity bytes.
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The 32 identity bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for NodeId {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl AsRef<[u8]> for NodeId {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Stable display identifier inside one node (`Display.id`, UTF-8 ≤ 128 bytes).
///
/// Stable means: it survives reconnects and re-layouts (an EDID hash or a
/// connector name), it is *not* an index.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct DisplayId(String);

impl DisplayId {
    /// Wraps a stable identifier.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The identifier as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for DisplayId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

impl From<String> for DisplayId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl AsRef<str> for DisplayId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DisplayId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A point in some two-dimensional space (node logical, display-local or native).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Point {
    /// Horizontal coordinate.
    pub x: f64,
    /// Vertical coordinate.
    pub y: f64,
}

impl Point {
    /// Builds a point.
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// Width/height pair.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Size {
    /// Horizontal extent.
    pub width: f64,
    /// Vertical extent.
    pub height: f64,
}

impl Size {
    /// Builds a size.
    pub const fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }
}

/// An axis-aligned rectangle (`Rect` in `protocol.proto`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rect {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width.
    pub width: f64,
    /// Height.
    pub height: f64,
}

impl Rect {
    /// Builds a rectangle from its left/top corner and its size.
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Left edge.
    pub fn left(&self) -> f64 {
        self.x
    }

    /// Top edge.
    pub fn top(&self) -> f64 {
        self.y
    }

    /// Right edge (`left + width`).
    pub fn right(&self) -> f64 {
        self.x + self.width
    }

    /// Bottom edge (`top + height`).
    pub fn bottom(&self) -> f64 {
        self.y + self.height
    }

    /// Width/height pair.
    pub fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }

    /// Half-open containment: `[left, right) × [top, bottom)`.
    pub fn contains(&self, point: Point) -> bool {
        point.x >= self.left()
            && point.x < self.right()
            && point.y >= self.top()
            && point.y < self.bottom()
    }

    /// Half-open overlap: touching rectangles do **not** intersect.
    pub fn intersects(&self, other: &Rect) -> bool {
        self.left() < other.right()
            && other.left() < self.right()
            && self.top() < other.bottom()
            && other.top() < self.bottom()
    }

    /// Smallest rectangle containing both.
    pub fn union(&self, other: &Rect) -> Rect {
        let left = self.left().min(other.left());
        let top = self.top().min(other.top());
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Rect::new(left, top, right - left, bottom - top)
    }

    /// All four numbers are finite.
    pub fn is_finite(&self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
    }

    /// Both extents are strictly positive.
    pub fn has_positive_area(&self) -> bool {
        self.width > 0.0 && self.height > 0.0
    }
}

/// Display rotation (`Display.rotation_degrees`, only `0 / 90 / 180 / 270`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rotation {
    /// `0°`.
    Deg0,
    /// `90°`.
    Deg90,
    /// `180°`.
    Deg180,
    /// `270°`.
    Deg270,
}

impl Rotation {
    /// Rotation in degrees.
    pub fn degrees(self) -> u16 {
        match self {
            Rotation::Deg0 => 0,
            Rotation::Deg90 => 90,
            Rotation::Deg180 => 180,
            Rotation::Deg270 => 270,
        }
    }

    /// Parses the wire value; anything but `0 / 90 / 180 / 270` is rejected.
    pub fn from_degrees(degrees: u16) -> Option<Self> {
        match degrees {
            0 => Some(Rotation::Deg0),
            90 => Some(Rotation::Deg90),
            180 => Some(Rotation::Deg180),
            270 => Some(Rotation::Deg270),
            _ => None,
        }
    }

    /// `true` for `90`/`270`, where the panel's native mode has swapped axes.
    pub fn swaps_axes(self) -> bool {
        matches!(self, Rotation::Deg90 | Rotation::Deg270)
    }
}

/// Per-axis logical → native scale (`Display.scale_x` / `Display.scale_y`).
///
/// `native = logical * scale`, so `1.0` is 100 %, `1.5` is 150 %, `1.25` is the
/// fractional GNOME scale. Values are positive and finite.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Scale {
    /// Horizontal ratio.
    pub x: f64,
    /// Vertical ratio.
    pub y: f64,
}

impl Scale {
    /// `1:1`.
    pub const ONE: Scale = Scale { x: 1.0, y: 1.0 };

    /// The same ratio on both axes.
    pub fn uniform(scale: f64) -> Self {
        Self { x: scale, y: scale }
    }

    /// Both ratios are finite and strictly positive.
    pub fn is_valid(&self) -> bool {
        self.x.is_finite() && self.x > 0.0 && self.y.is_finite() && self.y > 0.0
    }
}

/// A display edge (proto `Edge`; `EDGE_UNSPECIFIED` has no model representation).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Edge {
    /// Left edge, outward normal `(-1, 0)`.
    Left,
    /// Right edge, outward normal `(1, 0)`.
    Right,
    /// Top edge, outward normal `(0, -1)`.
    Top,
    /// Bottom edge, outward normal `(0, 1)`.
    Bottom,
}

impl Edge {
    /// All four edges.
    pub const ALL: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

    /// Unit vector pointing away from the display.
    pub fn outward_normal(self) -> Point {
        match self {
            Edge::Left => Point::new(-1.0, 0.0),
            Edge::Right => Point::new(1.0, 0.0),
            Edge::Top => Point::new(0.0, -1.0),
            Edge::Bottom => Point::new(0.0, 1.0),
        }
    }

    /// `true` for `Left`/`Right`: the edge runs vertically and its distances are
    /// measured from the top corner. Horizontal edges (`Top`/`Bottom`) measure
    /// from the left corner.
    pub fn is_vertical(self) -> bool {
        matches!(self, Edge::Left | Edge::Right)
    }

    /// The edge on the opposite side.
    pub fn opposite(self) -> Edge {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }
}

/// One display of one node (`Display` in `protocol.proto`).
///
/// `native_bounds` is `None` exactly when the platform did not expose physical
/// bounds, which is the wire's `bool native_known = false`.
#[derive(Clone, PartialEq, Debug)]
pub struct Display {
    /// Stable identifier, unique inside the node.
    pub id: DisplayId,
    /// Rectangle in the node's logical space (authoritative).
    pub logical_bounds: Rect,
    /// Physical rectangle in device pixels; `None` ≙ `native_known = false`.
    pub native_bounds: Option<Rect>,
    /// Panel rotation.
    pub rotation: Rotation,
    /// Logical → native scale.
    pub scale: Scale,
    /// `true` on the display that hosts the node logical origin.
    pub primary: bool,
}

impl Display {
    /// Size of the logical rectangle.
    pub fn logical_size(&self) -> Size {
        self.logical_bounds.size()
    }

    /// Size of the physical rectangle, when known.
    pub fn native_size(&self) -> Option<Size> {
        self.native_bounds.map(|bounds| bounds.size())
    }

    /// Size of the *un-rotated* panel mode, when native bounds are known.
    ///
    /// A portrait display (`rotation = 90`, logical `1080 × 1920`) reports its
    /// panel mode as `1920 × 1080`.
    pub fn native_mode_size(&self) -> Option<Size> {
        let native = self.native_size()?;
        Some(if self.rotation.swaps_axes() {
            Size::new(native.height, native.width)
        } else {
            native
        })
    }

    /// Length of an edge in logical units.
    pub fn edge_length(&self, edge: Edge) -> f64 {
        if edge.is_vertical() {
            self.logical_bounds.height
        } else {
            self.logical_bounds.width
        }
    }

    /// Point on an edge at `distance` from the edge's start corner, in the node's
    /// logical space. Distances are measured from the top corner for `Left`/
    /// `Right` and from the left corner for `Top`/`Bottom`
    /// (03-architecture.md, «Координаты»).
    pub fn point_on_edge(&self, edge: Edge, distance: f64) -> Point {
        let bounds = self.logical_bounds;
        match edge {
            Edge::Left => Point::new(bounds.left(), bounds.top() + distance),
            Edge::Right => Point::new(bounds.right(), bounds.top() + distance),
            Edge::Top => Point::new(bounds.left() + distance, bounds.top()),
            Edge::Bottom => Point::new(bounds.left() + distance, bounds.bottom()),
        }
    }

    /// Along-edge distance of a point given in the node's logical space.
    pub fn edge_distance_of(&self, edge: Edge, point: Point) -> f64 {
        if edge.is_vertical() {
            point.y - self.logical_bounds.top()
        } else {
            point.x - self.logical_bounds.left()
        }
    }

    /// Half-open containment in the node's logical space.
    pub fn contains_local(&self, point: Point) -> bool {
        self.logical_bounds.contains(point)
    }

    /// Converts a display-local logical point to device pixels.
    ///
    /// `None` when `native_known = false`. The sender must not use this to move
    /// the cursor; it exists for the receiver's native injection path.
    pub fn logical_to_native(&self, local: Point) -> Option<Point> {
        self.native_bounds?;
        Some(Point::new(local.x * self.scale.x, local.y * self.scale.y))
    }

    /// Converts a display-local point in device pixels to logical units.
    ///
    /// `None` when `native_known = false`.
    pub fn native_to_logical(&self, local_native: Point) -> Option<Point> {
        self.native_bounds?;
        Some(Point::new(
            local_native.x / self.scale.x,
            local_native.y / self.scale.y,
        ))
    }
}
