//! The 3D view: an orbit camera you drive with the mouse, and the scene it hands to the GPU.
//!
//! Mouse: left-drag orbits, right- or middle-drag (or shift + left-drag) pans, the wheel
//! zooms towards the pointer, double-click fits the part in view. Z points up, as in most CAD.

use super::build::next_mesh_id;
use super::gpu::{Draw, LineVertex, MeshData, Scene, Slot, Style};
use eframe::egui;
use glam::{vec3, Mat4, Vec3};
use std::sync::Arc;

/// Vertical field of view. Narrow, so the part looks nearly flat-on like a drawing.
const FOV_Y: f32 = 25.0 * std::f32::consts::PI / 180.0;

/// A standard direction to look from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum View {
    Iso,
    Top,
    Front,
    Right,
}

impl View {
    pub const ALL: [View; 4] = [View::Iso, View::Top, View::Front, View::Right];

    pub fn label(self) -> &'static str {
        match self {
            View::Iso => "Iso",
            View::Top => "Top",
            View::Front => "Front",
            View::Right => "Right",
        }
    }

    /// (yaw, pitch) in radians. Yaw turns around Z (0 = looking from +X), pitch is the
    /// angle above the XY plane.
    fn angles(self) -> (f32, f32) {
        let d = std::f32::consts::PI / 180.0;
        match self {
            View::Iso => (-45.0 * d, 35.264 * d),
            View::Top => (-90.0 * d, 90.0 * d),
            View::Front => (-90.0 * d, 0.0),
            View::Right => (0.0, 0.0),
        }
    }
}

/// Orbit camera: it circles a target point at some distance.
#[derive(Clone, Debug)]
pub struct Camera {
    pub target: Vec3,
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
}

impl Camera {
    fn new() -> Camera {
        let (yaw, pitch) = View::Iso.angles();
        Camera { target: Vec3::ZERO, yaw, pitch, distance: 300.0 }
    }

    /// Unit vector from the target towards the camera.
    fn backward(&self) -> Vec3 {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        vec3(cp * cy, cp * sy, sp)
    }

    /// Screen-right and screen-up directions in the world. Worked out from yaw alone for
    /// "right", so looking straight down (Top) still has a sensible up.
    fn right_up(&self) -> (Vec3, Vec3) {
        let right = vec3(-self.yaw.sin(), self.yaw.cos(), 0.0);
        let up = right.cross(-self.backward());
        (right, up)
    }

    pub fn eye(&self) -> Vec3 {
        self.target + self.backward() * self.distance
    }

    pub fn view_proj(&self, aspect: f32) -> Mat4 {
        let (_, up) = self.right_up();
        let view = glam::camera::rh::view::look_to_mat4(self.eye(), -self.backward(), up);
        // The near plane sits at 2% of the distance; the line nudge in shader.wgsl relies on it.
        // wgpu uses the DirectX convention: depth from 0 to 1, Y up.
        let proj = glam::camera::rh::proj::directx::perspective(FOV_Y, aspect.max(0.01), self.distance * 0.02, self.distance * 100.0);
        proj * view
    }

    /// Look from a standard direction, keeping the target and distance.
    pub fn look_from(&mut self, view: View) {
        (self.yaw, self.pitch) = view.angles();
    }

    /// Move so the box from `lo` to `hi` fills the view.
    pub fn fit(&mut self, lo: Vec3, hi: Vec3, aspect: f32) {
        self.target = (lo + hi) * 0.5;
        let radius = ((hi - lo).length() * 0.5).max(1.0);
        // Fit the sphere around the box into whichever of height and width is narrower.
        let half_height = FOV_Y * 0.5;
        let half_width = ((FOV_Y * 0.5).tan() * aspect.max(0.01)).atan();
        let half = half_height.min(half_width);
        self.distance = radius / half.sin() * 1.08;
    }
}

/// What the viewer shows this frame.
pub struct Content {
    /// The accepted design (or your edits in progress).
    pub design: Option<Arc<MeshData>>,
    /// Claude's proposal, if one is waiting and should be shown.
    pub proposal: Option<Arc<MeshData>>,
    pub compare: Compare,
    /// The size of everything shown, for fitting and the grid.
    pub bounds: Option<(Vec3, Vec3)>,
}

/// How a waiting proposal is shown next to the current design.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compare {
    /// The proposed part, with the current part's edges faintly on top.
    Proposed,
    /// Only the current part.
    Current,
    /// The current part, with the proposed one see-through over it.
    Both,
}

impl Compare {
    pub const ALL: [Compare; 3] = [Compare::Proposed, Compare::Current, Compare::Both];

    pub fn label(self) -> &'static str {
        match self {
            Compare::Proposed => "Proposed",
            Compare::Current => "Current",
            Compare::Both => "Both",
        }
    }
}

