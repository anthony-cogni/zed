//! IO shell for the workcat-db repo-as-database.
//!
//! Reads: brief metadata from `briefs/*.md` (the sanctioned
//! materialized-view cache) and node positions folded from
//! `node_moved` events in `events/*/*.jsonl`.
//!
//! Writes follow the two-grain model (DR-006): events are appended to
//! the current month's log the moment they happen; commits are
//! checkpoints that group a session's events under the write lock
//! (`tools/lock.py`), re-materialize briefs (`tools/fold.py`), and
//! push.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};

use crate::model::{self, ItemMeta};

/// Environment variable that overrides the workcat-db directory.
pub const DB_DIR_ENV_VAR: &str = "WORKCAT_DB_DIR";

/// The actor recorded on every event this client writes.
pub const ACTOR: &str = "workcat-map";

/// How many times to retry a busy lock, and how long between tries.
pub const LOCK_RETRIES: u32 = 4;
pub const LOCK_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Default workcat-db location: `~/workcat-db`.
pub fn default_db_dir() -> PathBuf {
    paths::home_dir().join("workcat-db")
}

/// Resolve the db directory: env var override, then default.
pub fn resolve_db_dir() -> PathBuf {
    std::env::var(DB_DIR_ENV_VAR)
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(default_db_dir)
}

/// Load every item's metadata from the materialized briefs.
pub fn load_items(db: &Path) -> Result<Vec<ItemMeta>> {
    let briefs = db.join("briefs");
    let mut items = Vec::new();
    let entries =
        std::fs::read_dir(&briefs).with_context(|| format!("reading {}", briefs.display()))?;
    let mut paths: Vec<_> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    paths.sort();
    for path in paths {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        match model::parse_brief(&text) {
            Some(item) => items.push(item),
            None => log::warn!("workcat_map: unparseable brief {}", path.display()),
        }
    }
    Ok(items)
}

