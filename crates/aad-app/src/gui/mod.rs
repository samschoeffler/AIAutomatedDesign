//! The app window (docs/SPEC.md, "The app window"):
//! - top: toolbar (undo/redo, views, export), and Claude's proposal with Accept / Reject;
//! - left: the feature list, numbers editable (tree.rs);
//! - middle: the 3D view (viewer.rs, drawn by gpu.rs);
//! - bottom: size and volume, build errors, messages.
//!
//! Your edits change a copy of the design (the *draft*), which is built and shown as you
//! go. It becomes the real design (saved, undoable) when you let go of the mouse and
//! aren't typing, and only if it's valid. Claude's changes never touch the design until
//! you accept them.

mod build;
pub mod render;
mod gpu;
mod tree;
pub mod viewer;

use crate::control::{self, lock, Changes, Shared};
use aad_model::{Design, Problem};
use build::Builder;
use eframe::egui;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use viewer::{Compare, Content, View, Viewer};

/// Open the window and run until it's closed.
pub fn run(app: Shared) -> Result<(), String> {
    let title = window_title(&lock(&app).design.name);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([1300.0, 820.0])
            .with_min_inner_size([720.0, 460.0]),
        renderer: eframe::Renderer::Wgpu,
        // The 3D view needs a depth buffer (so near faces hide far ones) and looks
        // smoother with multisampling; gpu.rs builds its pipelines to match.
        depth_buffer: 32,
        multisampling: gpu::MSAA_SAMPLES as u16,
        ..Default::default()
    };
    eframe::run_native(
        "AiAidedDesign",
        options,
        Box::new(move |cc| {
            let render_state = cc.wgpu_render_state.as_ref().ok_or("the window has no GPU renderer")?;
            gpu::init(render_state);
            // Let the bridge wake the window when Claude proposes something.
            let ctx = cc.egui_ctx.clone();
            lock(&app).wake = Some(Arc::new(move || ctx.request_repaint()));
            Ok(Box::new(Window::new(app)))
        }),
    )
    .map_err(|e| format!("couldn't open the window: {e}"))
}

fn window_title(name: &str) -> String {
    format!("{name} - AiAidedDesign")
}

/// What an export thread reports: a message, or why it failed.
type ExportResult = Result<String, String>;

/// What the window shows of a proposal that's waiting.
struct ProposalView {
    id: u32,
    description: String,
    changes: Changes,
    size: Option<String>,
    design: Design,
}

struct Window {
    app: Shared,
    /// The design as last seen in `app`, and the copy you're editing.
    synced: Design,
    draft: Design,
    proposal: Option<ProposalView>,
    design_build: Builder,
    proposal_build: Builder,
    viewer: Viewer,
    compare: Compare,
    reject_note: String,
    /// The latest message for the status bar, whether it's an error, and when it came.
    message: Option<(String, bool, Instant)>,
    /// Exports run on a thread and report back here.
    exports: (Sender<ExportResult>, Receiver<ExportResult>),
    title: String,
    can_undo: bool,
    can_redo: bool,
    file_name: String,
}

impl Window {
    fn new(app: Shared) -> Window {
        let (design, file_name) = {
            let a = lock(&app);
            let file = a.file.as_ref().and_then(|f| f.file_name()).map(|n| n.to_string_lossy().into_owned());
            (a.design.clone(), file.unwrap_or_else(|| "not saved".into()))
        };
        Window {
            app,
            title: window_title(&design.name),
            synced: design.clone(),
            draft: design,
            proposal: None,
            design_build: Builder::new(),
            proposal_build: Builder::new(),
            viewer: Viewer::new(),
            compare: Compare::Proposed,
            reject_note: String::new(),
            message: None,
            exports: channel(),
            can_undo: false,
            can_redo: false,
            file_name,
        }
    }

    fn say(&mut self, text: impl Into<String>, error: bool) {
        let text = text.into();
        crate::say::say_line(format!("\n({text})"));
        self.message = Some((text, error, Instant::now()));
    }

