//! The feature list on the left of the window: every feature, with its numbers editable,
//! plus suppress, reorder, delete and add. It edits a copy of the design (the window's
//! "draft"); the window decides when that copy becomes the real design.

use aad_model::{BodyOp, Design, Feature, HoleDepth, Plane, Problem, Side, SketchEntity, SketchPlane};
use eframe::egui;

/// Something done to a whole feature, applied after the list is drawn.
enum Action {
    MoveUp(usize),
    MoveDown(usize),
    Delete(usize),
}

/// Show the list and let the user edit `design` in place.
/// `problems` are shown under their features; ids in `claude_changes` get a marker.
pub fn show(ui: &mut egui::Ui, design: &mut Design, problems: &[Problem], claude_changes: &[String]) {
    let mut action = None;
    let count = design.features.len();
    if count == 0 {
        ui.weak("No features yet.");
    }
    for i in 0..count {
        // Split the list so a feature can be edited while looking at the ones before it.
        let (before, rest) = design.features.split_at_mut(i);
        let feature = &mut rest[0];
        ui.push_id(i, |ui| {
            egui::Frame::group(ui.style()).inner_margin(6.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                if let Some(a) = header(ui, feature, i, count, claude_changes) {
                    action = Some(a);
                }
                if !feature.is_suppressed() {
                    ui.add_space(2.0);
                    body(ui, feature, before);
                }
                for p in problems.iter().filter(|p| p.feature == feature.id()) {
                    ui.colored_label(ui.visuals().error_fg_color, &p.message);
                }
            });
        });
    }
    match action {
        Some(Action::MoveUp(i)) if i > 0 => design.features.swap(i, i - 1),
        Some(Action::MoveDown(i)) if i + 1 < count => design.features.swap(i, i + 1),
        Some(Action::Delete(i)) => {
            design.features.remove(i);
        }
        _ => {}
    }

    ui.add_space(6.0);
    ui.menu_button("➕ Add feature", |ui| {
        if ui.button("Sketch (rectangle)").clicked() {
            add_sketch(design);
            ui.close();
        }
        if let Some(on) = last_id(design, |f| matches!(f, Feature::Extrude { .. })) {
            if ui.button(format!("Sketch on top of {on}")).clicked() {
                add_sketch_on(design, on);
                ui.close();
            }
        }
        if ui.button("Extrude").clicked() {
            add_extrude(design);
            ui.close();
        }
        if ui.button("Holes").clicked() {
            add_hole(design);
            ui.close();
        }
    });
}

/// The feature's title row: kind, id, on/off, and move/delete buttons.
fn header(ui: &mut egui::Ui, f: &mut Feature, i: usize, count: usize, claude_changes: &[String]) -> Option<Action> {
    let mut action = None;
    ui.horizontal(|ui| {
        let kind = match f {
            Feature::Sketch { .. } => "Sketch",
            Feature::Extrude { op: BodyOp::Cut, .. } => "Cut",
            Feature::Extrude { .. } => "Extrude",
            Feature::Hole { .. } => "Holes",
        };
        let mut on = !f.is_suppressed();
        if ui.checkbox(&mut on, "").on_hover_text("Turn this feature off (suppress) or on").changed() {
            set_suppressed(f, !on);
        }
        let title = egui::RichText::new(format!("{kind}  {}", f.id())).strong();
        ui.label(if on { title } else { title.weak().strikethrough() });
        if claude_changes.iter().any(|c| c == f.id()) {
            ui.label(egui::RichText::new("● Claude").small().color(egui::Color32::from_rgb(110, 170, 255)))
                .on_hover_text("Claude's proposal changes or adds this feature");
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("🗑").on_hover_text("Delete").clicked() {
                action = Some(Action::Delete(i));
            }
            if ui.add_enabled(i + 1 < count, egui::Button::new("⬇").small()).on_hover_text("Move down").clicked() {
                action = Some(Action::MoveDown(i));
            }
            if ui.add_enabled(i > 0, egui::Button::new("⬆").small()).on_hover_text("Move up").clicked() {
                action = Some(Action::MoveUp(i));
            }
        });
    });
    action
}

