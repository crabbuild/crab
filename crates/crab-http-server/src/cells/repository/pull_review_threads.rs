use serde::{Deserialize, Serialize};

use super::*;

const MAX_PAGE_WIRE_BYTES: usize = 1024 * 1024;
const MAX_SUGGESTED_TEXT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PullReviewThreadSide {
    Old,
    New,
}

impl PullReviewThreadSide {
    const fn code(self) -> u8 {
        match self {
            Self::Old => 0,
            Self::New => 1,
        }
    }

    fn from_code(value: u64) -> cellule_runtime::Result<Self> {
        match value {
            0 => Ok(Self::Old),
            1 => Ok(Self::New),
            _ => Err(cellule_runtime::Error::Command(
                "repository pull review thread side is invalid",
            )),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewThreadRecord {
    pub pull: u64,
    pub number: u64,
    pub author: RepositoryAuthor,
    pub body: String,
    pub suggested_text: Option<String>,
    pub base_oid: String,
    pub head_oid: String,
    pub path: Vec<u8>,
    pub old_blob_oid: Option<String>,
    pub new_blob_oid: Option<String>,
    pub side: PullReviewThreadSide,
    pub start_line: u64,
    pub end_line: u64,
    pub resolved: bool,
    pub resolved_by: Option<RepositoryAuthor>,
    pub resolved_at_ms: Option<u64>,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewReplyRecord {
    pub pull: u64,
    pub thread: u64,
    pub number: u64,
    pub author: RepositoryAuthor,
    pub body: String,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewThreadPage {
    pub items: Vec<PullReviewThreadRecord>,
    pub next: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewReplyPage {
    pub items: Vec<PullReviewReplyRecord>,
    pub next: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewThreadKey {
    pub pull: u64,
    pub number: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewReplyKey {
    pub pull: u64,
    pub thread: u64,
    pub number: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewThreadSubmissionKey {
    pub pull: Option<u64>,
    pub submission_id: [u8; 16],
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewReplySubmissionKey {
    pub pull: Option<u64>,
    pub thread: Option<u64>,
    pub submission_id: [u8; 16],
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewThreadListInput {
    pub pull: u64,
    pub before: Option<u64>,
    pub limit: u8,
    pub path: Option<Vec<u8>>,
    pub resolved: Option<bool>,
    pub comparison_base_oid: Option<String>,
    pub comparison_head_oid: Option<String>,
    pub outdated: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewReplyListInput {
    pub pull: u64,
    pub thread: u64,
    pub before: Option<u64>,
    pub limit: u8,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreatePullReviewThreadInput {
    pub pull: u64,
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub body: String,
    pub suggested_text: Option<String>,
    pub base_oid: String,
    pub head_oid: String,
    pub path: Vec<u8>,
    pub old_blob_oid: Option<String>,
    pub new_blob_oid: Option<String>,
    pub side: PullReviewThreadSide,
    pub start_line: u64,
    pub end_line: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum CreatePullReviewThreadOutcome {
    Created(Box<PullReviewThreadRecord>),
    PullNotFound,
    PullClosed,
    RequestConflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdatePullReviewThreadInput {
    pub key: PullReviewThreadKey,
    pub actor: RepositoryAuthor,
    pub can_resolve: bool,
    pub version: u64,
    pub body: Option<String>,
    pub suggested_text: Option<Option<String>>,
    pub resolved: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum UpdatePullReviewThreadOutcome {
    Updated(Box<PullReviewThreadRecord>),
    NotFound,
    Forbidden,
    Conflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreatePullReviewReplyInput {
    pub pull: u64,
    pub thread: u64,
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub body: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum CreatePullReviewReplyOutcome {
    Created(PullReviewReplyRecord),
    ThreadNotFound,
    RequestConflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdatePullReviewReplyInput {
    pub key: PullReviewReplyKey,
    pub actor: RepositoryAuthor,
    pub version: u64,
    pub body: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum UpdatePullReviewReplyOutcome {
    Updated(PullReviewReplyRecord),
    NotFound,
    Forbidden,
    Conflict,
}

pub(crate) struct CreatePullReviewThread;

impl Command for CreatePullReviewThread {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = super::super::REPOSITORY_PULL_REVIEW_THREAD_CREATE_COMMAND_ID;
    const CODEC_VERSION: u32 = 1;
    type Input = CreatePullReviewThreadInput;
    type Output = CreatePullReviewThreadOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> cellule_runtime::Result<CommandResult<Self::Output>> {
        validate_create_thread(&input)?;
        let digest = thread_digest(&input);
        let existing = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT payload_digest, thread_number FROM repository_pull_review_thread_submissions WHERE pull_number = ? AND request_id = ?",
                vec![
                    integer(input.pull)?,
                    SqlValue::Blob(input.submission_id.to_vec()),
                ],
            )],
        })?;
        if let Some(row) = existing[0].rows.first() {
            if result_blob(row, 0)? != digest.as_bytes() {
                return Ok(CommandResult::Rejected(
                    CreatePullReviewThreadOutcome::RequestConflict,
                ));
            }
            let record = load_thread(
                context,
                PullReviewThreadKey {
                    pull: input.pull,
                    number: result_u64_from_row(row, 1)?,
                },
            )?
            .ok_or(cellule_runtime::Error::Command(
                "repository pull review thread submission has no thread",
            ))?;
            return Ok(CommandResult::Success(
                CreatePullReviewThreadOutcome::Created(Box::new(record)),
            ));
        }
        let state = pull_state(context, input.pull)?;
        let Some(state) = state else {
            return Ok(CommandResult::Rejected(
                CreatePullReviewThreadOutcome::PullNotFound,
            ));
        };
        if state != 0 {
            return Ok(CommandResult::Rejected(
                CreatePullReviewThreadOutcome::PullClosed,
            ));
        }
        let number = next_number(
            context,
            "repository_pull_review_thread_sequences",
            input.pull,
        )?;
        let now = timestamp(context.now_ms())?;
        let record = PullReviewThreadRecord {
            pull: input.pull,
            number,
            author: input.author,
            body: input.body,
            suggested_text: input.suggested_text,
            base_oid: input.base_oid,
            head_oid: input.head_oid,
            path: input.path,
            old_blob_oid: input.old_blob_oid,
            new_blob_oid: input.new_blob_oid,
            side: input.side,
            start_line: input.start_line,
            end_line: input.end_line,
            resolved: false,
            resolved_by: None,
            resolved_at_ms: None,
            version: 1,
            created_at_ms: now,
            updated_at_ms: now,
        };
        context.sql(&SqlBatch {
            statements: vec![
                statement(
                    "INSERT INTO repository_pull_review_thread_submissions(pull_number, request_id, payload_digest, thread_number) VALUES (?, ?, ?, ?)",
                    vec![
                        integer(record.pull)?,
                        SqlValue::Blob(input.submission_id.to_vec()),
                        SqlValue::Blob(digest.as_bytes().to_vec()),
                        integer(record.number)?,
                    ],
                ),
                insert_thread_statement(&record)?,
            ],
        })?;
        advance_revision(context)?;
        Ok(CommandResult::Success(
            CreatePullReviewThreadOutcome::Created(Box::new(record)),
        ))
    }
}

pub(crate) struct UpdatePullReviewThread;

impl Command for UpdatePullReviewThread {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = super::super::REPOSITORY_PULL_REVIEW_THREAD_UPDATE_COMMAND_ID;
    const CODEC_VERSION: u32 = 1;
    type Input = UpdatePullReviewThreadInput;
    type Output = UpdatePullReviewThreadOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> cellule_runtime::Result<CommandResult<Self::Output>> {
        validate_number(input.key.pull)?;
        validate_number(input.key.number)?;
        validate_number(input.version)?;
        validate_author(&input.actor)?;
        if input.body.is_none() && input.suggested_text.is_none() && input.resolved.is_none() {
            return Err(cellule_runtime::Error::Command(
                "repository pull review thread update is empty",
            ));
        }
        let Some(mut record) = load_thread(context, input.key.clone())? else {
            return Ok(CommandResult::Rejected(
                UpdatePullReviewThreadOutcome::NotFound,
            ));
        };
        if input.body.is_some() || input.suggested_text.is_some() {
            if !same_author(&record.author, &input.actor) {
                return Ok(CommandResult::Rejected(
                    UpdatePullReviewThreadOutcome::Forbidden,
                ));
            }
            if let Some(body) = &input.body {
                validate_body(body, true)?;
            }
            if let Some(suggested_text) = &input.suggested_text {
                validate_suggested_text(suggested_text.as_deref())?;
                if record.side == PullReviewThreadSide::Old && suggested_text.is_some() {
                    return Err(cellule_runtime::Error::Command(
                        "old pull review thread anchors cannot have suggestions",
                    ));
                }
            }
        }
        if input.resolved.is_some() && !input.can_resolve {
            return Ok(CommandResult::Rejected(
                UpdatePullReviewThreadOutcome::Forbidden,
            ));
        }
        if record.version != input.version {
            return Ok(CommandResult::Rejected(
                UpdatePullReviewThreadOutcome::Conflict,
            ));
        }
        let now = timestamp(context.now_ms())?;
        if let Some(body) = input.body {
            record.body = body;
        }
        if let Some(suggested_text) = input.suggested_text {
            record.suggested_text = suggested_text;
        }
        if let Some(resolved) = input.resolved {
            record.resolved = resolved;
            if resolved {
                record.resolved_by = Some(input.actor);
                record.resolved_at_ms = Some(now);
            } else {
                record.resolved_by = None;
                record.resolved_at_ms = None;
            }
        }
        record.version = next_version(record.version)?;
        record.updated_at_ms = now;
        let old_version = input.version;
        context.sql(&SqlBatch {
            statements: vec![update_thread_statement(&record, old_version)?],
        })?;
        advance_revision(context)?;
        Ok(CommandResult::Success(
            UpdatePullReviewThreadOutcome::Updated(Box::new(record)),
        ))
    }
}

pub(crate) struct CreatePullReviewReply;

impl Command for CreatePullReviewReply {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = super::super::REPOSITORY_PULL_REVIEW_REPLY_CREATE_COMMAND_ID;
    const CODEC_VERSION: u32 = 1;
    type Input = CreatePullReviewReplyInput;
    type Output = CreatePullReviewReplyOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> cellule_runtime::Result<CommandResult<Self::Output>> {
        validate_number(input.pull)?;
        validate_number(input.thread)?;
        validate_author(&input.author)?;
        validate_body(&input.body, true)?;
        if load_thread(
            context,
            PullReviewThreadKey {
                pull: input.pull,
                number: input.thread,
            },
        )?
        .is_none()
        {
            return Ok(CommandResult::Rejected(
                CreatePullReviewReplyOutcome::ThreadNotFound,
            ));
        }
        let digest = reply_digest(&input);
        let existing = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT payload_digest, reply_number FROM repository_pull_review_reply_submissions WHERE pull_number = ? AND thread_number = ? AND request_id = ?",
                vec![
                    integer(input.pull)?,
                    integer(input.thread)?,
                    SqlValue::Blob(input.submission_id.to_vec()),
                ],
            )],
        })?;
        if let Some(row) = existing[0].rows.first() {
            if result_blob(row, 0)? != digest.as_bytes() {
                return Ok(CommandResult::Rejected(
                    CreatePullReviewReplyOutcome::RequestConflict,
                ));
            }
            let record = load_reply(
                context,
                PullReviewReplyKey {
                    pull: input.pull,
                    thread: input.thread,
                    number: result_u64_from_row(row, 1)?,
                },
            )?
            .ok_or(cellule_runtime::Error::Command(
                "repository pull review reply submission has no reply",
            ))?;
            return Ok(CommandResult::Success(
                CreatePullReviewReplyOutcome::Created(record),
            ));
        }
        let number = next_reply_number(context, input.pull, input.thread)?;
        let now = timestamp(context.now_ms())?;
        let record = PullReviewReplyRecord {
            pull: input.pull,
            thread: input.thread,
            number,
            author: input.author,
            body: input.body,
            version: 1,
            created_at_ms: now,
            updated_at_ms: now,
        };
        context.sql(&SqlBatch {
            statements: vec![
                statement(
                    "INSERT INTO repository_pull_review_reply_submissions(pull_number, thread_number, request_id, payload_digest, reply_number) VALUES (?, ?, ?, ?, ?)",
                    vec![
                        integer(record.pull)?,
                        integer(record.thread)?,
                        SqlValue::Blob(input.submission_id.to_vec()),
                        SqlValue::Blob(digest.as_bytes().to_vec()),
                        integer(record.number)?,
                    ],
                ),
                insert_reply_statement(&record)?,
            ],
        })?;
        advance_revision(context)?;
        Ok(CommandResult::Success(
            CreatePullReviewReplyOutcome::Created(record),
        ))
    }
}

pub(crate) struct UpdatePullReviewReply;

impl Command for UpdatePullReviewReply {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = super::super::REPOSITORY_PULL_REVIEW_REPLY_UPDATE_COMMAND_ID;
    const CODEC_VERSION: u32 = 1;
    type Input = UpdatePullReviewReplyInput;
    type Output = UpdatePullReviewReplyOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> cellule_runtime::Result<CommandResult<Self::Output>> {
        validate_number(input.key.pull)?;
        validate_number(input.key.thread)?;
        validate_number(input.key.number)?;
        validate_number(input.version)?;
        validate_author(&input.actor)?;
        validate_body(&input.body, true)?;
        let Some(mut record) = load_reply(context, input.key.clone())? else {
            return Ok(CommandResult::Rejected(
                UpdatePullReviewReplyOutcome::NotFound,
            ));
        };
        if !same_author(&record.author, &input.actor) {
            return Ok(CommandResult::Rejected(
                UpdatePullReviewReplyOutcome::Forbidden,
            ));
        }
        if record.version != input.version {
            return Ok(CommandResult::Rejected(
                UpdatePullReviewReplyOutcome::Conflict,
            ));
        }
        record.body = input.body;
        record.version = next_version(record.version)?;
        record.updated_at_ms = timestamp(context.now_ms())?;
        context.sql(&SqlBatch {
            statements: vec![statement(
                "UPDATE repository_pull_review_replies SET body = ?, version = ?, updated_at_ms = ? WHERE pull_number = ? AND thread_number = ? AND number = ? AND version = ?",
                vec![
                    SqlValue::Text(record.body.clone()),
                    integer(record.version)?,
                    integer(record.updated_at_ms)?,
                    integer(record.pull)?,
                    integer(record.thread)?,
                    integer(record.number)?,
                    integer(input.version)?,
                ],
            )],
        })?;
        advance_revision(context)?;
        Ok(CommandResult::Success(
            UpdatePullReviewReplyOutcome::Updated(record),
        ))
    }
}

pub(crate) struct GetPullReviewThread;

impl Query for GetPullReviewThread {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = super::super::REPOSITORY_PULL_REVIEW_THREAD_GET_QUERY_ID;
    const CODEC_VERSION: u32 = 1;
    type Input = PullReviewThreadKey;
    type Output = Option<PullReviewThreadRecord>;

    fn execute(
        context: &mut QueryContext<'_>,
        key: Self::Input,
    ) -> cellule_runtime::Result<Self::Output> {
        validate_thread_key(&key)?;
        load_thread_query(context, key)
    }
}

pub(crate) struct ListPullReviewThreads;

impl Query for ListPullReviewThreads {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = super::super::REPOSITORY_PULL_REVIEW_THREAD_LIST_QUERY_ID;
    const CODEC_VERSION: u32 = 1;
    type Input = PullReviewThreadListInput;
    type Output = Option<PullReviewThreadPage>;

    fn execute(
        context: &mut QueryContext<'_>,
        input: Self::Input,
    ) -> cellule_runtime::Result<Self::Output> {
        validate_thread_list(&input)?;
        if !pull_exists_query(context, input.pull)? {
            return Ok(None);
        }
        let rows = list_threads(context, &input)?;
        let items = rows
            .iter()
            .map(|row| thread_from_row(row))
            .collect::<cellule_runtime::Result<Vec<_>>>()?;
        let (items, next) = bounded_page(items, input.limit, |item| item.number)?;
        Ok(Some(PullReviewThreadPage { items, next }))
    }
}

pub(crate) struct GetPullReviewThreadSubmission;

impl Query for GetPullReviewThreadSubmission {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = super::super::REPOSITORY_PULL_REVIEW_THREAD_SUBMISSION_QUERY_ID;
    const CODEC_VERSION: u32 = 1;
    type Input = PullReviewThreadSubmissionKey;
    type Output = Option<PullReviewThreadRecord>;

    fn execute(
        context: &mut QueryContext<'_>,
        key: Self::Input,
    ) -> cellule_runtime::Result<Self::Output> {
        let pull = key.pull.ok_or(cellule_runtime::Error::Command(
            "repository pull review thread submission scope is invalid",
        ))?;
        validate_number(pull)?;
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT thread_number FROM repository_pull_review_thread_submissions WHERE pull_number = ? AND request_id = ?",
                vec![integer(pull)?, SqlValue::Blob(key.submission_id.to_vec())],
            )],
        })?;
        result[0]
            .rows
            .first()
            .map(|row| result_u64_from_row(row, 0))
            .transpose()?
            .map(|number| load_thread_query(context, PullReviewThreadKey { pull, number }))
            .transpose()
            .map(Option::flatten)
    }
}

pub(crate) struct GetPullReviewReply;

impl Query for GetPullReviewReply {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = super::super::REPOSITORY_PULL_REVIEW_REPLY_GET_QUERY_ID;
    const CODEC_VERSION: u32 = 1;
    type Input = PullReviewReplyKey;
    type Output = Option<PullReviewReplyRecord>;

    fn execute(
        context: &mut QueryContext<'_>,
        key: Self::Input,
    ) -> cellule_runtime::Result<Self::Output> {
        validate_reply_key(&key)?;
        load_reply_query(context, key)
    }
}

pub(crate) struct ListPullReviewReplies;

impl Query for ListPullReviewReplies {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = super::super::REPOSITORY_PULL_REVIEW_REPLY_LIST_QUERY_ID;
    const CODEC_VERSION: u32 = 1;
    type Input = PullReviewReplyListInput;
    type Output = Option<PullReviewReplyPage>;

    fn execute(
        context: &mut QueryContext<'_>,
        input: Self::Input,
    ) -> cellule_runtime::Result<Self::Output> {
        validate_reply_list(&input)?;
        if !thread_exists_query(context, input.pull, input.thread)? {
            return Ok(None);
        }
        let rows = list_replies(context, &input)?;
        let items = rows
            .iter()
            .map(|row| reply_from_row(row))
            .collect::<cellule_runtime::Result<Vec<_>>>()?;
        let (items, next) = bounded_page(items, input.limit, |item| item.number)?;
        Ok(Some(PullReviewReplyPage { items, next }))
    }
}

pub(crate) struct GetPullReviewReplySubmission;

impl Query for GetPullReviewReplySubmission {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = super::super::REPOSITORY_PULL_REVIEW_REPLY_SUBMISSION_QUERY_ID;
    const CODEC_VERSION: u32 = 1;
    type Input = PullReviewReplySubmissionKey;
    type Output = Option<PullReviewReplyRecord>;

    fn execute(
        context: &mut QueryContext<'_>,
        key: Self::Input,
    ) -> cellule_runtime::Result<Self::Output> {
        let pull = key.pull.ok_or(cellule_runtime::Error::Command(
            "repository pull review reply submission scope is invalid",
        ))?;
        let thread = key.thread.ok_or(cellule_runtime::Error::Command(
            "repository pull review reply submission scope is invalid",
        ))?;
        validate_number(pull)?;
        validate_number(thread)?;
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT reply_number FROM repository_pull_review_reply_submissions WHERE pull_number = ? AND thread_number = ? AND request_id = ?",
                vec![
                    integer(pull)?,
                    integer(thread)?,
                    SqlValue::Blob(key.submission_id.to_vec()),
                ],
            )],
        })?;
        result[0]
            .rows
            .first()
            .map(|row| result_u64_from_row(row, 0))
            .transpose()?
            .map(|number| {
                load_reply_query(
                    context,
                    PullReviewReplyKey {
                        pull,
                        thread,
                        number,
                    },
                )
            })
            .transpose()
            .map(Option::flatten)
    }
}

fn same_author(left: &RepositoryAuthor, right: &RepositoryAuthor) -> bool {
    left.issuer == right.issuer && left.subject == right.subject
}

fn validate_create_thread(input: &CreatePullReviewThreadInput) -> cellule_runtime::Result<()> {
    validate_number(input.pull)?;
    validate_author(&input.author)?;
    validate_body(&input.body, true)?;
    validate_oid(&input.base_oid)?;
    validate_oid(&input.head_oid)?;
    validate_path(&input.path)?;
    validate_oid_option(&input.old_blob_oid)?;
    validate_oid_option(&input.new_blob_oid)?;
    validate_lines(input.start_line, input.end_line)?;
    validate_suggested_text(input.suggested_text.as_deref())?;
    match input.side {
        PullReviewThreadSide::Old => {
            if input.old_blob_oid.is_none() || input.suggested_text.is_some() {
                return Err(cellule_runtime::Error::Command(
                    "old pull review thread anchor is invalid",
                ));
            }
        }
        PullReviewThreadSide::New => {
            if input.new_blob_oid.is_none() {
                return Err(cellule_runtime::Error::Command(
                    "new pull review thread anchor is invalid",
                ));
            }
        }
    }
    Ok(())
}

fn validate_thread_key(key: &PullReviewThreadKey) -> cellule_runtime::Result<()> {
    validate_number(key.pull)?;
    validate_number(key.number)
}

fn validate_reply_key(key: &PullReviewReplyKey) -> cellule_runtime::Result<()> {
    validate_number(key.pull)?;
    validate_number(key.thread)?;
    validate_number(key.number)
}

fn validate_thread_list(input: &PullReviewThreadListInput) -> cellule_runtime::Result<()> {
    validate_number(input.pull)?;
    validate_list(input.before, input.limit)?;
    if input
        .path
        .as_ref()
        .is_some_and(|path| validate_path(path).is_err())
    {
        return Err(cellule_runtime::Error::Command(
            "repository pull review thread path is invalid",
        ));
    }
    if input.comparison_base_oid.is_some() != input.comparison_head_oid.is_some()
        || (input.outdated.is_some()
            && (input.comparison_base_oid.is_none() || input.comparison_head_oid.is_none()))
    {
        return Err(cellule_runtime::Error::Command(
            "repository pull review thread comparison is incomplete",
        ));
    }
    if let Some(base) = &input.comparison_base_oid {
        validate_oid(base)?;
    }
    if let Some(head) = &input.comparison_head_oid {
        validate_oid(head)?;
    }
    Ok(())
}

fn validate_reply_list(input: &PullReviewReplyListInput) -> cellule_runtime::Result<()> {
    validate_number(input.pull)?;
    validate_number(input.thread)?;
    validate_list(input.before, input.limit)
}

fn validate_path(path: &[u8]) -> cellule_runtime::Result<()> {
    if path.is_empty() || path.len() > 1024 || path.contains(&0) {
        return Err(cellule_runtime::Error::Command(
            "repository pull review thread path is invalid",
        ));
    }
    Ok(())
}

fn validate_oid(value: &str) -> cellule_runtime::Result<()> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || value.bytes().all(|byte| byte == b'0')
    {
        return Err(cellule_runtime::Error::Command(
            "repository pull review thread object ID is invalid",
        ));
    }
    Ok(())
}

fn validate_oid_option(value: &Option<String>) -> cellule_runtime::Result<()> {
    if let Some(value) = value {
        validate_oid(value)?;
    }
    Ok(())
}

fn validate_lines(start_line: u64, end_line: u64) -> cellule_runtime::Result<()> {
    validate_number(start_line)?;
    validate_number(end_line)?;
    if end_line < start_line || end_line - start_line >= 200 {
        return Err(cellule_runtime::Error::Command(
            "repository pull review thread line range is invalid",
        ));
    }
    Ok(())
}

fn validate_suggested_text(value: Option<&str>) -> cellule_runtime::Result<()> {
    if value.is_some_and(|value| value.len() > MAX_SUGGESTED_TEXT_BYTES || value.contains('\0')) {
        return Err(cellule_runtime::Error::Command(
            "repository pull review suggestion is invalid",
        ));
    }
    Ok(())
}

fn pull_state(context: &CommandContext<'_, '_>, pull: u64) -> cellule_runtime::Result<Option<u64>> {
    let result = context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT state FROM repository_pulls WHERE number = ?",
            vec![integer(pull)?],
        )],
    })?;
    result[0]
        .rows
        .first()
        .map(|row| result_u64_from_row(row, 0))
        .transpose()
}

