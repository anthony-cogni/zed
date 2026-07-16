//! Pure core: workcat item model, brief metadata parsing, the default
//! map scope, filters, lenses, the undo stack, and the position fold
//! over `node_moved` events.
//!
//! No IO and no GPUI types live here, so everything is unit-testable.

use std::collections::{BTreeMap, HashMap, HashSet};

/// The workcat status vocabulary (workcat-db README).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    NotStarted,
    Started,
    Paused,
    Blocked,
    Implemented,
    Merged,
    Complete,
    Canceled,
    Unknown,
}

/// All statuses in lifecycle order. Keyboard bindings 1-9 follow this
/// order, and the context menu lists them in this order.
pub const ALL_STATUSES: [Status; 9] = [
    Status::NotStarted,
    Status::Started,
    Status::Paused,
    Status::Blocked,
    Status::Implemented,
    Status::Merged,
    Status::Complete,
    Status::Canceled,
    Status::Unknown,
];

impl Status {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "not_started" => Self::NotStarted,
            "started" => Self::Started,
            "paused" => Self::Paused,
            "blocked" => Self::Blocked,
            "implemented" => Self::Implemented,
            "merged" => Self::Merged,
            "complete" => Self::Complete,
            "canceled" => Self::Canceled,
            "unknown" => Self::Unknown,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Started => "started",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::Implemented => "implemented",
            Self::Merged => "merged",
            Self::Complete => "complete",
            Self::Canceled => "canceled",
            Self::Unknown => "unknown",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::NotStarted => "Not started",
            Self::Started => "Started",
            Self::Paused => "Paused",
            Self::Blocked => "Blocked",
            Self::Implemented => "Implemented",
            Self::Merged => "Merged",
            Self::Complete => "Complete",
            Self::Canceled => "Canceled",
            Self::Unknown => "Unknown",
        }
    }

    /// The default map scope (DR-003 ruling 7): the incomplete rollup
    /// (`not_started` through `merged`) plus `unknown`. Terminal items
    /// (`complete`, `canceled`) are hidden.
    pub fn in_default_scope(self) -> bool {
        !matches!(self, Self::Complete | Self::Canceled)
    }

    /// Sort rank for the initial layout: urgent-ish first, unknown last
    /// so its visual distinctness clusters.
    pub fn layout_rank(self) -> usize {
        ALL_STATUSES.iter().position(|s| *s == self).unwrap_or(0)
    }

    /// Whether this status is an off-lifecycle terminal choice
    /// (`Canceled`, `Unknown`). The status menu draws these below a
    /// divider, separating them from the ordinary forward flow.
    pub fn is_terminal_choice(self) -> bool {
        matches!(self, Self::Canceled | Self::Unknown)
    }
}

/// One work item's metadata, parsed from its brief file.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemMeta {
    /// Full uuid.
    pub id: String,
    pub subject: String,
    /// The item's `ref` field (identity key).
    pub reference: String,
    pub status: Status,
    /// The `updated_at` metadata field verbatim (ISO-ish), if present.
    /// The detail panel humanizes it for the footer.
    pub updated_at: Option<String>,
    /// Dependencies as id8 prefixes (first 8 hex digits of the dep uuid).
    pub depends_on: Vec<String>,
    /// Brief body sections in file order: (heading, body text).
    /// Includes `Notes` when present (workcat-db commit 87fb2b8).
    pub sections: Vec<(String, String)>,
}

impl ItemMeta {
    /// The `## Notes` section body, if the brief has one.
    pub fn notes(&self) -> Option<&str> {
        self.sections
            .iter()
            .find(|(heading, _)| heading.eq_ignore_ascii_case("notes"))
            .map(|(_, body)| body.as_str())
    }
}

impl ItemMeta {
    pub fn id8(&self) -> &str {
        &self.id[..self.id.len().min(8)]
    }
}

/// The visibility filter over the map: which statuses show, plus a
/// free-text query matched against subject and ref (case-insensitive).
/// The default matches DR-003 ruling 7: the incomplete rollup plus
/// `unknown`; terminal statuses hidden.
#[derive(Debug, Clone, PartialEq)]
pub struct FilterState {
    pub visible_statuses: HashSet<Status>,
    pub query: String,
}

