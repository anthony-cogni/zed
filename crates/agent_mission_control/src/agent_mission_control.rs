//! Agent Mission Control: a dock panel that triages agent threads.
//!
//! Reads Zed's agent thread database and buckets every thread into one of
//! four attention states (Needs you / Awaiting merge / Running / Done) so a
//! user running many concurrent agent threads can reorient at a glance.

pub mod fixtures;
mod panel;
pub mod store;
pub mod triage;

use chrono::{DateTime, Utc};

pub use panel::{MissionControlPanel, MissionControlView, ToggleFocus, init};
pub use triage::{Role, Triage, TriageState};

/// A plain summary of one agent thread, ready for triage and display.
#[derive(Debug, Clone)]
pub struct ThreadDigest {
    pub id: String,
    pub title: String,
    pub updated_at: Option<DateTime<Utc>>,
    pub message_count: usize,
    /// Role of the very last message in the thread.
    pub last_role: Option<Role>,
    /// Concatenated text of the last agent message.
    pub final_agent_text: String,
    /// Whether the last agent message contains tool uses.
    pub final_agent_has_tool_use: bool,
    /// Triage result, computed at load time.
    pub state: TriageState,
    /// The extracted ask, when the agent left one.
    pub ask: Option<String>,
}

impl ThreadDigest {
    /// Build a digest from raw parts, computing the triage state.
    pub fn new(
        id: String,
        title: String,
        updated_at: Option<DateTime<Utc>>,
        message_count: usize,
        last_role: Option<Role>,
        final_agent_text: String,
        final_agent_has_tool_use: bool,
    ) -> Self {
        let triage = triage::triage(&triage::TriageInput {
            last_role,
            final_agent_text: &final_agent_text,
            final_agent_has_tool_use,
        });
        Self {
            id,
            title,
            updated_at,
            message_count,
            last_role,
            final_agent_text,
            final_agent_has_tool_use,
            state: triage.state,
            ask: triage.ask,
        }
    }
}