fn next_number(
    context: &CommandContext<'_, '_>,
    table: &str,
    pull: u64,
) -> cellule_runtime::Result<u64> {
    let insert = format!(
        "INSERT INTO {table}(pull_number, last) VALUES (?, 1) ON CONFLICT(pull_number) DO UPDATE SET last = last + 1 WHERE last < 9007199254740991"
    );
    let result = context.sql(&SqlBatch {
        statements: vec![
            statement(&insert, vec![integer(pull)?]),
            statement(
                &format!("SELECT last FROM {table} WHERE pull_number = ?"),
                vec![integer(pull)?],
            ),
        ],
    })?;
    if result[0].rows_affected != 1 {
        return Err(cellule_runtime::Error::Command(
            "repository pull review numbering is exhausted",
        ));
    }
    result_u64(&result, 1, 0)
}

fn next_reply_number(
    context: &CommandContext<'_, '_>,
    pull: u64,
    thread: u64,
) -> cellule_runtime::Result<u64> {
    let result = context.sql(&SqlBatch {
        statements: vec![
            statement(
                "INSERT INTO repository_pull_review_reply_sequences(pull_number, thread_number, last) VALUES (?, ?, 1) ON CONFLICT(pull_number, thread_number) DO UPDATE SET last = last + 1 WHERE last < 9007199254740991",
                vec![integer(pull)?, integer(thread)?],
            ),
            statement(
                "SELECT last FROM repository_pull_review_reply_sequences WHERE pull_number = ? AND thread_number = ?",
                vec![integer(pull)?, integer(thread)?],
            ),
        ],
    })?;
    if result[0].rows_affected != 1 {
        return Err(cellule_runtime::Error::Command(
            "repository pull review reply numbering is exhausted",
        ));
    }
    result_u64(&result, 1, 0)
}

