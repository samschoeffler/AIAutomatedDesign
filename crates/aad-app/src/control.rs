//! What the app does when the bridge asks: hand out the design, take proposals, measure,
//! export. The network side is in aad-link; the console prompt is in main.rs.
//!
//! The design only changes when the user accepts a proposal. A proposal is checked
//! (validated and built) before the user ever sees it, so a bad AI step comes straight
//! back to Claude as an error instead of reaching the prompt.

use crate::say::{say, say_line};
use aad_model::{Design, Feature};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The longest `proposal_status` will wait for the user, in seconds.
pub const MAX_WAIT_SECONDS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Accepted,
    Rejected,
    /// A newer proposal arrived before the user decided on this one.
    Superseded,
}

/// What a proposal changes, by feature id.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Changes {
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
    pub renamed: bool,
}

impl Changes {
    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty() && !self.renamed
    }
}

#[derive(Debug, Clone)]
pub struct Proposal {
    pub id: u32,
    /// The whole design as it would be after accepting.
    pub design: Design,
    pub status: Status,
    /// Claude's one-line description of the change.
    pub description: String,
    pub changes: Changes,
    pub measurements: Value,
    /// What the user typed when rejecting, if anything.
    pub user_note: String,
}

/// Everything the app knows. Shared between the console and the bridge's connections.
pub struct App {
    pub design: Design,
    /// Where accepted designs are saved. `None` means nothing is saved.
    pub file: Option<PathBuf>,
    /// The only folder `export` writes to.
    pub export_dir: PathBuf,
    pub proposals: Vec<Proposal>,
}

pub type Shared = Arc<Mutex<App>>;

/// Lock the app. A panic in another thread doesn't make the design unusable, so a
/// poisoned lock is simply taken over.
pub fn lock(app: &Shared) -> MutexGuard<'_, App> {
    app.lock().unwrap_or_else(|e| e.into_inner())
}

impl App {
    pub fn new(design: Design, file: Option<PathBuf>, export_dir: PathBuf) -> App {
        App { design, file, export_dir, proposals: Vec::new() }
    }

    pub fn pending(&self) -> Option<&Proposal> {
        self.proposals.iter().rev().find(|p| p.status == Status::Pending)
    }

    /// Accept the waiting proposal: it becomes the design, and is saved if there's a file.
    pub fn accept(&mut self) -> Result<String, String> {
        let p = self.proposals.iter_mut().rev().find(|p| p.status == Status::Pending).ok_or("nothing to accept")?;
        p.status = Status::Accepted;
        let id = p.id;
        self.design = p.design.clone();
        match &self.file {
            Some(f) => {
                self.design.save(f)?;
                Ok(format!("accepted #{id}, saved to {}", f.display()))
            }
            None => Ok(format!("accepted #{id} (not saved: the app was started without a design file)")),
        }
    }

    /// Reject the waiting proposal. `note` is passed back to Claude.
    pub fn reject(&mut self, note: &str) -> Result<String, String> {
        let p = self.proposals.iter_mut().rev().find(|p| p.status == Status::Pending).ok_or("nothing to reject")?;
        p.status = Status::Rejected;
        p.user_note = note.to_string();
        Ok(format!("rejected #{}", p.id))
    }
}

/// The arguments of `propose_features`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposeArgs {
    /// Kept as raw JSON so a bad feature can be reported by its position and id.
    #[serde(default)]
    pub features: Vec<Value>,
    #[serde(default)]
    pub replace_all: bool,
    #[serde(default)]
    pub remove: Vec<String>,
    pub name: Option<String>,
    #[serde(default)]
    pub description: String,
}

