//! The kernel: turns a feature list (aad-model) into solids, using the Truck B-rep kernel.
//!
//! The whole part is rebuilt from scratch on every change (docs/SPEC.md, "Rebuild on change").
//!
//! How features become geometry:
//! - **Sketch:** remembered until an extrude uses it. Shapes that sit fully inside another
//!   shape become holes in it (a rectangle with circles inside is a plate with holes).
//!   A sketch sits on an origin plane or on the start or end face of an earlier extrude,
//!   optionally moved along the normal by `offset`.
//! - **Extrude:** each region of the sketch is swept along the plane's normal.
//!   `new` starts a new body, `join` adds to the current (latest) body, `cut` removes from it.
//!   A negative distance extrudes the other way.
//! - **Hole:** a cylinder cut from the body that `on` built, at points in `on`'s sketch plane.
//!
//! ## Working around coplanar faces
//!
//! Truck's booleans fail when two solids share a flat face (for example a pocket cut that
//! starts on the plate's bottom face, or a boss sketched on its top face). Sketches sit on
//! the body's faces all the time, so there are workarounds:
//! - **Cuts and holes:** if a cutting tool ends exactly at the body's outer limit, it is
//!   lengthened 1 mm into the empty space beyond. Same result, no shared face.
//! - **Joins:** if the fuse fails, the new shape's ends are nudged off the shared face
//!   where that can't change the result (see [`join`]), and the fuse is tried again.
//!   If it still fails, the shape is kept as a separate, overlapping *piece* of the same
//!   body. Slicers and the viewer handle that fine; [`Part::has_overlapping_pieces`] says
//!   when it happened.

use aad_model::{BodyOp, Design, Feature, HoleDepth, Plane, Side, SketchEntity, SketchPlane};
use std::collections::HashMap;
use truck_meshalgo::prelude::{CalcVolume, MeshableShape, MeshedShape, PolygonMesh, PolylineCurve};
use truck_modeling::{builder, EuclideanSpace, InnerSpace, Rad, Vector3, Wire};

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

    /// Triangles and edges for drawing the part on screen (the app's 3D viewer).
    pub fn display_mesh(&self) -> DisplayMesh {
        let mut out = DisplayMesh::default();
        for solid in self.solids() {
            let meshed = solid.triangulation(MESH_TOLERANCE);

            // Edges are drawn only where there's a real corner. Where two faces meet smoothly
            // (a hole's wall is made of several curved faces, or one that meets itself at a
            // seam) there's no visible edge, so none is drawn.
            let faces: Vec<_> = meshed.face_iter().collect();
            let mut edge_faces: HashMap<_, Vec<usize>> = HashMap::new();
            for (i, face) in faces.iter().enumerate() {
                for edge in face.boundaries().iter().flat_map(|w| w.edge_iter()) {
                    edge_faces.entry(edge.id()).or_default().push(i);
                }
            }
            let is_smooth = |edge: &truck_topology::Edge<Point3, PolylineCurve<Point3>>| {
                let curve = edge.curve();
                let Some(&p) = curve.get(curve.len() / 2) else { return false };
                let normals: Option<Vec<Vector3>> = edge_faces
                    .get(&edge.id())
                    .map(|fs| fs.iter().map(|&i| face_normal_at(faces[i], p)).collect::<Option<_>>())
                    .unwrap_or(None);
                match normals.as_deref() {
                    Some([a, b]) => a.dot(*b) > SMOOTH_COS,
                    _ => false, // unsure: draw it
                }
            };
            // Each edge belongs to two faces, so skip repeats.
            let mut seen = std::collections::HashSet::new();
            for edge in meshed.edge_iter() {
                if seen.insert(edge.id()) && !is_smooth(&edge) {
                    let line: Vec<[f32; 3]> = edge.curve().iter().map(|p| to_f32(*p)).collect();
                    if line.len() >= 2 {
                        out.edges.push(line);
                    }
                }
            }

            // Triangles, with the surface normal at each corner where the mesh has one.
            let mesh = meshed.to_polygon();
            let (positions, normals) = (mesh.positions(), mesh.normals());
            for tri in mesh.faces().triangle_iter() {
                let p = tri.map(|v| positions[v.pos]);
                let flat = (p[1] - p[0]).cross(p[2] - p[0]);
                let flat = if flat.magnitude2() > 0.0 { flat.normalize() } else { Vector3::unit_z() };
                out.triangles.push(std::array::from_fn(|i| Corner {
                    position: to_f32(p[i]),
                    normal: {
                        let n = tri[i].nor.map_or(flat, |n| normals[n]);
                        [n.x as f32, n.y as f32, n.z as f32]
                    },
                }));
            }
        }
        out
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

/// What the viewer draws: see [`Part::display_mesh`]. Sizes in mm, as `f32` for the GPU.
#[derive(Debug, Clone, Default)]
pub struct DisplayMesh {
    /// Three corners per triangle, counter-clockwise when seen from outside the part.
    pub triangles: Vec<[Corner; 3]>,
    /// The part's real edges (where two faces meet), each as a line through these points.
    pub edges: Vec<Vec<[f32; 3]>>,
}

/// One corner of a display triangle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Corner {
    pub position: [f32; 3],
    /// Points out of the part, length 1.
    pub normal: [f32; 3],
}

