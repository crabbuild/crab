use crab_cell_runtime::{
    BoundedDecoder, BoundedEncoder, CellModule, CodecError, Command, CommandContext, CommandResult,
    Query, QueryContext, RegistryBuilder, SqlBatch, SqlResultSet, SqlStatement, SqlValue,
    WireValue,
};

use super::RepositoryModule;

const MAX_NUMBER: u64 = 9_007_199_254_740_991;

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
            version: 1,
            created_at_ms: now,
            updated_at_ms: now,
        };
        context.sql(&SqlBatch {
            statements: vec![statement(
                "INSERT INTO repository_issues(number, author_issuer, author_subject, author_name, title, body, state, version, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                vec![
                    integer(record.number)?,
                    SqlValue::Text(record.author.issuer.clone()),
                    SqlValue::Text(record.author.subject.clone()),
                    SqlValue::Text(record.author.name.clone()),
                    SqlValue::Text(record.title.clone()),
                    SqlValue::Text(record.body.clone()),
                    SqlValue::Integer(i64::from(record.state)),
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
                "SELECT number, author_issuer, author_subject, author_name, title, body, state, version, created_at_ms, updated_at_ms FROM repository_issues WHERE number = ?",
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

pub(crate) fn register(registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
    registry.bind_command::<CreateIssue>()?;
    registry.bind_command::<CreateComment>()?;
    registry.bind_query::<GetIssue>()?;
    registry.bind_query::<GetComment>()
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
        version: result_u64_from_row(row, 7)?,
        created_at_ms: result_u64_from_row(row, 8)?,
        updated_at_ms: result_u64_from_row(row, 9)?,
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
    if title.trim() != title || title.is_empty() || title.chars().count() > 256 {
        return Err(crab_cell_runtime::Error::Command(
            "repository issue title is invalid",
        ));
    }
    Ok(())
}

fn validate_body(body: &str, required: bool) -> crab_cell_runtime::Result<()> {
    if (required && body.is_empty()) || body.len() > 64 * 1024 {
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
    validate_number(record.version)?;
    if record.state > 1 || record.updated_at_ms < record.created_at_ms {
        return Err(crab_cell_runtime::Error::Command(
            "repository issue row is invalid",
        ));
    }
    Ok(())
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

impl WireValue for RepositoryAuthor {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_text(&self.issuer)?;
        encoder.write_text(&self.subject)?;
        encoder.write_text(&self.name)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            issuer: decoder.read_text()?.to_owned(),
            subject: decoder.read_text()?.to_owned(),
            name: decoder.read_text()?.to_owned(),
        })
    }
}

impl WireValue for CreateIssueInput {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        self.author.encode(encoder)?;
        encoder.write_text(&self.title)?;
        encoder.write_text(&self.body)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            author: RepositoryAuthor::decode(decoder)?,
            title: decoder.read_text()?.to_owned(),
            body: decoder.read_text()?.to_owned(),
        })
    }
}

impl WireValue for IssueRecord {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_u64(self.number)?;
        self.author.encode(encoder)?;
        encoder.write_text(&self.title)?;
        encoder.write_text(&self.body)?;
        encoder.write_u8(self.state)?;
        encoder.write_u64(self.version)?;
        encoder.write_u64(self.created_at_ms)?;
        encoder.write_u64(self.updated_at_ms)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            number: decoder.read_u64()?,
            author: RepositoryAuthor::decode(decoder)?,
            title: decoder.read_text()?.to_owned(),
            body: decoder.read_text()?.to_owned(),
            state: decoder.read_u8()?,
            version: decoder.read_u64()?,
            created_at_ms: decoder.read_u64()?,
            updated_at_ms: decoder.read_u64()?,
        })
    }
}

impl WireValue for CreateCommentInput {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_u64(self.issue)?;
        self.author.encode(encoder)?;
        encoder.write_text(&self.body)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            issue: decoder.read_u64()?,
            author: RepositoryAuthor::decode(decoder)?,
            body: decoder.read_text()?.to_owned(),
        })
    }
}