fn thread_digest(input: &CreatePullReviewThreadInput) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.repository.pull-review-thread.v1\0");
    hasher.update(&input.pull.to_be_bytes());
    hash_text(&mut hasher, &input.author.issuer);
    hash_text(&mut hasher, &input.author.subject);
    hash_text(&mut hasher, &input.body);
    match &input.suggested_text {
        Some(value) => {
            hasher.update(&[1]);
            hash_text(&mut hasher, value);
        }
        None => {
            hasher.update(&[0]);
        }
    }
    hash_text(&mut hasher, &input.base_oid);
    hash_text(&mut hasher, &input.head_oid);
    hasher.update(&(input.path.len() as u64).to_be_bytes());
    hasher.update(&input.path);
    for value in [&input.old_blob_oid, &input.new_blob_oid] {
        match value {
            Some(value) => {
                hasher.update(&[1]);
                hash_text(&mut hasher, value);
            }
            None => {
                hasher.update(&[0]);
            }
        }
    }
    hasher.update(&[input.side.code()]);
    hasher.update(&input.start_line.to_be_bytes());
    hasher.update(&input.end_line.to_be_bytes());
    hasher.finalize()
}

fn reply_digest(input: &CreatePullReviewReplyInput) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.repository.pull-review-reply.v1\0");
    hasher.update(&input.pull.to_be_bytes());
    hasher.update(&input.thread.to_be_bytes());
    hash_text(&mut hasher, &input.author.issuer);
    hash_text(&mut hasher, &input.author.subject);
    hash_text(&mut hasher, &input.body);
    hasher.finalize()
}

