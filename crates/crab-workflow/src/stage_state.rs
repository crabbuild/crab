//! Stage lifecycle state machine.
//!
//! The state machine is the source of truth for what a stage has durably
//! committed. Integer journal tags are append-only: once a variant has a tag,
//! it keeps it so persisted journals stay readable across upgrades.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Every observable state a stage passes through between resolution and commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StageState {
    Resolving,
    Resolved,
    CacheChecked,
    Running,
    Produced,
    Hashed,
    Staged,
    EntryWritten,
    RefPublished,
    LockfileUpdated,
    Committed,
    Failed,
    Aborted,
}

const LEGAL_TRANSITIONS: &[(StageState, StageState)] = {
    use StageState::*;
    &[
        (Resolving, Resolved),
        (Resolving, Failed),
        (Resolving, Aborted),
        (Resolved, CacheChecked),
        (Resolved, Failed),
        (Resolved, Aborted),
        (CacheChecked, Running),
        (CacheChecked, Produced),
        (CacheChecked, Failed),
        (CacheChecked, Aborted),
        (Running, Produced),
        (Running, Failed),
        (Running, Aborted),
        (Produced, Hashed),
        (Produced, Failed),
        (Produced, Aborted),
        (Hashed, Staged),
        (Hashed, Failed),
        (Hashed, Aborted),
        (Staged, EntryWritten),
        (Staged, Failed),
        (Staged, Aborted),
        (EntryWritten, RefPublished),
        (EntryWritten, Failed),
        (EntryWritten, Aborted),
        (RefPublished, LockfileUpdated),
        (RefPublished, Failed),
        (RefPublished, Aborted),
        (LockfileUpdated, Committed),
        (LockfileUpdated, Failed),
        (LockfileUpdated, Aborted),
    ]
};

impl StageState {
    /// Returns whether `next` is a legal successor of this state.
    pub fn can_transition_to(self, next: StageState) -> bool {
        LEGAL_TRANSITIONS
            .iter()
            .any(|&(from, to)| from == self && to == next)
    }

    /// Stable `u8` tag used for the SQL journal column.
    pub fn sql_tag(self) -> u8 {
        match self {
            StageState::Resolving => 0,
            StageState::Resolved => 1,
            StageState::CacheChecked => 2,
            StageState::Running => 3,
            StageState::Produced => 4,
            StageState::Hashed => 5,
            StageState::Staged => 6,
            StageState::EntryWritten => 7,
            StageState::RefPublished => 8,
            StageState::LockfileUpdated => 9,
            StageState::Committed => 10,
            StageState::Failed => 11,
            StageState::Aborted => 12,
        }
    }

    /// Inverse of [`StageState::sql_tag`].
    pub fn from_sql_tag(tag: u8) -> Option<StageState> {
        Some(match tag {
            0 => StageState::Resolving,
            1 => StageState::Resolved,
            2 => StageState::CacheChecked,
            3 => StageState::Running,
            4 => StageState::Produced,
            5 => StageState::Hashed,
            6 => StageState::Staged,
            7 => StageState::EntryWritten,
            8 => StageState::RefPublished,
            9 => StageState::LockfileUpdated,
            10 => StageState::Committed,
            11 => StageState::Failed,
            12 => StageState::Aborted,
            _ => return None,
        })
    }

    /// All variants, in definition order.
    pub const ALL: &'static [StageState] = &[
        StageState::Resolving,
        StageState::Resolved,
        StageState::CacheChecked,
        StageState::Running,
        StageState::Produced,
        StageState::Hashed,
        StageState::Staged,
        StageState::EntryWritten,
        StageState::RefPublished,
        StageState::LockfileUpdated,
        StageState::Committed,
        StageState::Failed,
        StageState::Aborted,
    ];
}