    /// Catch up with the shared app state: the design, the waiting proposal.
    fn sync(&mut self) {
        let a = lock(&self.app);
        if a.design != self.synced {
            // Changed outside the draft (accepted, undone, or from the console):
            // what you were editing is out of date.
            self.synced = a.design.clone();
            self.draft = a.design.clone();
        }
        self.can_undo = a.can_undo();
        self.can_redo = a.can_redo();
        match a.pending() {
            Some(p) if self.proposal.as_ref().is_some_and(|v| v.id == p.id) => {}
            Some(p) => {
                self.proposal = Some(ProposalView {
                    id: p.id,
                    description: p.description.clone(),
                    changes: p.changes.clone(),
                    size: control::size_text(&p.measurements),
                    design: p.design.clone(),
                });
                self.compare = Compare::Proposed;
                self.reject_note.clear();
            }
            None => self.proposal = None,
        }
    }

    /// Make the draft the real design, if it's ready.
    fn commit(&mut self, ctx: &egui::Context) {
        let busy = ctx.egui_is_using_pointer() || ctx.egui_wants_keyboard_input();
        if busy || self.draft == self.synced || !self.draft.validate().is_empty() {
            return;
        }
        let result = lock(&self.app).edit(self.draft.clone());
        match result {
            Ok(_) => self.synced = self.draft.clone(),
            Err(e) => self.say(format!("couldn't save: {e}"), true),
        }
    }

    fn undo_redo(&mut self, redo: bool) {
        let result = {
            let mut a = lock(&self.app);
            if redo {
                a.redo()
            } else {
                a.undo()
            }
        };
        if let Err(e) = result {
            self.say(format!("couldn't save: {e}"), true);
        }
    }

    fn decide(&mut self, accept: bool) {
        let result = {
            let mut a = lock(&self.app);
            if accept {
                a.accept()
            } else {
                a.reject(self.reject_note.trim())
            }
        };
        match result {
            Ok(msg) => self.say(msg, false),
            Err(e) => self.say(e, true),
        }
        self.reject_note.clear();
    }

    fn export(&mut self, extension: &str) {
        let name = format!("{}.{extension}", file_safe(&self.synced.name));
        let app = self.app.clone();
        let tx = self.exports.0.clone();
        self.say(format!("exporting {name}…"), false);
        std::thread::spawn(move || {
            let result = control::handle(&app, "export", serde_json::json!({ "file_name": name }))
                .map(|v| format!("exported {}", v["path"].as_str().unwrap_or("?")));
            let _ = tx.send(result);
        });
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(&self.synced.name);
            ui.weak(&self.file_name);
            ui.separator();
            if ui.add_enabled(self.can_undo, egui::Button::new("⟲ Undo")).on_hover_text("Ctrl+Z").clicked() {
                self.undo_redo(false);
            }
            if ui.add_enabled(self.can_redo, egui::Button::new("⟳ Redo")).on_hover_text("Ctrl+Y").clicked() {
                self.undo_redo(true);
            }
            ui.separator();
            if ui.button("Fit").on_hover_text("Fit the part in view (or double-click the view)").clicked() {
                self.viewer.request_fit();
            }
            for view in View::ALL {
                if ui.button(view.label()).clicked() {
                    self.viewer.look_from(view);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Export STEP").on_hover_text("Saved next to the design file").clicked() {
                    self.export("step");
                }
                if ui.button("Export STL").on_hover_text("Saved next to the design file").clicked() {
                    self.export("stl");
                }
            });
        });
    }

    fn proposal_bar(&mut self, ui: &mut egui::Ui) {
        let Some(p) = &self.proposal else { return };
        let (id, description, changes, size) = (p.id, p.description.clone(), p.changes.clone(), p.size.clone());
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(format!("Claude proposes a change (#{id})")).strong());
            if !description.is_empty() {
                ui.label(egui::RichText::new(description).strong());
            }
        });
        ui.horizontal_wrapped(|ui| {
            let list = |ids: &[String]| if ids.is_empty() { "-".to_string() } else { ids.join(", ") };
            ui.label(format!(
                "added: {}   changed: {}   removed: {}",
                list(&changes.added),
                list(&changes.changed),
                list(&changes.removed)
            ));
            if let Some(size) = size {
                ui.label(format!("   new size: {size}"));
            }
        });
        ui.horizontal(|ui| {
            ui.label("Show:");
            for c in Compare::ALL {
                ui.selectable_value(&mut self.compare, c, c.label());
            }
            ui.separator();
            let accept = egui::Button::new(egui::RichText::new("✔ Accept").strong().color(egui::Color32::WHITE))
                .fill(egui::Color32::from_rgb(40, 130, 70));
            if ui.add(accept).clicked() {
                self.decide(true);
            }
            if ui.button("✖ Reject").clicked() {
                self.decide(false);
            }
            ui.add(
                egui::TextEdit::singleline(&mut self.reject_note)
                    .hint_text("reason for Claude (optional)")
                    .desired_width(260.0),
            );
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.design_build.busy() || self.proposal_build.busy() {
                ui.spinner();
                ui.weak("building…");
            }
            if let Some(err) = self.design_build.error() {
                ui.colored_label(ui.visuals().error_fg_color, format!("Doesn't build: {err}"));
            } else if let Some(built) = self.design_build.shown() {
                if let Some(size) = control::size_text(&built.measurements) {
                    ui.label(size);
                }
                if let Some(note) = built.measurements["note"].as_str() {
                    ui.weak(format!("({note})"));
                }
            }
            if let Some((text, error, when)) = &self.message {
                if when.elapsed() < Duration::from_secs(12) {
                    ui.separator();
                    let color = if *error { ui.visuals().error_fg_color } else { ui.visuals().text_color() };
                    ui.colored_label(color, text);
                }
            }
        });
    }
}

