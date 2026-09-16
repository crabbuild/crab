use super::*;

const MAX_CHECK_RUNS_PER_COMMIT: u64 = 100;
const MAX_CHECK_STEPS: usize = 50;
const MAX_CHECK_ANNOTATIONS: usize = 50;
const MAX_CHECK_OUTPUT_BYTES: u32 = 192 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CheckStepRecord {
    pub name: String,
    pub status: u8,
    pub conclusion: Option<u8>,
    pub log: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CheckAnnotationRecord {
    pub path: String,
    pub start_line: u64,
    pub end_line: u64,
    pub level: u8,
    pub title: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CheckOutputRecord {
    pub title: String,
    pub summary: String,
    pub text: Option<String>,
    pub steps: Vec<CheckStepRecord>,
    pub annotations: Vec<CheckAnnotationRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CheckRunRecord {
    pub number: u64,
    pub create_submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub oid: String,
    pub name: String,
    pub status: u8,
    pub conclusion: Option<u8>,
    pub details_url: Option<String>,
    pub output_title: String,
    pub version: u64,
    pub started_at_ms: Option<u64>,
    pub completed_at_ms: Option<u64>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CheckRunDetail {
    pub run: CheckRunRecord,
    pub output: CheckOutputRecord,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CheckReportInput {
    pub status: u8,
    pub conclusion: Option<u8>,
    pub details_url: Option<String>,
    pub output: CheckOutputRecord,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CreateCheckRunInput {
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub oid: String,
    pub name: String,
    pub report: CheckReportInput,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CreateCheckRunOutcome {
    Created(Box<CheckRunDetail>),
    RequestConflict,
    RunLimit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UpdateCheckRunInput {
    pub submission_id: [u8; 16],
    pub actor: RepositoryAuthor,
    pub oid: String,
    pub number: u64,
    pub version: u64,
    pub report: CheckReportInput,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UpdateCheckRunOutcome {
    Updated(Box<CheckRunDetail>),
    RequestConflict,
    NotFound,
    Forbidden,
    Conflict,
    InvalidTransition,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CheckRunKey {
    pub oid: String,
    pub number: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CheckSubmissionKey {
    pub submission_id: [u8; 16],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ListCheckRunsInput {
    pub oid: String,
    pub before: Option<u64>,
    pub limit: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CheckRunPage {
    pub runs: Vec<CheckRunRecord>,
    pub next: Option<u64>,
}

pub(crate) struct CreateCheckRun;

impl Command for CreateCheckRun {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 12;
    const CODEC_VERSION: u32 = 1;
    type Input = CreateCheckRunInput;
    type Output = CreateCheckRunOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        validate_author(&input.author)?;
        validate_oid(&input.oid)?;
        validate_plain(&input.name, 100, "repository check name is invalid")?;
        validate_check_report(&input.report)?;
        let digest = create_submission_digest(&input)?;
        let existing = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT payload_digest, run_number FROM repository_check_create_submissions WHERE request_id = ?",
                vec![SqlValue::Blob(input.submission_id.to_vec())],
            )],
        })?;
        if let Some(row) = existing[0].rows.first() {
            if result_blob(row, 0)? != digest.as_bytes() {
                return Ok(CommandResult::Rejected(
                    CreateCheckRunOutcome::RequestConflict,
                ));
            }
            let detail = load_check_version(context, result_u64_from_row(row, 1)?, 1)?;
            return Ok(CommandResult::Success(CreateCheckRunOutcome::Created(
                Box::new(detail),
            )));
        }

        let count = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT COUNT(*) FROM repository_check_run_versions WHERE oid = ? AND version = 1",
                vec![SqlValue::Text(input.oid.clone())],
            )],
        })?;
        if result_u64(&count, 0, 0)? >= MAX_CHECK_RUNS_PER_COMMIT {
            return Ok(CommandResult::Rejected(CreateCheckRunOutcome::RunLimit));
        }
        let sequence = context.sql(&SqlBatch {
            statements: vec![
                statement(
                    "UPDATE repository_sequences SET last = last + 1 WHERE kind = 'check' AND last < 9007199254740991",
                    vec![],
                ),
                statement(
                    "SELECT last FROM repository_sequences WHERE kind = 'check'",
                    vec![],
                ),
            ],
        })?;
        if sequence[0].rows_affected != 1 {
            return Err(crab_cell_runtime::Error::Command(
                "repository check numbering is exhausted",
            ));
        }
        let now = timestamp(context.now_ms())?;
        let run = CheckRunRecord {
            number: result_u64(&sequence, 1, 0)?,
            create_submission_id: input.submission_id,
            author: input.author,
            oid: input.oid,
            name: input.name,
            status: input.report.status,
            conclusion: input.report.conclusion,
            details_url: input.report.details_url,
            output_title: input.report.output.title.clone(),
            version: 1,
            started_at_ms: (input.report.status != 0).then_some(now),
            completed_at_ms: (input.report.status == 2).then_some(now),
            created_at_ms: now,
            updated_at_ms: now,
        };
        context.sql(&SqlBatch {
            statements: vec![statement(
                "INSERT INTO repository_check_create_submissions(request_id, payload_digest, run_number) VALUES (?, ?, ?)",
                vec![
                    SqlValue::Blob(run.create_submission_id.to_vec()),
                    SqlValue::Blob(digest.as_bytes().to_vec()),
                    integer(run.number)?,
                ],
            )],
        })?;
        insert_check_version(context, &run, &input.report.output)?;
        advance_revision(context)?;
        Ok(CommandResult::Success(CreateCheckRunOutcome::Created(
            Box::new(CheckRunDetail {
                run,
                output: input.report.output,
            }),
        )))
    }
}

pub(crate) struct UpdateCheckRun;

impl Command for UpdateCheckRun {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 13;
    const CODEC_VERSION: u32 = 1;
    type Input = UpdateCheckRunInput;
    type Output = UpdateCheckRunOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        validate_author(&input.actor)?;
        validate_oid(&input.oid)?;
        validate_number(input.number)?;
        validate_number(input.version)?;
        validate_check_report(&input.report)?;
        let digest = update_submission_digest(&input)?;
        let existing = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT payload_digest, run_number, result_version FROM repository_check_update_submissions WHERE request_id = ?",
                vec![SqlValue::Blob(input.submission_id.to_vec())],
            )],
        })?;
        if let Some(row) = existing[0].rows.first() {
            if result_blob(row, 0)? != digest.as_bytes() {
                return Ok(CommandResult::Rejected(
                    UpdateCheckRunOutcome::RequestConflict,
                ));
            }
            let detail = load_check_version(
                context,
                result_u64_from_row(row, 1)?,
                result_u64_from_row(row, 2)?,
            )?;
            return Ok(CommandResult::Success(UpdateCheckRunOutcome::Updated(
                Box::new(detail),
            )));
        }

        let Some(current) = load_current_check(context, &input.oid, input.number)? else {
            return Ok(CommandResult::Rejected(UpdateCheckRunOutcome::NotFound));
        };
        if !same_author(&current.run.author, &input.actor) {
            return Ok(CommandResult::Rejected(UpdateCheckRunOutcome::Forbidden));
        }
        if current.run.version != input.version {
            return Ok(CommandResult::Rejected(UpdateCheckRunOutcome::Conflict));
        }
        if current.run.status == 2 || (current.run.status == 1 && input.report.status == 0) {
            return Ok(CommandResult::Rejected(
                UpdateCheckRunOutcome::InvalidTransition,
            ));
        }
        let now = timestamp(context.now_ms())?;
        let version = current
            .run
            .version
            .checked_add(1)
            .filter(|version| *version <= MAX_NUMBER)
            .ok_or(crab_cell_runtime::Error::Command(
                "repository check version is exhausted",
            ))?;
        let run = CheckRunRecord {
            number: current.run.number,
            create_submission_id: current.run.create_submission_id,
            author: current.run.author,
            oid: current.run.oid,
            name: current.run.name,
            status: input.report.status,
            conclusion: input.report.conclusion,
            details_url: input.report.details_url,
            output_title: input.report.output.title.clone(),
            version,
            started_at_ms: current
                .run
                .started_at_ms
                .or((input.report.status != 0).then_some(now)),
            completed_at_ms: (input.report.status == 2).then_some(now),
            created_at_ms: current.run.created_at_ms,
            updated_at_ms: now,
        };
        insert_check_version(context, &run, &input.report.output)?;
        context.sql(&SqlBatch {
            statements: vec![statement(
                "INSERT INTO repository_check_update_submissions(request_id, payload_digest, run_number, result_version) VALUES (?, ?, ?, ?)",
                vec![
                    SqlValue::Blob(input.submission_id.to_vec()),
                    SqlValue::Blob(digest.as_bytes().to_vec()),
                    integer(run.number)?,
                    integer(run.version)?,
                ],
            )],
        })?;
        advance_revision(context)?;
        Ok(CommandResult::Success(UpdateCheckRunOutcome::Updated(
            Box::new(CheckRunDetail {
                run,
                output: input.report.output,
            }),
        )))
    }
}

