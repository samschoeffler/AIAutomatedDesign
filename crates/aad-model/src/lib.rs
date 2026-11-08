//! The design model: a part is an ordered list of features.
//!
//! This is the "language" Claude and the app share (docs/SPEC.md, "The feature list").
//! The kernel turns this list into a solid; nothing here does geometry.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

/// A whole design, as saved in an `.aad.json` file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Design {
    pub name: String,
    /// Always "mm" for now (docs/SPEC.md: millimetres only).
    #[serde(default = "default_units")]
    pub units: String,
    pub features: Vec<Feature>,
}

fn default_units() -> String {
    "mm".to_string()
}

/// One step in the design. `#[serde(tag = "type")]` means the JSON looks like
/// `{ "type": "extrude", "id": "e1", ... }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Feature {
    Sketch {
        id: String,
        /// Where the sketch sits: an origin plane, or the start or end face of an extrude.
        plane: SketchPlane,
        /// Moves the sketch this far (mm) along the plane's normal. Left out when 0.
        #[serde(default, skip_serializing_if = "is_zero")]
        offset: f64,
        entities: Vec<SketchEntity>,
        #[serde(default)]
        suppressed: bool,
    },
    Extrude {
        id: String,
        /// The id of the sketch to extrude.
        sketch: String,
        distance: f64,
        #[serde(default)]
        op: BodyOp,
        #[serde(default)]
        suppressed: bool,
    },
    Hole {
        id: String,
        /// The id of the extrude whose body gets the holes.
        on: String,
        diameter: f64,
        /// Hole centres, in the sketch plane of `on`.
        at: Vec<[f64; 2]>,
        /// "through" (the default) or a blind depth in mm.
        #[serde(default)]
        depth: HoleDepth,
        #[serde(default)]
        suppressed: bool,
    },
}

/// The three standard planes a sketch can sit on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Plane {
    XY,
    XZ,
    YZ,
}

/// Where a sketch sits. In JSON: `"XY"` (or `"XZ"`, `"YZ"`), or
/// `{ "on": "<extrude id>", "side": "end" }` for the face that extrude ends on
/// (`"start"` for the face it starts from).
///
/// A sketch on an extrude's face uses the same x / y / normal directions as the sketch
/// that extrude came from; only the height changes. So the plate's hole positions and a
/// boss on its top face share coordinates, and a positive distance keeps going the same
/// way the extrude went.
#[derive(Debug, Clone, PartialEq)]
pub enum SketchPlane {
    Origin(Plane),
    Face { on: String, side: Side },
}

/// Which face of an extrude.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// Where the extrude starts: its sketch's plane.
    Start,
    /// Where it ends: its sketch's plane moved by its distance.
    #[default]
    End,
}

impl From<Plane> for SketchPlane {
    fn from(p: Plane) -> Self {
        SketchPlane::Origin(p)
    }
}

// Written by hand like HoleDepth: "a word or an object" with clear errors.
impl Serialize for SketchPlane {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            SketchPlane::Origin(p) => p.serialize(ser),
            SketchPlane::Face { on, side } => {
                use serde::ser::SerializeMap;
                let mut m = ser.serialize_map(Some(2))?;
                m.serialize_entry("on", on)?;
                m.serialize_entry("side", side)?;
                m.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for SketchPlane {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Face {
            on: String,
            #[serde(default)]
            side: Side,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Word(String),
            Face(Face),
            Other(serde_json::Value),
        }
        const HELP: &str = "a sketch plane is \"XY\", \"XZ\", \"YZ\", or {\"on\": \"<extrude id>\", \"side\": \"start\" or \"end\"}";
        match Raw::deserialize(de)? {
            Raw::Word(w) => match w.as_str() {
                "XY" => Ok(Plane::XY.into()),
                "XZ" => Ok(Plane::XZ.into()),
                "YZ" => Ok(Plane::YZ.into()),
                _ => Err(serde::de::Error::custom(format!("{HELP}, not {w:?}"))),
            },
            Raw::Face(f) => Ok(SketchPlane::Face { on: f.on, side: f.side }),
            Raw::Other(v) => Err(serde::de::Error::custom(format!("{HELP}, not {v}"))),
        }
    }
}

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

/// What an extrude does to the existing solid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyOp {
    #[default]
    New,
    Join,
    Cut,
}

