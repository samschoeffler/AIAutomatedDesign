//! The localhost link between the bridge and the app (docs/SPEC.md, "How the pieces fit").
//!
//! - The app listens on `127.0.0.1` on a port the OS picks, and writes a **session file**
//!   holding that port and a random **token**.
//! - The bridge reads the session file, connects, and sends one JSON request per line:
//!   `{"token": "...", "command": "get_design", "args": {...}}`.
//! - The app answers each with one line: `{"ok": <result>}` or `{"error": "<message>"}`.
//!
//! Only programs that can read the session file (in your own user folder) know the token,
//! so other users and web pages can't drive the app.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// What the bridge tells Claude when the app isn't running.
pub const NOT_OPEN: &str = "AiAidedDesign isn't open. Ask the user to start it \
(for example `AiAidedDesign my_part.aad.json`), then try again.";

/// How long the bridge waits for one answer. Long enough for a slow build plus
/// `proposal_status` waiting the full minute it's allowed.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(180);

/// Where the running app can be found, as written to the session file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub port: u16,
    pub token: String,
    /// The app's process id, so a stale file is easy to recognise.
    pub pid: u32,
}

/// The session file's path: `$AAD_SESSION` if set, otherwise
/// `%LOCALAPPDATA%\AiAidedDesign\session.json` (or the temp folder if that isn't set).
pub fn session_path() -> PathBuf {
    if let Some(p) = std::env::var_os("AAD_SESSION") {
        return p.into();
    }
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("AiAidedDesign").join("session.json")
}

impl Session {
    pub fn write(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(self).unwrap_or_default();
        std::fs::write(path, text).map_err(|e| format!("can't write {}: {e}", path.display()))
    }

    pub fn read(path: &Path) -> Result<Session, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("{} is damaged: {e}", path.display()))
    }
}

/// A fresh random token (128 bits, as hex).
///
/// `RandomState` is seeded from the operating system's random source, which is plenty for
/// a token that only guards a localhost port, and saves pulling in a random-number crate.
pub fn new_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    (0..2u8)
        .map(|i| {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u8(i);
            h.write_u128(nanos);
            h.write_u32(std::process::id());
            format!("{:016x}", h.finish())
        })
        .collect()
}

/// One request from the bridge.
#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub token: String,
    pub command: String,
    #[serde(default)]
    pub args: Value,
}

/// The app's answer: `{"ok": ...}` or `{"error": "..."}`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Ok(Value),
    Error(String),
}

/// Send one command to the running app and wait for its answer.
pub fn call(session_file: &Path, command: &str, args: Value) -> Result<Value, String> {
    let session = Session::read(session_file).map_err(|_| NOT_OPEN.to_string())?;
    let addr = ([127, 0, 0, 1], session.port).into();
    // A refused connection means the app closed (or crashed) and left its session file behind.
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).map_err(|_| NOT_OPEN.to_string())?;
    stream.set_read_timeout(Some(REPLY_TIMEOUT)).map_err(|e| e.to_string())?;

    let request = Request { token: session.token, command: command.to_string(), args };
    let line = serde_json::to_string(&request).map_err(|e| e.to_string())?;
    writeln!(stream, "{line}").and_then(|_| stream.flush()).map_err(|e| format!("couldn't reach the app: {e}"))?;

    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer).map_err(|e| format!("no answer from the app: {e}"))?;
    if answer.trim().is_empty() {
        return Err("the app closed the connection without answering".into());
    }
    match serde_json::from_str(&answer) {
        Ok(Reply::Ok(v)) => Ok(v),
        Ok(Reply::Error(e)) => Err(e),
        Err(e) => Err(format!("the app sent something unreadable: {e}")),
    }
}

/// What the app does with a command: `(command, args) -> result or error message`.
pub type Handler = dyn Fn(&str, Value) -> Result<Value, String> + Send + Sync;

/// Answer requests on `listener` in the background, one thread per connection.
/// Requests without the right token are refused.
pub fn serve(listener: TcpListener, token: String, handler: Arc<Handler>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (token, handler) = (token.clone(), handler.clone());
            std::thread::spawn(move || {
                let _ = serve_connection(stream, &token, &*handler);
            });
        }
    })
}

fn serve_connection(stream: TcpStream, token: &str, handler: &Handler) -> std::io::Result<()> {
    let mut writer = stream.try_clone()?;
    for line in BufReader::new(stream).lines() {
        let line = line?;
        let reply = match serde_json::from_str::<Request>(&line) {
            Err(e) => Reply::Error(format!("bad request: {e}")),
            Ok(req) if req.token != token => {
                Reply::Error("wrong token: the bridge doesn't match this app (restart the app)".into())
            }
            Ok(req) => match handler(&req.command, req.args) {
                Ok(v) => Reply::Ok(v),
                Err(e) => Reply::Error(e),
            },
        };
        writeln!(writer, "{}", serde_json::to_string(&reply).unwrap_or_default())?;
        writer.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_session_file(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("aad_link_test_{name}_{}.json", std::process::id()))
    }

    #[test]
    fn tokens_are_long_and_different() {
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }

    #[test]
    fn a_request_goes_there_and_back() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
        let port = listener.local_addr().expect("has an address").port();
        let token = new_token();
        serve(
            listener,
            token.clone(),
            Arc::new(|cmd: &str, args: Value| match cmd {
                "echo" => Ok(args),
                _ => Err(format!("unknown command {cmd}")),
            }),
        );

        let path = temp_session_file("roundtrip");
        Session { port, token, pid: std::process::id() }.write(&path).expect("writes");
        assert_eq!(call(&path, "echo", json!({"x": 1})), Ok(json!({"x": 1})));
        assert_eq!(call(&path, "nope", json!({})), Err("unknown command nope".into()));

        // Someone without the token is turned away.
        Session { port, token: "guess".into(), pid: 0 }.write(&path).expect("writes");
        assert!(call(&path, "echo", json!({})).unwrap_err().contains("wrong token"));
        std::fs::remove_file(&path).expect("cleans up");
    }

    #[test]
    fn no_app_means_a_clear_message() {
        let path = temp_session_file("missing");
        assert_eq!(call(&path, "get_design", json!({})), Err(NOT_OPEN.into()));

        // A session file left behind by a closed app: nothing listens on its port any more.
        let port = TcpListener::bind("127.0.0.1:0").expect("binds").local_addr().expect("addr").port();
        Session { port, token: new_token(), pid: 0 }.write(&path).expect("writes");
        assert_eq!(call(&path, "get_design", json!({})), Err(NOT_OPEN.into()));
        std::fs::remove_file(&path).expect("cleans up");
    }
}