pub(crate) struct ListCheckRuns;

impl Query for ListCheckRuns {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 9;
    const CODEC_VERSION: u32 = 1;
    type Input = ListCheckRunsInput;
    type Output = CheckRunPage;

    fn execute(
        context: &mut QueryContext<'_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        validate_oid(&input.oid)?;
        if input.limit == 0
            || u64::from(input.limit) > MAX_CHECK_RUNS_PER_COMMIT
            || input
                .before
                .is_some_and(|before| before == 0 || before > MAX_NUMBER)
        {
            return Err(crab_cell_runtime::Error::Command(
                "repository check page bounds are invalid",
            ));
        }
        let before = input.before.unwrap_or(MAX_NUMBER + 1);
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT run_number, create_request_id, author_issuer, author_subject, author_name, oid, name, status, conclusion, details_url, output_title, version, started_at_ms, completed_at_ms, created_at_ms, updated_at_ms FROM repository_check_run_versions AS current WHERE oid = ? AND run_number < ? AND NOT EXISTS (SELECT 1 FROM repository_check_run_versions AS newer WHERE newer.run_number = current.run_number AND newer.version > current.version) ORDER BY run_number DESC LIMIT ?",
                vec![
                    SqlValue::Text(input.oid),
                    integer(before)?,
                    integer(u64::from(input.limit) + 1)?,
                ],
            )],
        })?;
        let mut runs = result[0]
            .rows
            .iter()
            .map(|row| check_run_from_row(row))
            .collect::<crab_cell_runtime::Result<Vec<_>>>()?;
        let next = (runs.len() > usize::from(input.limit))
            .then(|| runs[usize::from(input.limit) - 1].number);
        runs.truncate(usize::from(input.limit));
        Ok(CheckRunPage { runs, next })
    }
}

