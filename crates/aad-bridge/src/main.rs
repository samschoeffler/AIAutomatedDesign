//! The MCP bridge (phase 3, docs/SPEC.md). Claude Code starts this program and talks MCP
//! (JSON-RPC, one message per line) to it over stdin/stdout. Each tool call is passed on to
//! the running AiAidedDesign app through aad-link. The bridge keeps no state of its own.
//!
//! Add it to Claude Code: `claude mcp add aad -- <path to>\aad-bridge.exe`

use serde_json::{json, Value};
use std::io::{BufRead, Write};

/// The MCP version we speak if the client doesn't say which it wants.
const PROTOCOL_VERSION: &str = "2025-06-18";

const INSTRUCTIONS: &str = "Tools for AiAidedDesign, a CAD app for FIRST Robotics parts. \
The user's open design is a list of features (sketch, extrude, hole), sizes in mm. \
Read it with get_design, change it with propose_features (the user accepts or rejects in the app), \
then wait with proposal_status. Use render to look at the part (or a waiting proposal) and check your work.";

const FEATURE_FORMAT: &str = "Feature format (all sizes in mm):
- sketch: {\"id\", \"type\":\"sketch\", \"plane\":<plane>, \"offset\":<mm, optional>, \"entities\":[{\"type\":\"rect\",\"x0\",\"y0\",\"x1\",\"y1\"} | {\"type\":\"circle\",\"cx\",\"cy\",\"r\"}]}. A shape fully inside another becomes a hole in it.
  <plane> is \"XY\" (sketch x,y = world X,Y; normal +Z), \"XZ\" (x,y = X,Z; normal -Y) or \"YZ\" (x,y = Y,Z; normal +X), \
or {\"on\":<extrude id>, \"side\":\"end\"|\"start\"} for the face that extrude ends on (default) or starts from. \
A face sketch uses the same x/y/normal as that extrude's sketch, so coordinates line up and it moves with the extrude; \
a positive distance keeps going the way that extrude went (use a negative one to go back out of a start face). \
\"offset\" moves the sketch along the normal. Put a boss on top of a plate with plane {\"on\":\"plate\"} and a join extrude.
- extrude: {\"id\", \"type\":\"extrude\", \"sketch\":<sketch id>, \"distance\", \"op\":\"new\"|\"join\"|\"cut\"}. A negative distance goes the other way.
- hole: {\"id\", \"type\":\"hole\", \"on\":<extrude id>, \"diameter\", \"at\":[[x,y],...], \"depth\":\"through\"|<mm>}. Positions are in the sketch plane of `on`.
Any feature may have \"suppressed\": true. A feature may only refer to features before it.
Kernel limits: a cut flush with a side face fails (overlap the edge or stop short of it); a join that \
can't be fused (for example one hanging over an edge) is kept as an overlapping piece, which measure notes.";

/// The tools Claude sees.
fn tools() -> Value {
    json!([
        {
            "name": "get_design",
            "description": "The user's current (accepted) design: its name, feature list and file, plus the id of any proposal still waiting for the user.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "propose_features",
            "description": format!("Propose a change to the design. The user sees it in the AiAidedDesign window and accepts or rejects it; \
nothing changes until they accept. A proposal applies to the current accepted design (get_design), and replaces any proposal still waiting. \
By default features merge by id: a feature with an existing id replaces it in place, a new id goes on the end, and ids in `remove` are deleted. \
With replace_all, `features` becomes the whole list. The result is validated and built first; problems come back as an error and the user isn't asked.\n\n{FEATURE_FORMAT}"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "features": { "type": "array", "items": { "type": "object" }, "description": "Features to add or replace (format in the tool description)." },
                    "remove": { "type": "array", "items": { "type": "string" }, "description": "Ids of features to delete." },
                    "replace_all": { "type": "boolean", "description": "Make `features` the whole feature list." },
                    "name": { "type": "string", "description": "A new name for the design." },
                    "description": { "type": "string", "description": "One line for the user saying what this change does." }
                }
            }
        },
        {
            "name": "proposal_status",
            "description": "Whether the user accepted, rejected (maybe with a note saying why) or is still deciding on a proposal. Set wait_seconds to wait for their answer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "proposal_id": { "type": "integer", "description": "Defaults to the latest proposal." },
                    "wait_seconds": { "type": "integer", "minimum": 0, "maximum": 60, "description": "Wait up to this long for the user to decide (default 0)." }
                }
            }
        },
        {
            "name": "measure",
            "description": "Bounding box (min, max, size), volume and body count, in mm. Measures the accepted design, or the waiting proposal.",
            "inputSchema": {
                "type": "object",
                "properties": { "target": { "type": "string", "enum": ["design", "proposal"] } }
            }
        },
        {
            "name": "export",
            "description": "Export the accepted design as STL (for 3D printing) or STEP (for other CAD apps), chosen by the file extension. \
Give only a file name such as \"bracket.step\"; it is written next to the user's design file.",
            "inputSchema": {
                "type": "object",
                "properties": { "file_name": { "type": "string", "description": "Ends in .stl, .step or .stp." } },
                "required": ["file_name"]
            }
        },
        {
            "name": "render",
            "description": "A picture of the part, to check your work: by default four views in one image (iso, top, front, right). \
Shows the waiting proposal if there is one (blue, with the current part's edges in orange), otherwise the accepted design (grey). \
The grid is on the XY plane in 10 mm squares; the X axis is red, Y green, Z blue (up).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "view": { "type": "string", "enum": ["all", "iso", "top", "front", "right"], "description": "Default all." },
                    "target": { "type": "string", "enum": ["design", "proposal"], "description": "Default: the proposal if one is waiting, else the design." },
                    "size": { "type": "integer", "minimum": 200, "maximum": 2000, "description": "Picture width and height in pixels (default 900)." }
                }
            }
        }
    ])
}

