//! IO shell: persist the map layout to disk and append gesture events.
//!
//! The layout lives in a JSON file written atomically (temp file + rename)
//! at gesture rest; each drag end also appends one JSON-lines event so the
//! write cadence is observable after the fact.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use crate::geometry::Layout;

/// Environment variable that overrides the storage directory.
pub const DIR_ENV_VAR: &str = "WORKCAT_MAP_DIR";

/// Default storage directory: `~/logs/workcat-map-spike`.
pub fn default_dir() -> PathBuf {
    paths::home_dir().join("logs").join("workcat-map-spike")
}

/// Resolve the storage directory: env var override, then default.
pub fn resolve_dir() -> PathBuf {
    std::env::var(DIR_ENV_VAR)
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(default_dir)
}

pub fn layout_path(dir: &Path) -> PathBuf {
    dir.join("layout.json")
}

pub fn events_path(dir: &Path) -> PathBuf {
    dir.join("events.jsonl")
}

/// Load the persisted layout, if one exists.
pub fn load_layout(dir: &Path) -> Result<Option<Layout>> {
    let path = layout_path(dir);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let layout =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(layout))
}

/// Write the layout atomically: temp file in the same directory, then rename.
pub fn save_layout(dir: &Path, layout: &Layout) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = layout_path(dir);
    let tmp = dir.join("layout.json.tmp");
    let json = serde_json::to_vec_pretty(layout).context("serializing layout")?;
    std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Append one JSON-lines event.
pub fn append_event(dir: &Path, event: &serde_json::Value) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = events_path(dir);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut line = serde_json::to_vec(event).context("serializing event")?;
    line.push(b'\n');
    file.write_all(&line)
        .with_context(|| format!("appending to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{DEFAULT_SEED, Layout};
    use pretty_assertions::assert_eq;

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::generate(DEFAULT_SEED, 4);
        save_layout(dir.path(), &layout).unwrap();
        let loaded = load_layout(dir.path()).unwrap();
        assert_eq!(loaded, Some(layout));
        // No temp file left behind.
        assert!(!dir.path().join("layout.json.tmp").exists());
    }

    #[test]
    fn load_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_layout(dir.path()).unwrap(), None);
    }

    #[test]
    fn events_append_as_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        append_event(dir.path(), &serde_json::json!({"event": "a"})).unwrap();
        append_event(dir.path(), &serde_json::json!({"event": "b"})).unwrap();
        let text = std::fs::read_to_string(events_path(dir.path())).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            serde_json::from_str::<serde_json::Value>(line).unwrap();
        }
    }
}