impl WireValue for CommentRecord {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_u64(self.issue)?;
        encoder.write_u64(self.number)?;
        self.author.encode(encoder)?;
        encoder.write_text(&self.body)?;
        encoder.write_u64(self.version)?;
        encoder.write_u64(self.created_at_ms)?;
        encoder.write_u64(self.updated_at_ms)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            issue: decoder.read_u64()?,
            number: decoder.read_u64()?,
            author: RepositoryAuthor::decode(decoder)?,
            body: decoder.read_text()?.to_owned(),
            version: decoder.read_u64()?,
            created_at_ms: decoder.read_u64()?,
            updated_at_ms: decoder.read_u64()?,
        })
    }
}

impl WireValue for CreateCommentOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Created(record) => {
                encoder.write_u8(1)?;
                record.encode(encoder)
            }
            Self::IssueNotFound => encoder.write_u8(2),
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            1 => Ok(Self::Created(CommentRecord::decode(decoder)?)),
            2 => Ok(Self::IssueNotFound),
            _ => Err(CodecError::Invalid("invalid create-comment outcome")),
        }
    }
}

impl WireValue for CommentKey {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_u64(self.issue)?;
        encoder.write_u64(self.number)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            issue: decoder.read_u64()?,
            number: decoder.read_u64()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_codec_v1_has_stable_command_and_query_fixtures() {
        let author = RepositoryAuthor {
            issuer: "i".into(),
            subject: "s".into(),
            name: "n".into(),
        };
        let issue = IssueRecord {
            number: 9,
            author: author.clone(),
            title: "t".into(),
            body: "b".into(),
            state: 0,
            version: 2,
            created_at_ms: 3,
            updated_at_ms: 4,
        };
        let comment = CommentRecord {
            issue: 7,
            number: 9,
            author: author.clone(),
            body: "b".into(),
            version: 2,
            created_at_ms: 3,
            updated_at_ms: 4,
        };
        assert_fixture(
            &CreateIssueInput {
                author: author.clone(),
                title: "t".into(),
                body: "b".into(),
            },
            "00000001690000000173000000016e00000001740000000162",
        );
        assert_fixture(
            &issue,
            "000000000000000900000001690000000173000000016e0000000174000000016200000000000000000200000000000000030000000000000004",
        );
        assert_fixture(
            &CreateCommentInput {
                issue: 7,
                author,
                body: "b".into(),
            },
            "000000000000000700000001690000000173000000016e0000000162",
        );
        assert_fixture(
            &CreateCommentOutcome::Created(comment.clone()),
            "010000000000000007000000000000000900000001690000000173000000016e0000000162000000000000000200000000000000030000000000000004",
        );
        assert_fixture(&7_u64, "0000000000000007");
        assert_fixture(
            &Some(issue),
            "01000000000000000900000001690000000173000000016e0000000174000000016200000000000000000200000000000000030000000000000004",
        );
        assert_fixture(
            &CommentKey {
                issue: 7,
                number: 9,
            },
            "00000000000000070000000000000009",
        );
        assert_fixture(
            &Some(comment),
            "010000000000000007000000000000000900000001690000000173000000016e0000000162000000000000000200000000000000030000000000000004",
        );
    }

    fn assert_fixture<T: WireValue + PartialEq + std::fmt::Debug>(value: &T, fixture: &str) {
        let bytes = decode_hex(fixture);
        let mut encoder = BoundedEncoder::new(80 * 1024).unwrap();
        value.encode(&mut encoder).unwrap();
        assert_eq!(encoder.finish(), bytes);

        let mut decoder = BoundedDecoder::new(&bytes, 80 * 1024).unwrap();
        assert_eq!(&T::decode(&mut decoder).unwrap(), value);
        decoder.finish().unwrap();
    }

    fn decode_hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let high = nibble(pair[0]);
                let low = nibble(pair[1]);
                (high << 4) | low
            })
            .collect()
    }

    fn nibble(byte: u8) -> u8 {
        match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            _ => panic!("invalid test fixture"),
        }
    }
}
