//! The AiAidedDesign app. For now (phase 3, docs/SPEC.md) it runs in a console: it holds
//! the design, lets Claude reach it through aad-bridge, and asks you to accept or reject
//! each change Claude proposes. The window comes in phase 4.
//!
//! Try it: `cargo run -p aad-app -- examples/hinge_leaf.aad.json`
//! (a file that doesn't exist yet starts an empty design, saved there on the first accept).

mod control;

use aad_model::Design;
use control::{lock, App, Shared};
use std::io::BufRead;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let file = std::env::args_os().nth(1).map(PathBuf::from);
    let design = match &file {
        Some(p) if p.exists() => Design::load(p)?,
        Some(p) => empty_design(&file_stem(p)),
        None => empty_design("Untitled"),
    };
    let problems = design.validate();
    for p in &problems {
        println!("warning: {}: {}", p.feature, p.message);
    }

    let file = file.map(|p| std::path::absolute(&p).unwrap_or(p));
    let export_dir = match file.as_ref().and_then(|f| f.parent()) {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    let app: Shared = Arc::new(Mutex::new(App::new(design, file, export_dir)));

    // Open the door for the bridge: a localhost port and a token, written to the session file.
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("can't open a local port: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let session = aad_link::Session { port, token: aad_link::new_token(), pid: std::process::id() };
    let session_file = aad_link::session_path();
    session.write(&session_file)?;
    let for_bridge = app.clone();
    aad_link::serve(
        listener,
        session.token.clone(),
        Arc::new(move |cmd: &str, args| control::handle(&for_bridge, cmd, args)),
    );

    {
        let a = lock(&app);
        println!("AiAidedDesign: {:?}, {} features", a.design.name, a.design.features.len());
        match &a.file {
            Some(f) => println!("design file: {}", f.display()),
            None => println!("no design file given: accepted changes won't be saved"),
        }
    }
    println!("waiting for Claude (type help for commands)");
    console(&app);

    // Tidy up, unless a newer app has taken over the session file.
    if aad_link::Session::read(&session_file).is_ok_and(|s| s == session) {
        let _ = std::fs::remove_file(&session_file);
    }
    Ok(())
}

fn empty_design(name: &str) -> Design {
    Design { name: name.to_string(), units: "mm".into(), features: Vec::new() }
}

/// "bracket" from "bracket.aad.json".
fn file_stem(path: &Path) -> String {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("Untitled");
    name.trim_end_matches(".json").trim_end_matches(".aad").to_string()
}

const HELP: &str = "\
  a, accept         accept Claude's proposal
  r, reject [why]   reject it (the reason is passed back to Claude)
  show              list the design's features
  quit              close the app";

/// Read your commands until you quit (or the console closes).
fn console(app: &Shared) {
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
        let mut a = lock(app);
        let outcome = match cmd {
            "" => continue,
            "a" | "accept" => a.accept(),
            "r" | "reject" => a.reject(rest.trim()),
            "show" => Ok(describe(&a.design)),
            "help" | "?" => Ok(HELP.to_string()),
            "q" | "quit" | "exit" => break,
            other => Err(format!("unknown command {other:?} (type help)")),
        };
        match outcome {
            Ok(msg) => println!("{msg}"),
            Err(e) => println!("{e}"),
        }
    }
}

fn describe(design: &Design) -> String {
    let mut out = format!("{:?}:", design.name);
    if design.features.is_empty() {
        out.push_str(" (empty)");
    }
    for f in &design.features {
        let json = serde_json::to_string(f).unwrap_or_default();
        out.push_str(&format!("\n  {json}"));
    }
    out
}