/// How deep a hole goes. In JSON: the word `"through"`, or a number of mm.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum HoleDepth {
    #[default]
    Through,
    Blind(f64),
}

// serde can't express "a keyword or a number" with derive alone, so these two
// impls are written by hand.
impl Serialize for HoleDepth {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            HoleDepth::Through => ser.serialize_str("through"),
            HoleDepth::Blind(mm) => ser.serialize_f64(*mm),
        }
    }
}

impl<'de> Deserialize<'de> for HoleDepth {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        /// The raw JSON value: either a string or a number.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Word(String),
            Mm(f64),
        }
        match Raw::deserialize(de)? {
            Raw::Word(w) if w == "through" => Ok(HoleDepth::Through),
            Raw::Word(w) => Err(serde::de::Error::custom(format!(
                "hole depth must be \"through\" or a number of mm, not {w:?}"
            ))),
            Raw::Mm(mm) => Ok(HoleDepth::Blind(mm)),
        }
    }
}

/// The shapes a sketch can hold (more come with the sketcher, phase 7).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SketchEntity {
    Rect { x0: f64, y0: f64, x1: f64, y1: f64 },
    Circle { cx: f64, cy: f64, r: f64 },
}

impl Feature {
    pub fn id(&self) -> &str {
        match self {
            Feature::Sketch { id, .. } | Feature::Extrude { id, .. } | Feature::Hole { id, .. } => id,
        }
    }

    pub fn is_suppressed(&self) -> bool {
        match self {
            Feature::Sketch { suppressed, .. }
            | Feature::Extrude { suppressed, .. }
            | Feature::Hole { suppressed, .. } => *suppressed,
        }
    }
}

/// A problem found by [`Design::validate`], worded so Claude can fix it.
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    pub feature: String,
    pub message: String,
}

impl Design {
    /// Read a design from JSON text.
    pub fn from_json(text: &str) -> Result<Design, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// Write a design as pretty JSON text.
    pub fn to_json(&self) -> String {
        // Serializing these plain types can't fail, so an empty string is never returned in practice.
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Read a design from an `.aad.json` file.
    pub fn load(path: &Path) -> Result<Design, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
        Design::from_json(&text).map_err(|e| format!("{} isn't a valid design file: {e}", path.display()))
    }

