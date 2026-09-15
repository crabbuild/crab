use crab_cell_runtime::{
    BoundedDecoder, BoundedEncoder, CellModule, Command, CommandContext, CommandResult, Query,
    QueryContext, RegistryBuilder, SqlBatch, SqlResultSet, SqlStatement, SqlValue, WireValue,
};

use super::RepositoryModule;

pub(crate) use operations::{ListComments, ListIssues, UpdateComment, UpdateIssue};

const MAX_NUMBER: u64 = 9_007_199_254_740_991;
const MAX_LIST_ITEMS: usize = 50;
const MAX_LIST_SCAN: u64 = 200;
const MAX_LABELS: usize = 20;
const MAX_ASSIGNEES: usize = 10;
const MAX_LIST_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepositoryAuthor {
    pub issuer: String,
    pub subject: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CreateIssueInput {
    pub author: RepositoryAuthor,
    pub title: String,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IssueRecord {
    pub number: u64,
    pub author: RepositoryAuthor,
    pub title: String,
    pub body: String,
    pub state: u8,
    pub label_ids: Vec<u64>,
    pub assignee_subjects: Vec<String>,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CreateCommentInput {
    pub issue: u64,
    pub author: RepositoryAuthor,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommentRecord {
    pub issue: u64,
    pub number: u64,
    pub author: RepositoryAuthor,
    pub body: String,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CreateCommentOutcome {
    Created(CommentRecord),
    IssueNotFound,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommentKey {
    pub issue: u64,
    pub number: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UpdateIssueInput {
    pub number: u64,
    pub actor: RepositoryAuthor,
    pub can_manage_metadata: bool,
    pub version: u64,
    pub title: Option<String>,
    pub body: Option<String>,
    pub state: Option<u8>,
    pub label_ids: Option<Vec<u64>>,
    pub assignee_subjects: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UpdateIssueOutcome {
    Updated(Box<IssueRecord>),
    NotFound,
    Forbidden,
    LabelForbidden,
    AssigneeForbidden,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UpdateCommentInput {
    pub key: CommentKey,
    pub actor: RepositoryAuthor,
    pub version: u64,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UpdateCommentOutcome {
    Updated(CommentRecord),
    NotFound,
    Forbidden,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ListIssuesInput {
    pub before: Option<u64>,
    pub limit: u8,
    pub state: u8,
    pub query: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IssueSummary {
    pub number: u64,
    pub author: RepositoryAuthor,
    pub title: String,
    pub state: u8,
    pub label_ids: Vec<u64>,
    pub assignee_subjects: Vec<String>,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IssuePage {
    pub items: Vec<IssueSummary>,
    pub next: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ListCommentsInput {
    pub issue: u64,
    pub before: Option<u64>,
    pub limit: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CommentPage {
    Found {
        items: Vec<CommentRecord>,
        next: Option<u64>,
    },
    IssueNotFound,
}

pub(crate) struct CreateIssue;

impl Command for CreateIssue {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = CreateIssueInput;
    type Output = IssueRecord;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        validate_author(&input.author)?;
        validate_title(&input.title)?;
        validate_body(&input.body, false)?;
        let now = timestamp(context.now_ms())?;
        let sequence = context.sql(&SqlBatch {
            statements: vec![
                statement(
                    "UPDATE repository_sequences SET last = last + 1 WHERE kind = 'issue' AND last < 9007199254740991",
                    vec![],
                ),
                statement(
                    "SELECT last FROM repository_sequences WHERE kind = 'issue'",
                    vec![],
                ),
            ],
        })?;
        if sequence[0].rows_affected != 1 {
            return Err(crab_cell_runtime::Error::Command(
                "repository issue numbering is exhausted",
            ));
        }
        let number = result_u64(&sequence, 1, 0)?;
        let record = IssueRecord {
            number,
            author: input.author,
            title: input.title,
            body: input.body,
            state: 0,
            label_ids: vec![],
            assignee_subjects: vec![],
            version: 1,
            created_at_ms: now,
            updated_at_ms: now,
        };
        context.sql(&SqlBatch {
            statements: vec![statement(
                "INSERT INTO repository_issues(number, author_issuer, author_subject, author_name, title, body, state, label_ids, assignee_subjects, version, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                vec![
                    integer(record.number)?,
                    SqlValue::Text(record.author.issuer.clone()),
                    SqlValue::Text(record.author.subject.clone()),
                    SqlValue::Text(record.author.name.clone()),
                    SqlValue::Text(record.title.clone()),
                    SqlValue::Text(record.body.clone()),
                    SqlValue::Integer(i64::from(record.state)),
                    SqlValue::Blob(encode_label_ids(&record.label_ids)?),
                    SqlValue::Blob(encode_assignees(&record.assignee_subjects)?),
                    integer(record.version)?,
                    integer(record.created_at_ms)?,
                    integer(record.updated_at_ms)?,
                ],
            )],
        })?;
        advance_revision(context)?;
        Ok(CommandResult::Success(record))
    }
}

pub(crate) struct CreateComment;

impl Command for CreateComment {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 2;
    const CODEC_VERSION: u32 = 1;
    type Input = CreateCommentInput;
    type Output = CreateCommentOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        validate_number(input.issue)?;
        validate_author(&input.author)?;
        validate_body(&input.body, true)?;
        let exists = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT 1 FROM repository_issues WHERE number = ?",
                vec![integer(input.issue)?],
            )],
        })?;
        if exists[0].rows.is_empty() {
            return Ok(CommandResult::Rejected(CreateCommentOutcome::IssueNotFound));
        }
        let now = timestamp(context.now_ms())?;
        let sequence = context.sql(&SqlBatch {
            statements: vec![
                statement(
                    "INSERT INTO repository_comment_sequences(issue_number, last) VALUES (?, 1) ON CONFLICT(issue_number) DO UPDATE SET last = last + 1 WHERE last < 9007199254740991",
                    vec![integer(input.issue)?],
                ),
                statement(
                    "SELECT last FROM repository_comment_sequences WHERE issue_number = ?",
                    vec![integer(input.issue)?],
                ),
            ],
        })?;
        if sequence[0].rows_affected != 1 {
            return Err(crab_cell_runtime::Error::Command(
                "repository comment numbering is exhausted",
            ));
        }
        let number = result_u64(&sequence, 1, 0)?;
        let record = CommentRecord {
            issue: input.issue,
            number,
            author: input.author,
            body: input.body,
            version: 1,
            created_at_ms: now,
            updated_at_ms: now,
        };
        context.sql(&SqlBatch {
            statements: vec![statement(
                "INSERT INTO repository_issue_comments(issue_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                vec![
                    integer(record.issue)?,
                    integer(record.number)?,
                    SqlValue::Text(record.author.issuer.clone()),
                    SqlValue::Text(record.author.subject.clone()),
                    SqlValue::Text(record.author.name.clone()),
                    SqlValue::Text(record.body.clone()),
                    integer(record.version)?,
                    integer(record.created_at_ms)?,
                    integer(record.updated_at_ms)?,
                ],
            )],
        })?;
        advance_revision(context)?;
        Ok(CommandResult::Success(CreateCommentOutcome::Created(
            record,
        )))
    }
}

pub(crate) struct GetIssue;

impl Query for GetIssue {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = u64;
    type Output = Option<IssueRecord>;

    fn execute(
        context: &mut QueryContext<'_>,
        number: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        validate_number(number)?;
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT number, author_issuer, author_subject, author_name, title, body, state, label_ids, assignee_subjects, version, created_at_ms, updated_at_ms FROM repository_issues WHERE number = ?",
                vec![integer(number)?],
            )],
        })?;
        result[0]
            .rows
            .first()
            .map(|row| issue_from_row(row.as_slice()))
            .transpose()
    }
}

pub(crate) struct GetComment;

impl Query for GetComment {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 2;
    const CODEC_VERSION: u32 = 1;
    type Input = CommentKey;
    type Output = Option<CommentRecord>;

    fn execute(
        context: &mut QueryContext<'_>,
        key: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        validate_number(key.issue)?;
        validate_number(key.number)?;
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT issue_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms FROM repository_issue_comments WHERE issue_number = ? AND number = ?",
                vec![integer(key.issue)?, integer(key.number)?],
            )],
        })?;
        result[0]
            .rows
            .first()
            .map(|row| comment_from_row(row.as_slice()))
            .transpose()
    }
}

