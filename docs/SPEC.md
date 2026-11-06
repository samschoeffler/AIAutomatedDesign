# AiAidedDesign: design spec (v0)

A Windows CAD app where you design parts by talking to Claude in Claude Code,
and can also edit them by hand. Built in Rust, written to be readable by
someone new to Rust.

## Decisions so far

| Topic | Decision |
|---|---|
| Codebase | Fresh project on the same stack as SolidCraft (Rust, eframe/egui, wgpu, JSON commands). No SolidCraft code is copied. |
| Purpose | FIRST Robotics parts; also a learning project, so clarity beats cleverness. |
| Geometry kernel | [Truck](https://github.com/ricosjp/truck), pure Rust B-rep. No 3D edge fillets for now (2D sketch corner radii are fine). |
| AI role | Text → whole part. Claude builds a complete parametric part from a description. |
| AI connection | MCP. Claude Code launches a small **bridge** exe (stdio MCP) that forwards to the running app over localhost. |
| Part format | JSON **feature list**: ordered, schema-checked steps. This is the file format and the feature tree. |
| Units | Millimetres only. |
| Platform | Windows only for v1. |
| App window | 3D viewer + feature tree + **full interactive sketching** (draw with the mouse, constraints). |
| AI edits | **Preview, then approve.** Claude's changes appear as a ghost until you click Accept. |
| AI vision | `render` tool: Claude can ask for a picture of the part to check its work. |
| Exports | STEP, STL, SVG (flat plate outlines for laser/router). |
| Built-in robotics features | Hole patterns (grid, bolt circle); bearing bores and 1/2 in hex bores. |

## How the pieces fit

```
Claude Code ──stdio MCP──▶ aad-bridge.exe ──localhost JSON──▶ AiAidedDesign.exe
                                                              ├─ aad-model   (feature list, validation)
                                                              ├─ aad-kernel  (feature list → Truck solid)
                                                              ├─ aad-export  (STEP / STL / SVG)
                                                              └─ aad-app     (window: viewer, tree, sketcher)
```

- **One source of truth:** the app holds the design. The bridge has no state; it
  only translates MCP tool calls into app commands. If the app isn't open, the
  bridge tells Claude to ask you to open it.
- **Rebuild on change:** any edit (from Claude or you) changes the feature list;
  the kernel rebuilds the solid from scratch. Simple and predictable; fine for
  robot-part sizes.

## The feature list

A design is a JSON file (`.aad.json`):

```json
{
  "name": "Hinge leaf",
  "units": "mm",
  "features": [
    { "id": "s1", "type": "sketch", "plane": "XY",
      "entities": [ { "type": "rect", "x0": 0, "y0": 0, "x1": 40, "y1": 100 } ] },
    { "id": "e1", "type": "extrude", "sketch": "s1", "distance": 3, "op": "new" },
    { "id": "h1", "type": "hole", "on": "e1", "diameter": 4,
      "at": [[5, 5], [35, 5], [5, 95], [35, 95]], "depth": "through" }
  ]
}
```

v1 feature types: `sketch` (line, rect, circle, arc, polygon, corner radius),
`extrude` (new / join / cut), `revolve`, `hole`, `hole_pattern` (grid, bolt
circle), `bore` (bearing, 1/2 in hex), `linear_pattern`, `circular_pattern`,
`mirror`, `boolean`.

Every feature has an `id`, can be **suppressed**, and is checked against a
schema before it reaches the kernel, so a bad AI step gives Claude a clear error
instead of a broken model.

## MCP tools (bridge)

| Tool | What it does |
|---|---|
| `get_design` | The current feature list. |
| `propose_features` | Claude proposes a full or partial feature list. It shows as a **ghost** in the app until you Accept or Reject. |
| `proposal_status` | Whether you accepted, rejected or are still deciding. |
| `measure` | Bounding box, volume, distances between named features. |
| `render` | PNG of the part (iso, top, front, side) so Claude can check its work. |
| `export` | STEP / STL / SVG to a path you allow. |

## The app window

- **Viewer:** orbit/pan/zoom, standard views, ghost overlay for proposals.
- **Feature tree:** every feature with editable numbers; reorder, suppress,
  delete; add a feature from a menu with a form.
- **Sketcher:** draw lines, arcs, circles, rectangles; constraints (coincident,
  horizontal, vertical, parallel, perpendicular, equal, tangent, distance,
  radius, angle). Needs a 2D constraint solver. **Open question** (below).
- **Proposal bar:** Accept / Reject when Claude has a change waiting.
- **Undo/redo** for everything.

## Build phases

1. **Model.** Feature list types, JSON load/save, validation. *(done for sketch / extrude / hole; other v1 feature types arrive with the phases that need them)*
2. **Kernel.** Sketch → extrude/cut/hole with Truck; STL + STEP export. *(done: `aad-kernel`, `aad-export`; try it with `cargo run -p aad-export --example export -- examples/bracket.aad.json`)*
3. **Bridge + control.** MCP stdio bridge, localhost protocol, propose/accept. *(done: `aad-link`, `aad-bridge`, and `aad-app` as a console app that asks you to accept or reject each proposal; `render` waits for phase 5. See "Trying the bridge" below)*
4. **Window.** Viewer, feature tree with number editing, ghost previews.
5. **Render tool.** Offscreen render to PNG for Claude.
6. **Robotics features.** Hole patterns, bearing and hex bores; SVG plate export.
7. **Sketcher.** Interactive drawing + constraint solver. Biggest phase.

## Open questions (to settle before each phase)

- **Constraint solver** for the sketcher: write our own (Newton's method, good
  learning project) or bind SolveSpace's proven `libslvs` (C library, more build
  setup)?
