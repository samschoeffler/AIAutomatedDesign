//! Kernel tests. Geometry is checked by volume and bounding box, which catches
//! inside-out solids (negative volume), missing cuts and wrong directions.

use super::*;
use std::f64::consts::PI;

fn design(features: &str) -> Design {
    Design::from_json(&format!(r#"{{ "name": "test", "features": [ {features} ] }}"#)).expect("test JSON loads")
}

fn built(features: &str) -> Part {
    build(&design(features)).unwrap_or_else(|e| panic!("build failed: {e}"))
}

/// Mesh volume is slightly under the true volume for curved faces, so allow 1%.
fn assert_volume(part: &Part, expected: f64) {
    let v = part.volume();
    assert!((v - expected).abs() <= expected.abs() * 0.01 + 0.01, "volume {v:.2}, expected {expected:.2}");
}

fn assert_box(part: &Part, min: [f64; 3], max: [f64; 3]) {
    let (lo, hi) = part.bounding_box().expect("has a body");
    for k in 0..3 {
        assert!((lo[k] - min[k]).abs() < 0.1 && (hi[k] - max[k]).abs() < 0.1, "box {lo:?}..{hi:?}, expected {min:?}..{max:?}");
    }
}

const PLATE: &str = r#"
    { "id": "s1", "type": "sketch", "plane": "XY", "entities": [ { "type": "rect", "x0": 0, "y0": 0, "x1": 40, "y1": 100 } ] },
    { "id": "e1", "type": "extrude", "sketch": "s1", "distance": 3 }"#;

#[test]
fn a_rectangle_extrudes_to_a_box() {
    let part = built(PLATE);
    assert_eq!(part.bodies.len(), 1);
    assert_volume(&part, 40.0 * 100.0 * 3.0);
    assert_box(&part, [0.0, 0.0, 0.0], [40.0, 100.0, 3.0]);
}

#[test]
fn negative_distance_goes_the_other_way() {
    let part = built(&PLATE.replace(r#""distance": 3"#, r#""distance": -3"#));
    assert_volume(&part, 12000.0);
    assert_box(&part, [0.0, 0.0, -3.0], [40.0, 100.0, 0.0]);
}

#[test]
fn every_plane_works() {
    for (plane, min, max) in [
        ("XZ", [0.0, -5.0, 0.0], [10.0, 0.0, 20.0]),
        ("YZ", [0.0, 0.0, 0.0], [5.0, 10.0, 20.0]),
    ] {
        let part = built(&format!(
            r#"{{ "id": "s1", "type": "sketch", "plane": "{plane}", "entities": [ {{ "type": "rect", "x0": 0, "y0": 0, "x1": 10, "y1": 20 }} ] }},
               {{ "id": "e1", "type": "extrude", "sketch": "s1", "distance": 5 }}"#
        ));
        assert_volume(&part, 1000.0);
        assert_box(&part, min, max);
    }
}

#[test]
fn a_circle_extrudes_to_a_cylinder() {
    let part = built(
        r#"{ "id": "s1", "type": "sketch", "plane": "XY", "entities": [ { "type": "circle", "cx": 5, "cy": 5, "r": 10 } ] },
           { "id": "e1", "type": "extrude", "sketch": "s1", "distance": 4 }"#,
    );
    assert_volume(&part, PI * 100.0 * 4.0);
}

#[test]
fn shapes_inside_shapes_become_holes() {
    let part = built(
        r#"{ "id": "s1", "type": "sketch", "plane": "XY", "entities": [
              { "type": "rect", "x0": 0, "y0": 0, "x1": 40, "y1": 40 },
              { "type": "circle", "cx": 10, "cy": 10, "r": 3 },
              { "type": "rect", "x0": 20, "y0": 20, "x1": 30, "y1": 30 } ] },
           { "id": "e1", "type": "extrude", "sketch": "s1", "distance": 2 }"#,
    );
    assert_volume(&part, (1600.0 - PI * 9.0 - 100.0) * 2.0);
}

#[test]
fn the_spec_example_has_four_through_holes() {
    let part = built(&format!(
        r#"{PLATE}, {{ "id": "h1", "type": "hole", "on": "e1", "diameter": 4, "at": [[5, 5], [35, 5], [5, 95], [35, 95]] }}"#
    ));
    assert_volume(&part, 12000.0 - 4.0 * PI * 4.0 * 3.0);
}

#[test]
fn blind_holes_stop_at_their_depth() {
    let part = built(&format!(r#"{PLATE}, {{ "id": "h1", "type": "hole", "on": "e1", "diameter": 4, "at": [[20, 50]], "depth": 1 }}"#));
    assert_volume(&part, 12000.0 - PI * 4.0 * 1.0);
}

#[test]
fn cut_extrude_removes_material() {
    let part = built(&format!(
        r#"{PLATE},
           {{ "id": "s2", "type": "sketch", "plane": "XY", "entities": [ {{ "type": "rect", "x0": 10, "y0": 10, "x1": 30, "y1": 30 }} ] }},
           {{ "id": "c1", "type": "extrude", "sketch": "s2", "distance": 10, "op": "cut" }}"#
    ));
    assert_volume(&part, 12000.0 - 400.0 * 3.0);
}

#[test]
fn cut_as_deep_as_the_plate_goes_through() {
    // Both ends of the cut line up with the plate's faces.
    let part = built(&format!(
        r#"{PLATE},
           {{ "id": "s2", "type": "sketch", "plane": "XY", "entities": [ {{ "type": "rect", "x0": 10, "y0": 10, "x1": 30, "y1": 30 }} ] }},
           {{ "id": "c1", "type": "extrude", "sketch": "s2", "distance": 3, "op": "cut" }}"#
    ));
    assert_volume(&part, 12000.0 - 400.0 * 3.0);
}

#[test]
fn a_shallow_pocket_leaves_a_floor() {
    let part = built(&format!(
        r#"{PLATE},
           {{ "id": "s2", "type": "sketch", "plane": "XY", "entities": [ {{ "type": "rect", "x0": 10, "y0": 10, "x1": 30, "y1": 30 }} ] }},
           {{ "id": "c1", "type": "extrude", "sketch": "s2", "distance": 1, "op": "cut" }}"#
    ));
    assert_volume(&part, 12000.0 - 400.0);
}

/// A boss standing on the plate. Truck may not fuse it (they share the bottom face);
/// then it's kept as an overlapping piece, and the shape is still right.
#[test]
fn join_adds_material() {
    let part = built(&format!(
        r#"{PLATE},
           {{ "id": "s2", "type": "sketch", "plane": "XY", "entities": [ {{ "type": "circle", "cx": 20, "cy": 50, "r": 5 }} ] }},
           {{ "id": "j1", "type": "extrude", "sketch": "s2", "distance": 8, "op": "join" }}"#
    ));
    assert_eq!(part.bodies.len(), 1);
    assert_box(&part, [0.0, 0.0, 0.0], [40.0, 100.0, 8.0]);
    let boss = PI * 25.0;
    let expected = if part.has_overlapping_pieces() { 12000.0 + boss * 8.0 } else { 12000.0 + boss * 5.0 };
    assert_volume(&part, expected);
}

#[test]
fn holes_go_through_joined_pieces_too() {
    // Hole through the middle of the boss: removes it from the plate and the boss.
    let part = built(&format!(
        r#"{PLATE},
           {{ "id": "s2", "type": "sketch", "plane": "XY", "entities": [ {{ "type": "circle", "cx": 20, "cy": 50, "r": 5 }} ] }},
           {{ "id": "j1", "type": "extrude", "sketch": "s2", "distance": 8, "op": "join" }},
           {{ "id": "h1", "type": "hole", "on": "e1", "diameter": 4, "at": [[20, 50], [5, 5]] }}"#
    ));
    let boss = PI * 25.0;
    let hole = PI * 4.0;
    let expected = if part.has_overlapping_pieces() {
        12000.0 - 2.0 * hole * 3.0 + (boss - hole) * 8.0
    } else {
        12000.0 - 2.0 * hole * 3.0 + (boss - hole) * 5.0
    };
    assert_volume(&part, expected);
}

/// A counterbore in the boss that stops exactly at the plate's top face.
#[test]
fn blind_hole_ending_on_a_face_works() {
    let part = built(&format!(
        r#"{PLATE},
           {{ "id": "s2", "type": "sketch", "plane": "XY", "entities": [ {{ "type": "circle", "cx": 20, "cy": 50, "r": 8 }} ] }},
           {{ "id": "j1", "type": "extrude", "sketch": "s2", "distance": 12, "op": "join" }},
           {{ "id": "h1", "type": "hole", "on": "j1", "diameter": 8, "at": [[20, 50]], "depth": 9 }}"#
    ));
    let (boss, hole) = (PI * 64.0, PI * 16.0);
    let expected = if part.has_overlapping_pieces() {
        12000.0 + boss * 12.0 - hole * 9.0
    } else {
        12000.0 + boss * 9.0 - hole * 9.0
    };
    assert_volume(&part, expected);
}

#[test]
fn holes_work_on_other_planes() {
    let part = built(
        r#"{ "id": "s1", "type": "sketch", "plane": "XZ", "entities": [ { "type": "rect", "x0": 0, "y0": 0, "x1": 20, "y1": 20 } ] },
           { "id": "e1", "type": "extrude", "sketch": "s1", "distance": 5 },
           { "id": "h1", "type": "hole", "on": "e1", "diameter": 6, "at": [[10, 10]] }"#,
    );
    assert_volume(&part, 2000.0 - PI * 9.0 * 5.0);
}

#[test]
fn suppressed_features_are_skipped() {
    let part = built(&format!(
        r#"{PLATE}, {{ "id": "h1", "type": "hole", "on": "e1", "diameter": 4, "at": [[5, 5]], "suppressed": true }}"#
    ));
    assert_volume(&part, 12000.0);
}

#[test]
fn new_extrudes_make_separate_bodies() {
    let part = built(&format!(
        r#"{PLATE},
           {{ "id": "s2", "type": "sketch", "plane": "XY", "entities": [ {{ "type": "rect", "x0": 100, "y0": 0, "x1": 110, "y1": 10 }} ] }},
           {{ "id": "e2", "type": "extrude", "sketch": "s2", "distance": 1 }}"#
    ));
    assert_eq!(part.bodies.len(), 2);
    assert_volume(&part, 12000.0 + 100.0);
}

#[test]
fn invalid_designs_are_refused_with_the_validation_message() {
    let err = build(&design(r#"{ "id": "e1", "type": "extrude", "sketch": "nope", "distance": 3 }"#)).unwrap_err();
    assert_eq!(err.feature, "e1");
    assert!(err.message.contains("nope"), "{err}");
}

#[test]
fn the_display_mesh_has_the_real_edges_and_outward_normals() {
    let mesh = built(PLATE).display_mesh();
    assert_eq!(mesh.edges.len(), 12, "a box has 12 edges");
    assert_eq!(mesh.triangles.len(), 12, "two triangles per face");
    let centre = [20.0f32, 50.0, 1.5];
    for tri in &mesh.triangles {
        for c in tri {
            let out: f32 = (0..3).map(|k| (c.position[k] - centre[k]) * c.normal[k]).sum();
            assert!(out > 0.0, "normal {:?} at {:?} points inwards", c.normal, c.position);
        }
    }
}

#[test]
fn hole_seams_are_not_drawn_as_edges() {
    let mut f = PLATE.to_string();
    f.push_str(r#", { "id": "h1", "type": "hole", "on": "e1", "diameter": 6, "at": [[20, 50]] }"#);
    // 12 box edges + a circle on the top and bottom face (each maybe split in a few arcs),
    // but no straight seam running down the hole's wall.
    let mesh = built(&f).display_mesh();
    for line in &mesh.edges {
        let vertical = line.iter().all(|p| (p[0] - line[0][0]).abs() < 1e-3 && (p[1] - line[0][1]).abs() < 1e-3);
        let in_hole = ((line[0][0] - 20.0).powi(2) + (line[0][1] - 50.0).powi(2)).sqrt() < 3.5;
        assert!(!(vertical && in_hole), "a seam is drawn: {line:?}");
    }
}

#[test]
fn hole_rims_are_drawn() {
    let mut f = PLATE.to_string();
    f.push_str(r#", { "id": "h1", "type": "hole", "on": "e1", "diameter": 6, "at": [[20, 50]] }"#);
    let mesh = built(&f).display_mesh();
    let near_hole = |p: &[f32; 3]| ((p[0] - 20.0).powi(2) + (p[1] - 50.0).powi(2)).sqrt() < 3.5;
    let rim_points: Vec<_> = mesh.edges.iter().flatten().filter(|p| near_hole(p)).collect();
    assert!(rim_points.iter().any(|p| p[2] > 2.9), "no rim edge on the top face: {} edges", mesh.edges.len());
    assert!(rim_points.iter().any(|p| p[2] < 0.1), "no rim edge on the bottom face");
}

