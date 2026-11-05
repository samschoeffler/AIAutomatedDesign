//! The kernel: turns a feature list (aad-model) into solids, using the Truck B-rep kernel.
//!
//! The whole part is rebuilt from scratch on every change (docs/SPEC.md, "Rebuild on change").
//!
//! How features become geometry:
//! - **Sketch:** remembered until an extrude uses it. Shapes that sit fully inside another
//!   shape become holes in it (a rectangle with circles inside is a plate with holes).
//! - **Extrude:** each region of the sketch is swept along the plane's normal.
//!   `new` starts a new body, `join` adds to the current (latest) body, `cut` removes from it.
//!   A negative distance extrudes the other way.
//! - **Hole:** a cylinder cut from the body that `on` built, at points in `on`'s sketch plane.
//!
//! ## Working around coplanar faces
//!
//! Truck's booleans fail when two solids share a flat face (for example a pocket cut that
//! starts on the plate's bottom face). Every sketch sits on a plane through the origin, so
//! this would happen all the time. Two workarounds:
//! - **Cuts and holes:** if a cutting tool ends exactly at the body's outer limit, it is
//!   lengthened 1 mm into the empty space beyond. Same result, no shared face.
//! - **Joins:** if Truck can't fuse the new shape into the body, it is kept as a separate,
//!   overlapping *piece* of the same body. Slicers and the viewer handle this fine;
//!   [`Part::has_overlapping_pieces`] says when it happened.

use aad_model::{BodyOp, Design, Feature, HoleDepth, Plane, SketchEntity};
use std::collections::HashMap;
use truck_meshalgo::prelude::{CalcVolume, MeshableShape, MeshedShape, PolygonMesh};
use truck_modeling::{builder, EuclideanSpace, Rad, Vector3, Wire};

pub use truck_modeling::{Point3, Solid};

/// How close two points must be to count as the same, in mm, for booleans.
const BOOLEAN_TOLERANCE: f64 = 0.05;

/// How far a triangle mesh may stray from the true surface, in mm.
pub const MESH_TOLERANCE: f64 = 0.05;

/// Two numbers this close count as "the same plane" when deciding to lengthen a cut.
const SAME: f64 = 1e-6;

/// A built part: one or more separate bodies.
#[derive(Debug, Clone)]
pub struct Part {
    pub bodies: Vec<Body>,
}

/// One body: usually a single solid, but joins Truck couldn't fuse add extra overlapping pieces.
#[derive(Debug, Clone)]
pub struct Body {
    pieces: Vec<Piece>,
}

#[derive(Debug, Clone)]
struct Piece {
    solid: Solid,
    /// A box guaranteed to hold the piece (it may be bigger, never smaller).
    extent: Extent,
}

impl Body {
    /// The solid pieces of this body.
    pub fn solids(&self) -> impl Iterator<Item = &Solid> {
        self.pieces.iter().map(|p| &p.solid)
    }

    /// A box that holds every piece; never smaller than the body.
    fn extent(&self) -> Extent {
        self.pieces.iter().map(|p| p.extent).reduce(Extent::union).expect("a body always has a piece")
    }
}

impl Part {
    /// Every solid in the part, across all bodies.
    pub fn solids(&self) -> impl Iterator<Item = &Solid> {
        self.bodies.iter().flat_map(Body::solids)
    }

    /// True if a join was kept as an overlapping piece (see the module docs).
    /// Then [`Part::volume`] counts the overlap twice.
    pub fn has_overlapping_pieces(&self) -> bool {
        self.bodies.iter().any(|b| b.pieces.len() > 1)
    }

    /// All solids as one triangle mesh (for the viewer, STL export and measuring).
    pub fn mesh(&self) -> PolygonMesh {
        let mut all = PolygonMesh::default();
        for solid in self.solids() {
            all.merge(solid.triangulation(MESH_TOLERANCE).to_polygon());
        }
        all
    }

    /// Volume in mm³, from the mesh (so very slightly under for curved faces).
    pub fn volume(&self) -> f64 {
        self.mesh().volume()
    }