pub(crate) struct GetCheckRun;

impl Query for GetCheckRun {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 10;
    const CODEC_VERSION: u32 = 1;
    type Input = CheckRunKey;
    type Output = Option<CheckRunDetail>;

    fn execute(
        context: &mut QueryContext<'_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        validate_oid(&input.oid)?;
        validate_number(input.number)?;
        load_current_check(context, &input.oid, input.number)
    }
}

pub(crate) struct GetCheckCreateSubmission;

impl Query for GetCheckCreateSubmission {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 11;
    const CODEC_VERSION: u32 = 1;
    type Input = CheckSubmissionKey;
    type Output = Option<CheckRunDetail>;

    fn execute(
        context: &mut QueryContext<'_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT run_number FROM repository_check_create_submissions WHERE request_id = ?",
                vec![SqlValue::Blob(input.submission_id.to_vec())],
            )],
        })?;
        result[0]
            .rows
            .first()
            .map(|row| load_check_version(context, result_u64_from_row(row, 0)?, 1))
            .transpose()
    }
}

pub(crate) struct GetCheckUpdateSubmission;

impl Query for GetCheckUpdateSubmission {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 12;
    const CODEC_VERSION: u32 = 1;
    type Input = CheckSubmissionKey;
    type Output = Option<CheckRunDetail>;

    fn execute(
        context: &mut QueryContext<'_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT run_number, result_version FROM repository_check_update_submissions WHERE request_id = ?",
                vec![SqlValue::Blob(input.submission_id.to_vec())],
            )],
        })?;
        result[0]
            .rows
            .first()
            .map(|row| {
                load_check_version(
                    context,
                    result_u64_from_row(row, 0)?,
                    result_u64_from_row(row, 1)?,
                )
            })
            .transpose()
    }
}