impl Default for FilterState {
    fn default() -> Self {
        Self {
            visible_statuses: ALL_STATUSES
                .into_iter()
                .filter(|status| status.in_default_scope())
                .collect(),
            query: String::new(),
        }
    }
}

impl FilterState {
    pub fn matches(&self, item: &ItemMeta) -> bool {
        if !self.visible_statuses.contains(&item.status) {
            return false;
        }
        let query = self.query.trim();
        if query.is_empty() {
            return true;
        }
        let query = query.to_lowercase();
        item.subject.to_lowercase().contains(&query)
            || item.reference.to_lowercase().contains(&query)
    }

    pub fn toggle_status(&mut self, status: Status) {
        if !self.visible_statuses.remove(&status) {
            self.visible_statuses.insert(status);
        }
    }

    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// A saved lens: a named filter (DR-001's lensed map). Folded from
/// `lens_saved` / `lens_deleted` events, last-write-wins per name.
/// Node geometry is still shared truth via `node_moved` events; a
/// lens's own `positions` snapshot below is a secondary, explicit-only
/// overlay (see its field doc), never reasserted outside of applying
/// the lens.
#[derive(Debug, Clone, PartialEq)]
pub struct Lens {
    pub name: String,
    pub visible_statuses: Vec<String>,
    pub query: String,
    /// Node positions captured when the lens was saved: `(id, x, y)`.
    /// A lens organizes a workstream by geometry as well as by filter,
    /// so applying it restores these positions (the SPA behavior) —
    /// but only at that explicit moment. Nothing else should re-merge
    /// this snapshot over live positions, or a stale layout reasserts
    /// itself over later moves (the lens-position-revert bug).
    pub positions: Vec<(String, f32, f32)>,
}

impl Lens {
    pub fn to_filter(&self) -> FilterState {
        FilterState {
            visible_statuses: self
                .visible_statuses
                .iter()
                .filter_map(|s| Status::parse(s))
                .collect(),
            query: self.query.clone(),
        }
    }
}

/// Fold `lens_saved`/`lens_deleted` events (last-write-wins per lens
/// name) from raw JSON-lines, the same shape as the position fold.
pub fn fold_lenses<'a>(lines: impl Iterator<Item = &'a str>) -> BTreeMap<String, Lens> {
    let mut lenses = BTreeMap::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match value.get("kind").and_then(|k| k.as_str()) {
            Some("lens_saved") => {
                let Some(name) = value.get("name").and_then(|v| v.as_str()) else {
                    continue;
                };
                let visible_statuses = value
                    .get("visible_statuses")
                    .and_then(|v| v.as_array())
                    .map(|entries| {
                        entries
                            .iter()
                            .filter_map(|s| s.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let query = value
                    .get("query")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let positions = value
                    .get("positions")
                    .and_then(|v| v.as_array())
                    .map(|entries| {
                        entries
                            .iter()
                            .filter_map(|entry| {
                                let id = entry.get("id")?.as_str()?.to_string();
                                let x = entry.get("x")?.as_f64()? as f32;
                                let y = entry.get("y")?.as_f64()? as f32;
                                Some((id, x, y))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                lenses.insert(
                    name.to_string(),
                    Lens {
                        name: name.to_string(),
                        visible_statuses,
                        query,
                        positions,
                    },
                );
            }
            Some("lens_deleted") => {
                if let Some(name) = value.get("name").and_then(|v| v.as_str()) {
                    lenses.remove(name);
                }
            }
            _ => {}
        }
    }
    lenses
}

/// One undoable operation. Compensations are event-sourced: undoing
/// appends new events restoring the prior value; history is never
/// rewritten (DR-005 ruling 16: workcat undo is pane-focus scoped and
/// independent of Zed's editor history).
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// One gesture's node moves (a single drag or a group drag or an
    /// auto-arrange): `(id, from, to)` per node.
    Move(Vec<(String, (f32, f32), (f32, f32))>),
    /// A status change: `(id, from, to)`.
    SetStatus(String, Status, Status),
}

/// A bounded undo/redo stack (200 steps, matching the SPA).
#[derive(Debug, Default)]
pub struct UndoStack {
    undo: Vec<Op>,
    redo: Vec<Op>,
}

pub const UNDO_CAPACITY: usize = 200;

impl UndoStack {
    /// Record a newly-performed operation.
    pub fn push(&mut self, op: Op) {
        self.redo.clear();
        self.undo.push(op);
        if self.undo.len() > UNDO_CAPACITY {
            self.undo.remove(0);
        }
    }

    /// Pop the most recent op for undoing; the caller applies the
    /// compensation, then the op moves to the redo side.
    pub fn undo(&mut self) -> Option<Op> {
        let op = self.undo.pop()?;
        self.redo.push(op.clone());
        Some(op)
    }

    pub fn redo(&mut self) -> Option<Op> {
        let op = self.redo.pop()?;
        self.undo.push(op.clone());
        Some(op)
    }

    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }
}

/// Parse one brief file's H1 subject and metadata block.
///
/// The format is materialized by `tools/fold.py` in workcat-db:
///
/// ```text
/// # Subject line
///
/// - id: <uuid>
/// - ref: <ref>
/// - status: <status>
/// - created_at: ...
/// - updated_at: ...
/// - depends_on: []            (or a nested list of brief filenames)
/// ```
pub fn parse_brief(text: &str) -> Option<ItemMeta> {
    let mut subject = None;
    let mut id = None;
    let mut reference = None;
    let mut status = None;
    let mut updated_at = None;
    let mut depends_on = Vec::new();
    let mut in_depends = false;
    let mut in_sections = false;
    let mut sections: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            // Metadata block is over once brief sections begin.
            in_sections = true;
            sections.push((heading.trim().to_string(), String::new()));
            continue;
        }
        if in_sections {
            if let Some((_, body)) = sections.last_mut() {
                if !body.is_empty() {
                    body.push('\n');
                }
                body.push_str(line);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("# ") {
            if subject.is_none() {
                subject = Some(rest.trim().to_string());
            }
            continue;
        }
        if in_depends {
            if let Some(entry) = line.strip_prefix("  - ") {
                depends_on.push(dep_id8(entry.trim()));
                continue;
            }
            in_depends = false;
        }
        let Some(rest) = line.strip_prefix("- ") else {
            continue;
        };
        if let Some(value) = rest.strip_prefix("id: ") {
            id = Some(value.trim().to_string());
        } else if let Some(value) = rest.strip_prefix("ref: ") {
            reference = Some(value.trim().to_string());
        } else if let Some(value) = rest.strip_prefix("status: ") {
            status = Status::parse(value.trim());
        } else if let Some(value) = rest.strip_prefix("updated_at: ") {
            updated_at = Some(value.trim().to_string());
        } else if rest.trim_end() == "depends_on:" {
            in_depends = true;
        }
    }
    for (_, body) in &mut sections {
        let trimmed = body.trim();
        *body = trimmed.to_string();
    }
    Some(ItemMeta {
        id: id?,
        subject: subject?,
        reference: reference?,
        status: status?,
        updated_at,
        depends_on,
        sections,
    })
}

/// A `depends_on` entry is either a brief filename
/// (`<id8>-<slug>.md`) or, for a dangling edge, a bare uuid. Either
/// way the first 8 characters are the dep's id8.
fn dep_id8(entry: &str) -> String {
    entry.chars().take(8).collect()
}

/// Fold `node_moved` events (last-write-wins per item id) from raw
/// JSON-lines. Lines that fail to parse or are not `node_moved` are
/// ignored; callers feed every log line in log order.
pub fn fold_node_positions<'a>(
    lines: impl Iterator<Item = &'a str>,
) -> HashMap<String, (f32, f32)> {
    let mut positions = HashMap::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("kind").and_then(|k| k.as_str()) != Some("node_moved") {
            continue;
        }
        let (Some(id), Some(x), Some(y)) = (
            value.get("id").and_then(|v| v.as_str()),
            value.get("x").and_then(|v| v.as_f64()),
            value.get("y").and_then(|v| v.as_f64()),
        ) else {
            continue;
        };
        positions.insert(id.to_string(), (x as f32, y as f32));
    }
    positions
}

/// Merge append-only event logs without a `git stash pop`. `committed`
/// is kept verbatim (its order, including the identical-timestamp epoch
/// block, is preserved); then every line present in `working` or
/// `stash` but not already committed is appended, de-duplicated and
/// sorted by its `ts` field. Ordering by `ts` makes the fold's
/// log-order last-write-wins correct even when a status change and its
/// successor were split across the working tree and the stash.
///
/// This replaces the checkpoint's `git stash pop`, which conflicts (and
/// strands events) whenever a concurrent append lands on the log during
/// the checkpoint window.
pub fn merge_append_log(committed: &str, working: &str, stash: &str) -> String {
    let committed_lines: Vec<&str> = committed
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let committed_set: HashSet<&str> = committed_lines.iter().copied().collect();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut appends: Vec<&str> = Vec::new();
    for line in working.lines().chain(stash.lines()) {
        if line.trim().is_empty() || committed_set.contains(line) || !seen.insert(line) {
            continue;
        }
        appends.push(line);
    }
    appends.sort_by_key(|line| ts_key(line));
    let mut out = String::new();
    for line in committed_lines.iter().chain(appends.iter()) {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The `ts` field of an event line, for ordering merged appends. Lines
/// that don't parse or lack a `ts` sort first (empty key), which only
/// affects malformed lines.
fn ts_key(line: &str) -> String {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|value| {
            value
                .get("ts")
                .and_then(|ts| ts.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_append_log_unions_and_ts_orders_without_conflict() {
        // committed has the epoch block (identical ts, must keep order)
        // plus one real event.
        let committed = concat!(
            r#"{"ts":"2026-07-07T16:53:00.5+00:00","actor":"m","kind":"item_snapshot","id":"a"}"#,
            "\n",
            r#"{"ts":"2026-07-07T16:53:00.5+00:00","actor":"m","kind":"item_snapshot","id":"b"}"#,
            "\n",
            r#"{"ts":"2026-07-08T01:00:00+00:00","kind":"status_set","id":"x","status":"started"}"#,
            "\n",
        );
        // The working tree gained a concurrent append (the successor)
        // during the checkpoint window...
        let working = concat!(
            r#"{"ts":"2026-07-08T01:00:00+00:00","kind":"status_set","id":"x","status":"started"}"#,
            "\n",
            r#"{"ts":"2026-07-08T02:00:02+00:00","kind":"status_set","id":"x","status":"implemented"}"#,
            "\n",
        );
        // ...while the stash holds the earlier pending event (its
        // predecessor), out of order relative to the working append.
        let stash = concat!(
            r#"{"ts":"2026-07-08T01:00:00+00:00","kind":"status_set","id":"x","status":"started"}"#,
            "\n",
            r#"{"ts":"2026-07-08T02:00:01+00:00","kind":"status_set","id":"x","status":"unknown"}"#,
            "\n",
        );
        let merged = merge_append_log(committed, working, stash);
        let lines: Vec<&str> = merged.lines().collect();
        // Epoch block order preserved; committed real event kept once.
        assert!(lines[0].contains(r#""id":"a""#));
        assert!(lines[1].contains(r#""id":"b""#));
        assert_eq!(lines.len(), 5, "3 committed + 2 unique appends");
        // The two new appends are ts-ordered: unknown (02:00:01) before
        // implemented (02:00:02), so the fold's last-write is implemented.
        let unknown = lines.iter().position(|l| l.contains("unknown")).unwrap();
        let implemented = lines.iter().position(|l| l.contains("implemented")).unwrap();
        assert!(unknown < implemented);
    }
    use pretty_assertions::assert_eq;

    const BRIEF: &str = "\
# Bootstrap the workcat repo-as-database substrate (epoch zero)

- id: 8b0ce58d-5d07-5d7b-9b28-db7e670fd7f6
- ref: agent_notes/2026-07-06/workcat-substrate-bootstrap-HANDOFF.md
- status: not_started
- created_at: 2026-07-07 05:24:05
- updated_at: 2026-07-07T16:53:00.562099+00:00
- depends_on:
  - e89113a0-run-the-workcat-commit-grain-benchmark-dr-004-ruli.md

## State

Not started.

## Next

- id: not-a-real-id (must not be parsed; sections are past the block)
";

    #[test]
    fn parses_brief_metadata() {
        let item = parse_brief(BRIEF).unwrap();
        assert_eq!(
            item,
            ItemMeta {
                id: "8b0ce58d-5d07-5d7b-9b28-db7e670fd7f6".into(),
                subject: "Bootstrap the workcat repo-as-database substrate (epoch zero)".into(),
                reference: "agent_notes/2026-07-06/workcat-substrate-bootstrap-HANDOFF.md".into(),
                status: Status::NotStarted,
                updated_at: Some("2026-07-07T16:53:00.562099+00:00".into()),
                depends_on: vec!["e89113a0".into()],
                sections: vec![
                    ("State".into(), "Not started.".into()),
                    (
                        "Next".into(),
                        "- id: not-a-real-id (must not be parsed; sections are past the block)"
                            .into()
                    ),
                ],
            }
        );
        assert_eq!(item.id8(), "8b0ce58d");
    }

    #[test]
    fn parses_notes_section() {
        let text = "# S\n\n- id: abc\n- ref: r\n- status: started\n- depends_on: []\n\n\
            ## State\n\nGoing.\n\n## Notes\n\nA note line.\nSecond line.\n";
        let item = parse_brief(text).unwrap();
        assert_eq!(item.notes(), Some("A note line.\nSecond line."));
        assert_eq!(item.sections.len(), 2);
    }

    #[test]
    fn parses_empty_depends_on() {
        let text = "# S\n\n- id: abc\n- ref: r\n- status: merged\n- depends_on: []\n\n## State\n";
        let item = parse_brief(text).unwrap();
        assert_eq!(item.status, Status::Merged);
        assert!(item.depends_on.is_empty());
    }

    #[test]
    fn missing_fields_yield_none() {
        assert_eq!(parse_brief("# Subject only\n"), None);
        assert_eq!(parse_brief("- id: x\n- ref: r\n- status: started\n"), None);
    }

    #[test]
    fn unknown_status_yields_none() {
        let text = "# S\n\n- id: abc\n- ref: r\n- status: bogus\n";
        assert_eq!(parse_brief(text), None);
    }

    #[test]
    fn dep_entries_reduce_to_id8() {
        assert_eq!(dep_id8("e89113a0-some-slug.md"), "e89113a0");
        assert_eq!(dep_id8("8b0ce58d-5d07-5d7b-9b28-db7e670fd7f6"), "8b0ce58d");
    }

    #[test]
    fn status_round_trips() {
        for status in ALL_STATUSES {
            assert_eq!(Status::parse(status.as_str()), Some(status));
        }
        assert_eq!(Status::parse("bogus"), None);
    }

    #[test]
    fn only_canceled_and_unknown_are_terminal_choices() {
        for status in ALL_STATUSES {
            assert_eq!(
                status.is_terminal_choice(),
                matches!(status, Status::Canceled | Status::Unknown),
                "{status:?}"
            );
        }
    }

    #[test]
    fn default_scope_hides_terminal_statuses() {
        let visible: Vec<_> = ALL_STATUSES
            .into_iter()
            .filter(|s| s.in_default_scope())
            .collect();
        assert_eq!(
            visible,
            vec![
                Status::NotStarted,
                Status::Started,
                Status::Paused,
                Status::Blocked,
                Status::Implemented,
                Status::Merged,
                Status::Unknown,
            ]
        );
    }

    fn item(subject: &str, reference: &str, status: Status) -> ItemMeta {
        ItemMeta {
            id: format!("{subject}-id"),
            subject: subject.into(),
            reference: reference.into(),
            status,
            updated_at: None,
            depends_on: Vec::new(),
            sections: Vec::new(),
        }
    }

    #[test]
    fn default_filter_matches_default_scope() {
        let filter = FilterState::default();
        assert!(filter.is_default());
        assert!(filter.matches(&item("a", "r", Status::Started)));
        assert!(filter.matches(&item("a", "r", Status::Unknown)));
        assert!(!filter.matches(&item("a", "r", Status::Complete)));
        assert!(!filter.matches(&item("a", "r", Status::Canceled)));
    }

    #[test]
    fn filter_query_matches_subject_and_ref_case_insensitive() {
        let mut filter = FilterState::default();
        filter.query = "CONNECTOR".into();
        assert!(filter.matches(&item("Fix the eda connector", "r", Status::Started)));
        assert!(filter.matches(&item("a", "notes/connector-fix.md", Status::Started)));
        assert!(!filter.matches(&item("unrelated", "r", Status::Started)));
        filter.query = "  ".into();
        assert!(filter.matches(&item("anything", "r", Status::Started)));
    }

    #[test]
    fn filter_toggle_status_round_trips() {
        let mut filter = FilterState::default();
        filter.toggle_status(Status::Complete);
        assert!(filter.matches(&item("a", "r", Status::Complete)));
        assert!(!filter.is_default());
        filter.toggle_status(Status::Complete);
        assert!(!filter.matches(&item("a", "r", Status::Complete)));
        assert!(filter.is_default());
    }

    #[test]
    fn lenses_fold_last_write_wins_and_delete() {
        let lines = [
            r#"{"kind":"lens_saved","name":"blocked","visible_statuses":["blocked"],"query":""}"#,
            r#"{"kind":"lens_saved","name":"eda","visible_statuses":["started","paused"],"query":"eda"}"#,
            r#"{"kind":"node_moved","id":"x","x":1,"y":2}"#,
            r#"{"kind":"lens_saved","name":"blocked","visible_statuses":["blocked","paused"],"query":"q"}"#,
            r#"{"kind":"lens_deleted","name":"eda"}"#,
            "garbage",
        ];
        let lenses = fold_lenses(lines.into_iter());
        assert_eq!(lenses.len(), 1);
        let lens = &lenses["blocked"];
        assert_eq!(lens.visible_statuses, vec!["blocked", "paused"]);
        assert_eq!(lens.query, "q");
        let filter = lens.to_filter();
        assert!(filter.visible_statuses.contains(&Status::Blocked));
        assert!(filter.visible_statuses.contains(&Status::Paused));
        assert_eq!(filter.visible_statuses.len(), 2);
    }

    #[test]
    fn undo_stack_round_trips_and_caps() {
        let mut stack = UndoStack::default();
        let move_op = Op::Move(vec![("id-a".into(), (0.0, 0.0), (5.0, 5.0))]);
        let status_op = Op::SetStatus("id-a".into(), Status::Started, Status::Merged);
        stack.push(move_op.clone());
        stack.push(status_op.clone());
        assert_eq!(stack.undo(), Some(status_op.clone()));
        assert_eq!(stack.redo(), Some(status_op.clone()));
        assert_eq!(stack.undo(), Some(status_op));
        assert_eq!(stack.undo(), Some(move_op.clone()));
        assert_eq!(stack.undo(), None);
        assert_eq!(stack.redo_len(), 2);
        // A new push clears redo.
        stack.push(move_op);
        assert_eq!(stack.redo_len(), 0);
        // Capacity: oldest entries fall off.
        for i in 0..(UNDO_CAPACITY + 10) {
            stack.push(Op::SetStatus(
                format!("id-{i}"),
                Status::Started,
                Status::Paused,
            ));
        }
        assert_eq!(stack.undo_len(), UNDO_CAPACITY);
    }

    #[test]
    fn fold_positions_last_write_wins() {
        let lines = [
            r#"{"ts":"t1","actor":"a","kind":"node_moved","id":"aaa","x":1.0,"y":2.0}"#,
            r#"{"ts":"t2","actor":"a","kind":"status_set","id":"aaa","status":"started"}"#,
            r#"{"ts":"t3","actor":"a","kind":"node_moved","id":"bbb","x":9,"y":8}"#,
            "not json at all",
            r#"{"ts":"t4","actor":"a","kind":"node_moved","id":"aaa","x":5.5,"y":6.5}"#,
            r#"{"kind":"node_moved","id":"ccc"}"#,
            "",
        ];
        let positions = fold_node_positions(lines.into_iter());
        assert_eq!(positions.len(), 2);
        assert_eq!(positions["aaa"], (5.5, 6.5));
        assert_eq!(positions["bbb"], (9.0, 8.0));
    }
}