/// The feature's settings.
fn body(ui: &mut egui::Ui, f: &mut Feature, before: &[Feature]) {
    match f {
        Feature::Sketch { plane, offset, entities, .. } => {
            let last_extrude = before.iter().rev().find(|f| matches!(f, Feature::Extrude { .. })).map(|f| f.id().to_string());
            ui.horizontal_wrapped(|ui| {
                ui.label("Plane");
                for p in [Plane::XY, Plane::XZ, Plane::YZ] {
                    let chosen = *plane == SketchPlane::Origin(p);
                    if ui.selectable_label(chosen, format!("{p:?}")).clicked() {
                        *plane = SketchPlane::Origin(p);
                    }
                }
                let on_face = matches!(plane, SketchPlane::Face { .. });
                let face_button = ui
                    .add_enabled(last_extrude.is_some(), egui::Button::selectable(on_face, "on a face"))
                    .on_hover_text("Sit on the start or end face of an earlier extrude, and move with it")
                    .on_disabled_hover_text("Needs an extrude earlier in the list");
                if face_button.clicked() && !on_face {
                    if let Some(on) = &last_extrude {
                        *plane = SketchPlane::Face { on: on.clone(), side: Side::End };
                    }
                }
            });
            if let SketchPlane::Face { on, side } = plane {
                ui.horizontal(|ui| {
                    ui.label("Face of");
                    pick_id(ui, "face_of", on, before, |f| matches!(f, Feature::Extrude { .. }));
                    ui.selectable_value(side, Side::Start, "start");
                    ui.selectable_value(side, Side::End, "end");
                });
            }
            ui.horizontal(|ui| {
                number(ui, "Offset", offset);
            })
            .response
            .on_hover_text("Moves the sketch this far along the plane's normal");
            let mut remove = None;
            for (k, e) in entities.iter_mut().enumerate() {
                ui.push_id(k, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        match e {
                            SketchEntity::Rect { x0, y0, x1, y1 } => {
                                ui.label("Rect");
                                number(ui, "x", x0);
                                number(ui, "y", y0);
                                ui.label("to");
                                number(ui, "x", x1);
                                number(ui, "y", y1);
                            }
                            SketchEntity::Circle { cx, cy, r } => {
                                ui.label("Circle");
                                number(ui, "x", cx);
                                number(ui, "y", cy);
                                let mut d = *r * 2.0;
                                if number(ui, "⌀", &mut d) {
                                    *r = d / 2.0;
                                }
                            }
                        }
                        if ui.small_button("✖").on_hover_text("Remove this shape").clicked() {
                            remove = Some(k);
                        }
                    });
                });
            }
            if let Some(k) = remove {
                entities.remove(k);
            }
            ui.horizontal(|ui| {
                if ui.small_button("+ rect").clicked() {
                    entities.push(SketchEntity::Rect { x0: 0.0, y0: 0.0, x1: 20.0, y1: 20.0 });
                }
                if ui.small_button("+ circle").clicked() {
                    entities.push(SketchEntity::Circle { cx: 10.0, cy: 10.0, r: 5.0 });
                }
            });
        }
        Feature::Extrude { sketch, distance, op, .. } => {
            ui.horizontal(|ui| {
                ui.label("Sketch");
                pick_id(ui, "sketch", sketch, before, |f| matches!(f, Feature::Sketch { .. }));
            });
            ui.horizontal(|ui| {
                number(ui, "Distance", distance);
            });
            ui.horizontal(|ui| {
                ui.label("Makes");
                ui.selectable_value(op, BodyOp::New, "new body");
                ui.selectable_value(op, BodyOp::Join, "join");
                ui.selectable_value(op, BodyOp::Cut, "cut");
            });
        }
        Feature::Hole { on, diameter, at, depth, .. } => {
            ui.horizontal(|ui| {
                ui.label("On");
                pick_id(ui, "on", on, before, |f| matches!(f, Feature::Extrude { .. }));
            });
            ui.horizontal(|ui| {
                number(ui, "⌀", diameter);
                let mut through = matches!(depth, HoleDepth::Through);
                if ui.checkbox(&mut through, "through").changed() {
                    *depth = if through { HoleDepth::Through } else { HoleDepth::Blind(5.0) };
                }
                if let HoleDepth::Blind(mm) = depth {
                    number(ui, "deep", mm);
                }
            });
            let mut remove = None;
            for (k, p) in at.iter_mut().enumerate() {
                ui.push_id(k, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!("{}.", k + 1));
                        number(ui, "x", &mut p[0]);
                        number(ui, "y", &mut p[1]);
                        if ui.small_button("✖").on_hover_text("Remove this hole").clicked() {
                            remove = Some(k);
                        }
                    });
                });
            }
            if let Some(k) = remove {
                at.remove(k);
            }
            if ui.small_button("+ hole").clicked() {
                let next = at.last().map_or([10.0, 10.0], |p| [p[0] + 10.0, p[1]]);
                at.push(next);
            }
        }
    }
}

