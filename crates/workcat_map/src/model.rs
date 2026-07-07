//! Pure core: workcat item model, brief metadata parsing, the default
//! map scope, and the position fold over `node_moved` events.
//!
//! No IO and no GPUI types live here, so everything is unit-testable.

use std::collections::HashMap;

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
    /// Dependencies as id8 prefixes (first 8 hex digits of the dep uuid).
    pub depends_on: Vec<String>,
}

impl ItemMeta {
    pub fn id8(&self) -> &str {
        &self.id[..self.id.len().min(8)]
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
    let mut depends_on = Vec::new();
    let mut in_depends = false;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# ") {
            if subject.is_none() {
                subject = Some(rest.trim().to_string());
            }
            continue;
        }
        if line.starts_with("## ") {
            // Metadata block is over once brief sections begin.
            break;
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
        } else if rest.trim_end() == "depends_on:" {
            in_depends = true;
        }
    }
    Some(ItemMeta {
        id: id?,
        subject: subject?,
        reference: reference?,
        status: status?,
        depends_on,
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

#[cfg(test)]
mod tests {
    use super::*;
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
                depends_on: vec!["e89113a0".into()],
            }
        );
        assert_eq!(item.id8(), "8b0ce58d");
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
