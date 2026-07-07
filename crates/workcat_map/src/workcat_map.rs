//! Workcat Map v1: a dock panel over the workcat-db repo-as-database.
//!
//! DR-005 ruling 17 scopes v1 to exactly: read, focus, drag, set
//! status. The history scrubber is not v1 (but every gesture already
//! lands in the one log, so its history is populating).
//!
//! - READ: brief metadata is parsed from `briefs/` (the sanctioned
//!   materialized view); the default scope is DR-003 ruling 7
//!   (incomplete rollup plus unknown). Dependency edges draw as arrows.
//! - FOCUS: click focuses a node (halo + detail strip); background
//!   click or escape clears.
//! - DRAG: a drag commits at gesture rest as a `node_moved` event
//!   appended to the log immediately; git commit/push happens at
//!   checkpoints (DR-006 two-grain).
//! - SET STATUS: keys 1-9 or the node context menu (the menu shows the
//!   keys); appends `status_set`, re-folds briefs via `tools/fold.py`,
//!   and checkpoints immediately under the write lock.

pub mod geometry;
pub mod model;
mod panel;
pub mod store;

pub use panel::{
    Checkpoint, ClearFocus, SetStatus, ToggleFocus, WorkcatMapPanel, WorkcatMapView, init,
};