/// A design name made safe to use as a file name.
fn file_safe(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_alphanumeric() || "-_ ".contains(c) { c } else { '_' }).collect();
    let s = s.trim().to_string();
    if s.is_empty() {
        "part".into()
    } else {
        s
    }
}

impl eframe::App for Window {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.sync();

        // Keyboard shortcuts, unless you're typing in a box.
        if !ctx.egui_wants_keyboard_input() {
            use egui::{Key, KeyboardShortcut, Modifiers};
            let redo = ctx.input_mut(|i| {
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::Y))
                    || i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z))
            });
            let undo = ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::Z)));
            if redo {
                self.undo_redo(true);
            } else if undo {
                self.undo_redo(false);
            }
            if redo || undo {
                self.sync();
            }
        }
        while let Ok(result) = self.exports.1.try_recv() {
            match result {
                Ok(msg) => self.say(msg, false),
                Err(e) => self.say(e, true),
            }
        }

        // Builds: what you're editing, and Claude's proposal.
        let waker = || {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        };
        self.design_build.poll(waker());
        self.proposal_build.poll(waker());
        self.design_build.want(&self.draft, waker());
        if let Some(p) = &self.proposal {
            self.proposal_build.want(&p.design, waker());
        }

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            self.toolbar(ui);
            ui.add_space(2.0);
        });
        if self.proposal.is_some() {
            egui::Panel::top("proposal")
                .frame(egui::Frame::new().fill(proposal_fill(ui.visuals().dark_mode)).inner_margin(8.0))
                .show(ui, |ui| self.proposal_bar(ui));
        }
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.add_space(2.0);
            self.status_bar(ui);
        });
        let problems: Vec<Problem> = self.draft.validate();
        let claude_changes: Vec<String> = self
            .proposal
            .as_ref()
            .map(|p| p.changes.added.iter().chain(&p.changes.changed).cloned().collect())
            .unwrap_or_default();
        egui::Panel::left("features").default_size(330.0).size_range(240.0..=600.0).show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut self.draft.name);
            });
            ui.separator();
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                tree::show(ui, &mut self.draft, &problems, &claude_changes);
            });
        });

        let showing_proposal = self.proposal.is_some();
        let design = self.design_build.shown().cloned();
        let proposal = if showing_proposal { self.proposal_build.shown().cloned() } else { None };
        let bounds = [design.as_ref(), proposal.as_ref()]
            .into_iter()
            .flatten()
            .filter_map(|b| b.bounds)
            .reduce(|(lo1, hi1), (lo2, hi2)| (lo1.min(lo2), hi1.max(hi2)));
        let content = Content {
            design: design.map(|b| b.mesh.clone()),
            proposal: proposal.map(|b| b.mesh.clone()),
            compare: self.compare,
            bounds,
        };
        egui::CentralPanel::no_frame()
            .frame(egui::Frame::new().fill(egui::Color32::from_rgb(38, 41, 48)))
            .show(ui, |ui| self.viewer.show(ui, &content));

        self.commit(&ctx);
        let title = window_title(&self.synced.name);
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
        // Keep the status-bar message timer ticking.
        if self.message.is_some() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }
}

/// The proposal bar's background: a blue that suits the light or dark theme.
fn proposal_fill(dark_mode: bool) -> egui::Color32 {
    if dark_mode {
        egui::Color32::from_rgb(30, 52, 88)
    } else {
        egui::Color32::from_rgb(214, 230, 252)
    }
}