/// A labelled number in mm: drag it sideways, or click and type. True if it changed.
fn number(ui: &mut egui::Ui, label: &str, value: &mut f64) -> bool {
    ui.label(label);
    ui.add(egui::DragValue::new(value).speed(0.1).max_decimals(3).suffix(" mm")).changed()
}

/// Choose the id of an earlier feature (the sketch an extrude uses, the body holes go in).
fn pick_id(ui: &mut egui::Ui, salt: &str, value: &mut String, before: &[Feature], fits: fn(&Feature) -> bool) {
    egui::ComboBox::from_id_salt(salt).selected_text(value.as_str()).show_ui(ui, |ui| {
        for f in before.iter().filter(|f| fits(f)) {
            ui.selectable_value(value, f.id().to_string(), f.id());
        }
    });
}

fn set_suppressed(f: &mut Feature, value: bool) {
    match f {
        Feature::Sketch { suppressed, .. } | Feature::Extrude { suppressed, .. } | Feature::Hole { suppressed, .. } => {
            *suppressed = value
        }
    }
}

/// An id like "sketch3" that isn't used yet.
fn fresh_id(design: &Design, prefix: &str) -> String {
    (1..).map(|n| format!("{prefix}{n}")).find(|id| design.features.iter().all(|f| f.id() != id)).expect("some number is free")
}

fn last_id(design: &Design, fits: fn(&Feature) -> bool) -> Option<String> {
    design.features.iter().rev().find(|f| fits(f)).map(|f| f.id().to_string())
}

fn add_sketch(design: &mut Design) {
    let id = fresh_id(design, "sketch");
    design.features.push(Feature::Sketch {
        id,
        plane: Plane::XY.into(),
        offset: 0.0,
        entities: vec![SketchEntity::Rect { x0: 0.0, y0: 0.0, x1: 50.0, y1: 30.0 }],
        suppressed: false,
    });
}

/// A sketch on the end face of `extrude` (the top of a plate), with a circle in it.
fn add_sketch_on(design: &mut Design, extrude: String) {
    let id = fresh_id(design, "sketch");
    design.features.push(Feature::Sketch {
        id,
        plane: SketchPlane::Face { on: extrude, side: Side::End },
        offset: 0.0,
        entities: vec![SketchEntity::Circle { cx: 10.0, cy: 10.0, r: 5.0 }],
        suppressed: false,
    });
}

fn add_extrude(design: &mut Design) {
    let Some(sketch) = last_id(design, |f| matches!(f, Feature::Sketch { .. })) else {
        // An extrude needs a sketch, so make one first.
        add_sketch(design);
        return add_extrude(design);
    };
    let has_body = design.features.iter().any(|f| matches!(f, Feature::Extrude { op: BodyOp::New, .. }));
    let id = fresh_id(design, "extrude");
    design.features.push(Feature::Extrude {
        id,
        sketch,
        distance: 5.0,
        op: if has_body { BodyOp::Join } else { BodyOp::New },
        suppressed: false,
    });
}

fn add_hole(design: &mut Design) {
    let Some(on) = last_id(design, |f| matches!(f, Feature::Extrude { .. })) else {
        add_extrude(design);
        return add_hole(design);
    };
    let id = fresh_id(design, "holes");
    design.features.push(Feature::Hole {
        id,
        on,
        diameter: 5.0,
        at: vec![[10.0, 10.0]],
        depth: HoleDepth::Through,
        suppressed: false,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adding_features_makes_a_valid_part_from_nothing() {
        let mut d = Design { name: "t".into(), units: "mm".into(), features: Vec::new() };
        add_hole(&mut d); // needs an extrude, which needs a sketch
        assert_eq!(d.features.len(), 3);
        assert!(d.validate().is_empty(), "{:?}", d.validate());
        add_extrude(&mut d);
        assert!(matches!(d.features[3], Feature::Extrude { op: BodyOp::Join, .. }));
        assert_eq!(d.features[3].id(), "extrude2");
        add_sketch_on(&mut d, "extrude1".into());
        assert!(d.validate().is_empty(), "{:?}", d.validate());
    }
}
