//! Talking to Zed's internals: resolve real thread titles from the local
//! threads database, reusing the read-only loader from
//! `agent_mission_control` (temp-copy of the db, zstd + JSON decode).

use agent_mission_control::store as thread_store;
use anyhow::Result;

/// Load the titles of the most recently updated threads.
///
/// Synchronous; run on a background executor.
pub fn load_recent_titles(limit: usize) -> Result<Vec<String>> {
    let path = thread_store::resolve_db_path(None);
    let digests = thread_store::load_digests(&path, limit)?;
    Ok(digests.into_iter().map(|digest| digest.title).collect())
}