/// Fold node positions from every event shard, in log order
/// (shard dirs sorted, files sorted, lines in file order), matching
/// `tools/fold.py`'s traversal.
pub fn load_positions(db: &Path) -> Result<HashMap<String, (f32, f32)>> {
    let ev_root = db.join("events");
    let mut text = String::new();
    let mut shards: Vec<_> = std::fs::read_dir(&ev_root)
        .with_context(|| format!("reading {}", ev_root.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.is_dir())
        .collect();
    shards.sort();
    for shard in shards {
        let mut files: Vec<_> = std::fs::read_dir(&shard)?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        files.sort();
        for file in files {
            text.push_str(
                &std::fs::read_to_string(&file)
                    .with_context(|| format!("reading {}", file.display()))?,
            );
            text.push('\n');
        }
    }
    Ok(model::fold_node_positions(text.lines()))
}

fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub fn node_moved_event(id: &str, x: f32, y: f32) -> serde_json::Value {
    serde_json::json!({
        "ts": timestamp(),
        "actor": ACTOR,
        "kind": "node_moved",
        "id": id,
        "x": x,
        "y": y,
    })
}

pub fn status_set_event(id: &str, status: &str) -> serde_json::Value {
    serde_json::json!({
        "ts": timestamp(),
        "actor": ACTOR,
        "kind": "status_set",
        "id": id,
        "status": status,
    })
}

/// Append one event line to the current month's shard, creating the
/// shard directory if the month rolled over.
pub fn append_event(db: &Path, event: &serde_json::Value) -> Result<()> {
    let shard = db
        .join("events")
        .join(chrono::Utc::now().format("%Y-%m").to_string());
    std::fs::create_dir_all(&shard).with_context(|| format!("creating {}", shard.display()))?;
    let path = shard.join("log.jsonl");
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

/// Progress notifications surfaced in the panel status line while a
/// checkpoint runs on the background executor.
pub type Progress = futures::channel::mpsc::UnboundedSender<String>;

fn report(progress: &Progress, message: impl Into<String>) {
    progress.unbounded_send(message.into()).ok();
}

async fn run(db: &Path, program: &str, args: &[&str]) -> Result<std::process::Output> {
    smol::process::Command::new(program)
        .args(args)
        .current_dir(db)
        .output()
        .await
        .with_context(|| format!("spawning {program} {}", args.join(" ")))
}

async fn run_ok(db: &Path, program: &str, args: &[&str]) -> Result<String> {
    let output = run(db, program, args).await?;
    if !output.status.success() {
        bail!(
            "{program} {} failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim(),
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn events_dirty(db: &Path) -> Result<bool> {
    Ok(
        !run_ok(db, "git", &["status", "--porcelain", "--", "events"])
            .await?
            .is_empty(),
    )
}

/// Acquire the write lock via `tools/lock.py`, retrying briefly if it
/// is busy (FCFS, DR-004 ruling 14). Exit code 1 means busy.
async fn acquire_lock(db: &Path, progress: &Progress) -> Result<()> {
    for attempt in 0..=LOCK_RETRIES {
        let output = run(
            db,
            "python3",
            &["tools/lock.py", "acquire", "--actor", ACTOR],
        )
        .await?;
        if output.status.success() {
            return Ok(());
        }
        let note = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if attempt < LOCK_RETRIES {
            report(
                progress,
                format!("waiting for lock ({note}), retry {}...", attempt + 1),
            );
            smol::Timer::at(Instant::now() + LOCK_RETRY_DELAY).await;
        } else {
            bail!("lock still busy after {} tries: {note}", LOCK_RETRIES + 1);
        }
    }
    unreachable!()
}

async fn release_lock(db: &Path) -> Result<()> {
    run_ok(
        db,
        "python3",
        &["tools/lock.py", "release", "--actor", ACTOR],
    )
    .await
    .map(|_| ())
}

/// Checkpoint the session per the workcat-db write protocol: acquire
/// the lock, (re)append pending events around the pull, fold briefs,
/// commit, push, release. Returns the short sha of the checkpoint
/// commit, or `None` when there was nothing to commit.
///
/// Uncommitted event appends made since the last checkpoint are
/// stashed across the lock acquire (whose `git pull --rebase` needs a
/// clean tree) and restored before folding, keeping appends
/// append-only relative to anything pulled.
pub async fn checkpoint(db: &Path, message: &str, progress: &Progress) -> Result<Option<String>> {
    let stashed = if events_dirty(db).await? {
        report(progress, "stashing pending events...");
        run_ok(
            db,
            "git",
            &[
                "stash",
                "push",
                "-m",
                "workcat-map-checkpoint",
                "--",
                "events",
            ],
        )
        .await?;
        true
    } else {
        false
    };

    report(progress, "acquiring write lock...");
    if let Err(error) = acquire_lock(db, progress).await {
        if stashed {
            run_ok(db, "git", &["stash", "pop"]).await.ok();
        }
        return Err(error);
    }

    let result = checkpoint_locked(db, message, stashed, progress).await;

    report(progress, "releasing write lock...");
    if let Err(release_error) = release_lock(db).await {
        log::error!("workcat_map: failed to release lock: {release_error:#}");
        if result.is_ok() {
            return Err(release_error);
        }
    }
    result
}

async fn checkpoint_locked(
    db: &Path,
    message: &str,
    stashed: bool,
    progress: &Progress,
) -> Result<Option<String>> {
    if stashed {
        run_ok(db, "git", &["stash", "pop"])
            .await
            .map_err(|error| {
                anyhow!(
                    "restoring pending events failed (they are safe in `git stash`; \
                 resolve by hand in the db repo): {error}"
                )
            })?;
    }
    if !events_dirty(db).await? {
        return Ok(None);
    }
    report(progress, "folding briefs...");
    run_ok(db, "python3", &["tools/fold.py"]).await?;
    run_ok(db, "git", &["add", "events", "briefs"]).await?;
    report(progress, "committing...");
    run_ok(db, "git", &["commit", "-m", message]).await?;
    report(progress, "pushing...");
    run_ok(db, "git", &["push"]).await?;
    let sha = run_ok(db, "git", &["rev-parse", "--short", "HEAD"]).await?;
    Ok(Some(sha))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn write(dir: &Path, rel: &str, text: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn load_items_parses_briefs_and_skips_garbage() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "briefs/aaaa0000-first.md",
            "# First\n\n- id: aaaa0000-x\n- ref: r1\n- status: started\n- depends_on: []\n",
        );
        write(
            dir.path(),
            "briefs/bbbb0000-second.md",
            "# Second\n\n- id: bbbb0000-x\n- ref: r2\n- status: complete\n- depends_on:\n  - aaaa0000-first.md\n",
        );
        write(dir.path(), "briefs/notes.txt", "not a brief");
        write(dir.path(), "briefs/broken.md", "no metadata here");
        let items = load_items(dir.path()).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].subject, "First");
        assert_eq!(items[1].depends_on, vec!["aaaa0000".to_string()]);
    }

    #[test]
    fn append_then_load_positions_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        append_event(dir.path(), &node_moved_event("id-a", 10.0, 20.0)).unwrap();
        append_event(dir.path(), &status_set_event("id-a", "started")).unwrap();
        append_event(dir.path(), &node_moved_event("id-a", 30.5, 40.5)).unwrap();
        let positions = load_positions(dir.path()).unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions["id-a"], (30.5, 40.5));
    }

    #[test]
    fn positions_fold_across_shards_in_order() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "events/2026-06/log.jsonl",
            r#"{"ts":"t","actor":"a","kind":"node_moved","id":"n","x":1,"y":1}"#,
        );
        write(
            dir.path(),
            "events/2026-07/log.jsonl",
            r#"{"ts":"t","actor":"a","kind":"node_moved","id":"n","x":2,"y":2}"#,
        );
        let positions = load_positions(dir.path()).unwrap();
        assert_eq!(positions["n"], (2.0, 2.0));
    }

    #[test]
    fn events_match_db_vocabulary() {
        let ev = status_set_event("some-id", "started");
        assert_eq!(ev["kind"], "status_set");
        assert_eq!(ev["actor"], ACTOR);
        assert_eq!(ev["id"], "some-id");
        assert_eq!(ev["status"], "started");
        assert!(ev["ts"].as_str().unwrap().contains('T'));
        let ev = node_moved_event("some-id", 1.5, 2.5);
        assert_eq!(ev["kind"], "node_moved");
        assert_eq!(ev["x"], 1.5);
        assert_eq!(ev["y"], 2.5);
    }
}