fn insert_thread_statement(
    record: &PullReviewThreadRecord,
) -> cellule_runtime::Result<SqlStatement> {
    Ok(statement(
        "INSERT INTO repository_pull_review_threads(pull_number, number, author_issuer, author_subject, author_name, body, suggested_text, base_oid, head_oid, path, old_blob_oid, new_blob_oid, side, start_line, end_line, resolved, resolved_by_issuer, resolved_by_subject, resolved_by_name, resolved_at_ms, version, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        vec![
            integer(record.pull)?,
            integer(record.number)?,
            SqlValue::Text(record.author.issuer.clone()),
            SqlValue::Text(record.author.subject.clone()),
            SqlValue::Text(record.author.name.clone()),
            SqlValue::Text(record.body.clone()),
            optional_text(&record.suggested_text),
            SqlValue::Text(record.base_oid.clone()),
            SqlValue::Text(record.head_oid.clone()),
            SqlValue::Blob(record.path.clone()),
            optional_text(&record.old_blob_oid),
            optional_text(&record.new_blob_oid),
            SqlValue::Integer(i64::from(record.side.code())),
            integer(record.start_line)?,
            integer(record.end_line)?,
            SqlValue::Integer(if record.resolved { 1 } else { 0 }),
            optional_author_field(&record.resolved_by, |author| &author.issuer),
            optional_author_field(&record.resolved_by, |author| &author.subject),
            optional_author_field(&record.resolved_by, |author| &author.name),
            optional_integer(record.resolved_at_ms)?,
            integer(record.version)?,
            integer(record.created_at_ms)?,
            integer(record.updated_at_ms)?,
        ],
    ))
}

