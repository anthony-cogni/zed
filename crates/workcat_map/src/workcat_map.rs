//! Workcat Map spike: a dock panel of ~100 draggable rectangles.
//!
//! This is the Zed-native half of the workcat twin client spikes. It
//! demonstrates exactly three things:
//!
//! 1. Owned geometry: node positions are application state; a drag commits
//!    at gesture rest and persists to disk as JSON, surviving restart.
//! 2. Talking to Zed's internals: a few nodes display real thread titles
//!    resolved from Zed's own threads database.
//! 3. Living as a pane: the map is a real dock panel that participates in
//!    Zed focus (focus in/out is observable in the UI and in logs).

pub mod geometry;
mod panel;
pub mod store;
pub mod threads;

pub use panel::{ToggleFocus, WorkcatMapPanel, WorkcatMapView, init};