mod operations;

pub(crate) fn register(registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
    registry.bind_command::<CreateIssue>()?;
    registry.bind_command::<CreateComment>()?;
    registry.bind_command::<UpdateIssue>()?;
    registry.bind_command::<UpdateComment>()?;
    registry.bind_query::<GetIssue>()?;
    registry.bind_query::<GetComment>()?;
    registry.bind_query::<ListIssues>()?;
    registry.bind_query::<ListComments>()
}

fn statement(sql: &str, parameters: Vec<SqlValue>) -> SqlStatement {
    SqlStatement {
        sql: sql.to_owned(),
        parameters,
    }
}

fn integer(value: u64) -> crab_cell_runtime::Result<SqlValue> {
    Ok(SqlValue::Integer(i64::try_from(value).map_err(|_| {
        crab_cell_runtime::Error::Command("repository integer exceeds SQLite range")
    })?))
}

fn advance_revision(context: &CommandContext<'_, '_>) -> crab_cell_runtime::Result<()> {
    let result = context.sql(&SqlBatch {
        statements: vec![statement(
            "UPDATE repository_identity SET app_revision = app_revision + 1 WHERE singleton = 1 AND app_revision < 9007199254740991",
            vec![],
        )],
    })?;
    if result[0].rows_affected != 1 {
        return Err(crab_cell_runtime::Error::Command(
            "repository identity is missing or its revision is exhausted",
        ));
    }
    Ok(())
}