fn update_thread_statement(
    record: &PullReviewThreadRecord,
    old_version: u64,
) -> cellule_runtime::Result<SqlStatement> {
    let mut statement = insert_thread_statement(record)?;
    statement.sql = "UPDATE repository_pull_review_threads SET body = ?, suggested_text = ?, resolved = ?, resolved_by_issuer = ?, resolved_by_subject = ?, resolved_by_name = ?, resolved_at_ms = ?, version = ?, updated_at_ms = ? WHERE pull_number = ? AND number = ? AND version = ?".into();
    statement.parameters = vec![
        SqlValue::Text(record.body.clone()),
        optional_text(&record.suggested_text),
        SqlValue::Integer(if record.resolved { 1 } else { 0 }),
        optional_author_field(&record.resolved_by, |author| &author.issuer),
        optional_author_field(&record.resolved_by, |author| &author.subject),
        optional_author_field(&record.resolved_by, |author| &author.name),
        optional_integer(record.resolved_at_ms)?,
        integer(record.version)?,
        integer(record.updated_at_ms)?,
        integer(record.pull)?,
        integer(record.number)?,
        integer(old_version)?,
    ];
    Ok(statement)
}

fn insert_reply_statement(record: &PullReviewReplyRecord) -> cellule_runtime::Result<SqlStatement> {
    Ok(statement(
        "INSERT INTO repository_pull_review_replies(pull_number, thread_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        vec![
            integer(record.pull)?,
            integer(record.thread)?,
            integer(record.number)?,
            SqlValue::Text(record.author.issuer.clone()),
            SqlValue::Text(record.author.subject.clone()),
            SqlValue::Text(record.author.name.clone()),
            SqlValue::Text(record.body.clone()),
            integer(record.version)?,
            integer(record.created_at_ms)?,
            integer(record.updated_at_ms)?,
        ],
    ))
}