fn insert_check_version(
    context: &CommandContext<'_, '_>,
    run: &CheckRunRecord,
    output: &CheckOutputRecord,
) -> crab_cell_runtime::Result<()> {
    context.sql(&SqlBatch {
        statements: vec![statement(
            "INSERT INTO repository_check_run_versions(run_number, version, create_request_id, author_issuer, author_subject, author_name, oid, name, status, conclusion, details_url, output_title, output, started_at_ms, completed_at_ms, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            vec![
                integer(run.number)?,
                integer(run.version)?,
                SqlValue::Blob(run.create_submission_id.to_vec()),
                SqlValue::Text(run.author.issuer.clone()),
                SqlValue::Text(run.author.subject.clone()),
                SqlValue::Text(run.author.name.clone()),
                SqlValue::Text(run.oid.clone()),
                SqlValue::Text(run.name.clone()),
                SqlValue::Integer(i64::from(run.status)),
                optional_integer(run.conclusion.map(u64::from))?,
                optional_text(run.details_url.clone()),
                SqlValue::Text(run.output_title.clone()),
                SqlValue::Blob(encode_check_output(output)?),
                optional_integer(run.started_at_ms)?,
                optional_integer(run.completed_at_ms)?,
                integer(run.created_at_ms)?,
                integer(run.updated_at_ms)?,
            ],
        )],
    })?;
    Ok(())
}

fn load_check_version(
    context: &impl CheckSqlContext,
    number: u64,
    version: u64,
) -> crab_cell_runtime::Result<CheckRunDetail> {
    let result = context.check_sql(&SqlBatch {
        statements: vec![statement(
            "SELECT run_number, create_request_id, author_issuer, author_subject, author_name, oid, name, status, conclusion, details_url, output_title, version, started_at_ms, completed_at_ms, created_at_ms, updated_at_ms, output FROM repository_check_run_versions WHERE run_number = ? AND version = ?",
            vec![integer(number)?, integer(version)?],
        )],
    })?;
    result[0]
        .rows
        .first()
        .map(|row| check_detail_from_row(row))
        .transpose()?
        .ok_or(crab_cell_runtime::Error::Command(
            "repository check submission has no result row",
        ))
}

trait CheckSqlContext {
    fn check_sql(&self, batch: &SqlBatch) -> crab_cell_runtime::Result<Vec<SqlResultSet>>;
}

impl CheckSqlContext for CommandContext<'_, '_> {
    fn check_sql(&self, batch: &SqlBatch) -> crab_cell_runtime::Result<Vec<SqlResultSet>> {
        self.sql(batch)
    }
}

impl CheckSqlContext for QueryContext<'_> {
    fn check_sql(&self, batch: &SqlBatch) -> crab_cell_runtime::Result<Vec<SqlResultSet>> {
        self.sql(batch)
    }
}

fn load_current_check(
    context: &impl CheckSqlContext,
    oid: &str,
    number: u64,
) -> crab_cell_runtime::Result<Option<CheckRunDetail>> {
    let result = context.check_sql(&SqlBatch {
        statements: vec![statement(
            "SELECT run_number, create_request_id, author_issuer, author_subject, author_name, oid, name, status, conclusion, details_url, output_title, version, started_at_ms, completed_at_ms, created_at_ms, updated_at_ms, output FROM repository_check_run_versions WHERE oid = ? AND run_number = ? ORDER BY version DESC LIMIT 1",
            vec![SqlValue::Text(oid.to_owned()), integer(number)?],
        )],
    })?;
    result[0]
        .rows
        .first()
        .map(|row| check_detail_from_row(row))
        .transpose()
}

fn check_detail_from_row(row: &[SqlValue]) -> crab_cell_runtime::Result<CheckRunDetail> {
    let run = check_run_from_row(row)?;
    let output = decode_check_output(result_blob(row, 16)?)?;
    if output.title != run.output_title {
        return Err(crab_cell_runtime::Error::Command(
            "repository check output title does not match its run",
        ));
    }
    Ok(CheckRunDetail { run, output })
}

fn check_run_from_row(row: &[SqlValue]) -> crab_cell_runtime::Result<CheckRunRecord> {
    let submission = <[u8; 16]>::try_from(result_blob(row, 1)?).map_err(|_| {
        crab_cell_runtime::Error::Command("repository check submission ID is invalid")
    })?;
    let status = u8::try_from(result_u64_from_row(row, 7)?)
        .map_err(|_| crab_cell_runtime::Error::Command("repository check status is invalid"))?;
    let conclusion = result_optional_u64(row, 8)?
        .map(u8::try_from)
        .transpose()
        .map_err(|_| crab_cell_runtime::Error::Command("repository check conclusion is invalid"))?;
    let run = CheckRunRecord {
        number: result_u64_from_row(row, 0)?,
        create_submission_id: submission,
        author: RepositoryAuthor {
            issuer: result_text(row, 2)?,
            subject: result_text(row, 3)?,
            name: result_text(row, 4)?,
        },
        oid: result_text(row, 5)?,
        name: result_text(row, 6)?,
        status,
        conclusion,
        details_url: result_optional_text(row, 9)?,
        output_title: result_text(row, 10)?,
        version: result_u64_from_row(row, 11)?,
        started_at_ms: result_optional_u64(row, 12)?,
        completed_at_ms: result_optional_u64(row, 13)?,
        created_at_ms: result_u64_from_row(row, 14)?,
        updated_at_ms: result_u64_from_row(row, 15)?,
    };
    validate_check_run(&run)?;
    Ok(run)
}