fn result_u64(sets: &[SqlResultSet], set: usize, column: usize) -> crab_cell_runtime::Result<u64> {
    match sets
        .get(set)
        .and_then(|set| set.rows.first())
        .and_then(|row| row.get(column))
    {
        Some(SqlValue::Integer(value)) => u64::try_from(*value)
            .map_err(|_| crab_cell_runtime::Error::Command("repository result is negative")),
        _ => Err(crab_cell_runtime::Error::Command(
            "repository query returned an invalid integer",
        )),
    }
}

fn result_text(row: &[SqlValue], column: usize) -> crab_cell_runtime::Result<String> {
    match row.get(column) {
        Some(SqlValue::Text(value)) => Ok(value.clone()),
        _ => Err(crab_cell_runtime::Error::Command(
            "repository query returned invalid text",
        )),
    }
}

fn result_blob(row: &[SqlValue], column: usize) -> crab_cell_runtime::Result<&[u8]> {
    match row.get(column) {
        Some(SqlValue::Blob(value)) => Ok(value),
        _ => Err(crab_cell_runtime::Error::Command(
            "repository query returned invalid bytes",
        )),
    }
}

fn issue_from_row(row: &[SqlValue]) -> crab_cell_runtime::Result<IssueRecord> {
    let state = u8::try_from(result_u64_from_row(row, 6)?)
        .map_err(|_| crab_cell_runtime::Error::Command("invalid issue state"))?;
    if state > 1 {
        return Err(crab_cell_runtime::Error::Command("invalid issue state"));
    }
    let record = IssueRecord {
        number: result_u64_from_row(row, 0)?,
        author: RepositoryAuthor {
            issuer: result_text(row, 1)?,
            subject: result_text(row, 2)?,
            name: result_text(row, 3)?,
        },
        title: result_text(row, 4)?,
        body: result_text(row, 5)?,
        state,
        label_ids: decode_label_ids(result_blob(row, 7)?)?,
        assignee_subjects: decode_assignees(result_blob(row, 8)?)?,
        version: result_u64_from_row(row, 9)?,
        created_at_ms: result_u64_from_row(row, 10)?,
        updated_at_ms: result_u64_from_row(row, 11)?,
    };
    validate_issue(&record)?;
    Ok(record)
}

fn comment_from_row(row: &[SqlValue]) -> crab_cell_runtime::Result<CommentRecord> {
    let record = CommentRecord {
        issue: result_u64_from_row(row, 0)?,
        number: result_u64_from_row(row, 1)?,
        author: RepositoryAuthor {
            issuer: result_text(row, 2)?,
            subject: result_text(row, 3)?,
            name: result_text(row, 4)?,
        },
        body: result_text(row, 5)?,
        version: result_u64_from_row(row, 6)?,
        created_at_ms: result_u64_from_row(row, 7)?,
        updated_at_ms: result_u64_from_row(row, 8)?,
    };
    validate_comment(&record)?;
    Ok(record)
}

fn result_u64_from_row(row: &[SqlValue], column: usize) -> crab_cell_runtime::Result<u64> {
    match row.get(column) {
        Some(SqlValue::Integer(value)) => u64::try_from(*value)
            .map_err(|_| crab_cell_runtime::Error::Command("repository result is negative")),
        _ => Err(crab_cell_runtime::Error::Command(
            "repository query returned an invalid integer",
        )),
    }
}