    /// Write a design to an `.aad.json` file.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        std::fs::write(path, self.to_json()).map_err(|e| format!("can't write {}: {e}", path.display()))
    }

    /// Check the design before it reaches the kernel. An empty list means it's fine.
    pub fn validate(&self) -> Vec<Problem> {
        let mut problems = Vec::new();
        // Ids seen so far: a feature may only refer to features *before* it.
        let mut seen: HashSet<&str> = HashSet::new();
        let mut sketches: HashSet<&str> = HashSet::new();
        let mut extrudes: HashSet<&str> = HashSet::new();
        let mut suppressed_ids: HashSet<&str> = HashSet::new();
        // Whether an active "new" extrude exists yet, so join/cut have something to work on.
        let mut has_body = false;
        let mut bad = |feature: &str, message: String| problems.push(Problem { feature: feature.to_string(), message });

        if self.units != "mm" {
            bad("design", format!("units must be \"mm\", not {:?}", self.units));
        }
        for f in &self.features {
            let id = f.id();
            if id.is_empty() {
                bad(id, "every feature needs an id".into());
            }
            if seen.contains(id) {
                bad(id, format!("the id {id:?} is used twice"));
            }
            if f.is_suppressed() {
                suppressed_ids.insert(id);
            }
            match f {
                Feature::Sketch { plane, offset, entities, suppressed, .. } => {
                    if let SketchPlane::Face { on, .. } = plane {
                        if !extrudes.contains(on.as_str()) {
                            bad(id, format!("sits on a face of {on:?}, which isn't an extrude earlier in the list"));
                        } else if suppressed_ids.contains(on.as_str()) && !*suppressed {
                            bad(id, format!("sits on a face of {on:?}, which is suppressed"));
                        }
                    }
                    if !offset.is_finite() {
                        bad(id, "the sketch offset isn't a finite number".into());
                    }
                    if entities.is_empty() {
                        bad(id, "a sketch needs at least one shape".into());
                    }
                    for e in entities {
                        match e {
                            SketchEntity::Rect { x0, y0, x1, y1 } if ![x0, y0, x1, y1].iter().all(|v| v.is_finite()) => {
                                bad(id, "a rectangle has a number that isn't finite".into())
                            }
                            SketchEntity::Circle { cx, cy, r } if ![cx, cy, r].iter().all(|v| v.is_finite()) => {
                                bad(id, "a circle has a number that isn't finite".into())
                            }
                            SketchEntity::Rect { x0, y0, x1, y1 } if x0 == x1 || y0 == y1 => {
                                bad(id, "a rectangle has zero width or height".into())
                            }
                            SketchEntity::Circle { r, .. } if *r <= 0.0 => bad(id, "a circle's radius must be above 0".into()),
                            _ => {}
                        }
                    }
                    sketches.insert(id);
                }
                Feature::Extrude { sketch, distance, op, suppressed, .. } => {
                    if !sketches.contains(sketch.as_str()) {
                        bad(id, format!("extrudes sketch {sketch:?}, which isn't a sketch earlier in the list"));
                    } else if suppressed_ids.contains(sketch.as_str()) && !*suppressed {
                        bad(id, format!("extrudes sketch {sketch:?}, which is suppressed"));
                    }
                    if !distance.is_finite() || *distance == 0.0 {
                        bad(id, "extrude distance must be a nonzero number".into());
                    }
                    if matches!(op, BodyOp::Join | BodyOp::Cut) && !has_body {
                        bad(id, "join and cut need an existing body, but nothing has been extruded as \"new\" yet".into());
                    }
                    if *op == BodyOp::New && !*suppressed {
                        has_body = true;
                    }
                    extrudes.insert(id);
                }
                Feature::Hole { on, diameter, at, depth, suppressed, .. } => {
                    if !seen.contains(on.as_str()) {
                        bad(id, format!("holes go on {on:?}, which isn't earlier in the list"));
                    } else if !extrudes.contains(on.as_str()) {
                        bad(id, format!("holes go on an extrude, but {on:?} isn't one"));
                    } else if suppressed_ids.contains(on.as_str()) && !*suppressed {
                        bad(id, format!("holes go on {on:?}, which is suppressed"));
                    }
                    if !diameter.is_finite() || *diameter <= 0.0 {
                        bad(id, "hole diameter must be above 0".into());
                    }
                    if at.is_empty() {
                        bad(id, "give at least one hole position".into());
                    } else if at.iter().flatten().any(|v| !v.is_finite()) {
                        bad(id, "a hole position has a number that isn't finite".into());
                    }
                    if let HoleDepth::Blind(mm) = depth {
                        if !mm.is_finite() || *mm <= 0.0 {
                            bad(id, "blind hole depth must be above 0".into());
                        }
                    }
                }
            }
            seen.insert(id);
        }
        problems
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example from docs/SPEC.md loads and is valid.
    #[test]
    fn the_spec_example_is_valid() {
        let text = r#"{
          "name": "Hinge leaf",
          "units": "mm",
          "features": [
            { "id": "s1", "type": "sketch", "plane": "XY",
              "entities": [ { "type": "rect", "x0": 0, "y0": 0, "x1": 40, "y1": 100 } ] },
            { "id": "e1", "type": "extrude", "sketch": "s1", "distance": 3, "op": "new" },
            { "id": "h1", "type": "hole", "on": "e1", "diameter": 4,
              "at": [[5, 5], [35, 5], [5, 95], [35, 95]], "depth": "through" }
          ]
        }"#;
        let design = Design::from_json(text).expect("loads");
        assert_eq!(design.features.len(), 3);
        assert!(design.validate().is_empty(), "{:?}", design.validate());
        // Saving and loading again gives the same design.
        assert_eq!(Design::from_json(&design.to_json()).expect("reloads"), design);
    }

    /// Mistakes come back as plain-language problems Claude can act on.
    #[test]
    fn mistakes_are_reported() {
        let text = r#"{ "name": "Bad", "features": [
            { "id": "e1", "type": "extrude", "sketch": "nope", "distance": 0 },
            { "id": "e1", "type": "hole", "on": "zzz", "diameter": -1, "at": [] }
        ] }"#;
        let problems = Design::from_json(text).expect("loads").validate();
        assert_eq!(problems.len(), 6, "{problems:?}");
    }

    fn problems_for(text: &str) -> Vec<String> {
        Design::from_json(text).expect("loads").validate().into_iter().map(|p| p.message).collect()
    }

    /// A one-circle sketch, an extrude, and a hole with the given extra JSON on the end.
    fn with_hole(extra: &str) -> String {
        format!(
            r#"{{ "name": "x", "features": [
            {{ "id": "s1", "type": "sketch", "plane": "XY", "entities": [ {{ "type": "circle", "cx": 0, "cy": 0, "r": 5 }} ] }},
            {{ "id": "e1", "type": "extrude", "sketch": "s1", "distance": 3 }},
            {{ "id": "h1", "type": "hole", "on": "e1", "diameter": 2, "at": [[0, 0]]{extra} }} ] }}"#
        )
    }

    #[test]
    fn hole_depth_reads_and_writes() {
        let depth_of = |text: String| match &Design::from_json(&text).expect("loads").features[2] {
            Feature::Hole { depth, .. } => *depth,
            other => panic!("expected a hole, got {other:?}"),
        };
        assert_eq!(depth_of(with_hole("")), HoleDepth::Through);
        assert_eq!(depth_of(with_hole(r#", "depth": "through""#)), HoleDepth::Through);
        assert_eq!(depth_of(with_hole(r#", "depth": 1.5"#)), HoleDepth::Blind(1.5));
        assert!(Design::from_json(&with_hole(r#", "depth": "deep""#)).is_err());

        let design = Design::from_json(&with_hole(r#", "depth": 1.5"#)).expect("loads");
        assert_eq!(Design::from_json(&design.to_json()).expect("reloads"), design);
        assert_eq!(problems_for(&with_hole(r#", "depth": -2"#)).len(), 1);
    }

    #[test]
    fn holes_must_go_on_extrudes_and_cuts_need_a_body() {
        let msgs = problems_for(
            r#"{ "name": "x", "features": [
            { "id": "s1", "type": "sketch", "plane": "XY", "entities": [ { "type": "circle", "cx": 0, "cy": 0, "r": 5 } ] },
            { "id": "e1", "type": "extrude", "sketch": "s1", "distance": 3, "op": "cut" },
            { "id": "h1", "type": "hole", "on": "s1", "diameter": 2, "at": [[0, 0]] } ] }"#,
        );
        assert_eq!(msgs.len(), 2, "{msgs:?}");
    }

    #[test]
    fn suppressed_references_are_reported() {
        let msgs = problems_for(
            r#"{ "name": "x", "features": [
            { "id": "s1", "type": "sketch", "plane": "XY", "suppressed": true,
              "entities": [ { "type": "circle", "cx": 0, "cy": 0, "r": 5 } ] },
            { "id": "e1", "type": "extrude", "sketch": "s1", "distance": 3 } ] }"#,
        );
        assert_eq!(msgs.len(), 1, "{msgs:?}");
    }

    #[test]
    fn non_finite_numbers_are_reported() {
        let mut design = Design::from_json(&with_hole("")).expect("loads");
        if let Feature::Extrude { distance, .. } = &mut design.features[1] {
            *distance = f64::NAN;
        }
        if let Feature::Hole { at, .. } = &mut design.features[2] {
            at[0][0] = f64::INFINITY;
        }
        assert_eq!(design.validate().len(), 2, "{:?}", design.validate());
    }

    #[test]
    fn sketch_planes_read_and_write() {
        let text = r#"{ "name": "x", "features": [
            { "id": "s1", "type": "sketch", "plane": "XY", "entities": [ { "type": "rect", "x0": 0, "y0": 0, "x1": 10, "y1": 10 } ] },
            { "id": "e1", "type": "extrude", "sketch": "s1", "distance": 3 },
            { "id": "s2", "type": "sketch", "plane": { "on": "e1" }, "entities": [ { "type": "circle", "cx": 5, "cy": 5, "r": 1 } ] },
            { "id": "s3", "type": "sketch", "plane": "YZ", "offset": -2.5, "entities": [ { "type": "circle", "cx": 5, "cy": 5, "r": 1 } ] } ] }"#;
        let design = Design::from_json(text).expect("loads");
        assert!(design.validate().is_empty(), "{:?}", design.validate());
        match &design.features[2] {
            Feature::Sketch { plane, offset, .. } => {
                assert_eq!(plane, &SketchPlane::Face { on: "e1".into(), side: Side::End }, "side defaults to end");
                assert_eq!(*offset, 0.0);
            }
            other => panic!("expected a sketch, got {other:?}"),
        }
        let json = design.to_json();
        assert!(json.contains(r#""offset": -2.5"#) && json.matches("offset").count() == 1, "offset only written when set");
        assert_eq!(Design::from_json(&json).expect("reloads"), design);

        // Clear errors for a bad plane.
        let bad = |plane: &str| {
            let t = format!(r#"{{ "name": "x", "features": [ {{ "id": "s1", "type": "sketch", "plane": {plane}, "entities": [] }} ] }}"#);
            Design::from_json(&t).unwrap_err().to_string()
        };
        assert!(bad(r#""XYZ""#).contains("\"XY\", \"XZ\", \"YZ\""));
        assert!(bad(r#"{ "on": "e1", "sid": "end" }"#).contains("sketch plane is"));
        assert!(bad(r#"{ "on": "e1", "side": "top" }"#).contains("sketch plane is"));
    }

    #[test]
    fn a_sketch_on_a_face_needs_an_earlier_extrude() {
        let msgs = problems_for(
            r#"{ "name": "x", "features": [
            { "id": "s0", "type": "sketch", "plane": "XY", "entities": [ { "type": "circle", "cx": 0, "cy": 0, "r": 5 } ] },
            { "id": "s1", "type": "sketch", "plane": { "on": "e1", "side": "end" }, "entities": [ { "type": "circle", "cx": 0, "cy": 0, "r": 5 } ] },
            { "id": "s2", "type": "sketch", "plane": { "on": "s0" }, "entities": [ { "type": "circle", "cx": 0, "cy": 0, "r": 5 } ] },
            { "id": "e1", "type": "extrude", "sketch": "s0", "distance": 3 } ] }"#,
        );
        assert_eq!(msgs.len(), 2, "{msgs:?}");
        assert!(msgs.iter().all(|m| m.contains("isn't an extrude earlier")), "{msgs:?}");
    }

    #[test]
    fn files_save_and_load() {
        let design = Design::from_json(&with_hole("")).expect("loads");
        let path = std::env::temp_dir().join("aad_model_file_test.aad.json");
        design.save(&path).expect("saves");
        assert_eq!(Design::load(&path).expect("loads"), design);
        std::fs::remove_file(&path).expect("cleans up");
        assert!(Design::load(&path).is_err());
    }
}
