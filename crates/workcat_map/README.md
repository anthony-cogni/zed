# Workcat Map (spike)

A dock panel that renders ~100 randomly placed, draggable rectangles in
GPUI. This is the Zed-native half of the workcat twin client spikes
(workcat design record 005, ruling 15). It is deliberately minimal: no
statuses, no lenses, no data model.

## What it demonstrates

**Owned geometry**: node positions are application state. Mouse down on a
rect starts a gesture; window-level mouse events (registered from a canvas
overlay, the same pattern pane resize handles use) move it; mouse up
commits. At gesture rest the whole layout is written to disk on a
background executor: `layout.json` (atomic temp-file + rename) plus one
JSON-lines event appended to `events.jsonl`. A fresh layout is also saved
at first creation, so positions survive closing the panel and restarting
Zed.

**Talking to Zed's internals**: the first few nodes display real thread
titles resolved from Zed's local threads database, reusing the read-only
loader in the `agent_mission_control` crate (temp-copy of `threads.db`,
zstd + JSON decode, background executor).

**Living as a pane**: the map is a real `workspace::Panel` in the right
dock. It participates in Zed focus: clicking the field focuses it, the
field border and a header badge track focus state, and focus in/out is
logged (`workcat_map: focus in` / `focus out`).

## Storage

- Directory: `~/logs/workcat-map-spike/`, overridable with the
  `WORKCAT_MAP_DIR` environment variable.
- `layout.json`: `{ "version": 1, "nodes": [{ "id", "x", "y" }, ...] }`.
- `events.jsonl`: one JSON object per persist (`generated` or
  `drag_end`), with a timestamp.
- Thread database override: `MISSION_CONTROL_THREADS_DB` (inherited from
  the mission control store).

## Layout of the code

- `geometry.rs` — pure core: seeded layout generation (small LCG, no
  `rand` dependency), reconciliation of saved positions with the required
  node count, clamping. Unit-tested.
- `store.rs` — IO shell: load/save layout, append events. Unit-tested
  against temp dirs.
- `threads.rs` — thread-title resolution via `agent_mission_control`.
- `panel.rs` — the GPUI view and the `Panel` wrapper.

## Building and verifying

Build the whole app without full Xcode (Command Line Tools only):

```
cargo build --features gpui_platform/runtime_shaders
```

Run against a scratch project with an isolated data dir so a running Zed
instance is not disturbed:

```
./target/debug/zed --user-data-dir /tmp/zed-spike-data /tmp/zed-spike-proj
```

Then check `~/Library/Logs/Zed/Zed.log` for `workcat_map:` lines
(generated/restored layout, resolved thread titles, focus in/out) and
`~/logs/workcat-map-spike/` for the persisted files.

Crate-only checks:

```
cargo test -p workcat_map --features gpui_platform/runtime_shaders
cargo clippy -p workcat_map --features gpui_platform/runtime_shaders
```
