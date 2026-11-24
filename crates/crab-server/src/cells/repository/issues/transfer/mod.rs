use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::*;
use crate::cells::{
    Error, Result,
    directory::IdentityKey,
    repository::collaboration::{
        Subject, SubjectKind, Target,
        activity::{self, ListActivity},
        attachments::{self, Upload},
        dependencies, duplicates,
        edit_history::transfer as edit_history,
        links, locks,
        milestones::GetAssignment,
        minimization,
        reactions::transfer as reactions,
    },
};

const COMMENT_PAGE: u8 = 8;
const ACTIVITY_PAGE: u8 = 16;
const PREFIX: &str = "collaboration/v1/issue-transfers/staged/";

pub(crate) mod publication;
mod recovery;

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
    Reactions { after: Option<reactions::Position> },
    Activity { before: Option<u64> },
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum CaptureInput {
    Start { number: u64, expected_version: u64 },
    Continue { source: Source, cursor: Cursor },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Frame {
    pub source: Source,
    pub records: Records,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "section", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Records {
    Issue {
        header: Box<Header>,
    },
    Comments {
        before: Option<u64>,
        items: Vec<Comment>,
        next: Option<u64>,
    },
    Reactions {
        page: reactions::Page,
    },
    Activity {
        before: Option<u64>,
        items: Vec<activity::Event>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        edits: Vec<edit_history::Edit>,
        next: Option<u64>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Header {
    pub issue: IssueRecord,
    pub attachments: Vec<Upload>,
    pub lifecycle: lifecycle::Lifecycle,
    pub labels: Vec<String>,
    pub milestone: Option<MilestoneKey>,
    pub issue_type: Option<issue_type::Selection>,
    pub lock: locks::Lock,
    #[serde(default, skip_serializing_if = "links::branches::State::is_empty")]
    pub branches: links::branches::State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<publication::Destination>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Comment {
    pub record: CommentRecord,
    pub attachments: Vec<Upload>,
    pub visibility: Option<minimization::State>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MilestoneKey {
    pub title: String,
    pub due_on: Option<String>,
}

impl Records {
    pub(crate) fn uploads(&self) -> impl Iterator<Item = &Upload> {
        let (attachments, comments): (&[Upload], &[Comment]) = match self {
            Self::Issue { header } => (&header.attachments, &[]),
            Self::Comments { items, .. } => (&[], items),
            Self::Reactions { .. } | Self::Activity { .. } => (&[], &[]),
        };
        attachments
            .iter()
            .chain(comments.iter().flat_map(|comment| &comment.attachments))
            .chain(
                match self {
                    Self::Activity { edits, .. } => Some(edits),
                    _ => None,
                }
                .into_iter()
                .flat_map(|edits| edits.iter().flat_map(|edit| &edit.attachments)),
            )
    }

    fn cursor(&self) -> Option<Cursor> {
        match self {
            Self::Issue { .. } => None,
            Self::Comments { before, .. } => Some(Cursor::Comments { before: *before }),
            Self::Reactions { page } => Some(Cursor::Reactions { after: page.after }),
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
            Self::Comments { next: None, .. } => Some(Cursor::Reactions { after: None }),
            Self::Reactions { page } => Some(match page.next {
                Some(after) => Cursor::Reactions { after: Some(after) },
                None => Cursor::Activity { before: None },
            }),
            Self::Activity { next, .. } => next.map(|before| Cursor::Activity {
                before: Some(before),
            }),
        }
    }

    fn address(&self) -> String {
        match self.cursor() {
            None => "issue".into(),
            Some(Cursor::Comments { before }) => format!("comments/{:016x}", before.unwrap_or(0)),
            Some(Cursor::Reactions { after: None }) => "reactions".into(),
            Some(Cursor::Reactions { after: Some(after) }) => {
                format!("reactions/{}", after.address())
            }
            Some(Cursor::Activity { before }) => format!("activity/{:016x}", before.unwrap_or(0)),
        }
    }

    fn validate(&self, source: Source) -> Result<()> {
        match self {
            Self::Issue { header } => {
                header.branches.validate()?;
                if let Some(origin) = header.origin {
                    validate_number(origin.number)?;
                    if origin.repository.is_nil() {
                        return Err(Error::Invalid("Issue transfer has an invalid origin"));
                    }
                }
                if header.issue.number != source.issue
                    || header.issue.version != source.version
                    || header.issue.state != 0
                {
                    return Err(Error::Invalid(
                        "Issue transfer header does not match its source",
                    ));
                }
                attachments::validate_transfer(
                    source.repository,
                    &header.issue.body,
                    &header.attachments,
                )?;
            }
            Self::Comments {
                before,
                items,
                next,
            } => {
                if items.iter().any(|item| item.record.issue != source.issue) {
                    return Err(Error::Invalid(
                        "Issue transfer comment belongs to another issue",
                    ));
                }
                validate_page(
                    *before,
                    *next,
                    COMMENT_PAGE,
                    items.iter().map(|item| item.record.number),
                )?;
                for item in items {
                    attachments::validate_transfer(
                        source.repository,
                        &item.record.body,
                        &item.attachments,
                    )?;
                }
            }
            Self::Activity {
                before,
                items,
                edits,
                next,
            } => {
                validate_page(
                    *before,
                    *next,
                    ACTIVITY_PAGE,
                    items.iter().map(|item| item.number),
                )?;
                let mut ordered = edits.iter().peekable();
                for event in items {
                    if let Some(edit) = ordered.next_if(|edit| edit.number == event.number) {
                        let subject = event.target.subject();
                        if subject.kind != SubjectKind::Issue || subject.number != source.issue {
                            return Err(Error::Invalid(
                                "Issue transfer edit belongs to another issue",
                            ));
                        }
                        edit.validate(source.repository, event)?;
                    }
                }
                if ordered.next().is_some() {
                    return Err(Error::Invalid("Issue transfer edit has no source activity"));
                }
            }
            Self::Reactions { page } => page.validate()?,
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

fn observed(context: &Transaction<'_>, issue: &IssueRecord) -> Result<Source> {
    let (repository, revision) = identity(context)?;
    Ok(Source {
        repository,
        issue: issue.number,
        version: issue.version,
        revision,
        issue_type_version: issue_type::load(context, issue.number)?.version,
    })
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum CaptureOutcome {
    Ready { frame: Box<Frame> },
    Missing,
    Changed,
    Closed,
    Archived,
    LinkedWork,
}

pub(crate) struct Capture;

fn has_linked_work(context: &Transaction<'_>, subject: Subject) -> Result<bool> {
    if links::branches::creation::pending(context, subject.number)?.is_some()
        || dependencies::has_edges(context, subject.number)?
        || duplicates::current(context, subject)?.is_some()
        || links::has_explicit_links(context, subject)?
    {
        return Ok(true);
    }
    let tables = context.query(
        "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'repository_issue_parents'",
        &[],
    )?;
    if tables.rows.is_empty() {
        return Ok(false);
    }
    let number = integer(subject.number)?;
    let linked = context.query(
        "SELECT 1 FROM repository_issue_parents WHERE parent = ? OR (child = ? AND parent IS NOT NULL)
         UNION ALL SELECT 1 FROM repository_project_items WHERE kind = 0 AND number = ? LIMIT 1",
        &[number.clone(), number.clone(), number],
    )?;
    Ok(!linked.rows.is_empty())
}

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
        let source = observed(context, &issue)?;
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
                if has_linked_work(context, subject)? {
                    return Ok(CaptureOutcome::LinkedWork);
                }
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
                let assigned_type = issue_type::load(context, number)?;
                Records::Issue {
                    header: Box::new(Header {
                        lifecycle: lifecycle::get(context, number, false)?,
                        attachments: attachments::capture_transfer(
                            context,
                            source.repository,
                            &issue.body,
                        )?,
                        issue,
                        labels,
                        milestone,
                        issue_type: assigned_type.selection,
                        lock,
                        branches: links::branches::load(context, number)?,
                        origin: publication::origin(context, number)?,
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
                let items = items
                    .into_iter()
                    .map(|record| {
                        let visibility = minimization::load(
                            context,
                            Target::IssueComment {
                                issue: number,
                                number: record.number,
                            },
                        )?;
                        Ok(Comment {
                            attachments: attachments::capture_transfer(
                                context,
                                source.repository,
                                &record.body,
                            )?,
                            record,
                            visibility,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Records::Comments {
                    before,
                    items,
                    next,
                }
            }
            CaptureInput::Continue {
                cursor: Cursor::Reactions { after },
                ..
            } => Records::Reactions {
                page: reactions::capture(context, number, after)?,
            },
            CaptureInput::Continue {
                cursor: Cursor::Activity { before },
                ..
            } => capture_activity(context, source, subject, before)?,
        };
        Ok(CaptureOutcome::Ready {
            frame: Box::new(Frame { source, records }),
        })
    }
}

fn capture_activity(
    context: &mut Transaction<'_>,
    source: Source,
    subject: Subject,
    before: Option<u64>,
) -> Result<Records> {
    let page = ListActivity::execute(
        context,
        activity::Query {
            subject: Some(subject),
            before,
            limit: ACTIVITY_PAGE,
        },
    )?
    .ok_or(Error::Invalid("Issue transfer activity target is missing"))?;
    let mut items = Vec::<activity::Event>::new();
    let mut edits = Vec::new();
    let mut next = page.next;
    let mut size = 0;
    for event in page.items {
        let edit = edit_history::capture(context, source.repository, &event)?;
        let bytes = serde_json::to_vec(&event)?.len()
            + edit
                .as_ref()
                .map(serde_json::to_vec)
                .transpose()?
                .map_or(0, |bytes| bytes.len())
            + 64;
        // Leave room for the frame and cursor around the event and its edit.
        if size + bytes > MAX_LIST_OUTPUT_BYTES - 1024 {
            let last = items.last().ok_or(Error::Invalid(
                "Issue transfer edit exceeds the frame byte limit",
            ))?;
            next = Some(last.number);
            break;
        }
        size += bytes;
        if let Some(edit) = edit {
            edits.push(edit);
        }
        items.push(event);
    }
    Ok(Records::Activity {
        before,
        items,
        edits,
        next,
    })
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StageInput {
    pub request: Uuid,
    pub destination: Uuid,
    pub actor: IdentityKey,
    pub frame: Frame,
    // The trusted HTTP coordinator attests to each immutable object after
    // checking its size and digest in the destination repository's store.
    pub copied: std::collections::BTreeMap<Uuid, String>,
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
    AwaitingCopies,
    Conflict,
    OutOfOrder,
    Archived,
    Expired,
}

#[derive(Deserialize, Serialize)]
struct Job {
    actor: IdentityKey,
    source: Source,
    progress: Progress,
    publication: Option<publication::DestinationState>,
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
        let copies = input
            .frame
            .records
            .uploads()
            .map(|upload| {
                (
                    attachments::transferred_id(input.request, upload.id),
                    upload.digest.as_str(),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let needs_copy = !copies.is_empty();
        let confirmed = !input.copied.is_empty();
        if confirmed
            && !copies.into_iter().eq(input
                .copied
                .iter()
                .map(|(id, digest)| (*id, digest.as_str())))
        {
            return Err(Error::AttachmentUnavailable);
        }
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
        let (mut job, reserved) = if let Some(job) = job {
            if job.source != source || job.actor != input.actor {
                return Ok(MutationResult::Rejected(StageOutcome::Conflict));
            }
            if matches!(
                job.publication,
                Some(publication::DestinationState::Cancelled)
            ) {
                return Ok(MutationResult::Rejected(StageOutcome::Expired));
            }
            let stored = context.query(
                "SELECT ordinal, digest FROM repository_issue_transfer_frames WHERE transfer = ? AND position = ?",
                &[SqlValue::Blob(input.request.as_bytes().to_vec()), SqlValue::Text(position.clone())],
            )?;
            let reserved = if let Some(row) = stored.rows.first() {
                if result_blob(row, 1)? != digest.as_bytes() {
                    return Ok(MutationResult::Rejected(StageOutcome::Conflict));
                }
                if result_u64_from_row(row, 0)? <= job.progress.frames {
                    return Ok(MutationResult::Success(StageOutcome::Saved {
                        progress: job.progress,
                    }));
                }
                true
            } else {
                false
            };
            if job.progress.next != cursor || (cursor.is_none() && job.progress.frames != 0) {
                return Ok(MutationResult::Rejected(StageOutcome::OutOfOrder));
            }
            (job, reserved)
        } else {
            if cursor.is_some() {
                return Ok(MutationResult::Rejected(StageOutcome::OutOfOrder));
            }
            let mut hasher = blake3::Hasher::new_derive_key("crab issue transfer destination v1");
            hasher.update(input.request.as_bytes());
            hasher.update(repository.as_bytes());
            (
                Job {
                    actor: input.actor,
                    source,
                    progress: Progress {
                        frames: 0,
                        next: None,
                        commitment: *hasher.finalize().as_bytes(),
                    },
                    publication: None,
                },
                false,
            )
        };
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(StageOutcome::Archived));
        }
        let ordinal = collaboration::next_version(job.progress.frames)?;
        if !reserved {
            if confirmed {
                return Err(Error::AttachmentUnavailable);
            }
            if job.progress.frames == 0 {
                context.install_schema(include_str!("schema.sql"))?;
            }
            // Reserve each attachment ID before the trusted worker writes its
            // object. Concurrent browser uploads cannot claim the same key.
            for upload in input.frame.records.uploads() {
                attachments::stage_transfer(context, input.request, upload.clone())?;
            }
            context.query(
                "INSERT INTO repository_issue_transfer_frames(transfer, position, ordinal, digest, payload) VALUES (?, ?, ?, ?, ?)",
                &[
                    SqlValue::Blob(input.request.as_bytes().to_vec()),
                    SqlValue::Text(position),
                    integer(ordinal)?,
                    SqlValue::Blob(digest.as_bytes().to_vec()),
                    SqlValue::Blob(bytes),
                ],
            )?;
        }
        if needs_copy && !confirmed {
            context.put_document(&key, &job)?;
            recovery::arm(
                context,
                recovery::Work::Expire(input.request),
                recovery::IDLE_MS,
            )?;
            return Ok(MutationResult::Success(StageOutcome::AwaitingCopies));
        }
        let mut hasher = blake3::Hasher::new_derive_key("crab issue transfer frame v1");
        hasher.update(&job.progress.commitment);
        hasher.update(digest.as_bytes());
        job.progress = Progress {
            frames: ordinal,
            next: input.frame.records.next(),
            commitment: *hasher.finalize().as_bytes(),
        };
        context.put_document(&key, &job)?;
        recovery::arm(
            context,
            recovery::Work::Expire(input.request),
            recovery::IDLE_MS,
        )?;
        Ok(MutationResult::Success(StageOutcome::Saved {
            progress: job.progress,
        }))
    }
}

#[cfg(test)]
mod tests;