/// The app sends a picture as base64 PNG under this key; it goes to Claude as an MCP image.
const IMAGE_KEY: &str = "image_png_base64";

/// How a tool call reaches the app (a real connection, or a stand-in in tests).
type AppCall<'a> = &'a dyn Fn(&str, Value) -> Result<Value, String>;

fn main() {
    let session_file = aad_link::session_path();
    let app = |cmd: &str, args: Value| aad_link::call(&session_file, cmd, args);
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = handle_message(&line, &app) {
            if writeln!(out, "{reply}").and_then(|_| out.flush()).is_err() {
                break;
            }
        }
    }
}

/// Answer one JSON-RPC message. Notifications (no `id`) and replies from the client get no answer.
fn handle_message(line: &str, app: AppCall) -> Option<Value> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return Some(rpc_error(Value::Null, -32700, &format!("parse error: {e}"))),
    };
    let id = msg.get("id").cloned()?;
    let method = msg.get("method").and_then(Value::as_str)?;
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(json!(PROTOCOL_VERSION)),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "aad-bridge", "version": env!("CARGO_PKG_VERSION") },
            "instructions": INSTRUCTIONS,
        }),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => call_tool(&params, app),
        other => return Some(rpc_error(id, -32601, &format!("unknown method {other:?}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// Run a tool. Failures are tool results with `isError`, so Claude reads the message.
fn call_tool(params: &Value, app: AppCall) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let known = tools().as_array().is_some_and(|t| t.iter().any(|t| t["name"] == name));
    let outcome = if known { app(name, args) } else { Err(format!("there's no tool called {name:?}")) };
    match outcome {
        Ok(mut v) => {
            // A picture goes first as an image; the rest of the answer follows as text.
            let image = v.as_object_mut().and_then(|o| o.remove(IMAGE_KEY));
            let mut content = Vec::new();
            if let Some(Value::String(data)) = image {
                content.push(json!({ "type": "image", "data": data, "mimeType": "image/png" }));
            }
            content.push(json!({ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }));
            json!({ "content": content, "isError": false })
        }
        Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
    }
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closed(_: &str, _: Value) -> Result<Value, String> {
        Err(aad_link::NOT_OPEN.to_string())
    }

    fn ask(msg: Value, app: AppCall) -> Option<Value> {
        handle_message(&msg.to_string(), app)
    }

    #[test]
    fn handshake_and_tool_list() {
        let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-03-26" } });
        let reply = ask(init, &closed).expect("answers");
        assert_eq!(reply["result"]["protocolVersion"], "2025-03-26");
        assert!(reply["result"]["capabilities"]["tools"].is_object());

        assert_eq!(ask(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }), &closed), None);

        let list = ask(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }), &closed).expect("answers");
        let names: Vec<&str> =
            list["result"]["tools"].as_array().expect("array").iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names, ["get_design", "propose_features", "proposal_status", "measure", "export", "render"]);
    }

    #[test]
    fn pictures_go_to_claude_as_images() {
        let picture = |_: &str, _: Value| Ok(json!({ IMAGE_KEY: "iVBORw0K", "description": "four views" }));
        let call = json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": { "name": "render" } });
        let reply = ask(call, &picture).expect("answers");
        let content = &reply["result"]["content"];
        assert_eq!(content[0], json!({ "type": "image", "data": "iVBORw0K", "mimeType": "image/png" }));
        let text = content[1]["text"].as_str().expect("text");
        assert!(text.contains("four views") && !text.contains("iVBOR"), "the text part leaves the picture out: {text}");
    }

    #[test]
    fn tool_calls_reach_the_app() {
        let echo = |cmd: &str, args: Value| Ok(json!({ "cmd": cmd, "args": args }));
        let call = json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                           "params": { "name": "measure", "arguments": { "target": "design" } } });
        let reply = ask(call, &echo).expect("answers");
        assert_eq!(reply["result"]["isError"], false);
        let text: Value = serde_json::from_str(reply["result"]["content"][0]["text"].as_str().expect("text")).expect("json");
        assert_eq!(text, json!({ "cmd": "measure", "args": { "target": "design" } }));
    }

    #[test]
    fn problems_come_back_as_tool_errors() {
        let call = |name: &str| json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": name } });
        let reply = ask(call("get_design"), &closed).expect("answers");
        assert_eq!(reply["result"]["isError"], true);
        assert_eq!(reply["result"]["content"][0]["text"], aad_link::NOT_OPEN);

        let reply = ask(call("delete_everything"), &|_, _| panic!("must not reach the app")).expect("answers");
        assert_eq!(reply["result"]["isError"], true);

        let reply = ask(json!({ "jsonrpc": "2.0", "id": 5, "method": "nope" }), &closed).expect("answers");
        assert_eq!(reply["error"]["code"], -32601);
    }
}