fn timestamp(now_ms: i64) -> crab_cell_runtime::Result<u64> {
    u64::try_from(now_ms)
        .map_err(|_| crab_cell_runtime::Error::Command("repository timestamp is negative"))
}

fn validate_number(number: u64) -> crab_cell_runtime::Result<()> {
    if number == 0 || number > MAX_NUMBER {
        return Err(crab_cell_runtime::Error::Command(
            "repository number is invalid",
        ));
    }
    Ok(())
}

fn validate_author(author: &RepositoryAuthor) -> crab_cell_runtime::Result<()> {
    for (value, maximum) in [
        (&author.issuer, 512),
        (&author.subject, 512),
        (&author.name, 160),
    ] {
        if value.is_empty()
            || value.chars().count() > maximum
            || value.chars().any(char::is_control)
        {
            return Err(crab_cell_runtime::Error::Command(
                "repository author identity is invalid",
            ));
        }
    }
    Ok(())
}

fn validate_title(title: &str) -> crab_cell_runtime::Result<()> {
    if title.trim() != title
        || title.is_empty()
        || title.chars().count() > 256
        || title.chars().any(char::is_control)
    {
        return Err(crab_cell_runtime::Error::Command(
            "repository issue title is invalid",
        ));
    }
    Ok(())
}

fn validate_body(body: &str, required: bool) -> crab_cell_runtime::Result<()> {
    if (required && body.trim().is_empty()) || body.len() > 64 * 1024 || body.contains('\0') {
        return Err(crab_cell_runtime::Error::Command(
            "repository discussion body is invalid",
        ));
    }
    Ok(())
}

fn validate_issue(record: &IssueRecord) -> crab_cell_runtime::Result<()> {
    validate_number(record.number)?;
    validate_author(&record.author)?;
    validate_title(&record.title)?;
    validate_body(&record.body, false)?;
    validate_label_ids(&record.label_ids)?;
    validate_assignees(&record.assignee_subjects)?;
    validate_number(record.version)?;
    if record.state > 1 || record.updated_at_ms < record.created_at_ms {
        return Err(crab_cell_runtime::Error::Command(
            "repository issue row is invalid",
        ));
    }
    Ok(())
}

fn validate_label_ids(labels: &[u64]) -> crab_cell_runtime::Result<()> {
    if labels.len() > MAX_LABELS
        || labels.contains(&0)
        || labels.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(crab_cell_runtime::Error::Command(
            "repository issue labels are invalid",
        ));
    }
    Ok(())
}

fn validate_assignees(assignees: &[String]) -> crab_cell_runtime::Result<()> {
    if assignees.len() > MAX_ASSIGNEES
        || assignees.iter().any(|subject| {
            subject.is_empty()
                || subject.chars().count() > 512
                || subject.chars().any(char::is_control)
        })
        || assignees.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(crab_cell_runtime::Error::Command(
            "repository issue assignees are invalid",
        ));
    }
    Ok(())
}

fn validate_list(before: Option<u64>, limit: u8) -> crab_cell_runtime::Result<()> {
    if limit == 0
        || usize::from(limit) > MAX_LIST_ITEMS
        || before.is_some_and(|value| value == 0 || value > MAX_NUMBER)
    {
        return Err(crab_cell_runtime::Error::Command(
            "repository list bounds are invalid",
        ));
    }
    Ok(())
}

fn validate_query(query: Option<&str>) -> crab_cell_runtime::Result<()> {
    if query.is_some_and(|query| {
        query.is_empty()
            || query.trim() != query
            || query.chars().count() > 256
            || query.chars().any(char::is_control)
            || query.to_lowercase() != query
    }) {
        return Err(crab_cell_runtime::Error::Command(
            "repository search query is invalid",
        ));
    }
    Ok(())
}

fn matches_query(query: Option<&str>, issue: &IssueRecord) -> bool {
    query.is_none_or(|query| {
        [&issue.title, &issue.body, &issue.author.name]
            .iter()
            .any(|value| value.to_lowercase().contains(query))
    })
}

fn same_author(left: &RepositoryAuthor, right: &RepositoryAuthor) -> bool {
    left.issuer == right.issuer && left.subject == right.subject
}