fn validate_check_run(run: &CheckRunRecord) -> crab_cell_runtime::Result<()> {
    validate_number(run.number)?;
    validate_number(run.version)?;
    validate_author(&run.author)?;
    validate_oid(&run.oid)?;
    validate_plain(&run.name, 100, "repository check name is invalid")?;
    validate_plain(
        &run.output_title,
        200,
        "repository check output title is invalid",
    )?;
    validate_check_state(run.status, run.conclusion)?;
    validate_target_url(run.details_url.as_deref())?;
    if run.updated_at_ms < run.created_at_ms
        || run
            .started_at_ms
            .is_some_and(|value| value < run.created_at_ms)
        || run
            .completed_at_ms
            .is_some_and(|value| value < run.created_at_ms)
        || (run.status == 0 && run.started_at_ms.is_some())
        || (run.status != 0 && run.started_at_ms.is_none())
        || (run.status == 2) != run.completed_at_ms.is_some()
    {
        return Err(crab_cell_runtime::Error::Command(
            "repository check timestamps are invalid",
        ));
    }
    Ok(())
}

fn validate_check_report(report: &CheckReportInput) -> crab_cell_runtime::Result<()> {
    validate_check_state(report.status, report.conclusion)?;
    validate_target_url(report.details_url.as_deref())?;
    validate_check_output(&report.output)
}

fn validate_check_output(output: &CheckOutputRecord) -> crab_cell_runtime::Result<()> {
    validate_plain(
        &output.title,
        200,
        "repository check output title is invalid",
    )?;
    validate_markdown(&output.summary, 32 * 1024, true)?;
    if let Some(text) = output.text.as_deref() {
        validate_markdown(text, 64 * 1024, false)?;
    }
    if output.steps.len() > MAX_CHECK_STEPS || output.annotations.len() > MAX_CHECK_ANNOTATIONS {
        return Err(crab_cell_runtime::Error::Command(
            "repository check output collection is too large",
        ));
    }
    for step in &output.steps {
        validate_plain(&step.name, 100, "repository check step name is invalid")?;
        validate_check_state(step.status, step.conclusion)?;
        if let Some(log) = step.log.as_deref() {
            validate_markdown(log, 64 * 1024, false)?;
        }
    }
    for annotation in &output.annotations {
        validate_plain(
            &annotation.path,
            1_024,
            "repository check annotation path is invalid",
        )?;
        if annotation.start_line == 0
            || annotation.end_line < annotation.start_line
            || annotation.end_line > MAX_NUMBER
            || annotation.level > 2
        {
            return Err(crab_cell_runtime::Error::Command(
                "repository check annotation is invalid",
            ));
        }
        if let Some(title) = annotation.title.as_deref() {
            validate_plain(title, 200, "repository check annotation title is invalid")?;
        }
        validate_markdown(&annotation.message, 4 * 1024, true)?;
    }
    encode_check_output(output).map(|_| ())
}

fn validate_check_state(status: u8, conclusion: Option<u8>) -> crab_cell_runtime::Result<()> {
    if status > 2
        || conclusion.is_some_and(|value| value > 6)
        || ((status == 2) != conclusion.is_some())
    {
        return Err(crab_cell_runtime::Error::Command(
            "repository check state is invalid",
        ));
    }
    Ok(())
}

fn validate_oid(oid: &str) -> crab_cell_runtime::Result<()> {
    if oid.len() != 40
        || !oid
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || oid.bytes().all(|byte| byte == b'0')
    {
        return Err(crab_cell_runtime::Error::Command(
            "repository check commit is invalid",
        ));
    }
    Ok(())
}

