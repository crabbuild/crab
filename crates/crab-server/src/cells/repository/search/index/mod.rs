//! Transactional discussion projections and explicit index preparation.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::cells::application::{Mutation, MutationResult, Read, SqlValue, Transaction};
use crate::cells::repository::collaboration::{Subject, SubjectKind};
use crate::cells::repository::{MAX_NUMBER, SqlBatch, integer, result_u64, statement};

const STATE_KEY: &str = "discussion-search/v1/state";
const FORMAT: u16 = 5;
const BATCH: usize = 8;
const MAX_DIRTY: usize = 128;

mod projection;
mod query;
pub(crate) use query::{Direction, LabelSummary, Page, SearchInput, Sort};
pub(in crate::cells::repository) use query::{Labelled, Metrics, page};
mod rebuild;
pub(crate) use rebuild::RebuildIndex;
pub use rebuild::RebuildSearchInput;
pub(crate) use rebuild::RebuildSearchOutcome;

/// A persisted stage of explicit discussion-index preparation.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchIndexPhase {
    Clear,
    ClearMilestones,
    Milestones,
    Issues,
    Pulls,
}

/// The bounded preparation cursor and compare-and-swap version.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SearchIndexProgress {
    pub phase: SearchIndexPhase,
    pub after: u64,
    pub processed: u64,
    pub version: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct State {
    format: u16,
    generation: Uuid,
    progress: Option<SearchIndexProgress>,
}

/// Readiness and generation for a repository's native discussion index.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum SearchIndexStatus {
    Missing,
    Outdated {
        generation: Uuid,
        format: u16,
    },
    Building {
        generation: Uuid,
        progress: SearchIndexProgress,
    },
    Ready {
        generation: Uuid,
        revision: u64,
    },
}

fn state(context: &Transaction<'_>) -> crate::cells::Result<Option<State>> {
    let state = context.document::<State>(STATE_KEY)?;
    if state
        .as_ref()
        .is_some_and(|state| state.format == 0 || state.format > FORMAT)
    {
        return Err(crate::cells::Error::Invalid(
            "discussion index format is unsupported",
        ));
    }
    Ok(state)
}

pub(crate) fn ensure_ready(context: &Transaction<'_>) -> crate::cells::Result<Uuid> {
    let Some(state) = state(context)? else {
        return Err(crate::cells::Error::IndexUnavailable);
    };
    if state.format != FORMAT || state.progress.is_some() {
        return Err(crate::cells::Error::IndexUnavailable);
    }
    Ok(state.generation)
}

fn revision(context: &Transaction<'_>) -> crate::cells::Result<u64> {
    let rows = context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT revision FROM discussion_index_revision WHERE singleton = 1",
            vec![],
        )],
    })?;
    result_u64(&rows, 0, 0)
}

pub(crate) fn initialize(context: &Transaction<'_>, generation: Uuid) -> crate::cells::Result<()> {
    context.install_schema(include_str!("schema.sql"))?;
    context.put_document(
        STATE_KEY,
        &State {
            format: FORMAT,
            generation,
            progress: None,
        },
    )
}

pub(crate) struct GetSearchIndexStatus;
impl Read for GetSearchIndexStatus {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 270;
    type Input = ();
    type Output = SearchIndexStatus;
    fn execute(context: &mut Transaction<'_>, _: ()) -> crate::cells::Result<SearchIndexStatus> {
        status(context)
    }
}

fn status(context: &Transaction<'_>) -> crate::cells::Result<SearchIndexStatus> {
    Ok(match state(context)? {
        None => SearchIndexStatus::Missing,
        Some(state) if state.format != FORMAT => SearchIndexStatus::Outdated {
            generation: state.generation,
            format: state.format,
        },
        Some(State {
            generation,
            progress: Some(progress),
            ..
        }) => SearchIndexStatus::Building {
            generation,
            progress,
        },
        Some(State {
            generation,
            progress: None,
            ..
        }) => SearchIndexStatus::Ready {
            generation,
            revision: revision(context)?,
        },
    })
}

pub(in crate::cells::repository) fn dirty(
    context: &Transaction<'_>,
    subject: Subject,
) -> crate::cells::Result<()> {
    context.sql(&SqlBatch {
        statements: vec![statement(
            "INSERT INTO discussion_index_dirty(kind, number) VALUES (?, ?) ON CONFLICT DO NOTHING",
            vec![
                SqlValue::Integer(kind(subject.kind)),
                integer(subject.number)?,
            ],
        )],
    })?;
    Ok(())
}

pub(crate) fn flush(context: &Transaction<'_>) -> crate::cells::Result<()> {
    let rows = context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT kind, number FROM discussion_index_dirty ORDER BY kind, number LIMIT 129",
            vec![],
        )],
    })?;
    if rows[0].rows.len() > MAX_DIRTY {
        return Err(crate::cells::Error::Invalid(
            "discussion mutation exceeds its index work limit",
        ));
    }
    for row in &rows[0].rows {
        let subject = subject(row)?;
        projection::refresh(context, subject)?;
    }
    if !rows[0].rows.is_empty() {
        context.sql(&SqlBatch { statements: vec![
            statement("DELETE FROM discussion_index_dirty", vec![]),
            statement("UPDATE discussion_index_revision SET revision = revision + 1 WHERE singleton = 1", vec![]),
        ] })?;
    }
    Ok(())
}

fn kind(kind: SubjectKind) -> i64 {
    match kind {
        SubjectKind::Issue => 0,
        SubjectKind::Pull => 1,
    }
}
fn subject(row: &[SqlValue]) -> crate::cells::Result<Subject> {
    let [SqlValue::Integer(kind), SqlValue::Integer(number)] = row else {
        return Err(crate::cells::Error::Invalid(
            "discussion subject row is invalid",
        ));
    };
    let kind = match kind {
        0 => SubjectKind::Issue,
        1 => SubjectKind::Pull,
        _ => {
            return Err(crate::cells::Error::Invalid(
                "discussion subject kind is invalid",
            ));
        }
    };
    let number = u64::try_from(*number)
        .map_err(|_| crate::cells::Error::Invalid("discussion number is negative"))?;
    crate::cells::repository::validate_number(number)?;
    Ok(Subject { kind, number })
}

fn rowid(subject: Subject) -> u64 {
    subject.number * 2 + u64::from(subject.kind == SubjectKind::Pull)
}

#[cfg(test)]
mod tests;
