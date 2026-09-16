use serde::{Deserialize, Serialize};

use super::*;

const MAX_REVIEW_DECISIONS: u64 = 96;
const MAX_PAGE_WIRE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PullState {
    #[default]
    Open,
    Closed,
    Merged,
}

impl PullState {
    const fn code(self) -> u8 {
        match self {
            Self::Open => 0,
            Self::Closed => 1,
            Self::Merged => 2,
        }
    }

    fn from_code(code: u64) -> crab_cell_runtime::Result<Self> {
        match code {
            0 => Ok(Self::Open),
            1 => Ok(Self::Closed),
            2 => Ok(Self::Merged),
            _ => Err(crab_cell_runtime::Error::Command(
                "repository pull state is invalid",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReviewState {
    Commented,
    Approved,
    ChangesRequested,
}

impl ReviewState {
    const fn code(self) -> u8 {
        match self {
            Self::Commented => 0,
            Self::Approved => 1,
            Self::ChangesRequested => 2,
        }
    }

    fn from_code(code: u64) -> crab_cell_runtime::Result<Self> {
        match code {
            0 => Ok(Self::Commented),
            1 => Ok(Self::Approved),
            2 => Ok(Self::ChangesRequested),
            _ => Err(crab_cell_runtime::Error::Command(
                "repository review state is invalid",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MergeMethod {
    FastForward,
    MergeCommit,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullMerge {
    pub request_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub method: MergeMethod,
    pub pull_version: u64,
    pub base_oid: String,
    pub head_oid: String,
    pub commit_oid: String,
    pub message: String,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewDecision {
    pub review: u64,
    pub author: RepositoryAuthor,
    pub state: ReviewState,
    pub commit_oid: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullRecord {
    pub number: u64,
    pub create_submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub title: String,
    pub body: String,
    pub state: PullState,
    pub base_ref: String,
    pub base_oid: String,
    pub head_ref: String,
    pub head_oid: String,
    pub label_ids: Vec<u64>,
    pub assignee_subjects: Vec<String>,
    pub merge_pending: Option<PullMerge>,
    pub merge: Option<PullMerge>,
    pub review_decisions: Vec<PullReviewDecision>,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullSummary {
    pub number: u64,
    pub author: RepositoryAuthor,
    pub title: String,
    pub state: PullState,
    pub base_ref: String,
    pub head_ref: String,
    pub label_ids: Vec<u64>,
    pub assignee_subjects: Vec<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullPage {
    pub items: Vec<PullSummary>,
    pub next: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullCommentRecord {
    pub pull: u64,
    pub number: u64,
    pub author: RepositoryAuthor,
    pub body: String,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewRecord {
    pub pull: u64,
    pub number: u64,
    pub author: RepositoryAuthor,
    pub body: String,
    pub state: ReviewState,
    pub commit_oid: String,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullChildKey {
    pub pull: u64,
    pub number: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullSubmissionKey {
    pub pull: Option<u64>,
    pub submission_id: [u8; 16],
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullListInput {
    pub before: Option<u64>,
    pub limit: u8,
    pub state: u8,
    pub query: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullChildListInput {
    pub pull: u64,
    pub before: Option<u64>,
    pub limit: u8,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullCommentPage {
    pub items: Vec<PullCommentRecord>,
    pub next: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewPage {
    pub items: Vec<PullReviewRecord>,
    pub next: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreatePullInput {
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub title: String,
    pub body: String,
    pub base_ref: String,
    pub base_oid: String,
    pub head_ref: String,
    pub head_oid: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum CreatePullOutcome {
    Created(Box<PullRecord>),
    RequestConflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdatePullInput {
    pub number: u64,
    pub actor: RepositoryAuthor,
    pub can_manage: bool,
    pub version: u64,
    pub title: Option<String>,
    pub body: Option<String>,
    pub state: Option<PullState>,
    pub label_ids: Option<Vec<u64>>,
    pub assignee_subjects: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum UpdatePullOutcome {
    Updated(Box<PullRecord>),
    NotFound,
    Forbidden,
    MergePending,
    InvalidState,
    InvalidLabel,
    Conflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreatePullCommentInput {
    pub pull: u64,
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub body: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum CreatePullCommentOutcome {
    Created(PullCommentRecord),
    PullNotFound,
    RequestConflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdatePullCommentInput {
    pub key: PullChildKey,
    pub actor: RepositoryAuthor,
    pub version: u64,
    pub body: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum UpdatePullCommentOutcome {
    Updated(PullCommentRecord),
    NotFound,
    Forbidden,
    Conflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreatePullReviewInput {
    pub pull: u64,
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub body: String,
    pub state: ReviewState,
    pub commit_oid: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum CreatePullReviewOutcome {
    Created(PullReviewRecord),
    PullNotFound,
    PullClosed,
    MergePending,
    OwnReview,
    DecisionLimit,
    RequestConflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdatePullReviewInput {
    pub key: PullChildKey,
    pub actor: RepositoryAuthor,
    pub version: u64,
    pub body: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum UpdatePullReviewOutcome {
    Updated(PullReviewRecord),
    NotFound,
    Forbidden,
    Conflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReservePullMergeInput {
    pub pull: u64,
    pub merge: PullMerge,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum ReservePullMergeOutcome {
    Reserved(Box<PullMerge>),
    PullNotFound,
    RequestConflict,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum PullMergeTransition {
    Begin,
    Abort,
    Complete,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransitionPullMergeInput {
    pub pull: u64,
    pub merge: PullMerge,
    pub transition: PullMergeTransition,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum TransitionPullMergeOutcome {
    Applied(Box<PullRecord>),
    NotFound,
    Conflict,
}

pub(crate) struct CreatePull;

impl Command for CreatePull {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 16;
    const CODEC_VERSION: u32 = 1;
    type Input = CreatePullInput;
    type Output = CreatePullOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        validate_create_pull(&input)?;
        let digest = create_pull_digest(&input);
        let existing = context.sql(&SqlBatch { statements: vec![statement(
            "SELECT payload_digest, pull_number FROM repository_pull_submissions WHERE request_id = ?",
            vec![SqlValue::Blob(input.submission_id.to_vec())],
        )] })?;
        if let Some(row) = existing[0].rows.first() {
            if result_blob(row, 0)? != digest.as_bytes() {
                return Ok(CommandResult::Rejected(CreatePullOutcome::RequestConflict));
            }
            return Ok(CommandResult::Success(CreatePullOutcome::Created(
                Box::new(load_pull(context, result_u64_from_row(row, 1)?)?.ok_or(
                    crab_cell_runtime::Error::Command("repository pull submission has no pull"),
                )?),
            )));
        }
        let sequence = context.sql(&SqlBatch { statements: vec![
            statement("UPDATE repository_sequences SET last = last + 1 WHERE kind = 'pull' AND last < 9007199254740991", vec![]),
            statement("SELECT last FROM repository_sequences WHERE kind = 'pull'", vec![]),
        ] })?;
        if sequence[0].rows_affected != 1 {
            return Err(crab_cell_runtime::Error::Command(
                "repository pull numbering is exhausted",
            ));
        }
        let now = timestamp(context.now_ms())?;
        let record = PullRecord {
            number: result_u64(&sequence, 1, 0)?,
            create_submission_id: input.submission_id,
            author: input.author,
            title: input.title,
            body: input.body,
            state: PullState::Open,
            base_ref: input.base_ref,
            base_oid: input.base_oid,
            head_ref: input.head_ref,
            head_oid: input.head_oid,
            label_ids: vec![],
            assignee_subjects: vec![],
            merge_pending: None,
            merge: None,
            review_decisions: vec![],
            version: 1,
            created_at_ms: now,
            updated_at_ms: now,
        };
        context.sql(&SqlBatch { statements: vec![
            statement("INSERT INTO repository_pull_submissions(request_id, payload_digest, pull_number) VALUES (?, ?, ?)", vec![SqlValue::Blob(record.create_submission_id.to_vec()), SqlValue::Blob(digest.as_bytes().to_vec()), integer(record.number)?]),
            insert_pull_statement(&record)?,
        ] })?;
        advance_revision(context)?;
        Ok(CommandResult::Success(CreatePullOutcome::Created(
            Box::new(record),
        )))
    }
}

pub(crate) struct UpdatePull;

impl Command for UpdatePull {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 17;
    const CODEC_VERSION: u32 = 1;
    type Input = UpdatePullInput;
    type Output = UpdatePullOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        validate_number(input.number)?;
        validate_author(&input.actor)?;
        validate_number(input.version)?;
        if let Some(title) = &input.title {
            validate_title(title)?;
        }
        if let Some(body) = &input.body {
            validate_body(body, false)?;
        }
        if let Some(labels) = &input.label_ids {
            validate_label_ids(labels)?;
        }
        if let Some(assignees) = &input.assignee_subjects {
            validate_assignees(assignees)?;
        }
        let Some(mut pull) = load_pull(context, input.number)? else {
            return Ok(CommandResult::Rejected(UpdatePullOutcome::NotFound));
        };
        if pull.version != input.version {
            return Ok(CommandResult::Rejected(UpdatePullOutcome::Conflict));
        }
        if pull.merge_pending.is_some() {
            return Ok(CommandResult::Rejected(UpdatePullOutcome::MergePending));
        }
        let author = same_author(&pull.author, &input.actor);
        if (input.title.is_some() || input.body.is_some()) && !author
            || input.state.is_some() && !(author || input.can_manage)
            || (input.label_ids.is_some() || input.assignee_subjects.is_some()) && !input.can_manage
        {
            return Ok(CommandResult::Rejected(UpdatePullOutcome::Forbidden));
        }
        if input.state == Some(PullState::Merged) || pull.merge.is_some() && input.state.is_some() {
            return Ok(CommandResult::Rejected(UpdatePullOutcome::InvalidState));
        }
        if let Some(labels) = &input.label_ids {
            for label in labels {
                let found = context.sql(&SqlBatch { statements: vec![statement("SELECT 1 FROM repository_labels WHERE number = ? AND deleted_version IS NULL", vec![integer(*label)?])] })?;
                if found[0].rows.is_empty() {
                    return Ok(CommandResult::Rejected(UpdatePullOutcome::InvalidLabel));
                }
            }
        }
        if let Some(value) = input.title {
            pull.title = value;
        }
        if let Some(value) = input.body {
            pull.body = value;
        }
        if let Some(value) = input.state {
            pull.state = value;
        }
        if let Some(value) = input.label_ids {
            pull.label_ids = value;
        }
        if let Some(value) = input.assignee_subjects {
            pull.assignee_subjects = value;
        }
        pull.version = pull
            .version
            .checked_add(1)
            .filter(|value| *value < MAX_NUMBER)
            .ok_or(crab_cell_runtime::Error::Command(
                "repository pull version is exhausted",
            ))?;
        pull.updated_at_ms = timestamp(context.now_ms())?;
        update_pull_row(context, &pull, input.version)?;
        advance_revision(context)?;
        Ok(CommandResult::Success(UpdatePullOutcome::Updated(
            Box::new(pull),
        )))
    }
}

pub(crate) struct GetPull;

impl Query for GetPull {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 15;
    const CODEC_VERSION: u32 = 1;
    type Input = u64;
    type Output = Option<PullRecord>;

    fn execute(
        context: &mut QueryContext<'_>,
        number: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        validate_number(number)?;
        load_pull_query(context, number)
    }
}

pub(crate) struct GetPullSubmission;

impl Query for GetPullSubmission {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 16;
    const CODEC_VERSION: u32 = 1;
    type Input = PullSubmissionKey;
    type Output = Option<PullRecord>;

    fn execute(
        context: &mut QueryContext<'_>,
        key: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        if key.pull.is_some() {
            return Err(crab_cell_runtime::Error::Command(
                "repository pull submission scope is invalid",
            ));
        }
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT pull_number FROM repository_pull_submissions WHERE request_id = ?",
                vec![SqlValue::Blob(key.submission_id.to_vec())],
            )],
        })?;
        result[0]
            .rows
            .first()
            .map(|row| result_u64_from_row(row, 0))
            .transpose()?
            .map(|number| load_pull_query(context, number))
            .transpose()
            .map(Option::flatten)
    }
}

pub(crate) struct ListPulls;

impl Query for ListPulls {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 17;
    const CODEC_VERSION: u32 = 1;
    type Input = PullListInput;
    type Output = PullPage;

    fn execute(
        context: &mut QueryContext<'_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        validate_list(input.before, input.limit)?;
        validate_query(input.query.as_deref())?;
        if input.state > 2 {
            return Err(crab_cell_runtime::Error::Command(
                "repository pull list state is invalid",
            ));
        }
        let upper = input.before.map_or(MAX_NUMBER, |value| value - 1);
        let result = context.sql(&SqlBatch { statements: vec![statement(
            "SELECT number, author_issuer, author_subject, author_name, title, state, base_ref, head_ref, label_ids, assignee_subjects, created_at_ms, updated_at_ms, body FROM repository_pulls WHERE number <= ? ORDER BY number DESC LIMIT 200",
            vec![integer(upper)?],
        )] })?;
        let mut items = Vec::new();
        let mut examined = 0_u64;
        let mut last_examined = None;
        for row in &result[0].rows {
            examined += 1;
            last_examined = Some(result_u64_from_row(row, 0)?);
            let state = PullState::from_code(result_u64_from_row(row, 5)?)?;
            let matches_state = input.state == 2
                || input.state == 0 && state == PullState::Open
                || input.state == 1 && state != PullState::Open;
            let title = result_text(row, 4)?;
            let body = result_text(row, 12)?;
            let author_name = result_text(row, 3)?;
            let matches_text = input.query.as_ref().is_none_or(|query| {
                [&title, &body, &author_name]
                    .iter()
                    .any(|value| value.to_lowercase().contains(query))
            });
            if matches_state && matches_text {
                items.push(PullSummary {
                    number: result_u64_from_row(row, 0)?,
                    author: author_from_row(row, 1)?,
                    title: result_text(row, 4)?,
                    state,
                    base_ref: result_text(row, 6)?,
                    head_ref: result_text(row, 7)?,
                    label_ids: decode_label_ids(result_blob(row, 8)?)?,
                    assignee_subjects: decode_assignees(result_blob(row, 9)?)?,
                    created_at_ms: result_u64_from_row(row, 10)?,
                    updated_at_ms: result_u64_from_row(row, 11)?,
                });
            }
            if items.len() == usize::from(input.limit) {
                break;
            }
        }
        let next = (examined == MAX_LIST_SCAN || items.len() == usize::from(input.limit))
            .then_some(last_examined)
            .flatten();
        Ok(PullPage { items, next })
    }
}

fn validate_create_pull(input: &CreatePullInput) -> crab_cell_runtime::Result<()> {
    validate_author(&input.author)?;
    validate_title(&input.title)?;
    validate_body(&input.body, false)?;
    validate_ref(&input.base_ref)?;
    validate_ref(&input.head_ref)?;
    validate_oid(&input.base_oid)?;
    validate_oid(&input.head_oid)?;
    if input.base_ref == input.head_ref || input.base_oid == input.head_oid {
        return Err(crab_cell_runtime::Error::Command(
            "repository pull branches are invalid",
        ));
    }
    Ok(())
}

fn validate_ref(value: &str) -> crab_cell_runtime::Result<()> {
    if !value.starts_with("refs/heads/")
        || value == "refs/heads/"
        || value.len() > 1_024
        || value.chars().any(char::is_control)
    {
        return Err(crab_cell_runtime::Error::Command(
            "repository pull ref is invalid",
        ));
    }
    Ok(())
}

fn validate_oid(value: &str) -> crab_cell_runtime::Result<()> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || value.bytes().all(|byte| byte == b'0')
    {
        return Err(crab_cell_runtime::Error::Command(
            "repository pull object ID is invalid",
        ));
    }
    Ok(())
}

fn create_pull_digest(input: &CreatePullInput) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.repository.pull-submission.v1\0");
    hash_text(&mut hasher, &input.author.issuer);
    hash_text(&mut hasher, &input.author.subject);
    for value in [
        &input.title,
        &input.body,
        &input.base_ref,
        &input.base_oid,
        &input.head_ref,
        &input.head_oid,
    ] {
        hash_text(&mut hasher, value);
    }
    hasher.finalize()
}

fn insert_pull_statement(record: &PullRecord) -> crab_cell_runtime::Result<SqlStatement> {
    Ok(statement(
        "INSERT INTO repository_pulls(number, create_request_id, author_issuer, author_subject, author_name, title, body, state, base_ref, base_oid, head_ref, head_oid, label_ids, assignee_subjects, pending_merge, completed_merge, version, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        vec![
            integer(record.number)?,
            SqlValue::Blob(record.create_submission_id.to_vec()),
            SqlValue::Text(record.author.issuer.clone()),
            SqlValue::Text(record.author.subject.clone()),
            SqlValue::Text(record.author.name.clone()),
            SqlValue::Text(record.title.clone()),
            SqlValue::Text(record.body.clone()),
            SqlValue::Integer(i64::from(record.state.code())),
            SqlValue::Text(record.base_ref.clone()),
            SqlValue::Text(record.base_oid.clone()),
            SqlValue::Text(record.head_ref.clone()),
            SqlValue::Text(record.head_oid.clone()),
            SqlValue::Blob(encode_label_ids(&record.label_ids)?),
            SqlValue::Blob(encode_assignees(&record.assignee_subjects)?),
            optional_wire(&record.merge_pending)?,
            optional_wire(&record.merge)?,
            integer(record.version)?,
            integer(record.created_at_ms)?,
            integer(record.updated_at_ms)?,
        ],
    ))
}

fn update_pull_row(
    context: &CommandContext<'_, '_>,
    record: &PullRecord,
    expected: u64,
) -> crab_cell_runtime::Result<()> {
    let result = context.sql(&SqlBatch { statements: vec![statement("UPDATE repository_pulls SET title = ?, body = ?, state = ?, label_ids = ?, assignee_subjects = ?, pending_merge = ?, completed_merge = ?, version = ?, updated_at_ms = ? WHERE number = ? AND version = ?", vec![SqlValue::Text(record.title.clone()), SqlValue::Text(record.body.clone()), SqlValue::Integer(i64::from(record.state.code())), SqlValue::Blob(encode_label_ids(&record.label_ids)?), SqlValue::Blob(encode_assignees(&record.assignee_subjects)?), optional_wire(&record.merge_pending)?, optional_wire(&record.merge)?, integer(record.version)?, integer(record.updated_at_ms)?, integer(record.number)?, integer(expected)?])] })?;
    if result[0].rows_affected != 1 {
        return Err(crab_cell_runtime::Error::Command(
            "repository pull update lost its transaction",
        ));
    }
    Ok(())
}

fn optional_wire<T: WireValue>(value: &Option<T>) -> crab_cell_runtime::Result<SqlValue> {
    match value {
        Some(value) => {
            let mut encoder = BoundedEncoder::new(128 * 1024).map_err(|_| {
                crab_cell_runtime::Error::Command("repository pull value is too large")
            })?;
            value.encode(&mut encoder).map_err(|_| {
                crab_cell_runtime::Error::Command("repository pull value is too large")
            })?;
            Ok(SqlValue::Blob(encoder.finish()))
        }
        None => Ok(SqlValue::Null),
    }
}

fn decode_optional_wire<T: WireValue>(value: &SqlValue) -> crab_cell_runtime::Result<Option<T>> {
    match value {
        SqlValue::Null => Ok(None),
        SqlValue::Blob(bytes) => {
            let mut decoder = BoundedDecoder::new(bytes, 128 * 1024).map_err(|_| {
                crab_cell_runtime::Error::Command("repository pull value is invalid")
            })?;
            let value = T::decode(&mut decoder).map_err(|_| {
                crab_cell_runtime::Error::Command("repository pull value is invalid")
            })?;
            decoder.finish().map_err(|_| {
                crab_cell_runtime::Error::Command("repository pull value is invalid")
            })?;
            Ok(Some(value))
        }
        _ => Err(crab_cell_runtime::Error::Command(
            "repository pull value is invalid",
        )),
    }
}

fn pull_select() -> &'static str {
    "SELECT number, create_request_id, author_issuer, author_subject, author_name, title, body, state, base_ref, base_oid, head_ref, head_oid, label_ids, assignee_subjects, pending_merge, completed_merge, version, created_at_ms, updated_at_ms FROM repository_pulls WHERE number = ?"
}

fn load_pull(
    context: &CommandContext<'_, '_>,
    number: u64,
) -> crab_cell_runtime::Result<Option<PullRecord>> {
    let result = context.sql(&SqlBatch { statements: vec![statement(pull_select(), vec![integer(number)?]), statement("SELECT review_number, author_issuer, author_subject, author_name, state, commit_oid FROM repository_pull_review_decisions WHERE pull_number = ? ORDER BY review_number", vec![integer(number)?])] })?;
    result[0]
        .rows
        .first()
        .map(|row| pull_from_rows(row, &result[1].rows))
        .transpose()
}

fn load_pull_query(
    context: &QueryContext<'_>,
    number: u64,
) -> crab_cell_runtime::Result<Option<PullRecord>> {
    let result = context.sql(&SqlBatch { statements: vec![statement(pull_select(), vec![integer(number)?]), statement("SELECT review_number, author_issuer, author_subject, author_name, state, commit_oid FROM repository_pull_review_decisions WHERE pull_number = ? ORDER BY review_number", vec![integer(number)?])] })?;
    result[0]
        .rows
        .first()
        .map(|row| pull_from_rows(row, &result[1].rows))
        .transpose()
}

fn pull_from_rows(
    row: &[SqlValue],
    decisions: &[Vec<SqlValue>],
) -> crab_cell_runtime::Result<PullRecord> {
    let submission = <[u8; 16]>::try_from(result_blob(row, 1)?).map_err(|_| {
        crab_cell_runtime::Error::Command("repository pull submission ID is invalid")
    })?;
    let review_decisions = decisions
        .iter()
        .map(|decision| {
            Ok(PullReviewDecision {
                review: result_u64_from_row(decision, 0)?,
                author: author_from_row(decision, 1)?,
                state: ReviewState::from_code(result_u64_from_row(decision, 4)?)?,
                commit_oid: result_text(decision, 5)?,
            })
        })
        .collect::<crab_cell_runtime::Result<Vec<_>>>()?;
    Ok(PullRecord {
        number: result_u64_from_row(row, 0)?,
        create_submission_id: submission,
        author: author_from_row(row, 2)?,
        title: result_text(row, 5)?,
        body: result_text(row, 6)?,
        state: PullState::from_code(result_u64_from_row(row, 7)?)?,
        base_ref: result_text(row, 8)?,
        base_oid: result_text(row, 9)?,
        head_ref: result_text(row, 10)?,
        head_oid: result_text(row, 11)?,
        label_ids: decode_label_ids(result_blob(row, 12)?)?,
        assignee_subjects: decode_assignees(result_blob(row, 13)?)?,
        merge_pending: decode_optional_wire(&row[14])?,
        merge: decode_optional_wire(&row[15])?,
        review_decisions,
        version: result_u64_from_row(row, 16)?,
        created_at_ms: result_u64_from_row(row, 17)?,
        updated_at_ms: result_u64_from_row(row, 18)?,
    })
}

fn author_from_row(row: &[SqlValue], start: usize) -> crab_cell_runtime::Result<RepositoryAuthor> {
    Ok(RepositoryAuthor {
        issuer: result_text(row, start)?,
        subject: result_text(row, start + 1)?,
        name: result_text(row, start + 2)?,
    })
}

pub(crate) struct CreatePullComment;

impl Command for CreatePullComment {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 18;
    const CODEC_VERSION: u32 = 1;
    type Input = CreatePullCommentInput;
    type Output = CreatePullCommentOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        validate_number(input.pull)?;
        validate_author(&input.author)?;
        validate_body(&input.body, true)?;
        if load_pull(context, input.pull)?.is_none() {
            return Ok(CommandResult::Rejected(
                CreatePullCommentOutcome::PullNotFound,
            ));
        }
        let digest = pull_child_digest(
            b"crab.repository.pull-comment-submission.v1\0",
            input.pull,
            &input.author,
            &input.body,
            None,
            None,
        );
        let existing = context.sql(&SqlBatch { statements: vec![statement("SELECT payload_digest, comment_number FROM repository_pull_comment_submissions WHERE pull_number = ? AND request_id = ?", vec![integer(input.pull)?, SqlValue::Blob(input.submission_id.to_vec())])] })?;
        if let Some(row) = existing[0].rows.first() {
            if result_blob(row, 0)? != digest.as_bytes() {
                return Ok(CommandResult::Rejected(
                    CreatePullCommentOutcome::RequestConflict,
                ));
            }
            let record = load_comment(
                context,
                PullChildKey {
                    pull: input.pull,
                    number: result_u64_from_row(row, 1)?,
                },
            )?
            .ok_or(crab_cell_runtime::Error::Command(
                "repository pull comment submission has no comment",
            ))?;
            return Ok(CommandResult::Success(CreatePullCommentOutcome::Created(
                record,
            )));
        }
        let number = next_child_number(context, "repository_pull_comment_sequences", input.pull)?;
        let now = timestamp(context.now_ms())?;
        let record = PullCommentRecord {
            pull: input.pull,
            number,
            author: input.author,
            body: input.body,
            version: 1,
            created_at_ms: now,
            updated_at_ms: now,
        };
        context.sql(&SqlBatch { statements: vec![
            statement("INSERT INTO repository_pull_comment_submissions(pull_number, request_id, payload_digest, comment_number) VALUES (?, ?, ?, ?)", vec![integer(record.pull)?, SqlValue::Blob(input.submission_id.to_vec()), SqlValue::Blob(digest.as_bytes().to_vec()), integer(record.number)?]),
            statement("INSERT INTO repository_pull_comments(pull_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)", vec![integer(record.pull)?, integer(record.number)?, SqlValue::Text(record.author.issuer.clone()), SqlValue::Text(record.author.subject.clone()), SqlValue::Text(record.author.name.clone()), SqlValue::Text(record.body.clone()), integer(record.version)?, integer(record.created_at_ms)?, integer(record.updated_at_ms)?]),
        ] })?;
        advance_revision(context)?;
        Ok(CommandResult::Success(CreatePullCommentOutcome::Created(
            record,
        )))
    }
}

pub(crate) struct UpdatePullComment;

impl Command for UpdatePullComment {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 19;
    const CODEC_VERSION: u32 = 1;
    type Input = UpdatePullCommentInput;
    type Output = UpdatePullCommentOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        validate_number(input.key.pull)?;
        validate_number(input.key.number)?;
        validate_number(input.version)?;
        validate_author(&input.actor)?;
        validate_body(&input.body, true)?;
        let Some(mut record) = load_comment(context, input.key)? else {
            return Ok(CommandResult::Rejected(UpdatePullCommentOutcome::NotFound));
        };
        if !same_author(&record.author, &input.actor) {
            return Ok(CommandResult::Rejected(UpdatePullCommentOutcome::Forbidden));
        }
        if record.version != input.version {
            return Ok(CommandResult::Rejected(UpdatePullCommentOutcome::Conflict));
        }
        record.body = input.body;
        record.version = next_version(record.version)?;
        record.updated_at_ms = timestamp(context.now_ms())?;
        context.sql(&SqlBatch { statements: vec![statement("UPDATE repository_pull_comments SET body = ?, version = ?, updated_at_ms = ? WHERE pull_number = ? AND number = ? AND version = ?", vec![SqlValue::Text(record.body.clone()), integer(record.version)?, integer(record.updated_at_ms)?, integer(record.pull)?, integer(record.number)?, integer(input.version)?])] })?;
        advance_revision(context)?;
        Ok(CommandResult::Success(UpdatePullCommentOutcome::Updated(
            record,
        )))
    }
}

pub(crate) struct CreatePullReview;

impl Command for CreatePullReview {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 20;
    const CODEC_VERSION: u32 = 1;
    type Input = CreatePullReviewInput;
    type Output = CreatePullReviewOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        validate_number(input.pull)?;
        validate_author(&input.author)?;
        validate_body(&input.body, input.state != ReviewState::Approved)?;
        validate_oid(&input.commit_oid)?;
        let digest = pull_child_digest(
            b"crab.repository.pull-review-submission.v1\0",
            input.pull,
            &input.author,
            &input.body,
            Some(input.state.code()),
            Some(&input.commit_oid),
        );
        let existing = context.sql(&SqlBatch { statements: vec![statement("SELECT payload_digest, review_number FROM repository_pull_review_submissions WHERE pull_number = ? AND request_id = ?", vec![integer(input.pull)?, SqlValue::Blob(input.submission_id.to_vec())])] })?;
        if let Some(row) = existing[0].rows.first() {
            if result_blob(row, 0)? != digest.as_bytes() {
                return Ok(CommandResult::Rejected(
                    CreatePullReviewOutcome::RequestConflict,
                ));
            }
            let record = load_review(
                context,
                PullChildKey {
                    pull: input.pull,
                    number: result_u64_from_row(row, 1)?,
                },
            )?
            .ok_or(crab_cell_runtime::Error::Command(
                "repository pull review submission has no review",
            ))?;
            return Ok(CommandResult::Success(CreatePullReviewOutcome::Created(
                record,
            )));
        }
        let Some(mut pull) = load_pull(context, input.pull)? else {
            return Ok(CommandResult::Rejected(
                CreatePullReviewOutcome::PullNotFound,
            ));
        };
        if pull.state != PullState::Open {
            return Ok(CommandResult::Rejected(CreatePullReviewOutcome::PullClosed));
        }
        if pull.merge_pending.is_some() {
            return Ok(CommandResult::Rejected(
                CreatePullReviewOutcome::MergePending,
            ));
        }
        if input.state != ReviewState::Commented && same_author(&pull.author, &input.author) {
            return Ok(CommandResult::Rejected(CreatePullReviewOutcome::OwnReview));
        }
        if input.state != ReviewState::Commented
            && !pull
                .review_decisions
                .iter()
                .any(|decision| same_author(&decision.author, &input.author))
            && pull.review_decisions.len() as u64 >= MAX_REVIEW_DECISIONS
        {
            return Ok(CommandResult::Rejected(
                CreatePullReviewOutcome::DecisionLimit,
            ));
        }
        let number = next_child_number(context, "repository_pull_review_sequences", input.pull)?;
        let now = timestamp(context.now_ms())?;
        let record = PullReviewRecord {
            pull: input.pull,
            number,
            author: input.author,
            body: input.body,
            state: input.state,
            commit_oid: input.commit_oid,
            version: 1,
            created_at_ms: now,
            updated_at_ms: now,
        };
        let mut statements = vec![
            statement(
                "INSERT INTO repository_pull_review_submissions(pull_number, request_id, payload_digest, review_number) VALUES (?, ?, ?, ?)",
                vec![
                    integer(record.pull)?,
                    SqlValue::Blob(input.submission_id.to_vec()),
                    SqlValue::Blob(digest.as_bytes().to_vec()),
                    integer(record.number)?,
                ],
            ),
            statement(
                "INSERT INTO repository_pull_reviews(pull_number, number, author_issuer, author_subject, author_name, body, state, commit_oid, version, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                vec![
                    integer(record.pull)?,
                    integer(record.number)?,
                    SqlValue::Text(record.author.issuer.clone()),
                    SqlValue::Text(record.author.subject.clone()),
                    SqlValue::Text(record.author.name.clone()),
                    SqlValue::Text(record.body.clone()),
                    SqlValue::Integer(i64::from(record.state.code())),
                    SqlValue::Text(record.commit_oid.clone()),
                    integer(record.version)?,
                    integer(record.created_at_ms)?,
                    integer(record.updated_at_ms)?,
                ],
            ),
        ];
        if record.state != ReviewState::Commented {
            statements.push(statement("INSERT INTO repository_pull_review_decisions(pull_number, author_issuer, author_subject, review_number, author_name, state, commit_oid) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(pull_number, author_issuer, author_subject) DO UPDATE SET review_number = excluded.review_number, author_name = excluded.author_name, state = excluded.state, commit_oid = excluded.commit_oid", vec![integer(record.pull)?, SqlValue::Text(record.author.issuer.clone()), SqlValue::Text(record.author.subject.clone()), integer(record.number)?, SqlValue::Text(record.author.name.clone()), SqlValue::Integer(i64::from(record.state.code())), SqlValue::Text(record.commit_oid.clone())]));
            pull.version = next_version(pull.version)?;
            pull.updated_at_ms = now;
            statements.push(statement("UPDATE repository_pulls SET version = ?, updated_at_ms = ? WHERE number = ? AND version = ?", vec![integer(pull.version)?, integer(now)?, integer(pull.number)?, integer(pull.version - 1)?]));
        }
        context.sql(&SqlBatch { statements })?;
        advance_revision(context)?;
        Ok(CommandResult::Success(CreatePullReviewOutcome::Created(
            record,
        )))
    }
}

pub(crate) struct UpdatePullReview;

impl Command for UpdatePullReview {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 21;
    const CODEC_VERSION: u32 = 1;
    type Input = UpdatePullReviewInput;
    type Output = UpdatePullReviewOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        validate_number(input.key.pull)?;
        validate_number(input.key.number)?;
        validate_number(input.version)?;
        validate_author(&input.actor)?;
        let Some(mut record) = load_review(context, input.key)? else {
            return Ok(CommandResult::Rejected(UpdatePullReviewOutcome::NotFound));
        };
        validate_body(&input.body, record.state != ReviewState::Approved)?;
        if !same_author(&record.author, &input.actor) {
            return Ok(CommandResult::Rejected(UpdatePullReviewOutcome::Forbidden));
        }
        if record.version != input.version {
            return Ok(CommandResult::Rejected(UpdatePullReviewOutcome::Conflict));
        }
        record.body = input.body;
        record.version = next_version(record.version)?;
        record.updated_at_ms = timestamp(context.now_ms())?;
        context.sql(&SqlBatch { statements: vec![statement("UPDATE repository_pull_reviews SET body = ?, version = ?, updated_at_ms = ? WHERE pull_number = ? AND number = ? AND version = ?", vec![SqlValue::Text(record.body.clone()), integer(record.version)?, integer(record.updated_at_ms)?, integer(record.pull)?, integer(record.number)?, integer(input.version)?])] })?;
        advance_revision(context)?;
        Ok(CommandResult::Success(UpdatePullReviewOutcome::Updated(
            record,
        )))
    }
}

pub(crate) struct ReservePullMerge;

impl Command for ReservePullMerge {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 22;
    const CODEC_VERSION: u32 = 1;
    type Input = ReservePullMergeInput;
    type Output = ReservePullMergeOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        validate_number(input.pull)?;
        validate_merge(&input.merge)?;
        if load_pull(context, input.pull)?.is_none() {
            return Ok(CommandResult::Rejected(
                ReservePullMergeOutcome::PullNotFound,
            ));
        }
        let digest = merge_digest(input.pull, &input.merge);
        let existing = context.sql(&SqlBatch { statements: vec![statement("SELECT payload_digest, merge_record FROM repository_pull_merge_submissions WHERE pull_number = ? AND request_id = ?", vec![integer(input.pull)?, SqlValue::Blob(input.merge.request_id.to_vec())])] })?;
        if let Some(row) = existing[0].rows.first() {
            if result_blob(row, 0)? != digest.as_bytes() {
                return Ok(CommandResult::Rejected(
                    ReservePullMergeOutcome::RequestConflict,
                ));
            }
            return Ok(CommandResult::Success(ReservePullMergeOutcome::Reserved(
                Box::new(decode_wire(result_blob(row, 1)?)?),
            )));
        }
        context.sql(&SqlBatch { statements: vec![statement("INSERT INTO repository_pull_merge_submissions(pull_number, request_id, payload_digest, merge_record) VALUES (?, ?, ?, ?)", vec![integer(input.pull)?, SqlValue::Blob(input.merge.request_id.to_vec()), SqlValue::Blob(digest.as_bytes().to_vec()), SqlValue::Blob(encode_wire(&input.merge)?)] )] })?;
        advance_revision(context)?;
        Ok(CommandResult::Success(ReservePullMergeOutcome::Reserved(
            Box::new(input.merge),
        )))
    }
}

pub(crate) struct TransitionPullMerge;

impl Command for TransitionPullMerge {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 23;
    const CODEC_VERSION: u32 = 1;
    type Input = TransitionPullMergeInput;
    type Output = TransitionPullMergeOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        validate_number(input.pull)?;
        validate_merge(&input.merge)?;
        let Some(mut pull) = load_pull(context, input.pull)? else {
            return Ok(CommandResult::Rejected(
                TransitionPullMergeOutcome::NotFound,
            ));
        };
        let old_version = pull.version;
        match input.transition {
            PullMergeTransition::Begin => {
                if let Some(done) = &pull.merge {
                    return if done.request_id == input.merge.request_id {
                        Ok(CommandResult::Success(TransitionPullMergeOutcome::Applied(
                            Box::new(pull),
                        )))
                    } else {
                        Ok(CommandResult::Rejected(
                            TransitionPullMergeOutcome::Conflict,
                        ))
                    };
                }
                if let Some(pending) = &pull.merge_pending {
                    return if pending.request_id == input.merge.request_id {
                        Ok(CommandResult::Success(TransitionPullMergeOutcome::Applied(
                            Box::new(pull),
                        )))
                    } else {
                        Ok(CommandResult::Rejected(
                            TransitionPullMergeOutcome::Conflict,
                        ))
                    };
                }
                if pull.state != PullState::Open || pull.version != input.merge.pull_version {
                    return Ok(CommandResult::Rejected(
                        TransitionPullMergeOutcome::Conflict,
                    ));
                }
                pull.merge_pending = Some(input.merge);
            }
            PullMergeTransition::Abort => {
                if pull.merge.is_some()
                    || pull
                        .merge_pending
                        .as_ref()
                        .is_none_or(|pending| pending.request_id != input.merge.request_id)
                {
                    return Ok(CommandResult::Success(TransitionPullMergeOutcome::Applied(
                        Box::new(pull),
                    )));
                }
                pull.merge_pending = None;
            }
            PullMergeTransition::Complete => {
                if let Some(done) = &pull.merge {
                    return if done.request_id == input.merge.request_id {
                        Ok(CommandResult::Success(TransitionPullMergeOutcome::Applied(
                            Box::new(pull),
                        )))
                    } else {
                        Ok(CommandResult::Rejected(
                            TransitionPullMergeOutcome::Conflict,
                        ))
                    };
                }
                if pull
                    .merge_pending
                    .as_ref()
                    .is_none_or(|pending| pending.request_id != input.merge.request_id)
                {
                    return Ok(CommandResult::Rejected(
                        TransitionPullMergeOutcome::Conflict,
                    ));
                }
                pull.state = PullState::Merged;
                pull.merge_pending = None;
                pull.merge = Some(input.merge);
            }
        }
        pull.version = next_version(pull.version)?;
        pull.updated_at_ms = timestamp(context.now_ms())?;
        update_pull_row(context, &pull, old_version)?;
        advance_revision(context)?;
        Ok(CommandResult::Success(TransitionPullMergeOutcome::Applied(
            Box::new(pull),
        )))
    }
}

pub(crate) struct GetPullComment;
impl Query for GetPullComment {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 18;
    const CODEC_VERSION: u32 = 1;
    type Input = PullChildKey;
    type Output = Option<PullCommentRecord>;
    fn execute(
        context: &mut QueryContext<'_>,
        key: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        load_comment_query(context, key)
    }
}
pub(crate) struct ListPullComments;
impl Query for ListPullComments {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 19;
    const CODEC_VERSION: u32 = 1;
    type Input = PullChildListInput;
    type Output = Option<PullCommentPage>;
    fn execute(
        context: &mut QueryContext<'_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        if load_pull_query(context, input.pull)?.is_none() {
            return Ok(None);
        }
        let rows = list_child_rows(
            context,
            "repository_pull_comments",
            &input,
            "author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms",
        )?;
        let items = rows
            .iter()
            .map(|row| comment_from_row(row))
            .collect::<crab_cell_runtime::Result<Vec<_>>>()?;
        let (items, next) = bounded_child_page(items, input.limit, |item| item.number)?;
        Ok(Some(PullCommentPage { items, next }))
    }
}
pub(crate) struct GetPullReview;
impl Query for GetPullReview {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 20;
    const CODEC_VERSION: u32 = 1;
    type Input = PullChildKey;
    type Output = Option<PullReviewRecord>;
    fn execute(
        context: &mut QueryContext<'_>,
        key: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        load_review_query(context, key)
    }
}
pub(crate) struct ListPullReviews;
impl Query for ListPullReviews {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 21;
    const CODEC_VERSION: u32 = 1;
    type Input = PullChildListInput;
    type Output = Option<PullReviewPage>;
    fn execute(
        context: &mut QueryContext<'_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        if load_pull_query(context, input.pull)?.is_none() {
            return Ok(None);
        }
        let rows = list_child_rows(
            context,
            "repository_pull_reviews",
            &input,
            "author_issuer, author_subject, author_name, body, state, commit_oid, version, created_at_ms, updated_at_ms",
        )?;
        let items = rows
            .iter()
            .map(|row| review_from_row(row))
            .collect::<crab_cell_runtime::Result<Vec<_>>>()?;
        let (items, next) = bounded_child_page(items, input.limit, |item| item.number)?;
        Ok(Some(PullReviewPage { items, next }))
    }
}
pub(crate) struct GetPullReviewSubmission;
impl Query for GetPullReviewSubmission {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 22;
    const CODEC_VERSION: u32 = 1;
    type Input = PullSubmissionKey;
    type Output = Option<PullReviewRecord>;
    fn execute(
        context: &mut QueryContext<'_>,
        key: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        let pull = key.pull.ok_or(crab_cell_runtime::Error::Command(
            "repository review submission scope is invalid",
        ))?;
        let result = context.sql(&SqlBatch { statements: vec![statement("SELECT review_number FROM repository_pull_review_submissions WHERE pull_number = ? AND request_id = ?", vec![integer(pull)?, SqlValue::Blob(key.submission_id.to_vec())])] })?;
        result[0]
            .rows
            .first()
            .map(|row| result_u64_from_row(row, 0))
            .transpose()?
            .map(|number| load_review_query(context, PullChildKey { pull, number }))
            .transpose()
            .map(Option::flatten)
    }
}
pub(crate) struct GetPullMergeSubmission;
impl Query for GetPullMergeSubmission {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 23;
    const CODEC_VERSION: u32 = 1;
    type Input = PullSubmissionKey;
    type Output = Option<PullMerge>;
    fn execute(
        context: &mut QueryContext<'_>,
        key: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        let pull = key.pull.ok_or(crab_cell_runtime::Error::Command(
            "repository merge submission scope is invalid",
        ))?;
        let result = context.sql(&SqlBatch { statements: vec![statement("SELECT merge_record FROM repository_pull_merge_submissions WHERE pull_number = ? AND request_id = ?", vec![integer(pull)?, SqlValue::Blob(key.submission_id.to_vec())])] })?;
        result[0]
            .rows
            .first()
            .map(|row| decode_wire(result_blob(row, 0)?))
            .transpose()
    }
}

fn next_child_number(
    context: &CommandContext<'_, '_>,
    table: &str,
    pull: u64,
) -> crab_cell_runtime::Result<u64> {
    let sql = format!(
        "INSERT INTO {table}(pull_number, last) VALUES (?, 1) ON CONFLICT(pull_number) DO UPDATE SET last = last + 1 WHERE last < 9007199254740991"
    );
    let select = format!("SELECT last FROM {table} WHERE pull_number = ?");
    let result = context.sql(&SqlBatch {
        statements: vec![
            statement(&sql, vec![integer(pull)?]),
            statement(&select, vec![integer(pull)?]),
        ],
    })?;
    if result[0].rows_affected != 1 {
        return Err(crab_cell_runtime::Error::Command(
            "repository pull child numbering is exhausted",
        ));
    }
    result_u64(&result, 1, 0)
}

fn pull_child_digest(
    domain: &[u8],
    pull: u64,
    author: &RepositoryAuthor,
    body: &str,
    state: Option<u8>,
    oid: Option<&str>,
) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hasher.update(&pull.to_be_bytes());
    hash_text(&mut hasher, &author.issuer);
    hash_text(&mut hasher, &author.subject);
    hash_text(&mut hasher, body);
    if let Some(state) = state {
        hasher.update(&[state]);
    }
    if let Some(oid) = oid {
        hash_text(&mut hasher, oid);
    }
    hasher.finalize()
}
fn merge_digest(pull: u64, merge: &PullMerge) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.repository.pull-merge-submission.v1\0");
    hasher.update(&pull.to_be_bytes());
    hash_text(&mut hasher, &merge.author.issuer);
    hash_text(&mut hasher, &merge.author.subject);
    hasher.update(&[match merge.method {
        MergeMethod::FastForward => 0,
        MergeMethod::MergeCommit => 1,
    }]);
    hasher.update(&merge.pull_version.to_be_bytes());
    for value in [
        &merge.base_oid,
        &merge.head_oid,
        &merge.commit_oid,
        &merge.message,
    ] {
        hash_text(&mut hasher, value);
    }
    hasher.finalize()
}
fn next_version(value: u64) -> crab_cell_runtime::Result<u64> {
    value
        .checked_add(1)
        .filter(|value| *value < MAX_NUMBER)
        .ok_or(crab_cell_runtime::Error::Command(
            "repository pull version is exhausted",
        ))
}
fn validate_merge(merge: &PullMerge) -> crab_cell_runtime::Result<()> {
    validate_author(&merge.author)?;
    validate_number(merge.pull_version)?;
    validate_oid(&merge.base_oid)?;
    validate_oid(&merge.head_oid)?;
    validate_oid(&merge.commit_oid)?;
    if merge.message.chars().count() > 256
        || merge
            .message
            .chars()
            .any(|character| character.is_control() && character != '\n')
    {
        return Err(crab_cell_runtime::Error::Command(
            "repository pull merge message is invalid",
        ));
    }
    Ok(())
}
fn encode_wire<T: WireValue>(value: &T) -> crab_cell_runtime::Result<Vec<u8>> {
    let mut encoder = BoundedEncoder::new(128 * 1024)
        .map_err(|_| crab_cell_runtime::Error::Command("repository pull value is too large"))?;
    value
        .encode(&mut encoder)
        .map_err(|_| crab_cell_runtime::Error::Command("repository pull value is too large"))?;
    Ok(encoder.finish())
}
fn decode_wire<T: WireValue>(bytes: &[u8]) -> crab_cell_runtime::Result<T> {
    let mut decoder = BoundedDecoder::new(bytes, 128 * 1024)
        .map_err(|_| crab_cell_runtime::Error::Command("repository pull value is invalid"))?;
    let value = T::decode(&mut decoder)
        .map_err(|_| crab_cell_runtime::Error::Command("repository pull value is invalid"))?;
    decoder
        .finish()
        .map_err(|_| crab_cell_runtime::Error::Command("repository pull value is invalid"))?;
    Ok(value)
}

fn load_comment(
    context: &CommandContext<'_, '_>,
    key: PullChildKey,
) -> crab_cell_runtime::Result<Option<PullCommentRecord>> {
    let result = context.sql(&SqlBatch { statements: vec![statement("SELECT pull_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms FROM repository_pull_comments WHERE pull_number = ? AND number = ?", vec![integer(key.pull)?, integer(key.number)?])] })?;
    result[0]
        .rows
        .first()
        .map(|row| comment_from_row(row))
        .transpose()
}
fn load_comment_query(
    context: &QueryContext<'_>,
    key: PullChildKey,
) -> crab_cell_runtime::Result<Option<PullCommentRecord>> {
    validate_number(key.pull)?;
    validate_number(key.number)?;
    let result = context.sql(&SqlBatch { statements: vec![statement("SELECT pull_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms FROM repository_pull_comments WHERE pull_number = ? AND number = ?", vec![integer(key.pull)?, integer(key.number)?])] })?;
    result[0]
        .rows
        .first()
        .map(|row| comment_from_row(row))
        .transpose()
}
fn comment_from_row(row: &[SqlValue]) -> crab_cell_runtime::Result<PullCommentRecord> {
    Ok(PullCommentRecord {
        pull: result_u64_from_row(row, 0)?,
        number: result_u64_from_row(row, 1)?,
        author: author_from_row(row, 2)?,
        body: result_text(row, 5)?,
        version: result_u64_from_row(row, 6)?,
        created_at_ms: result_u64_from_row(row, 7)?,
        updated_at_ms: result_u64_from_row(row, 8)?,
    })
}
fn load_review(
    context: &CommandContext<'_, '_>,
    key: PullChildKey,
) -> crab_cell_runtime::Result<Option<PullReviewRecord>> {
    let result = context.sql(&SqlBatch { statements: vec![statement("SELECT pull_number, number, author_issuer, author_subject, author_name, body, state, commit_oid, version, created_at_ms, updated_at_ms FROM repository_pull_reviews WHERE pull_number = ? AND number = ?", vec![integer(key.pull)?, integer(key.number)?])] })?;
    result[0]
        .rows
        .first()
        .map(|row| review_from_row(row))
        .transpose()
}
fn load_review_query(
    context: &QueryContext<'_>,
    key: PullChildKey,
) -> crab_cell_runtime::Result<Option<PullReviewRecord>> {
    validate_number(key.pull)?;
    validate_number(key.number)?;
    let result = context.sql(&SqlBatch { statements: vec![statement("SELECT pull_number, number, author_issuer, author_subject, author_name, body, state, commit_oid, version, created_at_ms, updated_at_ms FROM repository_pull_reviews WHERE pull_number = ? AND number = ?", vec![integer(key.pull)?, integer(key.number)?])] })?;
    result[0]
        .rows
        .first()
        .map(|row| review_from_row(row))
        .transpose()
}
fn review_from_row(row: &[SqlValue]) -> crab_cell_runtime::Result<PullReviewRecord> {
    Ok(PullReviewRecord {
        pull: result_u64_from_row(row, 0)?,
        number: result_u64_from_row(row, 1)?,
        author: author_from_row(row, 2)?,
        body: result_text(row, 5)?,
        state: ReviewState::from_code(result_u64_from_row(row, 6)?)?,
        commit_oid: result_text(row, 7)?,
        version: result_u64_from_row(row, 8)?,
        created_at_ms: result_u64_from_row(row, 9)?,
        updated_at_ms: result_u64_from_row(row, 10)?,
    })
}
fn list_child_rows(
    context: &QueryContext<'_>,
    table: &str,
    input: &PullChildListInput,
    columns: &str,
) -> crab_cell_runtime::Result<Vec<Vec<SqlValue>>> {
    validate_number(input.pull)?;
    validate_list(input.before, input.limit)?;
    let upper = input.before.map_or(MAX_NUMBER, |value| value - 1);
    let sql = format!(
        "SELECT pull_number, number, {columns} FROM {table} WHERE pull_number = ? AND number <= ? ORDER BY number DESC LIMIT ?"
    );
    let result = context.sql(&SqlBatch {
        statements: vec![statement(
            &sql,
            vec![
                integer(input.pull)?,
                integer(upper)?,
                SqlValue::Integer(i64::from(input.limit)),
            ],
        )],
    })?;
    Ok(result[0].rows.clone())
}
fn bounded_child_page<T: Serialize>(
    items: Vec<T>,
    requested: u8,
    number: impl Fn(&T) -> u64,
) -> crab_cell_runtime::Result<(Vec<T>, Option<u64>)> {
    let total = items.len();
    let mut used =
        4 + br#"{"items":["#.len() + br#"],"next":"#.len() + MAX_NUMBER.to_string().len() + 1;
    let mut kept = Vec::with_capacity(total);
    for item in items {
        let encoded = serde_json::to_vec(&item)
            .map_err(|_| crab_cell_runtime::Error::Command("repository pull child is invalid"))?;
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
        return Err(crab_cell_runtime::Error::Command(
            "repository pull child exceeds the page limit",
        ));
    }
    let next = (kept.len() < total || total == usize::from(requested))
        .then(|| kept.last().map(&number))
        .flatten();
    Ok((kept, next))
}