- **Localhost security:** *settled in phase 3.* The app writes its port and a random
  token to `%LOCALAPPDATA%\AiAidedDesign\session.json`; requests without the token
  are refused, so only the bridge (running as you) can drive the app.
- **Ghost previews** for edits that change existing features: show the before
  and after, or only the after?
- **Hole sizes:** which clearance table for M3/M4/M5 and #10 / 1/4 in screws?
- **Hex bore tolerance** for 1/2 in hex shaft (printed vs machined)?
- **Joins that Truck can't fuse.** Truck's booleans fail on shared flat faces, and
  since every sketch sits on an origin plane, a joined boss almost always shares the
  plate's bottom face. For now such a join stays as an overlapping piece of the same
  body (fine for STL and printing; STEP gets two overlapping solids; `volume` counts the
  overlap twice). Options: sketch planes with an offset, a fix upstream in Truck, or
  another kernel. The `monstertruck` fork was tried (Nov 2026): it handles flush cuts
  but still fails these joins, and returns an empty solid for a cut that misses.
- **Cuts flush with a side face** (a notch at the plate's edge) still fail in Truck;
  the error tells Claude to overlap the edge or stop short of it.

## Setting up Rust on Windows (you don't have it yet)

1. Install **Visual Studio Build Tools** with the "Desktop development with C++"
   workload (Rust needs its linker).
2. Install Rust from <https://rustup.rs> (run `rustup-init.exe`, accept defaults).
3. Open a new terminal and check: `cargo --version`.
4. In the project folder: `cargo test` builds and runs the tests.

## Trying the bridge (phase 3)

1. Build: `cargo build -p aad-app -p aad-bridge`.
2. Tell Claude Code about the bridge (once):
   `claude mcp add aad -- C:\Users\pizza\AIAutomatedDesign\target\debug\aad-bridge.exe`
3. Start the app on a design file (it's created on the first accept if it doesn't exist):
   `cargo run -p aad-app -- my_part.aad.json`
4. Ask Claude for a part. Each change shows up in the app's console; type `a` to
   accept or `r <reason>` to reject. Accepted changes are saved to the design file,
   and exports go into the same folder.

Proposals merge by feature id (same id replaces, new id is appended, `remove` deletes),
or replace the whole list with `replace_all`. A proposal is validated and built before
you see it, so a broken one goes straight back to Claude as an error.