/// Faces meeting at less than this angle (cosine of 20°) count as smooth: no edge drawn.
const SMOOTH_COS: f64 = 0.94;

/// The outward normal of a meshed face at one of its mesh points, if `p` is one.
fn face_normal_at(face: &truck_topology::Face<Point3, PolylineCurve<Point3>, Option<PolygonMesh>>, p: Point3) -> Option<Vector3> {
    let mesh = face.surface()?;
    let (positions, normals) = (mesh.positions(), mesh.normals());
    let tri = mesh.faces().triangle_iter().find(|t| t.iter().any(|v| (positions[v.pos] - p).magnitude2() < 1e-10))?;
    let corner = tri.iter().find(|v| (positions[v.pos] - p).magnitude2() < 1e-10)?;
    let n = match corner.nor {
        Some(n) => normals[n],
        None => {
            let q = tri.map(|v| positions[v.pos]);
            (q[1] - q[0]).cross(q[2] - q[0]).normalize()
        }
    };
    // A face used back to front has its mesh the wrong way round.
    Some(if face.orientation() { n } else { -n })
}

fn to_f32(p: Point3) -> [f32; 3] {
    [p.x as f32, p.y as f32, p.z as f32]
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

    /// Do the boxes overlap or at least touch? (A boss sitting on a plate's top face only
    /// touches it, but should still be fused into it.)
    fn touches(&self, other: &Extent) -> bool {
        (0..3).all(|k| self.lo[k] <= other.hi[k] + SAME && other.lo[k] <= self.hi[k] + SAME)
    }

    /// The box's eight corners.
    fn corners(&self) -> impl Iterator<Item = Point3> + '_ {
        (0..8).map(|i| {
            let pick = |k: usize| if i & (1 << k) == 0 { self.lo[k] } else { self.hi[k] };
            Point3::new(pick(0), pick(1), pick(2))
        })
    }
}

/// A sketch plane: a point on it (`origin`, where sketch (0, 0) is) and three directions.
/// `u` and `v` lie in the plane, `n` is its normal. They are right-handed (u × v = n), so a
/// sketch's x/y map onto u/v.
#[derive(Debug, Clone, Copy)]
struct Frame {
    origin: Point3,
    u: Vector3,
    v: Vector3,
    n: Vector3,
}

impl Frame {
    /// One of the three planes through the origin.
    fn of(plane: Plane) -> Frame {
        let (x, y, z) = (Vector3::unit_x(), Vector3::unit_y(), Vector3::unit_z());
        let origin = Point3::origin();
        match plane {
            Plane::XY => Frame { origin, u: x, v: y, n: z },
            Plane::XZ => Frame { origin, u: x, v: z, n: -y },
            Plane::YZ => Frame { origin, u: y, v: z, n: x },
        }
    }

    /// The same plane moved `h` along its normal.
    fn moved(self, h: f64) -> Frame {
        Frame { origin: self.origin + self.n * h, ..self }
    }

    /// The 3D point at sketch coordinates (x, y), lifted `h` along the normal.
    fn point(&self, x: f64, y: f64, h: f64) -> Point3 {
        self.origin + self.u * x + self.v * y + self.n * h
    }

