//! IO shell: load thread digests from Zed's threads database.
//!
//! The real Zed process may hold `threads.db` open, and `sqlez` only opens
//! databases read-write. To guarantee we never touch the user's live data,
//! we copy the database file (and its WAL/SHM sidecars, when present) to a
//! temporary directory and read the copy.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use sqlez::{connection::Connection, statement::Statement};

use crate::{Role, ThreadDigest};

/// Environment variable that overrides the database path.
pub const DB_PATH_ENV_VAR: &str = "MISSION_CONTROL_THREADS_DB";

/// Default number of threads to load.
pub const DEFAULT_LIMIT: usize = 50;

/// The default location of the threads database.
pub fn default_db_path() -> PathBuf {
    paths::data_dir().join("threads").join("threads.db")
}

/// Resolve the database path: explicit override, then env var, then default.
pub fn resolve_db_path(override_path: Option<PathBuf>) -> PathBuf {
    override_path
        .or_else(|| std::env::var(DB_PATH_ENV_VAR).ok().map(PathBuf::from))
        .unwrap_or_else(default_db_path)
}

/// Load up to `limit` thread digests, most recently updated first.
///
/// This is synchronous and should run on a background executor.
pub fn load_digests(db_path: &Path, limit: usize) -> Result<Vec<ThreadDigest>> {
    anyhow::ensure!(
        db_path.exists(),
        "threads database not found at {}",
        db_path.display()
    );

    // Copy the database to a temp dir so we never write to the live file.
    let temp_dir = tempfile::tempdir().context("creating temp dir for threads.db copy")?;
    let copy_path = temp_dir.path().join("threads.db");
    std::fs::copy(db_path, &copy_path)
        .with_context(|| format!("copying {} to temp", db_path.display()))?;
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = db_path.as_os_str().to_owned();
        sidecar.push(suffix);
        let sidecar = PathBuf::from(sidecar);
        if sidecar.exists() {
            let mut sidecar_copy = copy_path.as_os_str().to_owned();
            sidecar_copy.push(suffix);
            std::fs::copy(&sidecar, PathBuf::from(sidecar_copy))
                .with_context(|| format!("copying {} to temp", sidecar.display()))?;
        }
    }

    let connection = Connection::open_file(&copy_path.to_string_lossy());
    let mut statement = Statement::prepare(
        &connection,
        "SELECT id, summary, updated_at, data_type, data FROM threads \
         ORDER BY updated_at DESC LIMIT ?",
    )
    .context("preparing threads query")?;
    statement
        .with_bindings(&(limit as i64))
        .context("binding limit")?;
    let rows = statement.map(|row| {
        let id = row.column_text(0)?.to_string();
        let summary = row.column_text(1)?.to_string();
        let updated_at = row.column_text(2)?.to_string();
        let data_type = row.column_text(3)?.to_string();
        let data = row.column_blob(4)?.to_vec();
        Ok((id, summary, updated_at, data_type, data))
    })?;

    let mut digests = Vec::with_capacity(rows.len());
    for (id, summary, updated_at, data_type, data) in rows {
        match digest_from_row(&id, &summary, &updated_at, &data_type, &data) {
            Ok(digest) => digests.push(digest),
            Err(error) => {
                log::warn!("mission control: skipping thread {id}: {error:#}");
            }
        }
    }
    Ok(digests)
}

/// Pure: turn one raw database row into a digest.
fn digest_from_row(
    id: &str,
    summary: &str,
    row_updated_at: &str,
    data_type: &str,
    data: &[u8],
) -> Result<ThreadDigest> {
    let json_bytes = if data_type == "zstd" {
        zstd::decode_all(data).context("zstd-decompressing thread data")?
    } else {
        data.to_vec()
    };
    let value: serde_json::Value =
        serde_json::from_slice(&json_bytes).context("parsing thread JSON")?;

    let title = value
        .get("title")
        .and_then(|title| title.as_str())
        .filter(|title| !title.trim().is_empty())
        .unwrap_or(summary)
        .to_string();

    let updated_at = value
        .get("updated_at")
        .and_then(|updated| updated.as_str())
        .and_then(parse_timestamp)
        .or_else(|| parse_timestamp(row_updated_at));

    let empty = Vec::new();
    let messages = value
        .get("messages")
        .and_then(|messages| messages.as_array())
        .unwrap_or(&empty);

    let mut last_role = None;
    let mut final_agent_text = String::new();
    let mut final_agent_has_tool_use = false;
    for message in messages {
        let Some(object) = message.as_object() else {
            continue;
        };
        if object.contains_key("User") {
            last_role = Some(Role::User);
        } else if let Some(agent_message) = object.get("Agent") {
            last_role = Some(Role::Agent);
            (final_agent_text, final_agent_has_tool_use) = agent_message_content(agent_message);
        }
    }

    Ok(ThreadDigest::new(
        id.to_string(),
        title,
        updated_at,
        messages.len(),
        last_role,
        final_agent_text,
        final_agent_has_tool_use,
    ))
}

/// Pure: collect the text segments and tool-use flag of one agent message.
fn agent_message_content(agent_message: &serde_json::Value) -> (String, bool) {
    let mut text = String::new();
    let mut has_tool_use = false;
    let Some(segments) = agent_message
        .get("content")
        .and_then(|content| content.as_array())
    else {
        return (text, has_tool_use);
    };
    for segment in segments {
        let Some(object) = segment.as_object() else {
            continue;
        };
        if let Some(segment_text) = object.get("Text").and_then(|value| value.as_str()) {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(segment_text);
        } else if object.contains_key("ToolUse") {
            has_tool_use = true;
        }
    }
    (text, has_tool_use)
}

fn parse_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(raw) {
        return Some(parsed.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(parsed) = NaiveDateTime::parse_from_str(raw, format) {
            return Some(parsed.and_utc());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TriageState;
    use pretty_assertions::assert_eq;

    #[test]
    fn digest_from_zstd_row() {
        let thread_json = serde_json::json!({
            "title": "Fix flaky worktree test",
            "updated_at": "2026-07-06T10:00:00Z",
            "messages": [
                {"User": {"content": [{"Text": "please fix the flaky test"}]}},
                {"Agent": {"content": [
                    {"Text": "Fixed the race by pinning the executor seed. All 12 runs pass."},
                    {"ToolResult": {"tool": "terminal"}}
                ]}}
            ]
        });
        let data =
            zstd::encode_all(serde_json::to_vec(&thread_json).unwrap().as_slice(), 0).unwrap();
        let digest =
            digest_from_row("abc", "summary", "2026-07-06T10:00:00Z", "zstd", &data).unwrap();
        assert_eq!(digest.title, "Fix flaky worktree test");
        assert_eq!(digest.message_count, 2);
        assert_eq!(digest.last_role, Some(Role::Agent));
        assert_eq!(digest.state, TriageState::Done);
        assert!(!digest.final_agent_has_tool_use);
        assert!(digest.updated_at.is_some());
    }

    #[test]
    fn tool_use_in_final_agent_message_is_running() {
        let thread_json = serde_json::json!({
            "title": "t",
            "messages": [
                {"User": {"content": [{"Text": "go"}]}},
                {"Agent": {"content": [
                    {"Text": "Running the build now."},
                    {"ToolUse": {"tool": "terminal"}}
                ]}}
            ]
        });
        let data = serde_json::to_vec(&thread_json).unwrap();
        let digest = digest_from_row("x", "s", "bogus", "json", &data).unwrap();
        assert!(digest.final_agent_has_tool_use);
        assert_eq!(digest.state, TriageState::Running);
        assert_eq!(digest.updated_at, None);
    }
}