    /// The smallest box holding the whole part, as (min corner, max corner). `None` if there are no bodies.
    pub fn bounding_box(&self) -> Option<(Point3, Point3)> {
        let bb = self.mesh().bounding_box();
        (!bb.is_empty()).then(|| (bb.min(), bb.max()))
    }
}

/// Why a design couldn't be built, worded so Claude can fix it.
#[derive(Debug, Clone, PartialEq)]
pub struct BuildError {
    /// The id of the feature that failed ("design" if it isn't one feature's fault).
    pub feature: String,
    pub message: String,
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.feature, self.message)
    }
}

impl std::error::Error for BuildError {}

fn fail<T>(feature: &str, message: impl Into<String>) -> Result<T, BuildError> {
    Err(BuildError { feature: feature.to_string(), message: message.into() })
}

/// An axis-aligned box, as min and max corners.
#[derive(Debug, Clone, Copy)]
struct Extent {
    lo: [f64; 3],
    hi: [f64; 3],
}

impl Extent {
    fn around(points: impl IntoIterator<Item = Point3>) -> Extent {
        let mut e = Extent { lo: [f64::INFINITY; 3], hi: [f64::NEG_INFINITY; 3] };
        for p in points {
            for k in 0..3 {
                e.lo[k] = e.lo[k].min(p[k]);
                e.hi[k] = e.hi[k].max(p[k]);
            }
        }
        e
    }

    fn union(self, other: Extent) -> Extent {
        Extent {
            lo: [0, 1, 2].map(|k| self.lo[k].min(other.lo[k])),
            hi: [0, 1, 2].map(|k| self.hi[k].max(other.hi[k])),
        }
    }

    /// Do the boxes share some volume? Boxes that only touch don't count, so a tool
    /// that just touches a piece's face is never cut from it (Truck would fail on that face).
    fn overlaps(&self, other: &Extent) -> bool {
        (0..3).all(|k| self.lo[k] < other.hi[k] - SAME && other.lo[k] < self.hi[k] - SAME)
    }
}

/// A sketch plane as three directions: `u` and `v` lie in the plane, `n` is its normal.
/// They are right-handed (u × v = n), so a sketch's x/y map onto u/v.
struct Frame {
    u: Vector3,
    v: Vector3,
    n: Vector3,
}

impl Frame {
    fn of(plane: Plane) -> Frame {
        let (x, y, z) = (Vector3::unit_x(), Vector3::unit_y(), Vector3::unit_z());
        match plane {
            Plane::XY => Frame { u: x, v: y, n: z },
            Plane::XZ => Frame { u: x, v: z, n: -y },
            Plane::YZ => Frame { u: y, v: z, n: x },
        }
    }

    /// The 3D point at sketch coordinates (x, y), lifted `h` along the normal.
    fn point(&self, x: f64, y: f64, h: f64) -> Point3 {
        Point3::from_vec(self.u * x + self.v * y + self.n * h)
    }

    /// The range of heights (along `n`) that a box covers.
    fn heights(&self, e: &Extent) -> (f64, f64) {
        // n is always plus or minus one axis.
        let k = (0..3).find(|&k| self.n[k] != 0.0).expect("n is an axis");
        if self.n[k] > 0.0 {
            (e.lo[k], e.hi[k])
        } else {
            (-e.hi[k], -e.lo[k])
        }
    }
}

/// A flat region (an outline, minus any holes) swept between two heights.
struct Prism<'a> {
    frame: &'a Frame,
    outer: SketchEntity,
    holes: Vec<SketchEntity>,
    h0: f64,
    h1: f64,
}

