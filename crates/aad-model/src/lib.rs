//! The design model: a part is an ordered list of features.
//!
//! This is the "language" Claude and the app share (docs/SPEC.md, "The feature list").
//! The kernel turns this list into a solid; nothing here does geometry.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

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
        plane: Plane,
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
        /// The id of the feature whose body gets the holes.
        on: String,
        diameter: f64,
        /// Hole centres, in the sketch plane of `on`.
        at: Vec<[f64; 2]>,
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

/// What an extrude does to the existing solid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyOp {
    #[default]
    New,
    Join,
    Cut,
}

/// The shapes a sketch can hold (more come with the sketcher, phase 7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

    /// Check the design before it reaches the kernel. An empty list means it's fine.
    pub fn validate(&self) -> Vec<Problem> {
        let mut problems = Vec::new();
        // Ids seen so far: a feature may only refer to features *before* it.
        let mut seen: HashSet<&str> = HashSet::new();
        let mut sketches: HashSet<&str> = HashSet::new();
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
            match f {
                Feature::Sketch { entities, .. } => {
                    if entities.is_empty() {
                        bad(id, "a sketch needs at least one shape".into());
                    }
                    for e in entities {
                        match e {
                            SketchEntity::Rect { x0, y0, x1, y1 } if x0 == x1 || y0 == y1 => {
                                bad(id, "a rectangle has zero width or height".into())
                            }
                            SketchEntity::Circle { r, .. } if *r <= 0.0 => bad(id, "a circle's radius must be above 0".into()),
                            _ => {}
                        }
                    }
                    sketches.insert(id);
                }
                Feature::Extrude { sketch, distance, .. } => {
                    if !sketches.contains(sketch.as_str()) {
                        bad(id, format!("extrudes sketch {sketch:?}, which isn't a sketch earlier in the list"));
                    }
                    if *distance == 0.0 {
                        bad(id, "extrude distance can't be 0".into());
                    }
                }
                Feature::Hole { on, diameter, at, .. } => {
                    if !seen.contains(on.as_str()) {
                        bad(id, format!("holes go on {on:?}, which isn't earlier in the list"));
                    }
                    if *diameter <= 0.0 {
                        bad(id, "hole diameter must be above 0".into());
                    }
                    if at.is_empty() {
                        bad(id, "give at least one hole position".into());
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
              "at": [[5, 5], [35, 5], [5, 95], [35, 95]] }
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
}
