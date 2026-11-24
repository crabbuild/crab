use serde::{Deserialize, Serialize};

use super::super::{
    RepositoryAuthor, SqlValue, Transaction, integer, result_text, validate_number,
};
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

    pub(crate) fn subject(self) -> Subject {
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
        if matches!(self, Self::Issue { .. } | Self::Pull { .. }) {
            return self.subject().closed(context).map(|state| state.is_some());
        }
        Ok(self.select(context, "1")?.is_some())
    }

    pub(super) fn author(
        self,
        context: &Transaction<'_>,
    ) -> crate::cells::Result<Option<RepositoryAuthor>> {
        let row = self.select(context, "author_issuer, author_subject, author_name")?;
        row.map(|row| {
            Ok(RepositoryAuthor {
                issuer: result_text(&row, 0)?.to_owned(),
                subject: result_text(&row, 1)?.to_owned(),
                name: result_text(&row, 2)?.to_owned(),
            })
        })
        .transpose()
    }

    pub(super) fn content(
        self,
        context: &Transaction<'_>,
    ) -> crate::cells::Result<Option<super::edit_history::Content>> {
        let columns = if matches!(self, Self::PullThread { .. }) {
            "body, suggested_text"
        } else {
            "body, NULL"
        };
        self.select(context, columns)?
            .map(|row| {
                let suggestion = match row.get(1) {
                    Some(SqlValue::Null) => None,
                    _ => Some(result_text(&row, 1)?.to_owned()),
                };
                Ok(super::edit_history::Content {
                    body: result_text(&row, 0)?.to_owned(),
                    suggestion,
                })
            })
            .transpose()
    }

    fn select(
        self,
        context: &Transaction<'_>,
        columns: &'static str,
    ) -> crate::cells::Result<Option<Vec<SqlValue>>> {
        self.validate()?;
        let (table, address, parameters) = match self {
            Self::Issue { number } => ("repository_issues", "number = ?", vec![integer(number)?]),
            Self::Pull { number } => ("repository_pulls", "number = ?", vec![integer(number)?]),
            Self::IssueComment { issue, number } => (
                "repository_issue_comments",
                "issue_number = ? AND number = ?",
                vec![integer(issue)?, integer(number)?],
            ),
            Self::PullComment { pull, number } => (
                "repository_pull_comments",
                "pull_number = ? AND number = ?",
                vec![integer(pull)?, integer(number)?],
            ),
            Self::PullReview { pull, number } => (
                "repository_pull_reviews",
                "pull_number = ? AND number = ?",
                vec![integer(pull)?, integer(number)?],
            ),
            Self::PullThread { pull, number } => (
                "repository_pull_review_threads",
                "pull_number = ? AND number = ?",
                vec![integer(pull)?, integer(number)?],
            ),
            Self::PullReply {
                pull,
                thread,
                number,
            } => (
                "repository_pull_review_replies",
                "pull_number = ? AND thread_number = ? AND number = ?",
                vec![integer(pull)?, integer(thread)?, integer(number)?],
            ),
        };
        let result = context.query(
            &format!("SELECT {columns} FROM {table} WHERE {address}"),
            &parameters,
        )?;
        Ok(result.rows.into_iter().next())
    }
}