/// Work out the design a proposal would produce, and what it changes. Checks the result
/// with the model's validation, but doesn't build it.
pub fn apply(base: &Design, args: &ProposeArgs) -> Result<(Design, Changes), String> {
    let mut features = Vec::new();
    for (i, v) in args.features.iter().enumerate() {
        let f: Feature = serde_json::from_value(v.clone()).map_err(|e| {
            let id = v.get("id").and_then(Value::as_str).unwrap_or("?");
            format!("feature {} (id {id:?}) couldn't be read: {e}", i + 1)
        })?;
        features.push(f);
    }
    let mut ids = HashSet::new();
    if let Some(dup) = features.iter().find(|f| !ids.insert(f.id())) {
        return Err(format!("the id {:?} appears twice in this proposal", dup.id()));
    }

    let mut design = base.clone();
    let mut changes = Changes::default();
    if args.replace_all {
        if !args.remove.is_empty() {
            return Err("use replace_all or remove, not both (with replace_all, just leave features out)".into());
        }
        for f in &features {
            match base.features.iter().find(|b| b.id() == f.id()) {
                None => changes.added.push(f.id().to_string()),
                Some(b) if b != f => changes.changed.push(f.id().to_string()),
                Some(_) => {}
            }
        }
        changes.removed =
            base.features.iter().filter(|b| !ids.contains(b.id())).map(|b| b.id().to_string()).collect();
        design.features = features;
    } else {
        for id in &args.remove {
            let before = design.features.len();
            design.features.retain(|f| f.id() != id);
            if design.features.len() == before {
                return Err(format!("can't remove {id:?}: there's no feature with that id"));
            }
            changes.removed.push(id.clone());
        }
        for f in features {
            match design.features.iter_mut().find(|d| d.id() == f.id()) {
                Some(d) if *d != f => {
                    changes.changed.push(f.id().to_string());
                    *d = f;
                }
                Some(_) => {}
                None => {
                    changes.added.push(f.id().to_string());
                    design.features.push(f);
                }
            }
        }
    }
    if let Some(name) = &args.name {
        if *name != design.name {
            design.name = name.clone();
            changes.renamed = true;
        }
    }

    if changes.is_empty() {
        return Err("this proposal doesn't change anything".into());
    }
    let problems = design.validate();
    if !problems.is_empty() {
        let lines: Vec<String> = problems.iter().map(|p| format!("- {}: {}", p.feature, p.message)).collect();
        return Err(format!("the proposed design has problems:\n{}", lines.join("\n")));
    }
    Ok((design, changes))
}

/// Size, volume and body count of a built part, rounded to 0.01 mm.
pub fn measurements(part: &aad_kernel::Part) -> Value {
    // Adding 0.0 turns -0.0 into 0.0.
    let r = |v: f64| (v * 100.0).round() / 100.0 + 0.0;
    let bbox = part.bounding_box().map(|(lo, hi)| {
        json!({
            "min": [r(lo.x), r(lo.y), r(lo.z)],
            "max": [r(hi.x), r(hi.y), r(hi.z)],
            "size": [r(hi.x - lo.x), r(hi.y - lo.y), r(hi.z - lo.z)],
        })
    });
    let mut m = json!({
        "bodies": part.bodies.len(),
        "volume_mm3": r(part.volume()),
        "bounding_box_mm": bbox,
    });
    if part.has_overlapping_pieces() {
        m["note"] = json!("a join couldn't be fused, so it's kept as an overlapping piece; volume counts the overlap twice");
    }
    m
}

fn build(design: &Design) -> Result<aad_kernel::Part, String> {
    aad_kernel::build(design).map_err(|e| format!("the design doesn't build: {e}"))
}

fn parse<T: for<'de> Deserialize<'de> + Default>(args: Value) -> Result<T, String> {
    if args.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(args).map_err(|e| format!("bad arguments: {e}"))
}

/// Answer one command from the bridge.
pub fn handle(app: &Shared, command: &str, args: Value) -> Result<Value, String> {
    match command {
        "get_design" => {
            let a = lock(app);
            Ok(json!({ "design": a.design, "file": a.file, "pending_proposal": a.pending().map(|p| p.id) }))
        }
        "propose_features" => propose(app, parse(args)?),
        "proposal_status" => status(app, parse(args)?),
        "measure" => measure(app, parse(args)?),
        "export" => export(app, parse(args)?),
        other => Err(format!("unknown command {other:?}")),
    }
}