// Colours (RGBA, 0..1).
const PART: [f32; 4] = [0.74, 0.77, 0.82, 1.0];
const EDGE: [f32; 4] = [0.08, 0.09, 0.11, 1.0];
const PROPOSED: [f32; 4] = [0.48, 0.72, 1.0, 1.0];
const PROPOSED_EDGE: [f32; 4] = [0.05, 0.16, 0.35, 1.0];
const GHOST: [f32; 4] = [0.40, 0.68, 1.0, 0.35];
const OLD_EDGE: [f32; 4] = [1.0, 0.62, 0.20, 0.85];

pub struct Viewer {
    pub camera: Camera,
    /// Fit the view to the part on the next frame that has one.
    fit_pending: bool,
    grid: Option<(GridKey, Arc<MeshData>)>,
    aspect: f32,
}

/// What the grid was made for; a new grid is made when this changes.
type GridKey = (i32, i32, i32, i32, i32);

impl Viewer {
    pub fn new() -> Viewer {
        Viewer { camera: Camera::new(), fit_pending: true, grid: None, aspect: 1.5 }
    }

    pub fn request_fit(&mut self) {
        self.fit_pending = true;
    }

    pub fn look_from(&mut self, view: View) {
        self.camera.look_from(view);
        self.fit_pending = true;
    }

