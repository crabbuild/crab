use serde::{Deserialize, Serialize};

use super::super::{SqlBatch, Transaction, integer, statement, validate_number};
use super::{Subject, SubjectKind};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Target {
    Issue { number: u64 },
    Pull { number: u64 },
    IssueComment { issue: u64, number: u64 },
    PullComment { pull: u64, number: u64 },
    PullReview { pull: u64, number: u64 },
    PullThread { pull: u64, number: u64 },
    PullReply { pull: u64, thread: u64, number: u64 },
}

impl Target {
    pub(super) fn validate(self) -> crate::cells::Result<()> {
        validate_number(self.subject().number)?;
        match self {
            Self::Issue { .. } | Self::Pull { .. } => Ok(()),
            Self::IssueComment { number, .. }
            | Self::PullComment { number, .. }
            | Self::PullReview { number, .. }
            | Self::PullThread { number, .. } => validate_number(number),
            Self::PullReply { thread, number, .. } => {
                validate_number(thread)?;
                validate_number(number)
            }
        }
    }

    pub(super) fn subject(self) -> Subject {
        match self {
            Self::Issue { number } | Self::IssueComment { issue: number, .. } => Subject {
                kind: SubjectKind::Issue,
                number,
            },
            Self::Pull { number }
            | Self::PullComment { pull: number, .. }
            | Self::PullReview { pull: number, .. }
            | Self::PullThread { pull: number, .. }
            | Self::PullReply { pull: number, .. } => Subject {
                kind: SubjectKind::Pull,
                number,
            },
        }
    }

    pub(super) fn key(self) -> String {
        let root = self.subject().key();
        match self {
            Self::Issue { .. } | Self::Pull { .. } => root,
            Self::IssueComment { number, .. } | Self::PullComment { number, .. } => {
                format!("{root}/comment/{number:016x}")
            }
            Self::PullReview { number, .. } => format!("{root}/review/{number:016x}"),
            Self::PullThread { number, .. } => format!("{root}/thread/{number:016x}"),
            Self::PullReply { thread, number, .. } => {
                format!("{root}/thread/{thread:016x}/reply/{number:016x}")
            }
        }
    }

    pub(super) fn exists(self, context: &Transaction<'_>) -> crate::cells::Result<bool> {
        self.validate()?;
        let (query, parameters) = match self {
            Self::Issue { .. } | Self::Pull { .. } => {
                return self.subject().closed(context).map(|state| state.is_some());
            }
            Self::IssueComment { issue, number } => (
                "SELECT 1 FROM repository_issue_comments WHERE issue_number = ? AND number = ?",
                vec![integer(issue)?, integer(number)?],
            ),
            Self::PullComment { pull, number } => (
                "SELECT 1 FROM repository_pull_comments WHERE pull_number = ? AND number = ?",
                vec![integer(pull)?, integer(number)?],
            ),
            Self::PullReview { pull, number } => (
                "SELECT 1 FROM repository_pull_reviews WHERE pull_number = ? AND number = ?",
                vec![integer(pull)?, integer(number)?],
            ),
            Self::PullThread { pull, number } => (
                "SELECT 1 FROM repository_pull_review_threads WHERE pull_number = ? AND number = ?",
                vec![integer(pull)?, integer(number)?],
            ),
            Self::PullReply {
                pull,
                thread,
                number,
            } => (
                "SELECT 1 FROM repository_pull_review_replies WHERE pull_number = ? AND thread_number = ? AND number = ?",
                vec![integer(pull)?, integer(thread)?, integer(number)?],
            ),
        };
        let result = context.sql(&SqlBatch {
            statements: vec![statement(query, parameters)],
        })?;
        Ok(!result[0].rows.is_empty())
    }
}