fn optional_text(value: &Option<String>) -> SqlValue {
    value
        .as_ref()
        .map_or(SqlValue::Null, |value| SqlValue::Text(value.clone()))
}

fn optional_integer(value: Option<u64>) -> cellule_runtime::Result<SqlValue> {
    value.map_or(Ok(SqlValue::Null), integer)
}

fn optional_author_field(
    value: &Option<RepositoryAuthor>,
    field: impl Fn(&RepositoryAuthor) -> &String,
) -> SqlValue {
    value.as_ref().map_or(SqlValue::Null, |author| {
        SqlValue::Text(field(author).clone())
    })
}

fn load_thread(
    context: &CommandContext<'_, '_>,
    key: PullReviewThreadKey,
) -> cellule_runtime::Result<Option<PullReviewThreadRecord>> {
    validate_thread_key(&key)?;
    let result = context.sql(&SqlBatch {
        statements: vec![thread_select_statement(key.pull, key.number)?],
    })?;
    result[0]
        .rows
        .first()
        .map(|row| thread_from_row(row))
        .transpose()
}

fn load_thread_query(
    context: &QueryContext<'_>,
    key: PullReviewThreadKey,
) -> cellule_runtime::Result<Option<PullReviewThreadRecord>> {
    validate_thread_key(&key)?;
    let result = context.sql(&SqlBatch {
        statements: vec![thread_select_statement(key.pull, key.number)?],
    })?;
    result[0]
        .rows
        .first()
        .map(|row| thread_from_row(row))
        .transpose()
}

fn thread_select_statement(pull: u64, number: u64) -> cellule_runtime::Result<SqlStatement> {
    Ok(statement(
        "SELECT pull_number, number, author_issuer, author_subject, author_name, body, suggested_text, base_oid, head_oid, path, old_blob_oid, new_blob_oid, side, start_line, end_line, resolved, resolved_by_issuer, resolved_by_subject, resolved_by_name, resolved_at_ms, version, created_at_ms, updated_at_ms FROM repository_pull_review_threads WHERE pull_number = ? AND number = ?",
        vec![integer(pull)?, integer(number)?],
    ))
}

fn thread_from_row(row: &[SqlValue]) -> cellule_runtime::Result<PullReviewThreadRecord> {
    let resolved = result_u64_from_row(row, 15)?;
    let resolved_by = optional_author(row, 16)?;
    let resolved_at_ms = result_optional_u64(row, 19)?;
    if (resolved == 0) != resolved_by.is_none() || (resolved == 0) != resolved_at_ms.is_none() {
        return Err(cellule_runtime::Error::Command(
            "repository pull review thread resolution is invalid",
        ));
    }
    let record = PullReviewThreadRecord {
        pull: result_u64_from_row(row, 0)?,
        number: result_u64_from_row(row, 1)?,
        author: author_from_row(row, 2)?,
        body: result_text(row, 5)?,
        suggested_text: result_optional_text(row, 6)?,
        base_oid: result_text(row, 7)?,
        head_oid: result_text(row, 8)?,
        path: result_blob(row, 9)?.to_vec(),
        old_blob_oid: result_optional_text(row, 10)?,
        new_blob_oid: result_optional_text(row, 11)?,
        side: PullReviewThreadSide::from_code(result_u64_from_row(row, 12)?)?,
        start_line: result_u64_from_row(row, 13)?,
        end_line: result_u64_from_row(row, 14)?,
        resolved: resolved != 0,
        resolved_by,
        resolved_at_ms,
        version: result_u64_from_row(row, 20)?,
        created_at_ms: result_u64_from_row(row, 21)?,
        updated_at_ms: result_u64_from_row(row, 22)?,
    };
    validate_thread_record(&record)?;
    Ok(record)
}

fn validate_thread_record(record: &PullReviewThreadRecord) -> cellule_runtime::Result<()> {
    validate_number(record.pull)?;
    validate_number(record.number)?;
    validate_author(&record.author)?;
    validate_body(&record.body, true)?;
    validate_suggested_text(record.suggested_text.as_deref())?;
    validate_oid(&record.base_oid)?;
    validate_oid(&record.head_oid)?;
    validate_path(&record.path)?;
    validate_oid_option(&record.old_blob_oid)?;
    validate_oid_option(&record.new_blob_oid)?;
    validate_lines(record.start_line, record.end_line)?;
    if record.side == PullReviewThreadSide::Old
        && (record.old_blob_oid.is_none() || record.suggested_text.is_some())
    {
        return Err(cellule_runtime::Error::Command(
            "old pull review thread record is invalid",
        ));
    }
    if record.side == PullReviewThreadSide::New && record.new_blob_oid.is_none() {
        return Err(cellule_runtime::Error::Command(
            "new pull review thread record is invalid",
        ));
    }
    validate_number(record.version)?;
    if record.updated_at_ms < record.created_at_ms {
        return Err(cellule_runtime::Error::Command(
            "repository pull review thread timestamp is invalid",
        ));
    }
    if let Some(author) = &record.resolved_by {
        validate_author(author)?;
    }
    Ok(())
}

fn load_reply(
    context: &CommandContext<'_, '_>,
    key: PullReviewReplyKey,
) -> cellule_runtime::Result<Option<PullReviewReplyRecord>> {
    validate_reply_key(&key)?;
    let result = context.sql(&SqlBatch {
        statements: vec![reply_select_statement(&key)?],
    })?;
    result[0]
        .rows
        .first()
        .map(|row| reply_from_row(row))
        .transpose()
}