impl Prism<'_> {
    fn solid(&self) -> Solid {
        let mut wires = vec![outline(&self.outer, self.frame, self.h0)];
        // Holes run the opposite way round to the outline.
        wires.extend(self.holes.iter().map(|h| outline(h, self.frame, self.h0).inverse()));
        let face = builder::try_attach_plane(&wires).expect("sketch shapes are always flat and valid after validation");
        builder::tsweep(&face, self.frame.n * (self.h1 - self.h0))
    }

    fn extent(&self) -> Extent {
        let (xa, ya, xb, yb) = match self.outer {
            SketchEntity::Rect { x0, y0, x1, y1 } => (x0, y0, x1, y1),
            SketchEntity::Circle { cx, cy, r } => (cx - r, cy - r, cx + r, cy + r),
        };
        let mut corners = Vec::new();
        for x in [xa, xb] {
            for y in [ya, yb] {
                for h in [self.h0, self.h1] {
                    corners.push(self.frame.point(x, y, h));
                }
            }
        }
        Extent::around(corners)
    }

    /// Lengthen a cutting tool past the body where it would otherwise end flush with it.
    fn lengthened_past(mut self, body: &Extent) -> Self {
        let (lo, hi) = self.frame.heights(body);
        if self.h0 <= lo + SAME {
            self.h0 = lo - 1.0;
        }
        if self.h1 >= hi - SAME {
            self.h1 = hi + 1.0;
        }
        self
    }
}

/// What we remember about each extrude, so holes can find their body and plane.
struct ExtrudeInfo {
    plane: Plane,
    distance: f64,
    body: usize,
}

/// Build a design into a part. The design is validated first, and any problem is returned as an error.
pub fn build(design: &Design) -> Result<Part, BuildError> {
    if let Some(p) = design.validate().first() {
        return fail(&p.feature, p.message.clone());
    }

    let mut sketches: HashMap<&str, (Plane, &[SketchEntity])> = HashMap::new();
    let mut extrudes: HashMap<&str, ExtrudeInfo> = HashMap::new();
    let mut bodies: Vec<Body> = Vec::new();

    for feature in design.features.iter().filter(|f| !f.is_suppressed()) {
        let id = feature.id();
        match feature {
            Feature::Sketch { plane, entities, .. } => {
                sketches.insert(id, (*plane, entities.as_slice()));
            }
            Feature::Extrude { sketch, distance, op, .. } => {
                let (plane, entities) = sketches[sketch.as_str()];
                let frame = Frame::of(plane);
                let (h0, h1) = (distance.min(0.0), distance.max(0.0));
                let prisms = regions(id, entities)?
                    .into_iter()
                    .map(|(outer, holes)| Prism { frame: &frame, outer, holes, h0, h1 });
                if *op == BodyOp::New {
                    bodies.push(Body { pieces: Vec::new() });
                }
                // Validation guarantees a body exists before any join or cut.
                let body = bodies.last_mut().expect("validated");
                for prism in prisms {
                    match op {
                        BodyOp::New | BodyOp::Join => join(body, prism.solid(), prism.extent()),
                        BodyOp::Cut => {
                            let tool = prism.lengthened_past(&body.extent());
                            cut(id, body, tool.solid(), tool.extent())?
                        }
                    }
                }
                extrudes.insert(id, ExtrudeInfo { plane, distance: *distance, body: bodies.len() - 1 });
            }
            Feature::Hole { on, diameter, at, depth, .. } => {
                let info = &extrudes[on.as_str()];
                let frame = Frame::of(info.plane);
                let body = &mut bodies[info.body];
                // Heights the hole spans before lengthening; "through" spans the whole body.
                let (h0, h1) = match *depth {
                    HoleDepth::Through => frame.heights(&body.extent()),
                    HoleDepth::Blind(mm) if info.distance > 0.0 => (info.distance - mm, info.distance),
                    HoleDepth::Blind(mm) => (info.distance, info.distance + mm),
                };
                for &[x, y] in at {
                    let circle = SketchEntity::Circle { cx: x, cy: y, r: diameter / 2.0 };
                    let tool = Prism { frame: &frame, outer: circle, holes: vec![], h0, h1 }.lengthened_past(&body.extent());
                    cut(id, body, tool.solid(), tool.extent())?;
                }
            }
        }
    }
    Ok(Part { bodies })
}

/// Fuse a solid into a body, or keep it as an extra overlapping piece if Truck can't.
fn join(body: &mut Body, solid: Solid, extent: Extent) {
    for piece in body.pieces.iter_mut().filter(|p| p.extent.overlaps(&extent)) {
        if let Some(fused) = truck_shapeops::or(&piece.solid, &solid, BOOLEAN_TOLERANCE) {
            piece.solid = fused;
            piece.extent = piece.extent.union(extent);
            return;
        }
    }
    body.pieces.push(Piece { solid, extent });
}

