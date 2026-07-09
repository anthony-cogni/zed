//! Realistic fixture digests for tests and the headless visual binary.

use chrono::{Duration, Utc};

use crate::{Role, ThreadDigest};

fn digest(
    id: &str,
    title: &str,
    minutes_ago: i64,
    message_count: usize,
    last_role: Option<Role>,
    final_agent_text: &str,
    final_agent_has_tool_use: bool,
) -> ThreadDigest {
    ThreadDigest::new(
        id.to_string(),
        title.to_string(),
        Some(Utc::now() - Duration::minutes(minutes_ago)),
        message_count,
        last_role,
        final_agent_text.to_string(),
        final_agent_has_tool_use,
    )
}

/// Eleven realistic threads covering all four triage states.
pub fn fixture_digests() -> Vec<ThreadDigest> {
    vec![
        digest(
            "fixture-01",
            "movellus 6-cell: dev half + tlul test_0006",
            135,
            42,
            Some(Role::Agent),
            "What I set out to do: run the dev half of the 6-cell matrix plus the \
             tlul test_0006 regression. What I did: all six cells pass with the \
             three required assertions; pushed the branch and opened the PR with \
             green CI. What is left: nothing on my side. \
             What I need from you: merge PR #134",
            false,
        ),
        digest(
            "fixture-02",
            "Fix cgx search turn-limit crash",
            48,
            17,
            Some(Role::Agent),
            "The crash was an off-by-one in the turn accounting; the fix is small \
             and covered by a new regression test. All suites pass locally. \
             What I need from you: decide whether we raise the turn limit to 50 \
             or keep 40 and split the search into two passes.",
            false,
        ),
        digest(
            "fixture-03",
            "Gauntlet re-run after zstd bump",
            230,
            23,
            Some(Role::Agent),
            "The gauntlet failed twice on the same seed, which looks environmental \
             rather than a real regression from the zstd bump. \
             Do you want me to re-run the gauntlet? (yes/no)",
            false,
        ),
        digest(
            "fixture-04",
            "Rung 7 branch protection audit",
            2900,
            31,
            Some(Role::Agent),
            "The audit is complete and documented. I opened a pull request with the \
             protection changes; CI is green and it is awaiting review.",
            false,
        ),
        digest(
            "fixture-05",
            "Panel persistence migration",
            1500,
            26,
            Some(Role::Agent),
            "The migration runs cleanly against a copy of the production database \
             and all 58 tests pass. Note: I did not commit the generated migration \
             file; the changes are still uncommitted on the worktree pending your \
             review of the column rename.",
            false,
        ),
        digest(
            "fixture-06",
            "Refactor eda-connector job dispatch",
            8,
            11,
            Some(Role::User),
            "",
            false,
        ),
        digest(
            "fixture-07",
            "Visual test runner font fix",
            3,
            29,
            Some(Role::Agent),
            "The blank-text repro is confirmed: the headless context falls back to \
             NoopTextSystem when font-kit is disabled. Rebuilding with the feature \
             enabled now to verify the fix.",
            true,
        ),
        digest(
            "fixture-08",
            "VPN host access doc update",
            720,
            9,
            Some(Role::Agent),
            "The Tailscale section is rewritten and validated against the live \
             eda-lab host list. Say the word and I'll apply the same restructuring \
             to the Twingate section.",
            false,
        ),
        digest(
            "fixture-09",
            "Workspace dep dedupe sweep",
            4300,
            14,
            Some(Role::Agent),
            "Deduplicated eleven crates down to workspace versions, rebuilt from a \
             clean target directory, and verified the binary size dropped by 4%. \
             The lockfile diff contains only the expected removals.",
            false,
        ),
        digest(
            "fixture-11",
            "Connector retry backoff hardening",
            75,
            21,
            Some(Role::Agent),
            "What I set out to do: harden the connector retry backoff. What I did: \
             capped the exponent, added jitter, and property-tested the schedule. \
             What is left: nothing. **What I need from you**: merge PR #140",
            false,
        ),
        digest(
            "fixture-10",
            "Threads.db introspection skill",
            5800,
            38,
            Some(Role::Agent),
            "The skill is finished and exercised end to end against sixty real \
             threads. Classification matches the manual audit on every thread, \
             and the README documents the two known schema variants.",
            false,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TriageState;

    #[test]
    fn fixtures_cover_all_states() {
        let digests = fixture_digests();
        assert_eq!(digests.len(), 11);
        for state in TriageState::ALL {
            assert!(
                digests.iter().any(|digest| digest.state == state),
                "no fixture in state {state:?}"
            );
        }
    }
}
