//! Core types and persistence for resumable git operations.
//!
//! An [`OpRecord`] captures the full state of a merge, rebase, or
//! cherry-pick so the UI can resume after a crash without leaving the
//! repository in a half-finished state.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AgentError, Result};

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// Strategy for a merge operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeStrategy {
    MergeCommit,
    Squash,
    FastForwardOnly,
}

/// Action for a single commit in an interactive rebase todo list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TodoAction {
    Pick,
    Reword,
    Squash,
    Fixup,
    Drop,
    Exec(String),
}

/// One entry in the interactive rebase todo list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub sha: String,
    pub action: TodoAction,
    pub subject: String,
}

/// The operation being performed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Op {
    Merge {
        from: String,
        into: String,
        strategy: MergeStrategy,
        message: Option<String>,
    },
    Rebase {
        onto: String,
        branch: String,
        todo: Option<Vec<TodoItem>>,
    },
    CherryPick {
        shas: Vec<String>,
        target: String,
    },
}

/// Lifecycle state of an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpState {
    InProgress,
    Paused,
    Done,
    Aborted,
}

/// A persisted record of a single merge / rebase / cherry-pick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpRecord {
    pub id: Uuid,
    pub op: Op,
    pub state: OpState,
    pub created_at: DateTime<Utc>,
    pub paused_reason: Option<String>,
}

// ---------------------------------------------------------------------------
// State transitions
// ---------------------------------------------------------------------------

