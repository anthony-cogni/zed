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
    /// The extracted ask or offer, verbatim (trimmed to ~140 chars), when
    /// present. A `Done` state may still carry a soft offer here.
    pub ask: Option<String>,
}

/// An extracted ask, tagged by whether it demands the user's attention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub text: String,
    /// Hard asks demand attention (NeedsYou); soft offers do not.
    pub hard: bool,
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
///
/// Hard markers demand the user's attention; soft markers are offers that
/// leave the thread effectively done. Label-style markers are stripped from
/// the captured ask; the others are kept because they are part of the
/// question itself.
const HARD_LABEL_MARKERS: &[&str] = &["what i need from you", "i need from you"];
const HARD_QUESTION_MARKERS: &[&str] = &[
    "do you want",
    "should i",
    "yes/no",
    "please test",
    "please confirm",
];
const SOFT_MARKERS: &[&str] = &[
    "let me know",
    "want me to",
    "say the word",
    "if you'd like",
    "if you\u{2019}d like",
];

static MERGE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)merge (PR|#)|PR #\d+|open(ed)? a (PR|pull request)|did not commit|uncommitted|haven't committed|awaiting review",
    )
    .expect("merge regex is valid")
});

/// Negations that disqualify a matched marker from counting as an ask,
/// e.g. "What I need from you: nothing required".
static NEGATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)nothing( is)? (required|needed|further)|no action (needed|required)|nothing from you",
    )
    .expect("negation regex is valid")
});