    /// Show the viewer, filling the space `ui` has left.
    pub fn show(&mut self, ui: &mut egui::Ui, content: &Content) {
        let rect = ui.available_rect_before_wrap();
        let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
        self.aspect = rect.width() / rect.height().max(1.0);

        if self.fit_pending {
            if let Some((lo, hi)) = content.bounds {
                self.camera.fit(lo, hi, self.aspect);
                self.fit_pending = false;
            }
        }
        self.handle_input(ui, &response, rect, content);

        let mut scene = Scene {
            view_proj: self.camera.view_proj(self.aspect),
            // Light from over the viewer's left shoulder.
            light: {
                let (right, up) = self.camera.right_up();
                self.camera.backward() * 2.0 - right * 0.6 + up * 1.0
            },
            meshes: Vec::new(),
            draws: Vec::new(),
        };
        let grid = self.grid_for(content.bounds);
        scene.meshes.push((Slot::Grid, grid));
        scene.draws.push(Draw { slot: Slot::Grid, style: Style::Lines, color: [1.0; 4] });

        let proposal = content.proposal.clone();
        if let Some(m) = &content.design {
            scene.meshes.push((Slot::Design, m.clone()));
        }
        if let Some(m) = &proposal {
            scene.meshes.push((Slot::Proposal, m.clone()));
        }
        let draw = |slot, style, color| Draw { slot, style, color };
        match (proposal.is_some(), content.compare) {
            (true, Compare::Proposed) => {
                scene.draws.push(draw(Slot::Proposal, Style::Solid, PROPOSED));
                scene.draws.push(draw(Slot::Proposal, Style::Lines, PROPOSED_EDGE));
                scene.draws.push(draw(Slot::Design, Style::Lines, OLD_EDGE));
            }
            (true, Compare::Both) => {
                scene.draws.push(draw(Slot::Design, Style::Solid, PART));
                scene.draws.push(draw(Slot::Design, Style::Lines, EDGE));
                scene.draws.push(draw(Slot::Proposal, Style::Ghost, GHOST));
                scene.draws.push(draw(Slot::Proposal, Style::Lines, PROPOSED_EDGE));
            }
            _ => {
                scene.draws.push(draw(Slot::Design, Style::Solid, PART));
                scene.draws.push(draw(Slot::Design, Style::Lines, EDGE));
            }
        }
        ui.painter().add(eframe::egui_wgpu::Callback::new_paint_callback(rect, scene));

        // Axis key in the corner: which way X, Y and Z point.
        self.draw_axis_key(ui, rect);
        if content.design.is_none() && content.proposal.is_none() {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Nothing to show yet.\nAsk Claude for a part, or add a feature on the left.",
                egui::FontId::proportional(16.0),
                ui.visuals().weak_text_color(),
            );
        }
    }

    fn handle_input(&mut self, ui: &egui::Ui, response: &egui::Response, rect: egui::Rect, content: &Content) {
        let cam = &mut self.camera;
        let shift = ui.input(|i| i.modifiers.shift);
        let delta = response.drag_delta();
        let pan = response.dragged_by(egui::PointerButton::Secondary)
            || response.dragged_by(egui::PointerButton::Middle)
            || (shift && response.dragged_by(egui::PointerButton::Primary));
        // How many mm one screen pixel covers at the target.
        let mm_per_px = 2.0 * cam.distance * (FOV_Y * 0.5).tan() / rect.height().max(1.0);
        if pan {
            let (right, up) = cam.right_up();
            cam.target += (-right * delta.x + up * delta.y) * mm_per_px;
        } else if response.dragged_by(egui::PointerButton::Primary) {
            cam.yaw -= delta.x * 0.008;
            let limit = std::f32::consts::FRAC_PI_2;
            cam.pitch = (cam.pitch + delta.y * 0.008).clamp(-limit, limit);
        }

        if response.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                let factor = (-scroll * 0.0025).exp();
                // Zoom towards the point under the pointer: move the target with it.
                if let Some(pointer) = response.hover_pos() {
                    let (right, up) = cam.right_up();
                    let off = pointer - rect.center();
                    let towards = (right * off.x - up * off.y) * mm_per_px;
                    cam.target += towards * (1.0 - factor);
                }
                cam.distance = (cam.distance * factor).clamp(1.0, 100_000.0);
            }
        }
        if response.double_clicked() {
            if let Some((lo, hi)) = content.bounds {
                cam.fit(lo, hi, self.aspect);
            }
        }
    }

    /// A grid on the XY plane (10 mm squares, a stronger line every 50 mm) under the part,
    /// plus the X and Y axes in red and green.
    fn grid_for(&mut self, bounds: Option<(Vec3, Vec3)>) -> Arc<MeshData> {
        let (lo, hi) = bounds.unwrap_or((vec3(-50.0, -50.0, 0.0), vec3(50.0, 50.0, 0.0)));
        let size = (hi - lo).max_element().max(20.0);
        let step = if size > 600.0 { 50.0 } else { 10.0 };
        let margin = (size * 0.25).max(step);
        let snap = |v: f32, up: bool| {
            let s = if up { (v / step).ceil() } else { (v / step).floor() };
            s as i32
        };
        let key = (
            snap(lo.x.min(0.0) - margin, false),
            snap(lo.y.min(0.0) - margin, false),
            snap(hi.x.max(0.0) + margin, true),
            snap(hi.y.max(0.0) + margin, true),
            step as i32,
        );
        if let Some((k, mesh)) = &self.grid {
            if *k == key {
                return mesh.clone();
            }
        }
        let (x0, y0, x1, y1) = (key.0 as f32 * step, key.1 as f32 * step, key.2 as f32 * step, key.3 as f32 * step);
        let minor = [0.55, 0.58, 0.65, 0.16];
        let major = [0.60, 0.63, 0.70, 0.32];
        let mut lines = Vec::new();
        let mut seg = |a: Vec3, b: Vec3, color: [f32; 4]| {
            lines.push(LineVertex { position: a.into(), color });
            lines.push(LineVertex { position: b.into(), color });
        };
        // A stronger line every 50 mm (or every 200 mm on the coarse grid).
        let major_every = if step < 50.0 { 5 } else { 4 };
        for i in key.0..=key.2 {
            let x = i as f32 * step;
            if i != 0 {
                seg(vec3(x, y0, 0.0), vec3(x, y1, 0.0), if i % major_every == 0 { major } else { minor });
            }
        }
        for j in key.1..=key.3 {
            let y = j as f32 * step;
            if j != 0 {
                seg(vec3(x0, y, 0.0), vec3(x1, y, 0.0), if j % major_every == 0 { major } else { minor });
            }
        }
        seg(vec3(x0, 0.0, 0.0), vec3(x1, 0.0, 0.0), [0.90, 0.30, 0.30, 0.7]); // X axis
        seg(vec3(0.0, y0, 0.0), vec3(0.0, y1, 0.0), [0.35, 0.80, 0.35, 0.7]); // Y axis
        let mesh = Arc::new(MeshData { id: next_mesh_id(), triangles: Vec::new(), lines });
        self.grid = Some((key, mesh.clone()));
        mesh
    }

    fn draw_axis_key(&self, ui: &egui::Ui, rect: egui::Rect) {
        let centre = egui::pos2(rect.left() + 44.0, rect.bottom() - 44.0);
        let (right, up) = self.camera.right_up();
        let painter = ui.painter_at(rect);
        let axes = [
            (Vec3::X, "X", egui::Color32::from_rgb(230, 80, 80)),
            (Vec3::Y, "Y", egui::Color32::from_rgb(90, 200, 90)),
            (Vec3::Z, "Z", egui::Color32::from_rgb(90, 140, 240)),
        ];
        for (axis, name, color) in axes {
            let end = centre + egui::vec2(axis.dot(right), -axis.dot(up)) * 28.0;
            painter.line_segment([centre, end], egui::Stroke::new(2.0, color));
            painter.text(end, egui::Align2::CENTER_CENTER, name, egui::FontId::proportional(12.0), color);
        }
    }
}