impl OpRecord {
    /// Transition to a new state, validating the move is legal.
    ///
    /// Legal transitions:
    /// - InProgress → Paused, Done, Aborted
    /// - Paused → InProgress, Aborted
    /// - Done → (terminal — no transitions)
    /// - Aborted → (terminal — no transitions)
    pub fn transition(&mut self, new_state: OpState, reason: Option<String>) -> Result<()> {
        let allowed = match self.state {
            OpState::InProgress => matches!(
                new_state,
                OpState::Paused | OpState::Done | OpState::Aborted
            ),
            OpState::Paused => {
                matches!(new_state, OpState::InProgress | OpState::Aborted)
            }
            OpState::Done | OpState::Aborted => false,
        };

        if !allowed {
            return Err(AgentError::Other(format!(
                "illegal op state transition: {:?} → {:?}",
                self.state, new_state
            )));
        }

        self.state = new_state;
        self.paused_reason = reason;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// Returns the canonical ops directory for a repository working tree.
pub fn ops_dir_for_repo(work_dir: &Path) -> PathBuf {
    work_dir.join(".crab").join("desktop").join("ops")
}

impl OpRecord {
    /// Serialize this record to `<ops_dir>/<id>.json`.
    pub fn save(&self, ops_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(ops_dir)?;
        let path = ops_dir.join(format!("{}.json", self.id));
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;
        Ok(())
    }

    /// Deserialize an `OpRecord` from a JSON file.
    pub fn load(path: &Path) -> Result<Self> {
        let data = std::fs::read_to_string(path)?;
        let record: Self = serde_json::from_str(&data)?;
        Ok(record)
    }

    /// Remove this record's file from the ops directory.
    pub fn delete(&self, ops_dir: &Path) -> Result<()> {
        let path = ops_dir.join(format!("{}.json", self.id));
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }
}

/// Load all `OpRecord` files from the given ops directory.
///
/// Returns an empty vec if the directory does not exist.
pub fn load_all(ops_dir: &Path) -> Result<Vec<OpRecord>> {
    if !ops_dir.exists() {
        return Ok(Vec::new());
    }

    let mut records = Vec::new();
    for entry in std::fs::read_dir(ops_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            let record = OpRecord::load(&path)?;
            records.push(record);
        }
    }
    Ok(records)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_merge_record() -> OpRecord {
        OpRecord {
            id: Uuid::new_v4(),
            op: Op::Merge {
                from: "feature/login".to_owned(),
                into: "main".to_owned(),
                strategy: MergeStrategy::MergeCommit,
                message: Some("Merge feature/login into main".to_owned()),
            },
            state: OpState::InProgress,
            created_at: Utc::now(),
            paused_reason: None,
        }
    }

    fn sample_rebase_record() -> OpRecord {
        OpRecord {
            id: Uuid::new_v4(),
            op: Op::Rebase {
                onto: "main".to_owned(),
                branch: "feature/signup".to_owned(),
                todo: Some(vec![TodoItem {
                    sha: "abc1234".to_owned(),
                    action: TodoAction::Pick,
                    subject: "add signup form".to_owned(),
                }]),
            },
            state: OpState::InProgress,
            created_at: Utc::now(),
            paused_reason: None,
        }
    }

    fn sample_cherry_pick_record() -> OpRecord {
        OpRecord {
            id: Uuid::new_v4(),
            op: Op::CherryPick {
                shas: vec!["deadbeef".to_owned(), "cafebabe".to_owned()],
                target: "release/1.0".to_owned(),
            },
            state: OpState::InProgress,
            created_at: Utc::now(),
            paused_reason: None,
        }
    }

    #[test]
    fn save_and_load_round_trip() {
        let tmp = TempDir::new().unwrap();
        let ops_dir = tmp.path().join("ops");

        let original = sample_merge_record();
        original.save(&ops_dir).unwrap();

        let path = ops_dir.join(format!("{}.json", original.id));
        let loaded = OpRecord::load(&path).unwrap();

        assert_eq!(original.id, loaded.id);
        assert_eq!(original.op, loaded.op);
        assert_eq!(original.state, loaded.state);
        assert_eq!(original.paused_reason, loaded.paused_reason);
    }

    #[test]
    fn load_all_finds_multiple_records() {
        let tmp = TempDir::new().unwrap();
        let ops_dir = tmp.path().join("ops");

        let r1 = sample_merge_record();
        let r2 = sample_rebase_record();
        let r3 = sample_cherry_pick_record();

        r1.save(&ops_dir).unwrap();
        r2.save(&ops_dir).unwrap();
        r3.save(&ops_dir).unwrap();

        let all = load_all(&ops_dir).unwrap();
        assert_eq!(all.len(), 3);

        let ids: Vec<Uuid> = all.iter().map(|r| r.id).collect();
        assert!(ids.contains(&r1.id));
        assert!(ids.contains(&r2.id));
        assert!(ids.contains(&r3.id));
    }

    #[test]
    fn transition_in_progress_to_paused() {
        let mut record = sample_merge_record();
        assert_eq!(record.state, OpState::InProgress);

        record
            .transition(OpState::Paused, Some("conflicts in 3 files".to_owned()))
            .unwrap();

        assert_eq!(record.state, OpState::Paused);
        assert_eq!(
            record.paused_reason.as_deref(),
            Some("conflicts in 3 files")
        );
    }

    #[test]
    fn transition_done_to_anything_fails() {
        let mut record = sample_merge_record();
        record.state = OpState::Done;

        let err = record.transition(OpState::InProgress, None).unwrap_err();
        assert!(err.to_string().contains("illegal op state transition"));

        let err = record.transition(OpState::Paused, None).unwrap_err();
        assert!(err.to_string().contains("illegal op state transition"));

        let err = record.transition(OpState::Aborted, None).unwrap_err();
        assert!(err.to_string().contains("illegal op state transition"));
    }

    #[test]
    fn transition_paused_to_in_progress() {
        let mut record = sample_merge_record();
        record
            .transition(OpState::Paused, Some("conflicts".to_owned()))
            .unwrap();

        record.transition(OpState::InProgress, None).unwrap();

        assert_eq!(record.state, OpState::InProgress);
        assert_eq!(record.paused_reason, None);
    }

    #[test]
    fn delete_removes_file() {
        let tmp = TempDir::new().unwrap();
        let ops_dir = tmp.path().join("ops");

        let record = sample_merge_record();
        record.save(&ops_dir).unwrap();

        let path = ops_dir.join(format!("{}.json", record.id));
        assert!(path.exists());

        record.delete(&ops_dir).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn load_all_returns_empty_for_missing_dir() {
        let tmp = TempDir::new().unwrap();
        let ops_dir = tmp.path().join("nonexistent");

        let all = load_all(&ops_dir).unwrap();
        assert!(all.is_empty());
    }

    #[test]
    fn transition_aborted_is_terminal() {
        let mut record = sample_merge_record();
        record.state = OpState::Aborted;

        let err = record.transition(OpState::InProgress, None).unwrap_err();
        assert!(err.to_string().contains("illegal op state transition"));
    }

    #[test]
    fn ops_dir_for_repo_returns_expected_path() {
        let work_dir = Path::new("/home/user/my-repo");
        let expected = PathBuf::from("/home/user/my-repo/.crab/desktop/ops");
        assert_eq!(ops_dir_for_repo(work_dir), expected);
    }
}
