# Workcat Map (v1)

A dock panel that renders the workcat work-item map from the workcat-db
repo-as-database. This is v1 of the Zed-native client (workcat design
record 005, ruling 17): read, focus, drag, set status. The history
scrubber is not v1, but every gesture already lands in the one log.

## The four capabilities

**Read**: on load, brief metadata (id, ref, status, depends_on) is
parsed from `briefs/` — the sanctioned materialized-view cache — and
node positions are folded from `node_moved` events in the log
(last-write-wins per item). The default scope is DR-003 ruling 7: the
incomplete rollup (`not_started` through `merged`) plus `unknown`
(drawn with a dashed border); `complete` and `canceled` are hidden.
Items render as status-colored nodes on a scrollable field; dependency
edges draw as arrows from dependent to dependency. Items without a
persisted position get a deterministic grid slot, grouped by status.

**Focus**: click focuses a node (halo border + a detail strip with
subject, status, ref, and dependency count). Click the background or
press `escape` to clear. Edges touching the focused node highlight.

**Drag**: direct manipulation; the gesture commits at rest. A real drag
(beyond the click slop) appends one `node_moved` event
(`{ts, actor: "workcat-map", kind: "node_moved", id, x, y}`) to the
current month's log immediately. Commit/push is deferred to a
checkpoint (DR-006 two-grain); the header counts uncommitted events.

**Set status**: with a node focused, keys `1`-`9` set the status
(log order: not_started, started, paused, blocked, implemented, merged,
complete, canceled, unknown), or right-click the node for a context
menu carrying the same actions — the menu displays the key bindings,
per the menus-teach-gestures principle (DR-003 ruling 8). A status
change appends a `status_set` event and checkpoints immediately.

## The write protocol

Checkpoints follow the workcat-db README: acquire the write lock
(`tools/lock.py acquire`, FCFS with brief retries — a busy lock shows
"waiting for lock" in the status line and never blocks the UI thread),
re-materialize briefs (`tools/fold.py`, never hand-edited or
reimplemented), `git commit` + `push`, release the lock. Pending
(uncommitted) event appends are stashed across the acquire's
`git pull --rebase` and restored before folding. Checkpoints run on
status changes and on the manual Checkpoint action (`c`).

## Storage

- Database: `~/workcat-db`, overridable with `WORKCAT_DB_DIR`.
- Events append to `events/YYYY-MM/log.jsonl` in that repo.

## Layout of the code

- `model.rs` — pure core: status vocabulary, brief metadata parsing,
  the default scope filter, the position fold. Unit-tested.
- `geometry.rs` — pure core: grid layout, drag clamping, click slop.
  Unit-tested.
- `store.rs` — IO shell: brief/event reads, event appends, and the
  checkpoint subprocess cycle (async, background executor).
- `panel.rs` — the GPUI view, actions, context menu, and the `Panel`
  wrapper.
- `examples/workcat_cycle.rs` — headless driver for the same store
  code paths: `load`, `move`, `status`, `checkpoint`. Point
  `WORKCAT_DB_DIR` at a throwaway clone to prove the cycle safely.

## Building and verifying

Build the whole app without full Xcode (Command Line Tools only):

```
cargo build --features gpui_platform/runtime_shaders
```

Run against a scratch project with an isolated data dir so a running
Zed instance is not disturbed:

```
./target/debug/zed --user-data-dir /tmp/zed-v1-data /tmp/zed-v1-proj
```

Then check `~/Library/Logs/Zed/Zed.log` for the `workcat_map:` load
line (item/edge counts).

Crate-only checks:

```
cargo test -p workcat_map --features gpui_platform/runtime_shaders
cargo clippy -p workcat_map --all-targets --features gpui_platform/runtime_shaders
```

Headless cycle against a throwaway clone:

```
git clone --bare ~/workcat-db /tmp/workcat-db-origin.git
git clone /tmp/workcat-db-origin.git /tmp/workcat-db-test
export WORKCAT_DB_DIR=/tmp/workcat-db-test
cargo run -p workcat_map --example workcat_cycle -- load
cargo run -p workcat_map --example workcat_cycle -- move <id> 240 180
cargo run -p workcat_map --example workcat_cycle -- status <id> started
```