fn propose(app: &Shared, args: ProposeArgs) -> Result<Value, String> {
    let base = lock(app).design.clone();
    let (design, changes) = apply(&base, &args)?;
    // Build without holding the lock: it can take a moment, and the console needs the lock.
    let measurements = measurements(&build(&design)?);

    let mut a = lock(app);
    if a.design != base {
        return Err("the design changed while this proposal was being checked; call get_design and propose again".into());
    }
    let mut text = String::new();
    for p in a.proposals.iter_mut().filter(|p| p.status == Status::Pending) {
        p.status = Status::Superseded;
        text.push_str(&format!("\n(proposal #{} was replaced by a newer one)\n", p.id));
    }
    let id = a.proposals.len() as u32 + 1;
    let proposal = Proposal {
        id,
        design,
        status: Status::Pending,
        description: args.description,
        changes: changes.clone(),
        measurements: measurements.clone(),
        user_note: String::new(),
    };
    text.push_str(&describe_proposal(&proposal));
    a.proposals.push(proposal);
    // Never print while holding the lock (see say.rs).
    drop(a);
    say(text);
    Ok(json!({
        "proposal_id": id,
        "status": Status::Pending,
        "changes": changes,
        "measurements": measurements,
        "next": "The user accepts or rejects this in the AiAidedDesign console. \
                 Call proposal_status with wait_seconds to hear back.",
    }))
}

/// How a new proposal looks in the console, ending with the accept/reject prompt.
fn describe_proposal(p: &Proposal) -> String {
    let list = |ids: &[String]| if ids.is_empty() { "-".to_string() } else { ids.join(", ") };
    let mut t = format!("\n── Claude proposes a change (#{}) ──\n", p.id);
    if !p.description.is_empty() {
        t.push_str(&format!("   {}\n", p.description));
    }
    t.push_str(&format!(
        "   added: {}   changed: {}   removed: {}\n",
        list(&p.changes.added),
        list(&p.changes.changed),
        list(&p.changes.removed)
    ));
    if p.changes.renamed {
        t.push_str(&format!("   new name: {:?}\n", p.design.name));
    }
    if let Some(size) = p.measurements["bounding_box_mm"]["size"].as_array() {
        let s: Vec<String> = size.iter().map(|v| format!("{:.1}", v.as_f64().unwrap_or(0.0))).collect();
        let volume = p.measurements["volume_mm3"].as_f64().unwrap_or(0.0);
        t.push_str(&format!("   size {} mm, volume {volume:.0} mm³\n", s.join(" x ")));
    }
    t.push_str("   [a]ccept / [r]eject (add a reason for Claude: r too thin)\n> ");
    t
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    /// Defaults to the latest proposal.
    proposal_id: Option<u32>,
    #[serde(default)]
    wait_seconds: u64,
}