fn encode_label_ids(labels: &[u64]) -> crab_cell_runtime::Result<Vec<u8>> {
    let mut encoder = BoundedEncoder::new(16 * 1024)
        .map_err(|_| crab_cell_runtime::Error::Command("repository label encoding failed"))?;
    encoder
        .write_count(labels.len())
        .map_err(|_| crab_cell_runtime::Error::Command("repository label encoding failed"))?;
    for label in labels {
        encoder
            .write_u64(*label)
            .map_err(|_| crab_cell_runtime::Error::Command("repository label encoding failed"))?;
    }
    Ok(encoder.finish())
}

fn decode_label_ids(bytes: &[u8]) -> crab_cell_runtime::Result<Vec<u64>> {
    let mut decoder = BoundedDecoder::new(bytes, 16 * 1024)
        .map_err(|_| crab_cell_runtime::Error::Command("repository label encoding is invalid"))?;
    let count = decoder
        .read_count()
        .map_err(|_| crab_cell_runtime::Error::Command("repository label encoding is invalid"))?;
    if count > MAX_LABELS {
        return Err(crab_cell_runtime::Error::Command(
            "repository label encoding is invalid",
        ));
    }
    let mut labels = Vec::with_capacity(count);
    for _ in 0..count {
        labels.push(decoder.read_u64().map_err(|_| {
            crab_cell_runtime::Error::Command("repository label encoding is invalid")
        })?);
    }
    decoder
        .finish()
        .map_err(|_| crab_cell_runtime::Error::Command("repository label encoding is invalid"))?;
    validate_label_ids(&labels)?;
    Ok(labels)
}

fn encode_assignees(assignees: &[String]) -> crab_cell_runtime::Result<Vec<u8>> {
    let mut encoder = BoundedEncoder::new(16 * 1024)
        .map_err(|_| crab_cell_runtime::Error::Command("repository assignee encoding failed"))?;
    encoder
        .write_count(assignees.len())
        .map_err(|_| crab_cell_runtime::Error::Command("repository assignee encoding failed"))?;
    for assignee in assignees {
        encoder.write_text(assignee).map_err(|_| {
            crab_cell_runtime::Error::Command("repository assignee encoding failed")
        })?;
    }
    Ok(encoder.finish())
}

fn decode_assignees(bytes: &[u8]) -> crab_cell_runtime::Result<Vec<String>> {
    let mut decoder = BoundedDecoder::new(bytes, 16 * 1024).map_err(|_| {
        crab_cell_runtime::Error::Command("repository assignee encoding is invalid")
    })?;
    let count = decoder.read_count().map_err(|_| {
        crab_cell_runtime::Error::Command("repository assignee encoding is invalid")
    })?;
    if count > MAX_ASSIGNEES {
        return Err(crab_cell_runtime::Error::Command(
            "repository assignee encoding is invalid",
        ));
    }
    let mut assignees = Vec::with_capacity(count);
    for _ in 0..count {
        assignees.push(
            decoder
                .read_text()
                .map_err(|_| {
                    crab_cell_runtime::Error::Command("repository assignee encoding is invalid")
                })?
                .to_owned(),
        );
    }
    decoder.finish().map_err(|_| {
        crab_cell_runtime::Error::Command("repository assignee encoding is invalid")
    })?;
    validate_assignees(&assignees)?;
    Ok(assignees)
}

fn encoded_size<T: WireValue>(value: &T) -> crab_cell_runtime::Result<usize> {
    let mut encoder = BoundedEncoder::new(MAX_LIST_OUTPUT_BYTES as u32)
        .map_err(|_| crab_cell_runtime::Error::Command("repository result encoding failed"))?;
    value
        .encode(&mut encoder)
        .map_err(|_| crab_cell_runtime::Error::Command("repository result encoding failed"))?;
    Ok(encoder.finish().len())
}

fn validate_comment(record: &CommentRecord) -> crab_cell_runtime::Result<()> {
    validate_number(record.issue)?;
    validate_number(record.number)?;
    validate_author(&record.author)?;
    validate_body(&record.body, true)?;
    validate_number(record.version)?;
    if record.updated_at_ms < record.created_at_ms {
        return Err(crab_cell_runtime::Error::Command(
            "repository comment row is invalid",
        ));
    }
    Ok(())
}

mod codec;

#[cfg(test)]
mod tests;