fn validate_plain(
    value: &str,
    maximum: usize,
    error: &'static str,
) -> crab_cell_runtime::Result<()> {
    if value.is_empty()
        || value.trim() != value
        || value.chars().count() > maximum
        || value.chars().any(char::is_control)
    {
        return Err(crab_cell_runtime::Error::Command(error));
    }
    Ok(())
}

fn validate_markdown(value: &str, maximum: usize, required: bool) -> crab_cell_runtime::Result<()> {
    if value.len() > maximum || value.contains('\0') || (required && value.trim().is_empty()) {
        return Err(crab_cell_runtime::Error::Command(
            "repository check markdown is invalid",
        ));
    }
    Ok(())
}

fn validate_target_url(value: Option<&str>) -> crab_cell_runtime::Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.len() > 2_048 || value.chars().any(char::is_control) {
        return Err(crab_cell_runtime::Error::Command(
            "repository check target is invalid",
        ));
    }
    let url = url::Url::parse(value)
        .map_err(|_| crab_cell_runtime::Error::Command("repository check target is invalid"))?;
    crate::config::validate_identity_url(&url, true)
        .map_err(|_| crab_cell_runtime::Error::Command("repository check target is invalid"))
}

fn encode_check_output(output: &CheckOutputRecord) -> crab_cell_runtime::Result<Vec<u8>> {
    let mut encoder = BoundedEncoder::new(MAX_CHECK_OUTPUT_BYTES)
        .map_err(|_| crab_cell_runtime::Error::Command("repository check output is too large"))?;
    output
        .encode(&mut encoder)
        .map_err(|_| crab_cell_runtime::Error::Command("repository check output is too large"))?;
    Ok(encoder.finish())
}

fn decode_check_output(bytes: &[u8]) -> crab_cell_runtime::Result<CheckOutputRecord> {
    let mut decoder = BoundedDecoder::new(bytes, MAX_CHECK_OUTPUT_BYTES)
        .map_err(|_| crab_cell_runtime::Error::Command("repository check output is invalid"))?;
    let output = CheckOutputRecord::decode(&mut decoder)
        .map_err(|_| crab_cell_runtime::Error::Command("repository check output is invalid"))?;
    decoder
        .finish()
        .map_err(|_| crab_cell_runtime::Error::Command("repository check output is invalid"))?;
    validate_check_output(&output)?;
    Ok(output)
}

fn create_submission_digest(
    input: &CreateCheckRunInput,
) -> crab_cell_runtime::Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.repository.check-create-submission.v1\0");
    hash_text(&mut hasher, &input.author.issuer);
    hash_text(&mut hasher, &input.author.subject);
    hash_text(&mut hasher, &input.oid);
    hash_text(&mut hasher, &input.name);
    hash_report(&mut hasher, &input.report)?;
    Ok(hasher.finalize())
}

fn update_submission_digest(
    input: &UpdateCheckRunInput,
) -> crab_cell_runtime::Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.repository.check-update-submission.v1\0");
    hash_text(&mut hasher, &input.actor.issuer);
    hash_text(&mut hasher, &input.actor.subject);
    hash_text(&mut hasher, &input.oid);
    hasher.update(&input.number.to_be_bytes());
    hasher.update(&input.version.to_be_bytes());
    hash_report(&mut hasher, &input.report)?;
    Ok(hasher.finalize())
}

fn hash_report(
    hasher: &mut blake3::Hasher,
    report: &CheckReportInput,
) -> crab_cell_runtime::Result<()> {
    hasher.update(&[report.status]);
    match report.conclusion {
        Some(value) => hasher.update(&[1, value]),
        None => hasher.update(&[0]),
    };
    hash_optional_text(hasher, report.details_url.as_deref());
    let output = encode_check_output(&report.output)?;
    hasher.update(&(output.len() as u64).to_be_bytes());
    hasher.update(&output);
    Ok(())
}

fn optional_integer(value: Option<u64>) -> crab_cell_runtime::Result<SqlValue> {
    value.map_or(Ok(SqlValue::Null), integer)
}

fn optional_text(value: Option<String>) -> SqlValue {
    value.map_or(SqlValue::Null, SqlValue::Text)
}

fn result_optional_u64(row: &[SqlValue], column: usize) -> crab_cell_runtime::Result<Option<u64>> {
    match row.get(column) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Integer(value)) => u64::try_from(*value)
            .map(Some)
            .map_err(|_| crab_cell_runtime::Error::Command("repository result is negative")),
        _ => Err(crab_cell_runtime::Error::Command(
            "repository query returned an invalid optional integer",
        )),
    }
}
