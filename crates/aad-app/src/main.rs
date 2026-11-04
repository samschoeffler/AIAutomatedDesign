//! The AiAidedDesign window (phase 4, docs/SPEC.md): viewer, feature tree, sketcher.
//! For now it only loads a design file and reports whether it's valid.
//!
//! Try it: `cargo run -p aad-app -- examples/hinge_leaf.aad.json`

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: AiAidedDesign <design.aad.json>");
        return;
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return eprintln!("can't read {path}: {e}"),
    };
    let design = match aad_model::Design::from_json(&text) {
        Ok(d) => d,
        Err(e) => return eprintln!("{path} isn't a valid design file: {e}"),
    };
    let problems = design.validate();
    if problems.is_empty() {
        println!("{:?}: {} features, all valid", design.name, design.features.len());
    } else {
        for p in problems {
            println!("{}: {}", p.feature, p.message);
        }
    }
}
