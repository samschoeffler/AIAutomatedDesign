//! Building designs into meshes on a background thread, so the window never waits for the
//! kernel (a build can take a moment, and dragging a number asks for one every frame).
//!
//! A [`Builder`] runs at most one build at a time. If more designs are asked for meanwhile,
//! only the newest is kept and built next: the in-between ones are already out of date.

use super::gpu::{LineVertex, MeshData, SolidVertex};
use aad_model::Design;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;

/// A built design, ready to draw.
pub struct Built {
    pub mesh: Arc<MeshData>,
    /// Min and max corner, in mm. `None` for an empty design.
    pub bounds: Option<(glam::Vec3, glam::Vec3)>,
    /// Size, volume and so on, as Claude's `measure` tool gives them.
    pub measurements: Value,
}

pub struct Builder {
    /// The design being built now, and where its result will arrive.
    running: Option<(Design, Receiver<Result<Built, String>>)>,
    /// The next design to build.
    queued: Option<Design>,
    /// The design the latest result is for, and the result.
    done: Option<(Design, Result<Arc<Built>, String>)>,
    /// The latest build that worked. Stays on screen when a later one fails.
    good: Option<Arc<Built>>,
}

impl Builder {
    pub fn new() -> Builder {
        Builder { running: None, queued: None, done: None, good: None }
    }

    /// Ask for `design` to be shown. Does nothing if it's already built or on its way.
    /// `wake` is called from the build thread when the result is ready.
    pub fn want(&mut self, design: &Design, wake: impl Fn() + Send + 'static) {
        let already = |d: Option<&Design>| d == Some(design);
        if already(self.done.as_ref().map(|d| &d.0)) && self.running.is_none() {
            return;
        }
        if already(self.running.as_ref().map(|r| &r.0)) {
            self.queued = None; // back to what's already building
            return;
        }
        if self.running.is_some() {
            self.queued = Some(design.clone());
        } else {
            self.start(design.clone(), wake);
        }
    }

    /// Pick up a finished build, and start the queued one. Call once per frame.
    pub fn poll(&mut self, wake: impl Fn() + Send + 'static) {
        let Some((_, rx)) = &self.running else { return };
        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("the build stopped unexpectedly".into()),
        };
        let (design, _) = self.running.take().expect("checked above");
        let result = result.map(Arc::new);
        if let Ok(built) = &result {
            self.good = Some(built.clone());
        }
        self.done = Some((design, result));
        if let Some(next) = self.queued.take() {
            self.start(next, wake);
        }
    }

    /// The latest finished build, and the design it's for (tests look at this).
    #[cfg(test)]
    pub fn latest(&self) -> Option<(&Design, &Result<Arc<Built>, String>)> {
        self.done.as_ref().map(|(d, r)| (d, r))
    }

    /// The latest good build (a failed build leaves the last good one on screen).
    pub fn shown(&self) -> Option<&Arc<Built>> {
        self.good.as_ref()
    }

    /// Why the latest build failed, if it did.
    pub fn error(&self) -> Option<&str> {
        self.done.as_ref().and_then(|(_, r)| r.as_ref().err()).map(String::as_str)
    }

    /// True while a build is running.
    pub fn busy(&self) -> bool {
        self.running.is_some()
    }

    fn start(&mut self, design: Design, wake: impl Fn() + Send + 'static) {
        let (tx, rx) = channel();
        let to_build = design.clone();
        std::thread::spawn(move || {
            let _ = tx.send(build(&to_build));
            wake();
        });
        self.running = Some((design, rx));
    }
}

/// Every mesh gets its own number, so the GPU knows when to replace its copy.
pub fn next_mesh_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Build one design into a mesh (on the background thread, or for the render tool).
pub fn build(design: &Design) -> Result<Built, String> {
    if design.features.iter().all(|f| f.is_suppressed()) {
        return Ok(Built {
            mesh: Arc::new(MeshData { id: next_mesh_id(), ..Default::default() }),
            bounds: None,
            measurements: Value::Null,
        });
    }
    let problems = design.validate();
    if let Some(p) = problems.first() {
        return Err(format!("{}: {}", p.feature, p.message));
    }
    let part = aad_kernel::build(design).map_err(|e| e.to_string())?;
    let display = part.display_mesh();

    let triangles: Vec<SolidVertex> = display
        .triangles
        .iter()
        .flatten()
        .map(|c| SolidVertex { position: c.position, normal: c.normal })
        .collect();
    let white = [1.0; 4]; // edges are tinted when drawn
    let mut lines = Vec::new();
    for edge in &display.edges {
        for pair in edge.windows(2) {
            lines.push(LineVertex { position: pair[0], color: white });
            lines.push(LineVertex { position: pair[1], color: white });
        }
    }

    let bounds = triangles.iter().fold(None, |acc: Option<(glam::Vec3, glam::Vec3)>, v| {
        let p = glam::Vec3::from(v.position);
        Some(acc.map_or((p, p), |(lo, hi)| (lo.min(p), hi.max(p))))
    });
    Ok(Built {
        mesh: Arc::new(MeshData { id: next_mesh_id(), triangles, lines }),
        bounds,
        measurements: crate::control::measurements(&part),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn plate(thickness: f64) -> Design {
        Design::from_json(&format!(
            r#"{{ "name": "Plate", "features": [
            {{ "id": "s1", "type": "sketch", "plane": "XY", "entities": [ {{ "type": "rect", "x0": 0, "y0": 0, "x1": 40, "y1": 20 }} ] }},
            {{ "id": "e1", "type": "extrude", "sketch": "s1", "distance": {thickness} }} ] }}"#
        ))
        .expect("loads")
    }

    fn wait_until_idle(b: &mut Builder) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while b.busy() {
            assert!(Instant::now() < deadline, "build took too long");
            std::thread::sleep(Duration::from_millis(10));
            b.poll(|| {});
        }
    }

    #[test]
    fn builds_in_the_background_and_keeps_only_the_newest_request() {
        let mut b = Builder::new();
        b.want(&plate(3.0), || {});
        b.want(&plate(4.0), || {});
        b.want(&plate(5.0), || {}); // replaces 4 in the queue
        wait_until_idle(&mut b);
        let (design, result) = b.latest().expect("built");
        assert_eq!(design, &plate(5.0));
        let built = result.as_ref().expect("builds");
        let (lo, hi) = built.bounds.expect("has a size");
        assert!((hi - lo - glam::vec3(40.0, 20.0, 5.0)).abs().max_element() < 0.01);
        assert_eq!(built.mesh.lines.len(), 24, "12 edges, 2 points each");
    }

    #[test]
    fn a_failed_build_keeps_the_last_good_mesh() {
        let mut b = Builder::new();
        b.want(&plate(3.0), || {});
        wait_until_idle(&mut b);
        b.want(&plate(0.0), || {}); // zero thickness doesn't validate
        wait_until_idle(&mut b);
        assert!(b.error().expect("failed").contains("e1"), "the error names the feature");
        let (lo, hi) = b.shown().expect("the old mesh stays").bounds.expect("has a size");
        assert!((hi.z - lo.z - 3.0).abs() < 0.01);
    }
}