    /// The range of heights (along `n`, measured from the plane) that a box covers.
    fn heights(&self, e: &Extent) -> (f64, f64) {
        let mut range = (f64::INFINITY, f64::NEG_INFINITY);
        for corner in e.corners() {
            let h = (corner - self.origin).dot(self.n);
            range = (range.0.min(h), range.1.max(h));
        }
        range
    }
}

/// A flat region (an outline, minus any holes) swept between two heights.
#[derive(Clone)]
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

/// What we remember about each extrude, so holes and sketches on its faces can find it.
struct ExtrudeInfo {
    /// The plane of the sketch it extruded.
    frame: Frame,
    distance: f64,
    body: usize,
}

/// Build a design into a part. The design is validated first, and any problem is returned as an error.
pub fn build(design: &Design) -> Result<Part, BuildError> {
    if let Some(p) = design.validate().first() {
        return fail(&p.feature, p.message.clone());
    }

    let mut sketches: HashMap<&str, (Frame, &[SketchEntity])> = HashMap::new();
    let mut extrudes: HashMap<&str, ExtrudeInfo> = HashMap::new();
    let mut bodies: Vec<Body> = Vec::new();

    for feature in design.features.iter().filter(|f| !f.is_suppressed()) {
        let id = feature.id();
        match feature {
            Feature::Sketch { plane, offset, entities, .. } => {
                let frame = match plane {
                    SketchPlane::Origin(p) => Frame::of(*p),
                    // Validation guarantees `on` is an earlier, active extrude.
                    SketchPlane::Face { on, side } => {
                        let info = &extrudes[on.as_str()];
                        match side {
                            Side::Start => info.frame,
                            Side::End => info.frame.moved(info.distance),
                        }
                    }
                };
                sketches.insert(id, (frame.moved(*offset), entities.as_slice()));
            }
            Feature::Extrude { sketch, distance, op, .. } => {
                let (frame, entities) = sketches[sketch.as_str()];
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
                        BodyOp::New | BodyOp::Join => join(body, &prism),
                        BodyOp::Cut => {
                            let tool = prism.lengthened_past(&body.extent());
                            cut(id, body, tool.solid(), tool.extent())?
                        }
                    }
                }
                extrudes.insert(id, ExtrudeInfo { frame, distance: *distance, body: bodies.len() - 1 });
            }
            Feature::Hole { on, diameter, at, depth, .. } => {
                let info = &extrudes[on.as_str()];
                let frame = info.frame;
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

/// Fuse a prism into a body, or keep it as an extra overlapping piece if Truck can't.
///
/// Truck fails when the prism's end face lies exactly on one of the body's faces (a boss
/// sketched on the plate's top face, or a post that starts on the plate's bottom face).
/// So when the first try fails, each end of the prism is nudged [`NUDGE`] mm where that
/// doesn't change the result: *into* the body if the material there is already solid
/// (the post starts 0.5 mm higher, still inside the plate), or *back into* the body if the
/// material just beyond the end is solid (the boss starts 0.5 mm down inside the plate).
/// Then no faces are shared and the fuse works.
fn join(body: &mut Body, prism: &Prism) {
    let (solid, extent) = (prism.solid(), prism.extent());
    for piece in body.pieces.iter_mut().filter(|p| p.extent.touches(&extent)) {
        let fused = truck_shapeops::or(&piece.solid, &solid, BOOLEAN_TOLERANCE).or_else(|| {
            let nudged = prism.nudged_into(&piece.solid)?;
            truck_shapeops::or(&piece.solid, &nudged.solid(), BOOLEAN_TOLERANCE)
        });
        if let Some(fused) = fused {
            piece.solid = fused;
            piece.extent = piece.extent.union(extent);
            return;
        }
    }
    body.pieces.push(Piece { solid, extent });
}

/// How far a join's end is moved to get it off a shared face (see [`join`]).
const NUDGE: f64 = 0.5;

impl Prism<'_> {
    /// This prism with its ends moved off any face it shares with `solid`, where that
    /// leaves the joined result the same. `None` if neither end can safely move.
    fn nudged_into(&self, solid: &Solid) -> Option<Self> {
        let inside = Inside::new(solid);
        let points = self.sample_points();
        // Is the slab of this prism's outline between heights a and b all inside `solid`?
        let solid_between = |a: f64, b: f64| {
            [0.15, 0.85].iter().all(|t| {
                let h = a + (b - a) * t;
                points.iter().all(|&(x, y)| inside.contains(self.frame.point(x, y, h)))
            })
        };
        let mut out = self.clone();
        let long_enough = self.h1 - self.h0 > 3.0 * NUDGE;
        if long_enough && solid_between(self.h0, self.h0 + NUDGE) {
            out.h0 += NUDGE; // the start is buried in material: start a little later
        } else if solid_between(self.h0 - NUDGE, self.h0) {
            out.h0 -= NUDGE; // the start sits on material: start a little inside it
        }
        if long_enough && solid_between(self.h1 - NUDGE, self.h1) {
            out.h1 -= NUDGE;
        } else if solid_between(self.h1, self.h1 + NUDGE) {
            out.h1 += NUDGE;
        }
        (out.h0 != self.h0 || out.h1 != self.h1).then_some(out)
    }

    /// Points spread over the prism's region (inside the outline, outside its holes), in
    /// sketch coordinates. Used to check that material is there before nudging.
    fn sample_points(&self) -> Vec<(f64, f64)> {
        let mut points = Vec::new();
        match self.outer {
            SketchEntity::Rect { x0, y0, x1, y1 } => {
                let (xa, xb, ya, yb) = (x0.min(x1), x0.max(x1), y0.min(y1), y0.max(y1));
                // A 9 x 9 grid, kept a hair inside the edges.
                let inset = |a: f64, b: f64, t: f64| a + (b - a) * (0.01 + 0.98 * t);
                for i in 0..9 {
                    for j in 0..9 {
                        points.push((inset(xa, xb, i as f64 / 8.0), inset(ya, yb, j as f64 / 8.0)));
                    }
                }
            }
            SketchEntity::Circle { cx, cy, r } => {
                points.push((cx, cy));
                for ring in [0.33, 0.66, 0.97] {
                    for k in 0..16 {
                        let a = k as f64 * std::f64::consts::TAU / 16.0;
                        points.push((cx + r * ring * a.cos(), cy + r * ring * a.sin()));
                    }
                }
            }
        }
        points.retain(|&(x, y)| !self.holes.iter().any(|h| contains(h, x, y)));
        points
    }
}

/// Whether a sketch shape covers the point (x, y).
fn contains(shape: &SketchEntity, x: f64, y: f64) -> bool {
    match *shape {
        SketchEntity::Rect { x0, y0, x1, y1 } => x >= x0.min(x1) && x <= x0.max(x1) && y >= y0.min(y1) && y <= y0.max(y1),
        SketchEntity::Circle { cx, cy, r } => (x - cx).hypot(y - cy) <= r,
    }
}

/// Answers "is this point inside the solid?" using its triangle mesh: a ray from the
/// point crosses the surface an odd number of times exactly when the point is inside.
struct Inside {
    triangles: Vec<[Point3; 3]>,
}

impl Inside {
    fn new(solid: &Solid) -> Inside {
        let mesh = solid.triangulation(MESH_TOLERANCE).to_polygon();
        let positions = mesh.positions();
        let triangles = mesh.faces().triangle_iter().map(|t| t.map(|v| positions[v.pos])).collect();
        Inside { triangles }
    }

    fn contains(&self, p: Point3) -> bool {
        // A slightly skewed direction, so the ray doesn't run exactly along an edge.
        let dir = Vector3::new(1.0, 0.000_123_7, 0.000_317_9).normalize();
        let crossings = self.triangles.iter().filter(|t| ray_hits(p, dir, t)).count();
        crossings % 2 == 1
    }
}

/// Does the ray from `p` along `dir` pass through the triangle? (Möller–Trumbore.)
fn ray_hits(p: Point3, dir: Vector3, t: &[Point3; 3]) -> bool {
    let (e1, e2) = (t[1] - t[0], t[2] - t[0]);
    let pv = dir.cross(e2);
    let det = e1.dot(pv);
    if det.abs() < 1e-12 {
        return false; // the ray runs along the triangle
    }
    let tv = p - t[0];
    let u = tv.dot(pv) / det;
    if !(0.0..=1.0).contains(&u) {
        return false;
    }
    let qv = tv.cross(e1);
    let v = dir.dot(qv) / det;
    if v < 0.0 || u + v > 1.0 {
        return false;
    }
    e2.dot(qv) / det > 0.0
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
