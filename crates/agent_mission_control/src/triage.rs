//! Pure triage logic for agent threads.
//!
//! Given a digest of the final state of a thread (who spoke last, what the
//! final agent message said), decide which attention bucket the thread
//! belongs in and extract the concrete "ask" the agent left for the user.
//!
//! This module is deliberately free of IO and gpui types so it can be tested
//! exhaustively with plain unit tests.

use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use regex::Regex;

/// Who authored a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Agent,
}

/// The attention bucket a thread belongs in, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TriageState {
    /// The final agent message asks the user for something.
    NeedsYou,
    /// The agent finished work that is waiting on a merge/commit/review.
    AwaitingMerge,
    /// The agent has not replied yet, or its final message ends mid-work.
    Running,
    /// The final agent message reads as a completed summary with no ask.
    Done,
}

impl TriageState {
    pub fn label(&self) -> &'static str {
        match self {
            TriageState::NeedsYou => "Needs you",
            TriageState::AwaitingMerge => "Awaiting merge",
            TriageState::Running => "Running",
            TriageState::Done => "Done",
        }
    }

    pub const ALL: [TriageState; 4] = [
        TriageState::NeedsYou,
        TriageState::AwaitingMerge,
        TriageState::Running,
        TriageState::Done,
    ];
}

/// The result of triaging a thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Triage {
    pub state: TriageState,
    /// The extracted ask, verbatim (trimmed to ~140 chars), when present.
    pub ask: Option<String>,
}

/// Everything triage needs to know about the end of a thread.
#[derive(Debug, Clone, Copy)]
pub struct TriageInput<'a> {
    /// Role of the very last message in the thread, if any.
    pub last_role: Option<Role>,
    /// Concatenated text segments of the last agent message ("" if none).
    pub final_agent_text: &'a str,
    /// Whether the last agent message contains tool uses (ends mid-work).
    pub final_agent_has_tool_use: bool,
}

/// A final agent message shorter than this, with no ask and no merge
/// language, is considered a mid-work stub (the agent got cut off).
const STUB_LEN: usize = 30;

/// Maximum length (in chars) of an extracted ask.
const MAX_ASK_LEN: usize = 140;

/// Trailer markers that signal an ask, searched case-insensitively.
/// Label-style markers are stripped from the captured ask; question-style
/// markers are kept because they are part of the question itself.
const LABEL_MARKERS: [&str; 2] = ["what i need from you", "i need from you"];
const QUESTION_MARKERS: [&str; 5] = [
    "want me to",
    "say the word",
    "should i",
    "do you want",
    "let me know",
];

static MERGE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)merge (PR|#)|PR #\d+|open(ed)? a (PR|pull request)|did not commit|uncommitted|haven't committed|awaiting review",
    )
    .expect("merge regex is valid")
});

/// Decide which attention bucket a thread belongs in.
///
/// Priority order (from research on real thread history):
/// 1. Running: user spoke last, or the agent's final message ends mid-work.
/// 2. NeedsYou: the final agent message carries a structured trailer ask.
/// 3. AwaitingMerge: the final agent message uses merge/commit/review
///    language. When both an ask and merge language exist, AwaitingMerge
///    wins only if the ask itself is about merging.
/// 4. Done: completed summary, no ask.
pub fn triage(input: &TriageInput) -> Triage {
    let text = input.final_agent_text.trim();

    // Rule 1: Running.
    match input.last_role {
        None | Some(Role::User) => {
            return Triage {
                state: TriageState::Running,
                ask: None,
            };
        }
        Some(Role::Agent) => {}
    }
    if input.final_agent_has_tool_use {
        return Triage {
            state: TriageState::Running,
            ask: None,
        };
    }

    let ask = extract_ask(text);
    let has_merge_language = MERGE_RE.is_match(text);

    // A very short stub with no ask and no merge language means the agent
    // stopped mid-work.
    if text.chars().count() < STUB_LEN && ask.is_none() && !has_merge_language {
        return Triage {
            state: TriageState::Running,
            ask: None,
        };
    }

    match (ask, has_merge_language) {
        (Some(ask), true) => {
            if MERGE_RE.is_match(&ask) {
                Triage {
                    state: TriageState::AwaitingMerge,
                    ask: Some(ask),
                }
            } else {
                Triage {
                    state: TriageState::NeedsYou,
                    ask: Some(ask),
                }
            }
        }
        (Some(ask), false) => Triage {
            state: TriageState::NeedsYou,
            ask: Some(ask),
        },
        (None, true) => Triage {
            state: TriageState::AwaitingMerge,
            ask: None,
        },
        (None, false) => Triage {
            state: TriageState::Done,
            ask: None,
        },
    }
}