fn load_reply_query(
    context: &QueryContext<'_>,
    key: PullReviewReplyKey,
) -> cellule_runtime::Result<Option<PullReviewReplyRecord>> {
    validate_reply_key(&key)?;
    let result = context.sql(&SqlBatch {
        statements: vec![reply_select_statement(&key)?],
    })?;
    result[0]
        .rows
        .first()
        .map(|row| reply_from_row(row))
        .transpose()
}

fn reply_select_statement(key: &PullReviewReplyKey) -> cellule_runtime::Result<SqlStatement> {
    Ok(statement(
        "SELECT pull_number, thread_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms FROM repository_pull_review_replies WHERE pull_number = ? AND thread_number = ? AND number = ?",
        vec![
            integer(key.pull)?,
            integer(key.thread)?,
            integer(key.number)?,
        ],
    ))
}

fn reply_from_row(row: &[SqlValue]) -> cellule_runtime::Result<PullReviewReplyRecord> {
    let record = PullReviewReplyRecord {
        pull: result_u64_from_row(row, 0)?,
        thread: result_u64_from_row(row, 1)?,
        number: result_u64_from_row(row, 2)?,
        author: author_from_row(row, 3)?,
        body: result_text(row, 6)?,
        version: result_u64_from_row(row, 7)?,
        created_at_ms: result_u64_from_row(row, 8)?,
        updated_at_ms: result_u64_from_row(row, 9)?,
    };
    validate_number(record.pull)?;
    validate_number(record.thread)?;
    validate_number(record.number)?;
    validate_author(&record.author)?;
    validate_body(&record.body, true)?;
    validate_number(record.version)?;
    if record.updated_at_ms < record.created_at_ms {
        return Err(cellule_runtime::Error::Command(
            "repository pull review reply timestamp is invalid",
        ));
    }
    Ok(record)
}

fn author_from_row(row: &[SqlValue], start: usize) -> cellule_runtime::Result<RepositoryAuthor> {
    Ok(RepositoryAuthor {
        issuer: result_text(row, start)?,
        subject: result_text(row, start + 1)?,
        name: result_text(row, start + 2)?,
    })
}

fn optional_author(
    row: &[SqlValue],
    start: usize,
) -> cellule_runtime::Result<Option<RepositoryAuthor>> {
    let issuer = result_optional_text(row, start)?;
    let subject = result_optional_text(row, start + 1)?;
    let name = result_optional_text(row, start + 2)?;
    match (issuer, subject, name) {
        (None, None, None) => Ok(None),
        (Some(issuer), Some(subject), Some(name)) => Ok(Some(RepositoryAuthor {
            issuer,
            subject,
            name,
        })),
        _ => Err(cellule_runtime::Error::Command(
            "repository pull review thread resolver is invalid",
        )),
    }
}

fn result_optional_u64(row: &[SqlValue], column: usize) -> cellule_runtime::Result<Option<u64>> {
    match row.get(column) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Integer(value)) => u64::try_from(*value)
            .map(Some)
            .map_err(|_| cellule_runtime::Error::Command("repository result is negative")),
        _ => Err(cellule_runtime::Error::Command(
            "repository query returned invalid optional integer",
        )),
    }
}

fn list_threads(
    context: &QueryContext<'_>,
    input: &PullReviewThreadListInput,
) -> cellule_runtime::Result<Vec<Vec<SqlValue>>> {
    let upper = input.before.map_or(MAX_NUMBER, |value| value - 1);
    let mut sql = String::from(
        "SELECT pull_number, number, author_issuer, author_subject, author_name, body, suggested_text, base_oid, head_oid, path, old_blob_oid, new_blob_oid, side, start_line, end_line, resolved, resolved_by_issuer, resolved_by_subject, resolved_by_name, resolved_at_ms, version, created_at_ms, updated_at_ms FROM repository_pull_review_threads WHERE pull_number = ? AND number <= ?",
    );
    let mut parameters = vec![integer(input.pull)?, integer(upper)?];
    if let Some(path) = &input.path {
        sql.push_str(" AND path = ?");
        parameters.push(SqlValue::Blob(path.clone()));
    }
    if let Some(resolved) = input.resolved {
        sql.push_str(" AND resolved = ?");
        parameters.push(SqlValue::Integer(i64::from(resolved)));
    }
    match input.outdated {
        Some(false) => {
            sql.push_str(" AND base_oid = ? AND head_oid = ?");
            parameters.push(SqlValue::Text(input.comparison_base_oid.clone().ok_or(
                cellule_runtime::Error::Command("thread comparison is incomplete"),
            )?));
            parameters.push(SqlValue::Text(input.comparison_head_oid.clone().ok_or(
                cellule_runtime::Error::Command("thread comparison is incomplete"),
            )?));
        }
        Some(true) => {
            sql.push_str(" AND NOT (base_oid = ? AND head_oid = ?)");
            parameters.push(SqlValue::Text(input.comparison_base_oid.clone().ok_or(
                cellule_runtime::Error::Command("thread comparison is incomplete"),
            )?));
            parameters.push(SqlValue::Text(input.comparison_head_oid.clone().ok_or(
                cellule_runtime::Error::Command("thread comparison is incomplete"),
            )?));
        }
        None => {}
    }
    sql.push_str(" ORDER BY number DESC LIMIT ?");
    parameters.push(SqlValue::Integer(i64::from(input.limit)));
    Ok(context.sql(&SqlBatch {
        statements: vec![statement(&sql, parameters)],
    })?[0]
        .rows
        .clone())
}

fn list_replies(
    context: &QueryContext<'_>,
    input: &PullReviewReplyListInput,
) -> cellule_runtime::Result<Vec<Vec<SqlValue>>> {
    let upper = input.before.map_or(MAX_NUMBER, |value| value - 1);
    Ok(context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT pull_number, thread_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms FROM repository_pull_review_replies WHERE pull_number = ? AND thread_number = ? AND number <= ? ORDER BY number DESC LIMIT ?",
            vec![
                integer(input.pull)?,
                integer(input.thread)?,
                integer(upper)?,
                SqlValue::Integer(i64::from(input.limit)),
            ],
        )],
    })?[0]
        .rows
        .clone())
}

fn pull_exists_query(context: &QueryContext<'_>, pull: u64) -> cellule_runtime::Result<bool> {
    let result = context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT 1 FROM repository_pulls WHERE number = ?",
            vec![integer(pull)?],
        )],
    })?;
    Ok(!result[0].rows.is_empty())
}

