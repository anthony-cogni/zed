# Agent Mission Control

A dock panel for Zed that triages your agent conversation threads so you can
reorient at a glance: which agents are running, which are waiting on you,
which are waiting on a merge, and which are done.

Zed stores every agent thread in a local SQLite database but gives you no
cross-thread overview. If you run many concurrent agent sessions, the cost of
"where was I?" grows with every thread. This panel reads that database and
answers the question inside the editor.

## What it shows

Threads are grouped into four triage states, in priority order:

**Needs you**: the final agent message contains a hard ask — a structured
trailer ("What I need from you: ..."), a direct question ("Should I ...?",
"Do you want ...", "yes/no"), or a review request ("please test"). The ask is
extracted verbatim and shown as the row's second line.

**Awaiting merge**: the final message asks for a PR merge or mentions
uncommitted work ("merge PR #134", "did not commit", "awaiting review").
This is a first-class state because it is the most common concrete ask in
practice.

**Running**: the last message is from the user (the agent has not replied
yet), or the agent's final message ends mid-work.

**Done**: a completed summary with no hard ask. Soft offers ("Let me know if
you'd like...", "Want me to...", "Say the word") do not demand attention —
the thread files under Done, but the offer text is kept as the second line so
the information is not lost.

Each row: status dot (theme status colors), thread title, ask/offer or
last-activity line, relative age, and message count. Clicking a row expands
the tail of the final agent message inline. The header shows totals, a
"Needs me" filter (NeedsYou + AwaitingMerge only), and a manual refresh
button. Data refreshes automatically every 30 seconds.

## Design notes: why these triage rules

The rules were derived from a 60-thread sample of real agent history, then
refined over two rounds against live renders:

- Boolean heuristics ("last message ends with `?`") caught only 6 of 19 real
  asks in the sample. Matching the structured trailer text directly measured
  roughly 3x better recall. The extractor scans for marker phrases with word
  boundaries, hard markers before soft ones, last occurrence first.
- Negations are respected: an ask beginning with "Nothing"/"None"/"No
  action" (e.g. "Nothing required. Optional follow-up: ...") does not count;
  triage falls back to other signals in the message.
- Markdown is stripped from extracted asks (`**What I need from you**:`
  yields a clean ask), captured lines are trimmed at sentence boundaries
  after a `?`, and leading punctuation is removed.
- Splitting hard asks from soft offers dropped the live "Needs you" count
  from 39/50 threads to 13/50 — the difference between a triage panel and a
  wall of red.

## Architecture

Pure core, effectful shell:

- `src/triage.rs` — pure functions: `triage()` state classification, ask
  extraction, relative-age formatting. All rules live here, covered by
  table-driven unit tests (9 tests). No IO.
- `src/store.rs` — IO shell: locates `threads.db` (macOS:
  `~/Library/Application Support/Zed/threads/threads.db`; override with the
  `MISSION_CONTROL_THREADS_DB` env var), copies it (plus `-wal`/`-shm`) to a
  tempdir and reads the copy — `sqlez` has no read-only open mode, and the
  real database must never be touched. Decompresses each thread's zstd blob,
  parses the JSON, and produces plain `ThreadDigest` values. Runs on the
  background executor.
- `src/panel.rs` — GUI: `MissionControlView` is a standalone, workspace-free
  `Render` entity holding the list, filter, and expansion state;
  `MissionControlPanel` wraps it as a `workspace::dock::Panel` (right dock,
  360px, `ToggleFocus` action). The view/panel split is deliberate: the view
  can be rendered headlessly with injected fixture data.
- `src/fixtures.rs` — realistic `ThreadDigest` fixtures covering all states,
  used by tests and the visual binary.
- `src/bin/visual.rs` — headless visual verification (below).

Thread messages are objects keyed by role (`{"User": {...}}` /
`{"Agent": {...}}`) with a `content` array of `{"Text"|"ToolUse"|"ToolResult"}`
segments; the digest keeps the final-message text and tool-use signals that
triage needs, not the full transcript.

## Headless visual verification (no screen recording permission)

The crate ships a binary that renders the panel to PNG **offscreen** — no
visible window, no screen-capture APIs, no macOS permissions. It uses gpui's
`HeadlessAppContext` with the Metal headless renderer: the scene is drawn
into an offscreen `MTLTexture` and read back as an `image::RgbaImage`. This
is how the panel was iterated on: render, inspect the PNG, fix, re-render.

```sh
# fixtures -> target/visual/mission_control_fixtures.png
cargo run -p agent_mission_control --bin mission_control_visual \
  --features "visual-tests,gpui_platform/runtime_shaders"

# with the "Needs me" filter active -> mission_control_filtered.png
... -- --filter

# against your real threads.db (read-only) -> mission_control_live.png
... -- --live
```

PNGs come out at 2x the logical window size (TestWindow's scale factor is
hardcoded to 2.0), so the 420x760 window yields 840x1520 images.

Gotchas baked into the setup:

- The `visual-tests` feature must include `gpui_platform/font-kit`, or the
  headless platform silently uses `NoopTextSystem` and text renders blank.
- Do not use `#[gpui::test]` for image capture — `TestAppContext` has no
  renderer factory. A plain `fn main()` binary with
  `required-features = ["visual-tests"]` is the vehicle.
- On machines without full Xcode, every cargo invocation that compiles
  `gpui_macos` needs `gpui_platform/runtime_shaders` (shaders compile at app
  launch via the OS Metal framework instead of `xcrun metal` at build time).

## Building and testing

```sh
# unit tests (triage core + store parsing + fixture coverage)
cargo test -p agent_mission_control --features gpui_platform/runtime_shaders

# full Zed with the panel wired in
cargo build --features gpui_platform/runtime_shaders
```

The panel is registered in `crates/zed/src/zed.rs` (`initialize_panels`) and
initialized in `crates/zed/src/main.rs`. Note: `DebugPanel::load` consumes
the context reborrow, so this panel must be loaded before it.

## Roadmap (v2)

Tracked externally (eng-tools-anthony,
`agent_notes/2026-07-06/mission-control-panel-v2-HANDOFF.md`, catalogue item
`2a1a74b4`):

- Virtualize the list with `uniform_list` (currently a plain scroll flex,
  fine at the default 50-thread limit).
- Row click opens the actual agent thread (route through an event the panel
  wrapper handles; the view stays workspace-free).
- Persist dock position / width / filter state.
- Default keymap entry for `mission_control::ToggleFocus`.
- Parent/child thread families collapsed into one row with a rollup
  (research found 12 of 60 threads were loop-generated subthread noise).
- Mark-handled/dismiss so attended asks leave the "Needs you" group.
