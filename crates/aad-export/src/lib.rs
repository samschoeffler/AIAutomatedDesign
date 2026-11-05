//! Exports: STL (triangle mesh, for 3D printing) and STEP (exact solid, for other CAD apps).
//! SVG plate outlines come in phase 6.

use aad_kernel::Part;
use std::path::Path;
use truck_meshalgo::rexport_polymesh::stl::{self, StlType};
use truck_modeling::{builder, BSplineCurve, Curve, KnotVec, Leader, Solid};
use truck_stepio::out::{CompleteStepDisplay, StepHeaderDescriptor, StepModels};

/// The part as a binary STL file.
pub fn stl_bytes(part: &Part) -> Vec<u8> {
    let mut bytes = Vec::new();
    // Writing into memory can't fail.
    stl::write(&part.mesh(), &mut bytes, StlType::Binary).expect("writing STL to memory");
    bytes
}

/// The part as STEP (AP214) text. `name` goes into the file header.
pub fn step_text(part: &Part, name: &str) -> String {
    let solids: Vec<Solid> = part.solids().map(step_ready).collect();
    let compressed: Vec<_> = solids.iter().map(Solid::compress).collect();
    let header = StepHeaderDescriptor {
        file_name: name.to_string(),
        organization_system: "AiAidedDesign".to_string(),
        ..Default::default()
    };
    CompleteStepDisplay::new(compressed.iter().collect::<StepModels<_, _, _>>(), header).to_string()
}

pub fn write_stl(part: &Part, path: &Path) -> Result<(), String> {
    std::fs::write(path, stl_bytes(part)).map_err(|e| format!("can't write {}: {e}", path.display()))
}

pub fn write_step(part: &Part, name: &str, path: &Path) -> Result<(), String> {
    std::fs::write(path, step_text(part, name)).map_err(|e| format!("can't write {}: {e}", path.display()))
}

/// A copy of `solid` with every boolean "intersection curve" swapped for a plain B-spline.
///
/// truck-stepio 0.3 writes intersection curves with a duplicated entity number, which makes
/// the file invalid, and plain B-splines are what other CAD apps read most reliably anyway.
fn step_ready(solid: &Solid) -> Solid {
    let copy = builder::clone(solid);
    for edge in copy.edge_iter() {
        let mut curve = edge.curve();
        if !matches!(curve, Curve::IntersectionCurve(_)) {
            continue;
        }
        // Fit a smooth B-spline to it within 0.01 mm when possible.
        curve.to_bspline_leader(0.01, 0.1, 20);
        let Curve::IntersectionCurve(inter) = &curve else { unreachable!() };
        let plain = match inter.leader() {
            Leader::BSpline(b) => b.clone(),
            // Fitting failed: use the polyline itself, as a straight-segment B-spline.
            Leader::Polyline(poly) => {
                let points: Vec<_> = poly.iter().copied().collect();
                let n = points.len();
                let mut knots: Vec<f64> = (0..n).map(|i| i as f64).collect();
                knots.insert(0, 0.0);
                knots.push((n - 1) as f64);
                BSplineCurve::new(KnotVec::from(knots), points)
            }
        };
        edge.set_curve(Curve::BSplineCurve(plain));
    }
    copy
}

#[cfg(test)]
mod tests {
    use super::*;
    use aad_model::Design;
    use truck_meshalgo::prelude::*;
    use truck_stepio::r#in::Table;

    /// A plate with a pocket, a joined boss and holes: exercises booleans (intersection curves).
    fn sample() -> Part {
        let design = Design::from_json(
            r#"{ "name": "sample", "features": [
            { "id": "s1", "type": "sketch", "plane": "XY", "entities": [ { "type": "rect", "x0": 0, "y0": 0, "x1": 40, "y1": 100 } ] },
            { "id": "e1", "type": "extrude", "sketch": "s1", "distance": 3 },
            { "id": "s2", "type": "sketch", "plane": "XY", "entities": [ { "type": "rect", "x0": 10, "y0": 60, "x1": 30, "y1": 80 } ] },
            { "id": "c1", "type": "extrude", "sketch": "s2", "distance": 1, "op": "cut" },
            { "id": "h1", "type": "hole", "on": "e1", "diameter": 4, "at": [[5, 5], [35, 5], [5, 95], [35, 95]] }
            ] }"#,
        )
        .expect("loads");
        aad_kernel::build(&design).expect("builds")
    }

    #[test]
    fn stl_has_the_right_size() {
        let bytes = stl_bytes(&sample());
        // Binary STL: 80-byte header, triangle count, then 50 bytes per triangle.
        let count = u32::from_le_bytes(bytes[80..84].try_into().unwrap()) as usize;
        assert!(count > 0);
        assert_eq!(bytes.len(), 84 + 50 * count);
    }

    #[test]
    fn step_reads_back_as_the_same_shape() {
        let part = sample();
        let has_intersections =
            part.solids().any(|s| s.edge_iter().any(|e| matches!(e.curve(), Curve::IntersectionCurve(_))));
        assert!(has_intersections, "the sample should exercise the intersection-curve fix");
        let text = step_text(&part, "sample");
        assert!(!text.contains("INTERSECTION_CURVE"), "intersection curves should be replaced");

        // Read the file back with truck's own STEP reader and compare volumes.
        let table = Table::from_step(&text).expect("valid STEP");
        assert_eq!(table.shell.len(), 1);
        let volume: f64 = table
            .shell
            .values()
            .map(|shell| {
                let shell = table.to_compressed_shell(shell).expect("converts");
                shell.robust_triangulation(0.05).to_polygon().volume()
            })
            .sum();
        let expected = part.volume();
        assert!((volume.abs() - expected).abs() < expected * 0.01, "read back {volume:.1}, expected {expected:.1}");
    }

    #[test]
    fn files_are_written() {
        let dir = std::env::temp_dir();
        let (stl_path, step_path) = (dir.join("aad_export_test.stl"), dir.join("aad_export_test.step"));
        write_stl(&sample(), &stl_path).expect("writes STL");
        write_step(&sample(), "sample", &step_path).expect("writes STEP");
        assert!(std::fs::metadata(&stl_path).unwrap().len() > 84);
        assert!(std::fs::read_to_string(&step_path).unwrap().starts_with("ISO-10303-21;"));
        let _ = std::fs::remove_file(stl_path);
        let _ = std::fs::remove_file(step_path);
    }
}