fn status(app: &Shared, args: StatusArgs) -> Result<Value, String> {
    let deadline = Instant::now() + Duration::from_secs(args.wait_seconds.min(MAX_WAIT_SECONDS));
    loop {
        {
            let a = lock(app);
            let p = match args.proposal_id {
                Some(id) => a.proposals.iter().find(|p| p.id == id),
                None => a.proposals.last(),
            }
            .ok_or("there's no such proposal")?;
            if p.status != Status::Pending || Instant::now() >= deadline {
                let mut v = json!({ "proposal_id": p.id, "status": p.status });
                if !p.user_note.is_empty() {
                    v["user_note"] = json!(p.user_note);
                }
                return Ok(v);
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Target {
    #[default]
    Design,
    Proposal,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct MeasureArgs {
    #[serde(default)]
    target: Target,
}

fn measure(app: &Shared, args: MeasureArgs) -> Result<Value, String> {
    let design = match args.target {
        Target::Proposal => {
            return lock(app).pending().map(|p| p.measurements.clone()).ok_or("no proposal is waiting".into());
        }
        Target::Design => lock(app).design.clone(),
    };
    if design.features.is_empty() {
        return Err("the design is empty".into());
    }
    Ok(measurements(&build(&design)?))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportArgs {
    file_name: String,
}

#[derive(Debug, PartialEq)]
enum Format {
    Stl,
    Step,
}

/// Exports may only go into the export folder, so the name must be a plain file name.
fn check_file_name(name: &str) -> Result<Format, String> {
    if name.is_empty() || name.starts_with('.') || name.contains(['/', '\\', ':']) {
        return Err(format!(
            "{name:?} isn't a plain file name; give just a name like \"bracket.stl\" (exports go next to the design file)"
        ));
    }
    match name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()).as_deref() {
        Some("stl") => Ok(Format::Stl),
        Some("step" | "stp") => Ok(Format::Step),
        _ => Err(format!("{name:?} must end in .stl, .step or .stp")),
    }
}

fn export(app: &Shared, args: ExportArgs) -> Result<Value, String> {
    let name = args.file_name.trim();
    let format = check_file_name(name)?;
    let (design, dir) = {
        let a = lock(app);
        (a.design.clone(), a.export_dir.clone())
    };
    if design.features.is_empty() {
        return Err("the design is empty; nothing to export".into());
    }
    let part = build(&design)?;
    let path = dir.join(name);
    match format {
        Format::Stl => aad_export::write_stl(&part, &path)?,
        Format::Step => aad_export::write_step(&part, &design.name, &path)?,
    }
    say_line(format!("\n(exported {})", path.display()));
    Ok(json!({ "path": path, "measurements": measurements(&part) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plate() -> Design {
        Design::from_json(
            r#"{ "name": "Plate", "features": [
            { "id": "s1", "type": "sketch", "plane": "XY", "entities": [ { "type": "rect", "x0": 0, "y0": 0, "x1": 40, "y1": 20 } ] },
            { "id": "e1", "type": "extrude", "sketch": "s1", "distance": 3 } ] }"#,
        )
        .expect("loads")
    }

    fn args(v: Value) -> ProposeArgs {
        serde_json::from_value(v).expect("args parse")
    }

    fn shared(design: Design) -> Shared {
        Arc::new(Mutex::new(App::new(design, None, std::env::temp_dir())))
    }

    const HOLE: &str = r#"{ "id": "h1", "type": "hole", "on": "e1", "diameter": 4, "at": [[5, 5]] }"#;

    #[test]
    fn merging_adds_changes_and_removes() {
        let hole: Value = serde_json::from_str(HOLE).expect("json");
        let (d, c) = apply(&plate(), &args(json!({ "features": [hole] }))).expect("applies");
        assert_eq!(d.features.len(), 3);
        assert_eq!(c.added, ["h1"]);

        let thicker = json!({ "id": "e1", "type": "extrude", "sketch": "s1", "distance": 5 });
        let (d, c) = apply(&d, &args(json!({ "features": [thicker], "remove": ["h1"] }))).expect("applies");
        assert_eq!((c.changed.as_slice(), c.removed.as_slice()), (&["e1".to_string()][..], &["h1".to_string()][..]));
        assert_eq!(d.features.len(), 2);
    }

    #[test]
    fn replace_all_reports_what_went() {
        let only_sketch = plate().features[0].clone();
        let v = json!({ "features": [only_sketch], "replace_all": true });
        let (d, c) = apply(&plate(), &args(v)).expect("applies");
        assert_eq!(d.features.len(), 1);
        assert_eq!(c.removed, ["e1"]);
    }

    #[test]
    fn bad_proposals_are_explained() {
        let err = |v: Value| apply(&plate(), &args(v)).unwrap_err();
        assert!(err(json!({})).contains("doesn't change anything"));
        assert!(err(json!({ "remove": ["zz"] })).contains("no feature"));
        assert!(err(json!({ "features": [{ "id": "x", "type": "blob" }] })).contains("feature 1 (id \"x\")"));
        let hole: Value = serde_json::from_str(HOLE).expect("json");
        assert!(err(json!({ "features": [hole.clone(), hole] })).contains("twice"));
        // Validation problems come back with the feature's id.
        let stray = json!({ "id": "h9", "type": "hole", "on": "nope", "diameter": 4, "at": [[5, 5]] });
        assert!(err(json!({ "features": [stray] })).contains("h9:"));
    }

    #[test]
    fn propose_then_accept() {
        let app = shared(plate());
        let hole: Value = serde_json::from_str(HOLE).expect("json");
        let reply = handle(&app, "propose_features", json!({ "features": [hole], "description": "a hole" })).expect("ok");
        assert_eq!(reply["proposal_id"], 1);
        assert_eq!(reply["measurements"]["bodies"], 1);
        // Nothing changes until the user accepts.
        assert_eq!(lock(&app).design.features.len(), 2);
        assert_eq!(handle(&app, "get_design", Value::Null).expect("ok")["pending_proposal"], 1);

        lock(&app).accept().expect("accepts");
        assert_eq!(lock(&app).design.features.len(), 3);
        let s = handle(&app, "proposal_status", json!({ "proposal_id": 1 })).expect("ok");
        assert_eq!(s["status"], "accepted");
    }

    #[test]
    fn a_new_proposal_replaces_a_waiting_one_and_rejection_notes_reach_claude() {
        let app = shared(plate());
        let rename = |n: &str| handle(&app, "propose_features", json!({ "name": n })).expect("ok");
        rename("A");
        rename("B");
        let s = handle(&app, "proposal_status", json!({ "proposal_id": 1 })).expect("ok");
        assert_eq!(s["status"], "superseded");

        lock(&app).reject("wrong name").expect("rejects");
        let s = handle(&app, "proposal_status", Value::Null).expect("ok");
        assert_eq!((s["status"].as_str(), s["user_note"].as_str()), (Some("rejected"), Some("wrong name")));
        assert!(lock(&app).accept().is_err());
    }

    #[test]
    fn status_waits_for_the_user() {
        let app = shared(plate());
        handle(&app, "propose_features", json!({ "name": "X" })).expect("ok");
        let app2 = app.clone();
        let user = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            lock(&app2).accept().expect("accepts");
        });
        let s = handle(&app, "proposal_status", json!({ "wait_seconds": 10 })).expect("ok");
        assert_eq!(s["status"], "accepted");
        user.join().expect("joins");
    }

    /// Regression: a frozen console (QuickEdit selection) used to hang every bridge call.
    #[test]
    fn a_frozen_console_does_not_block_claude() {
        let app = shared(plate());
        let frozen = crate::say::WRITING.lock().unwrap_or_else(|e| e.into_inner());
        let (tx, rx) = std::sync::mpsc::channel();
        let app2 = app.clone();
        std::thread::spawn(move || {
            let p = handle(&app2, "propose_features", json!({ "name": "Y", "description": "rename" }));
            let s = handle(&app2, "proposal_status", json!({}));
            let d = handle(&app2, "get_design", json!({}));
            let _ = tx.send((p.is_ok(), s.is_ok(), d.is_ok()));
        });
        let answers = rx.recv_timeout(Duration::from_secs(30)).expect("bridge calls finish while the console is frozen");
        assert_eq!(answers, (true, true, true));
        // The console (here, this test) can still take the lock to accept.
        assert!(lock(&app).accept().is_ok());
        drop(frozen);
    }

    #[test]
    fn exports_stay_in_the_export_folder() {
        assert_eq!(check_file_name("part.STL"), Ok(Format::Stl));
        assert_eq!(check_file_name("part.stp"), Ok(Format::Step));
        for bad in ["", "../x.stl", "C:\\x.stl", "a/b.stl", ".stl", "part.obj", "part"] {
            assert!(check_file_name(bad).is_err(), "{bad:?} should be refused");
        }

        let app = shared(plate());
        let name = format!("aad_app_export_test_{}.stl", std::process::id());
        let reply = handle(&app, "export", json!({ "file_name": name })).expect("exports");
        let path = PathBuf::from(reply["path"].as_str().expect("path"));
        assert!(path.exists());
        std::fs::remove_file(path).expect("cleans up");
    }

    #[test]
    fn measuring_the_design() {
        let m = handle(&shared(plate()), "measure", Value::Null).expect("measures");
        assert_eq!(m["bounding_box_mm"]["size"], json!([40.0, 20.0, 3.0]));
        assert!((m["volume_mm3"].as_f64().expect("number") - 2400.0).abs() < 1.0);
    }
}