/// Extract the ask following the LAST occurrence of any trailer marker in
/// the final agent message. Returns the sentence/line, trimmed to ~140 chars.
pub fn extract_ask(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();

    #[derive(Clone, Copy)]
    struct Candidate {
        start: usize,
        end: usize,
        marker: &'static str,
        is_label: bool,
    }

    let mut candidates = Vec::new();
    for (markers, is_label) in [(&LABEL_MARKERS[..], true), (&QUESTION_MARKERS[..], false)] {
        for &marker in markers {
            if let Some(start) = lower.rfind(marker) {
                candidates.push(Candidate {
                    start,
                    end: start + marker.len(),
                    marker,
                    is_label,
                });
            }
        }
    }

    let mut chosen = *candidates.iter().max_by_key(|c| c.start)?;
    // If another marker starts earlier and overlaps the chosen one (e.g.
    // "Do you want" contains the start of "want me to"), prefer the
    // earlier, enclosing marker so the ask keeps the whole phrase.
    loop {
        match candidates
            .iter()
            .filter(|c| c.start < chosen.start && c.end > chosen.start)
            .min_by_key(|c| c.start)
        {
            Some(&outer) => chosen = outer,
            None => break,
        }
    }

    let (pos, marker, is_label) = (chosen.start, chosen.marker, chosen.is_label);
    let rest = &text[pos..];
    let capture = if is_label {
        rest[marker.len()..]
            .trim_start_matches(|c: char| c == ':' || c == '-' || c == '—' || c.is_whitespace())
    } else {
        rest
    };

    let line = capture.lines().find(|line| !line.trim().is_empty())?.trim();
    if line.is_empty() {
        return None;
    }
    Some(truncate_chars(line, MAX_ASK_LEN))
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut truncated: String = text.chars().take(max.saturating_sub(1)).collect();
        truncated.push('…');
        truncated
    }
}