impl fmt::Display for StageState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            StageState::Resolving => "Resolving",
            StageState::Resolved => "Resolved",
            StageState::CacheChecked => "CacheChecked",
            StageState::Running => "Running",
            StageState::Produced => "Produced",
            StageState::Hashed => "Hashed",
            StageState::Staged => "Staged",
            StageState::EntryWritten => "EntryWritten",
            StageState::RefPublished => "RefPublished",
            StageState::LockfileUpdated => "LockfileUpdated",
            StageState::Committed => "Committed",
            StageState::Failed => "Failed",
            StageState::Aborted => "Aborted",
        };
        f.write_str(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn sql_tags_are_stable_and_unique() {
        let mut seen = HashSet::new();
        for &state in StageState::ALL {
            let tag = state.sql_tag();
            assert!(seen.insert(tag), "duplicate tag {tag} on {state}");
            assert_eq!(StageState::from_sql_tag(tag), Some(state));
        }
        assert_eq!(seen.len(), StageState::ALL.len());
        assert_eq!(StageState::from_sql_tag(200), None);
    }

    #[test]
    fn legal_transitions_accepted() {
        let happy_path = [
            StageState::Resolving,
            StageState::Resolved,
            StageState::CacheChecked,
            StageState::Running,
            StageState::Produced,
            StageState::Hashed,
            StageState::Staged,
            StageState::EntryWritten,
            StageState::RefPublished,
            StageState::LockfileUpdated,
            StageState::Committed,
        ];
        for pair in happy_path.windows(2) {
            assert!(
                pair[0].can_transition_to(pair[1]),
                "expected {} to {} legal",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn illegal_transitions_rejected() {
        let legal: HashSet<(StageState, StageState)> = LEGAL_TRANSITIONS.iter().copied().collect();
        for &from in StageState::ALL {
            for &to in StageState::ALL {
                let expected = legal.contains(&(from, to));
                assert_eq!(
                    from.can_transition_to(to),
                    expected,
                    "{} to {} should be {}",
                    from,
                    to,
                    if expected { "legal" } else { "illegal" }
                );
            }
        }
    }

    #[test]
    fn terminal_states_accept_nothing() {
        for terminal in [
            StageState::Committed,
            StageState::Failed,
            StageState::Aborted,
        ] {
            for &next in StageState::ALL {
                assert!(
                    !terminal.can_transition_to(next),
                    "terminal {terminal} should not transition to {next}"
                );
            }
        }
    }

    #[test]
    fn no_self_transitions() {
        for &state in StageState::ALL {
            assert!(
                !state.can_transition_to(state),
                "self-transition {state} to {state} must be rejected"
            );
        }
    }

    #[test]
    fn display_matches_variant_name() {
        assert_eq!(StageState::Resolving.to_string(), "Resolving");
        assert_eq!(StageState::Resolved.to_string(), "Resolved");
        assert_eq!(StageState::CacheChecked.to_string(), "CacheChecked");
        assert_eq!(StageState::Running.to_string(), "Running");
        assert_eq!(StageState::Produced.to_string(), "Produced");
        assert_eq!(StageState::Hashed.to_string(), "Hashed");
        assert_eq!(StageState::Staged.to_string(), "Staged");
        assert_eq!(StageState::EntryWritten.to_string(), "EntryWritten");
        assert_eq!(StageState::RefPublished.to_string(), "RefPublished");
        assert_eq!(StageState::LockfileUpdated.to_string(), "LockfileUpdated");
        assert_eq!(StageState::Committed.to_string(), "Committed");
        assert_eq!(StageState::Failed.to_string(), "Failed");
        assert_eq!(StageState::Aborted.to_string(), "Aborted");
    }

    #[test]
    fn serde_roundtrip_via_string() {
        let json = serde_json::to_value(StageState::Resolved).unwrap();
        assert_eq!(json, serde_json::json!("Resolved"));
        let parsed: StageState = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, StageState::Resolved);

        for &state in StageState::ALL {
            let encoded = serde_json::to_value(state).unwrap();
            assert_eq!(encoded, serde_json::json!(state.to_string()));
            let decoded: StageState = serde_json::from_value(encoded).unwrap();
            assert_eq!(decoded, state);
        }
    }
}