/// Remove a tool from every piece it might touch.
fn cut(id: &str, body: &mut Body, mut tool: Solid, extent: Extent) -> Result<(), BuildError> {
    tool.not(); // Turn the tool inside out: "piece AND (NOT tool)" is "piece minus tool".
    for piece in body.pieces.iter_mut().filter(|p| p.extent.overlaps(&extent)) {
        match truck_shapeops::and(&piece.solid, &tool, BOOLEAN_TOLERANCE) {
            Some(result) => piece.solid = result,
            None => {
                return fail(
                    id,
                    "cutting failed in the geometry kernel; this usually means the cut lines up exactly \
                     with a side face of the part, so try making it overlap or stop short of that face",
                )
            }
        }
    }
    Ok(())
}

/// Split a sketch into regions: each outline with the shapes that sit inside it as holes.
fn regions(id: &str, entities: &[SketchEntity]) -> Result<Vec<(SketchEntity, Vec<SketchEntity>)>, BuildError> {
    let mut found = Vec::new();
    for (i, outer) in entities.iter().enumerate() {
        if entities.iter().enumerate().any(|(j, other)| j != i && inside(outer, other)) {
            continue; // This shape is a hole in another one; handled with that shape.
        }
        let mut holes = Vec::new();
        for (j, inner) in entities.iter().enumerate() {
            if j != i && inside(inner, outer) {
                if entities.iter().enumerate().any(|(k, mid)| k != i && k != j && inside(inner, mid) && inside(mid, outer)) {
                    return fail(id, "a shape inside a hole inside a shape isn't supported yet; use a separate sketch and extrude");
                }
                holes.push(*inner);
            }
        }
        found.push((*outer, holes));
    }
    Ok(found)
}

/// The closed outline of one shape, running anticlockwise when seen from the +n side.
fn outline(entity: &SketchEntity, frame: &Frame, h: f64) -> Wire {
    match *entity {
        SketchEntity::Rect { x0, y0, x1, y1 } => {
            let (xa, xb) = (x0.min(x1), x0.max(x1));
            let (ya, yb) = (y0.min(y1), y0.max(y1));
            let corners = [(xa, ya), (xb, ya), (xb, yb), (xa, yb)].map(|(x, y)| builder::vertex(frame.point(x, y, h)));
            (0..4).map(|k| builder::line(&corners[k], &corners[(k + 1) % 4])).collect()
        }
        SketchEntity::Circle { cx, cy, r } => {
            let start = builder::vertex(frame.point(cx + r, cy, h));
            // Any angle of a full turn or more makes a closed circle.
            builder::rsweep(&start, frame.point(cx, cy, h), frame.n, Rad(7.0))
        }
    }
}

/// Is shape `a` entirely inside shape `b` (in sketch coordinates)?
fn inside(a: &SketchEntity, b: &SketchEntity) -> bool {
    use SketchEntity::*;
    match (*a, *b) {
        (Rect { x0, y0, x1, y1 }, Rect { x0: bx0, y0: by0, x1: bx1, y1: by1 }) => {
            let within = |v: f64, p: f64, q: f64| v > p.min(q) && v < p.max(q);
            [x0, x1].iter().all(|&x| within(x, bx0, bx1)) && [y0, y1].iter().all(|&y| within(y, by0, by1))
        }
        (Rect { x0, y0, x1, y1 }, Circle { cx, cy, r }) => {
            [(x0, y0), (x0, y1), (x1, y0), (x1, y1)].iter().all(|&(x, y)| (x - cx).hypot(y - cy) < r)
        }
        (Circle { cx, cy, r }, Rect { x0, y0, x1, y1 }) => {
            cx - r > x0.min(x1) && cx + r < x0.max(x1) && cy - r > y0.min(y1) && cy + r < y0.max(y1)
        }
        (Circle { cx, cy, r }, Circle { cx: bx, cy: by, r: br }) => (cx - bx).hypot(cy - by) + r < br,
    }
}

#[cfg(test)]
mod tests;