/// Compact relative age like "2h" or "3d".
pub fn relative_age(updated_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let delta = now.signed_duration_since(updated_at);
    let minutes = delta.num_minutes();
    if minutes < 1 {
        "now".to_string()
    } else if minutes < 60 {
        format!("{}m", minutes)
    } else if minutes < 60 * 24 {
        format!("{}h", delta.num_hours())
    } else {
        format!("{}d", delta.num_days())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;
    use pretty_assertions::assert_eq;

    fn agent_input(text: &str) -> TriageInput<'_> {
        TriageInput {
            last_role: Some(Role::Agent),
            final_agent_text: text,
            final_agent_has_tool_use: false,
        }
    }

    #[test]
    fn table_driven_triage() {
        struct Case {
            name: &'static str,
            input: TriageInput<'static>,
            expected_state: TriageState,
            expected_ask: Option<&'static str>,
        }

        let cases = vec![
            Case {
                name: "user spoke last means running",
                input: TriageInput {
                    last_role: Some(Role::User),
                    final_agent_text: "",
                    final_agent_has_tool_use: false,
                },
                expected_state: TriageState::Running,
                expected_ask: None,
            },
            Case {
                name: "empty thread means running",
                input: TriageInput {
                    last_role: None,
                    final_agent_text: "",
                    final_agent_has_tool_use: false,
                },
                expected_state: TriageState::Running,
                expected_ask: None,
            },
            Case {
                name: "final agent message with tool use is mid-work",
                input: TriageInput {
                    last_role: Some(Role::Agent),
                    final_agent_text: "Let me check the build output before continuing. \
                        I will run the tests next and report back with results.",
                    final_agent_has_tool_use: true,
                },
                expected_state: TriageState::Running,
                expected_ask: None,
            },
            Case {
                name: "very short stub is mid-work",
                input: agent_input("Looking into it now."),
                expected_state: TriageState::Running,
                expected_ask: None,
            },
            Case {
                name: "wikipedia-style trailer with merge ask",
                input: agent_input(
                    "What I set out to do: wire the connector regression into CI. \
                     What I did: added the workflow, fixed the flaky fixture, and got \
                     checks green on the branch. What is left: nothing on my side. \
                     What I need from you: merge PR #134",
                ),
                expected_state: TriageState::AwaitingMerge,
                expected_ask: Some("merge PR #134"),
            },
            Case {
                name: "trailer ask that is not about merging",
                input: agent_input(
                    "All three suites pass locally and the fix is committed. \
                     What I need from you: decide whether we raise the turn limit to 50 \
                     or keep 40 and split the search into two passes.",
                ),
                expected_state: TriageState::NeedsYou,
                expected_ask: Some(
                    "decide whether we raise the turn limit to 50 or keep 40 and split the search into two passes.",
                ),
            },
            Case {
                name: "question-style ask keeps the question text",
                input: agent_input(
                    "The gauntlet failed twice on the same seed, which looks environmental \
                     rather than a real regression. Do you want me to re-run the gauntlet?",
                ),
                expected_state: TriageState::NeedsYou,
                expected_ask: Some("Do you want me to re-run the gauntlet?"),
            },
            Case {
                name: "merge language without a trailer ask",
                input: agent_input(
                    "Everything is implemented and validated. I opened a PR with the \
                     changes; CI is green and it is awaiting review.",
                ),
                expected_state: TriageState::AwaitingMerge,
                expected_ask: None,
            },
            Case {
                name: "uncommitted work is awaiting merge",
                input: agent_input(
                    "The refactor is complete and all 42 tests pass. Note that I did not \
                     commit the migration file; the changes are still uncommitted on the \
                     worktree.",
                ),
                expected_state: TriageState::AwaitingMerge,
                expected_ask: None,
            },
            Case {
                name: "ask trailer plus merge language, ask wins when not about merge",
                input: agent_input(
                    "I opened a PR with the fix and CI is green. \
                     What I need from you: confirm the new default timeout of 30s is acceptable.",
                ),
                expected_state: TriageState::NeedsYou,
                expected_ask: Some("confirm the new default timeout of 30s is acceptable."),
            },
            Case {
                name: "plain done summary",
                input: agent_input(
                    "Renamed the module, updated all twelve call sites, and verified the \
                     full test suite passes. The documentation now reflects the new name \
                     everywhere it was referenced.",
                ),
                expected_state: TriageState::Done,
                expected_ask: None,
            },
            Case {
                name: "last marker occurrence wins",
                input: agent_input(
                    "Let me know if the naming bothers you. I also cleaned up the fixtures. \
                     Should I delete the legacy snapshot directory as well?",
                ),
                expected_state: TriageState::NeedsYou,
                expected_ask: Some("Should I delete the legacy snapshot directory as well?"),
            },
            Case {
                name: "say the word marker",
                input: agent_input(
                    "The Tailscale entry is updated and validated against the live host. \
                     Say the word and I'll apply the same change to the Twingate config.",
                ),
                expected_state: TriageState::NeedsYou,
                expected_ask: Some(
                    "Say the word and I'll apply the same change to the Twingate config.",
                ),
            },
        ];

        for case in cases {
            let result = triage(&case.input);
            assert_eq!(
                result.state, case.expected_state,
                "state for: {}",
                case.name
            );
            assert_eq!(
                result.ask.as_deref(),
                case.expected_ask,
                "ask for: {}",
                case.name
            );
        }
    }

    #[test]
    fn ask_is_truncated_to_140_chars() {
        let long_tail = "x".repeat(300);
        let text = format!("Work is done. What I need from you: {}", long_tail);
        let ask = extract_ask(&text).unwrap();
        assert_eq!(ask.chars().count(), 140);
        assert!(ask.ends_with('…'));
    }

    #[test]
    fn ask_on_following_line_is_captured() {
        let text = "Everything else is green.\nWhat I need from you:\n- merge PR #7\n";
        let ask = extract_ask(text).unwrap();
        assert_eq!(ask, "merge PR #7");
        let result = triage(&agent_input(text));
        assert_eq!(result.state, TriageState::AwaitingMerge);
    }

    #[test]
    fn relative_ages() {
        let now = Utc.with_ymd_and_hms(2026, 7, 6, 12, 0, 0).unwrap();
        let cases = [
            (now - chrono::Duration::seconds(20), "now"),
            (now - chrono::Duration::minutes(5), "5m"),
            (now - chrono::Duration::hours(2), "2h"),
            (now - chrono::Duration::days(3), "3d"),
        ];
        for (updated, expected) in cases {
            assert_eq!(relative_age(updated, now), expected);
        }
    }
}