fn thread_exists_query(
    context: &QueryContext<'_>,
    pull: u64,
    thread: u64,
) -> cellule_runtime::Result<bool> {
    let result = context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT 1 FROM repository_pull_review_threads WHERE pull_number = ? AND number = ?",
            vec![integer(pull)?, integer(thread)?],
        )],
    })?;
    Ok(!result[0].rows.is_empty())
}

fn bounded_page<T: Serialize>(
    items: Vec<T>,
    requested: u8,
    number: impl Fn(&T) -> u64,
) -> cellule_runtime::Result<(Vec<T>, Option<u64>)> {
    let total = items.len();
    let mut used =
        4 + br#"{"items":["#.len() + br#"],"next":"#.len() + MAX_NUMBER.to_string().len() + 1;
    let mut kept = Vec::with_capacity(total);
    for item in items {
        let encoded = serde_json::to_vec(&item).map_err(|_| {
            cellule_runtime::Error::Command("repository pull review page is invalid")
        })?;
        let separator = usize::from(!kept.is_empty());
        if used
            .checked_add(separator + encoded.len())
            .is_none_or(|size| size > MAX_PAGE_WIRE_BYTES)
        {
            break;
        }
        used += separator + encoded.len();
        kept.push(item);
    }
    if kept.is_empty() && total != 0 {
        return Err(cellule_runtime::Error::Command(
            "repository pull review page exceeds the limit",
        ));
    }
    let next = (kept.len() < total || total == usize::from(requested))
        .then(|| kept.last().map(&number))
        .flatten();
    Ok((kept, next))
}

fn next_version(value: u64) -> cellule_runtime::Result<u64> {
    value
        .checked_add(1)
        .filter(|value| *value < MAX_NUMBER)
        .ok_or(cellule_runtime::Error::Command(
            "repository pull review version is exhausted",
        ))
}

#[cfg(test)]
mod tests {
    use cellule_runtime::{BoundedDecoder, BoundedEncoder, WireValue};

    use super::*;

    fn author() -> RepositoryAuthor {
        RepositoryAuthor {
            issuer: "https://issuer.example".into(),
            subject: "reviewer".into(),
            name: "Reviewer".into(),
        }
    }

    fn oid(byte: char) -> String {
        std::iter::repeat_n(byte, 40).collect()
    }

    fn input() -> CreatePullReviewThreadInput {
        CreatePullReviewThreadInput {
            pull: 1,
            submission_id: [1; 16],
            author: author(),
            body: "Please clarify this branch".into(),
            suggested_text: Some("clarified branch".into()),
            base_oid: oid('1'),
            head_oid: oid('2'),
            path: vec![0xff, b'.', b't', b'x', b't'],
            old_blob_oid: Some(oid('3')),
            new_blob_oid: Some(oid('4')),
            side: PullReviewThreadSide::New,
            start_line: 3,
            end_line: 5,
        }
    }

    #[test]
    fn validation_preserves_raw_paths_and_rejects_invalid_ranges_and_suggestions() {
        let valid = input();
        assert!(validate_create_thread(&valid).is_ok());

        let mut invalid = valid.clone();
        invalid.path = vec![0];
        assert!(validate_create_thread(&invalid).is_err());

        let mut invalid = valid.clone();
        invalid.start_line = 0;
        assert!(validate_create_thread(&invalid).is_err());

        let mut invalid = valid.clone();
        invalid.end_line = 203;
        assert!(validate_create_thread(&invalid).is_err());

        let mut invalid = valid.clone();
        invalid.suggested_text = Some("x".repeat(MAX_SUGGESTED_TEXT_BYTES + 1));
        assert!(validate_create_thread(&invalid).is_err());

        let mut invalid = valid;
        invalid.suggested_text = Some("bad\0suggestion".into());
        assert!(validate_create_thread(&invalid).is_err());

        let mut old_side = input();
        old_side.side = PullReviewThreadSide::Old;
        assert!(validate_create_thread(&old_side).is_err());
    }

    #[test]
    fn list_validation_requires_bounded_pages_and_complete_comparisons() {
        let valid = PullReviewThreadListInput {
            pull: 1,
            before: Some(2),
            limit: 30,
            path: Some(vec![0xff, b'.', b't', b'x', b't']),
            resolved: Some(false),
            comparison_base_oid: Some(oid('1')),
            comparison_head_oid: Some(oid('2')),
            outdated: Some(true),
        };
        assert!(validate_thread_list(&valid).is_ok());

        let mut invalid = valid.clone();
        invalid.limit = 0;
        assert!(validate_thread_list(&invalid).is_err());
        invalid.limit = 51;
        assert!(validate_thread_list(&invalid).is_err());
        invalid.limit = 30;
        invalid.comparison_head_oid = None;
        assert!(validate_thread_list(&invalid).is_err());
        invalid.comparison_head_oid = Some(oid('2'));
        invalid.before = Some(0);
        assert!(validate_thread_list(&invalid).is_err());
    }

    #[test]
    fn record_codec_round_trips_raw_path_and_resolved_metadata() {
        let input = input();
        let record = PullReviewThreadRecord {
            pull: input.pull,
            number: 7,
            author: input.author,
            body: input.body,
            suggested_text: input.suggested_text,
            base_oid: input.base_oid,
            head_oid: input.head_oid,
            path: input.path,
            old_blob_oid: input.old_blob_oid,
            new_blob_oid: input.new_blob_oid,
            side: input.side,
            start_line: input.start_line,
            end_line: input.end_line,
            resolved: true,
            resolved_by: Some(author()),
            resolved_at_ms: Some(42),
            version: 2,
            created_at_ms: 41,
            updated_at_ms: 42,
        };
        let mut encoder = BoundedEncoder::new(1024 * 1024).unwrap();
        record.encode(&mut encoder).unwrap();
        let bytes = encoder.finish();
        assert!(bytes.len() < 1024 * 1024);
        let mut decoder = BoundedDecoder::new(&bytes, 1024 * 1024).unwrap();
        let decoded = PullReviewThreadRecord::decode(&mut decoder).unwrap();
        decoder.finish().unwrap();
        assert_eq!(decoded, record);
        assert_eq!(decoded.path, vec![0xff, b'.', b't', b'x', b't']);
    }
}
