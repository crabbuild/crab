use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::*;
use crate::cells::{
    Error, Result,
    directory::IdentityKey,
    repository::collaboration::{
        Subject, SubjectKind,
        activity::{self, ListActivity},
        locks,
        milestones::GetAssignment,
    },
};

const COMMENT_PAGE: u8 = 8;
const ACTIVITY_PAGE: u8 = 16;
const PREFIX: &str = "collaboration/v1/issue-transfers/staged/";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Source {
    pub repository: Uuid,
    pub issue: u64,
    pub version: u64,
    pub revision: u64,
    pub issue_type_version: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "section", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Cursor {
    Comments { before: Option<u64> },
    Activity { before: Option<u64> },
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum CaptureInput {
    Start { number: u64, expected_version: u64 },
    Continue { source: Source, cursor: Cursor },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Frame {
    pub source: Source,
    pub records: Records,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "section", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Records {
    Issue {
        header: Box<Header>,
    },
    Comments {
        before: Option<u64>,
        items: Vec<CommentRecord>,
        next: Option<u64>,
    },
    Activity {
        before: Option<u64>,
        items: Vec<activity::Event>,
        next: Option<u64>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Header {
    pub issue: IssueRecord,
    pub labels: Vec<String>,
    pub milestone: Option<MilestoneKey>,
    pub issue_type: Option<issue_type::Selection>,
    pub lock: locks::Lock,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MilestoneKey {
    pub title: String,
    pub due_on: Option<String>,
}

impl Records {
    fn cursor(&self) -> Option<Cursor> {
        match self {
            Self::Issue { .. } => None,
            Self::Comments { before, .. } => Some(Cursor::Comments { before: *before }),
            Self::Activity { before, .. } => Some(Cursor::Activity { before: *before }),
        }
    }

    fn next(&self) -> Option<Cursor> {
        match self {
            Self::Issue { .. } => Some(Cursor::Comments { before: None }),
            Self::Comments {
                next: Some(before), ..
            } => Some(Cursor::Comments {
                before: Some(*before),
            }),
            Self::Comments { next: None, .. } => Some(Cursor::Activity { before: None }),
            Self::Activity { next, .. } => next.map(|before| Cursor::Activity {
                before: Some(before),
            }),
        }
    }

    fn address(&self) -> String {
        match self.cursor() {
            None => "issue".into(),
            Some(Cursor::Comments { before }) => format!("comments/{:016x}", before.unwrap_or(0)),
            Some(Cursor::Activity { before }) => format!("activity/{:016x}", before.unwrap_or(0)),
        }
    }

    fn validate(&self, source: Source) -> Result<()> {
        match self {
            Self::Issue { header } => {
                if header.issue.number != source.issue
                    || header.issue.version != source.version
                    || header.issue.state != 0
                {
                    return Err(Error::Invalid(
                        "Issue transfer header does not match its source",
                    ));
                }
            }
            Self::Comments {
                before,
                items,
                next,
            } => {
                if items.iter().any(|item| item.issue != source.issue) {
                    return Err(Error::Invalid(
                        "Issue transfer comment belongs to another issue",
                    ));
                }
                validate_page(
                    *before,
                    *next,
                    COMMENT_PAGE,
                    items.iter().map(|item| item.number),
                )?;
            }
            Self::Activity {
                before,
                items,
                next,
            } => {
                validate_page(
                    *before,
                    *next,
                    ACTIVITY_PAGE,
                    items.iter().map(|item| item.number),
                )?;
            }
        }
        Ok(())
    }
}

fn validate_page(
    before: Option<u64>,
    next: Option<u64>,
    limit: u8,
    numbers: impl ExactSizeIterator<Item = u64>,
) -> Result<()> {
    if numbers.len() > usize::from(limit)
        || before
            .zip(next)
            .is_some_and(|(before, next)| before <= next)
    {
        return Err(Error::Invalid(
            "Issue transfer frame does not advance its cursor",
        ));
    }
    let mut previous = before;
    for value in before.into_iter().chain(next) {
        validate_number(value)?;
    }
    for number in numbers {
        validate_number(number)?;
        if previous.is_some_and(|previous| previous <= number)
            || next.is_some_and(|next| number < next)
        {
            return Err(Error::Invalid(
                "Issue transfer frame has invalid record order",
            ));
        }
        previous = Some(number);
    }
    Ok(())
}

fn identity(context: &Transaction<'_>) -> Result<(Uuid, u64)> {
    let rows = context.query(
        "SELECT repository_uuid, app_revision FROM repository_identity WHERE singleton = 1",
        &[],
    )?;
    let row = rows
        .rows
        .first()
        .ok_or(Error::Invalid("Repository identity is missing"))?;
    let id = Uuid::from_slice(result_blob(row, 0)?).map_err(Error::sdk)?;
    Ok((id, result_u64_from_row(row, 1)?))
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum CaptureOutcome {
    Ready { frame: Box<Frame> },
    Missing,
    Changed,
    Closed,
    Archived,
}

pub(crate) struct Capture;

impl Read for Capture {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 456;
    type Input = CaptureInput;
    type Output = CaptureOutcome;

    fn execute(context: &mut Transaction<'_>, input: CaptureInput) -> Result<CaptureOutcome> {
        let (number, expected_version) = match &input {
            CaptureInput::Start {
                number,
                expected_version,
            } => (*number, *expected_version),
            CaptureInput::Continue { source, .. } => (source.issue, source.version),
        };
        validate_number(expected_version)?;
        let Some(issue) = GetIssue::execute(context, number)? else {
            return Ok(CaptureOutcome::Missing);
        };
        let (repository, revision) = identity(context)?;
        let assigned_type = issue_type::load(context, number)?;
        let source = Source {
            repository,
            issue: number,
            version: issue.version,
            revision,
            issue_type_version: assigned_type.version,
        };
        if issue.version != expected_version
            || matches!(&input, CaptureInput::Continue { source: expected, .. } if *expected != source)
        {
            return Ok(CaptureOutcome::Changed);
        }
        if issue.state != 0 {
            return Ok(CaptureOutcome::Closed);
        }
        if settings::archived(context)? {
            return Ok(CaptureOutcome::Archived);
        }
        let subject = Subject {
            kind: SubjectKind::Issue,
            number,
        };
        let records = match input {
            CaptureInput::Start { .. } => {
                let labels = ListLabels::execute(context, ())?
                    .labels
                    .into_iter()
                    .filter(|label| issue.label_ids.contains(&label.number))
                    .map(|label| label.name)
                    .collect();
                let milestone = GetAssignment::execute(context, subject)?
                    .and_then(|assignment| assignment.milestone)
                    .map(|milestone| MilestoneKey {
                        title: milestone.title,
                        due_on: milestone.due_on,
                    });
                let lock = locks::load(context, subject)?;
                Records::Issue {
                    header: Box::new(Header {
                        issue,
                        labels,
                        milestone,
                        issue_type: assigned_type.selection,
                        lock,
                    }),
                }
            }
            CaptureInput::Continue {
                cursor: Cursor::Comments { before },
                ..
            } => {
                let CommentPage::Found { items, next } = ListComments::execute(
                    context,
                    ListCommentsInput {
                        issue: number,
                        before,
                        limit: COMMENT_PAGE,
                    },
                )?
                else {
                    return Err(Error::Invalid("Issue transfer comment target is missing"));
                };
                Records::Comments {
                    before,
                    items,
                    next,
                }
            }
            CaptureInput::Continue {
                cursor: Cursor::Activity { before },
                ..
            } => {
                let page = ListActivity::execute(
                    context,
                    activity::Query {
                        subject: Some(subject),
                        before,
                        limit: ACTIVITY_PAGE,
                    },
                )?
                .ok_or(Error::Invalid("Issue transfer activity target is missing"))?;
                Records::Activity {
                    before,
                    items: page.items,
                    next: page.next,
                }
            }
        };
        Ok(CaptureOutcome::Ready {
            frame: Box::new(Frame { source, records }),
        })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StageInput {
    pub request: Uuid,
    pub destination: Uuid,
    pub actor: IdentityKey,
    pub frame: Frame,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Progress {
    pub frames: u64,
    pub next: Option<Cursor>,
    pub commitment: [u8; 32],
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum StageOutcome {
    Saved { progress: Progress },
    Conflict,
    OutOfOrder,
    Archived,
}

#[derive(Deserialize, Serialize)]
struct Job {
    actor: IdentityKey,
    source: Source,
    progress: Progress,
}

pub(crate) struct Stage;

impl Mutation for Stage {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 456;
    type Input = StageInput;
    type Output = StageOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: StageInput,
    ) -> Result<MutationResult<StageOutcome>> {
        let (repository, _) = identity(context)?;
        let source = input.frame.source;
        if input.request.is_nil()
            || source.repository.is_nil()
            || repository != input.destination
            || repository == source.repository
        {
            return Err(Error::Invalid(
                "Issue transfer does not target this repository",
            ));
        }
        validate_number(source.issue)?;
        validate_number(source.version)?;
        input.actor.validate()?;
        input.frame.records.validate(source)?;
        let key = format!("{PREFIX}{}", input.request);
        let job = context.document::<Job>(&key)?;
        let cursor = input.frame.records.cursor();
        let position = input.frame.records.address();
        let bytes = serde_json::to_vec(&input.frame)?;
        if bytes.len() > MAX_LIST_OUTPUT_BYTES + 32 * 1024 {
            return Err(Error::Invalid(
                "Issue transfer frame exceeds its byte limit",
            ));
        }
        let digest = blake3::hash(&bytes);
        let (previous, commitment) = if let Some(job) = &job {
            if job.source != source || job.actor != input.actor {
                return Ok(MutationResult::Rejected(StageOutcome::Conflict));
            }
            let stored = context.query(
                "SELECT digest FROM repository_issue_transfer_frames WHERE transfer = ? AND position = ?",
                &[SqlValue::Blob(input.request.as_bytes().to_vec()), SqlValue::Text(position.clone())],
            )?;
            if let Some(row) = stored.rows.first() {
                return Ok(if result_blob(row, 0)? == digest.as_bytes() {
                    MutationResult::Success(StageOutcome::Saved {
                        progress: job.progress,
                    })
                } else {
                    MutationResult::Rejected(StageOutcome::Conflict)
                });
            }
            if cursor.is_none() || job.progress.next != cursor {
                return Ok(MutationResult::Rejected(StageOutcome::OutOfOrder));
            }
            (job.progress.frames, job.progress.commitment)
        } else {
            if cursor.is_some() {
                return Ok(MutationResult::Rejected(StageOutcome::OutOfOrder));
            }
            let mut hasher = blake3::Hasher::new_derive_key("crab issue transfer destination v1");
            hasher.update(input.request.as_bytes());
            hasher.update(repository.as_bytes());
            (0, *hasher.finalize().as_bytes())
        };
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(StageOutcome::Archived));
        }
        if previous == 0 {
            context.install_schema(include_str!("schema.sql"))?;
        }
        let mut hasher = blake3::Hasher::new_derive_key("crab issue transfer frame v1");
        hasher.update(&commitment);
        hasher.update(digest.as_bytes());
        let progress = Progress {
            frames: collaboration::next_version(previous)?,
            next: input.frame.records.next(),
            commitment: *hasher.finalize().as_bytes(),
        };
        // Stage bytes without allocating an issue number. Only a later source
        // publication decision can make this copy visible in the destination.
        context.query(
            "INSERT INTO repository_issue_transfer_frames(transfer, position, ordinal, digest, payload) VALUES (?, ?, ?, ?, ?)",
            &[
                SqlValue::Blob(input.request.as_bytes().to_vec()),
                SqlValue::Text(position),
                integer(progress.frames)?,
                SqlValue::Blob(digest.as_bytes().to_vec()),
                SqlValue::Blob(bytes),
            ],
        )?;
        context.put_document(
            &key,
            &Job {
                actor: input.actor,
                source,
                progress,
            },
        )?;
        Ok(MutationResult::Success(StageOutcome::Saved { progress }))
    }
}

#[cfg(test)]
mod tests;
