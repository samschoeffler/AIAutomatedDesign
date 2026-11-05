//! Build a design file and export it.
//!
//! `cargo run -p aad-export --example export -- examples/bracket.aad.json`
//! writes `bracket.stl` and `bracket.step` next to the input.

use aad_model::Design;
use std::path::Path;

fn main() -> Result<(), String> {
    let input = std::env::args().nth(1).ok_or("usage: export <design.aad.json>")?;
    let input = Path::new(&input);
    let design = Design::load(input)?;
    let part = aad_kernel::build(&design).map_err(|e| e.to_string())?;

    let stem = input.file_name().and_then(|n| n.to_str()).unwrap_or("part").trim_end_matches(".aad.json");
    let (stl, step) = (input.with_file_name(format!("{stem}.stl")), input.with_file_name(format!("{stem}.step")));
    aad_export::write_stl(&part, &stl)?;
    aad_export::write_step(&part, &design.name, &step)?;

    let (lo, hi) = part.bounding_box().ok_or("the part is empty")?;
    println!("{}: {} bodies, volume {:.0} mm³", design.name, part.bodies.len(), part.volume());
    println!("size {:.1} x {:.1} x {:.1} mm", hi.x - lo.x, hi.y - lo.y, hi.z - lo.z);
    if part.has_overlapping_pieces() {
        println!("note: a join was kept as an overlapping piece (volume counts the overlap twice)");
    }
    println!("wrote {} and {}", stl.display(), step.display());
    Ok(())
}
