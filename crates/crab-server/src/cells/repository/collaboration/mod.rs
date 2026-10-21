//! Repository collaboration documents within the native Cell transaction.

use serde::{Deserialize, Serialize};

use super::{RepositoryTransaction, SqlBatch, SqlValue, integer, statement, validate_number};

pub(crate) mod activity;
pub(crate) mod links;
pub(crate) mod milestones;
pub(crate) mod reactions;
mod target;
pub(crate) use target::Target;

const PREFIX: &str = "collaboration/v1";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SubjectKind {
    Issue,
    Pull,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Subject {
    pub kind: SubjectKind,
    pub number: u64,
}

impl Subject {
    pub(crate) fn key(self) -> String {
        let kind = match self.kind {
            SubjectKind::Issue => "issue",
            SubjectKind::Pull => "pull",
        };
        format!("{kind}/{:016x}", self.number)
    }

    pub(crate) fn closed(
        self,
        context: &RepositoryTransaction<'_>,
    ) -> crate::cells::Result<Option<bool>> {
        validate_number(self.number)?;
        let query = match self.kind {
            SubjectKind::Issue => "SELECT state FROM repository_issues WHERE number = ?",
            SubjectKind::Pull => "SELECT state FROM repository_pulls WHERE number = ?",
        };
        let rows = context.sql(&SqlBatch {
            statements: vec![statement(query, vec![integer(self.number)?])],
        })?;
        match rows[0].rows.first().map(Vec::as_slice) {
            None => Ok(None),
            Some([SqlValue::Integer(state)])
                if (0..=1).contains(state) || self.kind == SubjectKind::Pull && *state == 2 =>
            {
                Ok(Some(*state != 0))
            }
            _ => Err(crate::cells::Error::Invalid(
                "collaboration subject state is invalid",
            )),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubjectSummary {
    pub subject: Subject,
    pub title: String,
    pub state: SubjectState,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SubjectState {
    Open,
    Closed,
    Merged,
}

impl Subject {
    pub(crate) fn summary(
        self,
        context: &RepositoryTransaction<'_>,
    ) -> crate::cells::Result<Option<SubjectSummary>> {
        validate_number(self.number)?;
        let query = match self.kind {
            SubjectKind::Issue => "SELECT title, state FROM repository_issues WHERE number = ?",
            SubjectKind::Pull => "SELECT title, state FROM repository_pulls WHERE number = ?",
        };
        let rows = context.sql(&SqlBatch {
            statements: vec![statement(query, vec![integer(self.number)?])],
        })?;
        let Some(row) = rows[0].rows.first() else {
            return Ok(None);
        };
        let [SqlValue::Text(title), SqlValue::Integer(code)] = row.as_slice() else {
            return Err(crate::cells::Error::Invalid(
                "collaboration subject summary is invalid",
            ));
        };
        let state = match (self.kind, code) {
            (_, 0) => SubjectState::Open,
            (_, 1) => SubjectState::Closed,
            (SubjectKind::Pull, 2) => SubjectState::Merged,
            _ => {
                return Err(crate::cells::Error::Invalid(
                    "collaboration subject state is invalid",
                ));
            }
        };
        Ok(Some(SubjectSummary {
            subject: self,
            title: title.clone(),
            state,
        }))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum Decision<T> {
    Applied(T),
    Archived,
    NotFound,
    Conflict,
    Forbidden,
    NameConflict,
    LimitReached,
}

fn next_version(value: u64) -> crate::cells::Result<u64> {
    value
        .checked_add(1)
        .filter(|next| *next <= super::MAX_NUMBER)
        .ok_or(crate::cells::Error::Invalid(
            "collaboration version is exhausted",
        ))
}