/// Decide which attention bucket a thread belongs in.
///
/// Priority order (from research on real thread history):
/// 1. Running: user spoke last, or the agent's final message ends mid-work.
/// 2. NeedsYou: the final agent message carries a HARD trailer ask (a
///    negated marker like "What I need from you: nothing required" does
///    not count).
/// 3. AwaitingMerge: the final agent message uses merge/commit/review
///    language. When both a hard ask and merge language exist,
///    AwaitingMerge wins only if the ask itself is about merging.
/// 4. Done: completed summary. A soft offer ("Let me know...", "Say the
///    word...") does not demand attention, but its text is kept in `ask`
///    so the panel can still show it.
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

    match ask {
        Some(Ask {
            text: ask,
            hard: true,
        }) => {
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
        soft_or_none => {
            let offer = soft_or_none.map(|ask| ask.text);
            if has_merge_language {
                Triage {
                    state: TriageState::AwaitingMerge,
                    ask: offer,
                }
            } else {
                Triage {
                    state: TriageState::Done,
                    ask: offer,
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Candidate {
    start: usize,
    marker: &'static str,
    is_label: bool,
    hard: bool,
}

/// Extract the ask left in the final agent message, preferring the LAST
/// non-negated HARD marker, then the last non-negated soft marker. Returns
/// the cleaned sentence/line, trimmed to ~140 chars.
pub fn extract_ask(text: &str) -> Option<Ask> {
    let lower = text.to_ascii_lowercase();

    let mut candidates = Vec::new();
    for (markers, is_label, hard) in [
        (HARD_LABEL_MARKERS, true, true),
        (HARD_QUESTION_MARKERS, false, true),
        (SOFT_MARKERS, false, false),
    ] {
        for &marker in markers {
            let mut offset = 0;
            while let Some(found) = lower[offset..].find(marker) {
                let start = offset + found;
                if is_word_boundary(&lower, start, start + marker.len()) {
                    candidates.push(Candidate {
                        start,
                        marker,
                        is_label,
                        hard,
                    });
                }
                offset = start + 1;
            }
        }
    }

    // Hard asks always win over soft offers, regardless of position.
    for hard_pass in [true, false] {
        let mut pass: Vec<Candidate> = candidates
            .iter()
            .copied()
            .filter(|candidate| candidate.hard == hard_pass)
            .collect();
        pass.sort_by(|a, b| b.start.cmp(&a.start));
        let mut skipped_lines: Vec<usize> = Vec::new();
        for anchor in &pass {
            let (line_start, line_end) = line_bounds(text, anchor.start);
            if skipped_lines.contains(&line_start) {
                continue;
            }
            // When several markers share a line ("Do you want me to re-run
            // the gauntlet? (yes/no)"), capture from the earliest one so
            // the whole sentence is kept.
            let leader = pass
                .iter()
                .filter(|candidate| candidate.start >= line_start && candidate.start < line_end)
                .min_by_key(|candidate| candidate.start)
                .copied()
                .unwrap_or(*anchor);
            let Some(ask) = capture_ask(text, &leader) else {
                skipped_lines.push(line_start);
                continue;
            };
            if NEGATION_RE.is_match(&ask) {
                skipped_lines.push(line_start);
                continue;
            }
            return Some(Ask {
                text: ask,
                hard: hard_pass,
            });
        }
    }
    None
}

fn is_word_boundary(lower: &str, start: usize, end: usize) -> bool {
    let bytes = lower.as_bytes();
    let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
    let after_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
    before_ok && after_ok
}

fn line_bounds(text: &str, pos: usize) -> (usize, usize) {
    let start = text[..pos].rfind('\n').map_or(0, |newline| newline + 1);
    let end = text[pos..]
        .find('\n')
        .map_or(text.len(), |newline| pos + newline);
    (start, end)
}

fn capture_ask(text: &str, candidate: &Candidate) -> Option<String> {
    let rest = &text[candidate.start..];
    let source = if candidate.is_label {
        &rest[candidate.marker.len()..]
    } else {
        rest
    };
    let line = source
        .lines()
        .map(clean_ask_line)
        .find(|line| !line.is_empty())?;
    Some(truncate_chars(trim_to_sentence(&line), MAX_ASK_LEN))
}

/// Cut a captured line after a `?` when a new sentence follows it, so an
/// ask like "Should I raise the cap? The rest is committed." keeps only
/// the question. A trailing parenthetical like "(yes/no)" is preserved.
fn trim_to_sentence(line: &str) -> &str {
    for (index, _) in line.match_indices('?') {
        let mut rest = line[index + 1..].chars();
        if rest.next() == Some(' ')
            && let Some(next) = rest.next()
            && next.is_uppercase()
        {
            return &line[..=index];
        }
    }
    line
}

/// Strip markdown emphasis (`**`) and leading/trailing punctuation residue
/// (asterisks, colons, dashes, quotes, stray sentence punctuation) from a
/// captured line. A line that is nothing but residue cleans to "".
fn clean_ask_line(line: &str) -> String {
    let line = line.replace("**", "");
    line.trim_start_matches(|c: char| {
        matches!(c, '*' | ':' | '-' | '—' | '"' | ')' | '(' | ',' | '.') || c.is_whitespace()
    })
    .trim_end_matches(|c: char| matches!(c, '*' | ':') || c.is_whitespace())
    .to_string()
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
                name: "soft offer only triages done but keeps the offer text",
                input: agent_input(
                    "The Tailscale entry is updated and validated against the live host. \
                     Say the word and I'll apply the same change to the Twingate config.",
                ),
                expected_state: TriageState::Done,
                expected_ask: Some(
                    "Say the word and I'll apply the same change to the Twingate config.",
                ),
            },
            Case {
                name: "markdown-bold trailer is cleaned",
                input: agent_input(
                    "What I set out to do: land the regression suite. What I did: all \
                     cells green, branch pushed, PR opened. What is left: nothing. \
                     **What I need from you**: merge PR #134",
                ),
                expected_state: TriageState::AwaitingMerge,
                expected_ask: Some("merge PR #134"),
            },
            Case {
                name: "negated trailer does not count as an ask",
                input: agent_input(
                    "The dashboard is live and the lens events are flowing end to end. \
                     What I need from you: nothing required \u{2014} go try the merged \
                     behavior on the live dashboard.",
                ),
                expected_state: TriageState::Done,
                expected_ask: None,
            },
            Case {
                name: "nothing further negation with soft follow-up mention",
                input: agent_input(
                    "The catalogue rows are reconciled and the export is committed. \
                     **What I need from you**: Nothing further on this item. Optional \
                     follow-up: if you'd prefer the report inline, I can restructure it.",
                ),
                expected_state: TriageState::Done,
                expected_ask: None,
            },
            Case {
                name: "soft let-me-know with nothing-required is plain done",
                input: agent_input(
                    "All twelve call sites are updated and the suite passes. Let me \
                     know if anything surprises you, but nothing is needed from your side.",
                ),
                expected_state: TriageState::Done,
                expected_ask: None,
            },
            Case {
                name: "hard ask beats a later soft offer",
                input: agent_input(
                    "Should I raise the connection cap to 64? The rest is committed. \
                     Let me know if the naming reads oddly.",
                ),
                expected_state: TriageState::NeedsYou,
                expected_ask: Some("Should I raise the connection cap to 64?"),
            },
            Case {
                name: "please test is a hard ask",
                input: agent_input(
                    "The session-initialization fix is deployed to the dev stack and \
                     the smoke checks pass. Please test the ACP flow end to end.",
                ),
                expected_state: TriageState::NeedsYou,
                expected_ask: Some("Please test the ACP flow end to end."),
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
        assert!(ask.hard);
        assert_eq!(ask.text.chars().count(), 140);
        assert!(ask.text.ends_with('…'));
    }

    #[test]
    fn ask_on_following_line_is_captured() {
        let text = "Everything else is green.\nWhat I need from you:\n- merge PR #7\n";
        let ask = extract_ask(text).unwrap();
        assert_eq!(ask.text, "merge PR #7");
        let result = triage(&agent_input(text));
        assert_eq!(result.state, TriageState::AwaitingMerge);
    }

    #[test]
    fn markers_inside_words_do_not_match() {
        // "should i" must not match inside "should include".
        let text = "The generated manifest should include every fixture; all of \
                    that is verified by the new round-trip test suite.";
        assert_eq!(extract_ask(text), None);
        assert_eq!(triage(&agent_input(text)).state, TriageState::Done);
    }

    #[test]
    fn multi_marker_line_captures_whole_sentence() {
        let text = "The rest is green. Do you want me to re-run the gauntlet? (yes/no)";
        let ask = extract_ask(text).unwrap();
        assert!(ask.hard);
        assert_eq!(ask.text, "Do you want me to re-run the gauntlet? (yes/no)");
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
