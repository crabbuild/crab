mod codec;

use super::*;

const MAX_CHECK_RUNS_PER_COMMIT: u64 = 100;
const MAX_CHECK_STEPS: usize = 50;
const MAX_CHECK_ANNOTATIONS: usize = 50;
const MAX_CHECK_OUTPUT_BYTES: u32 = 192 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CheckStepRecord {
    pub name: String,
    pub status: u8,
    pub conclusion: Option<u8>,
    pub log: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CheckAnnotationRecord {
    pub path: String,
    pub start_line: u64,
    pub end_line: u64,
    pub level: u8,
    pub title: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CheckOutputRecord {
    pub title: String,
    pub summary: String,
    pub text: Option<String>,
    pub steps: Vec<CheckStepRecord>,
    pub annotations: Vec<CheckAnnotationRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CheckRunDetail {
    pub run: CheckRunRecord,
    pub output: CheckOutputRecord,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CheckReportInput {
    pub status: u8,
    pub conclusion: Option<u8>,
    pub details_url: Option<String>,
    pub output: CheckOutputRecord,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CreateCheckRunInput {
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub oid: String,
    pub name: String,
    pub report: CheckReportInput,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum CreateCheckRunOutcome {
    Created(Box<CheckRunDetail>),
    RequestConflict,
    RunLimit,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct UpdateCheckRunInput {
    pub submission_id: [u8; 16],
    pub actor: RepositoryAuthor,
    pub oid: String,
    pub number: u64,
    pub version: u64,
    pub report: CheckReportInput,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum UpdateCheckRunOutcome {
    Updated(Box<CheckRunDetail>),
    RequestConflict,
    NotFound,
    Forbidden,
    Conflict,
    InvalidTransition,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CheckRunKey {
    pub oid: String,
    pub number: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CheckSubmissionKey {
    pub submission_id: [u8; 16],
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ListCheckRunsInput {
    pub oid: String,
    pub before: Option<u64>,
    pub limit: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CheckRunPage {
    pub runs: Vec<CheckRunRecord>,
    pub next: Option<u64>,
}

pub(crate) struct CreateCheckRun;

impl Mutation for CreateCheckRun {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 12;
    type Input = CreateCheckRunInput;
    type Output = CreateCheckRunOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
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
                return Ok(MutationResult::Rejected(
                    CreateCheckRunOutcome::RequestConflict,
                ));
            }
            let detail = load_check_version(context, result_u64_from_row(row, 1)?, 1)?;
            return Ok(MutationResult::Success(CreateCheckRunOutcome::Created(
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
            return Ok(MutationResult::Rejected(CreateCheckRunOutcome::RunLimit));
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
            return Err(crate::cells::Error::Invalid(
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
        Ok(MutationResult::Success(CreateCheckRunOutcome::Created(
            Box::new(CheckRunDetail {
                run,
                output: input.report.output,
            }),
        )))
    }
}

pub(crate) struct UpdateCheckRun;

impl Mutation for UpdateCheckRun {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 13;
    type Input = UpdateCheckRunInput;
    type Output = UpdateCheckRunOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
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
                return Ok(MutationResult::Rejected(
                    UpdateCheckRunOutcome::RequestConflict,
                ));
            }
            let detail = load_check_version(
                context,
                result_u64_from_row(row, 1)?,
                result_u64_from_row(row, 2)?,
            )?;
            return Ok(MutationResult::Success(UpdateCheckRunOutcome::Updated(
                Box::new(detail),
            )));
        }

        let Some(current) = load_current_check(context, &input.oid, input.number)? else {
            return Ok(MutationResult::Rejected(UpdateCheckRunOutcome::NotFound));
        };
        if !same_author(&current.run.author, &input.actor) {
            return Ok(MutationResult::Rejected(UpdateCheckRunOutcome::Forbidden));
        }
        if current.run.version != input.version {
            return Ok(MutationResult::Rejected(UpdateCheckRunOutcome::Conflict));
        }
        if current.run.status == 2 || (current.run.status == 1 && input.report.status == 0) {
            return Ok(MutationResult::Rejected(
                UpdateCheckRunOutcome::InvalidTransition,
            ));
        }
        let now = timestamp(context.now_ms())?;
        let version = current
            .run
            .version
            .checked_add(1)
            .filter(|version| *version <= MAX_NUMBER)
            .ok_or(crate::cells::Error::Invalid(
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
        Ok(MutationResult::Success(UpdateCheckRunOutcome::Updated(
            Box::new(CheckRunDetail {
                run,
                output: input.report.output,
            }),
        )))
    }
}

pub(crate) struct ListCheckRuns;

impl Read for ListCheckRuns {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 9;
    type Input = ListCheckRunsInput;
    type Output = CheckRunPage;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        validate_oid(&input.oid)?;
        if input.limit == 0
            || u64::from(input.limit) > MAX_CHECK_RUNS_PER_COMMIT
            || input
                .before
                .is_some_and(|before| before == 0 || before > MAX_NUMBER)
        {
            return Err(crate::cells::Error::Invalid(
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
            .collect::<crate::cells::Result<Vec<_>>>()?;
        let next = (runs.len() > usize::from(input.limit))
            .then(|| runs[usize::from(input.limit) - 1].number);
        runs.truncate(usize::from(input.limit));
        Ok(CheckRunPage { runs, next })
    }
}

pub(crate) struct GetCheckRun;

impl Read for GetCheckRun {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 10;
    type Input = CheckRunKey;
    type Output = Option<CheckRunDetail>;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        validate_oid(&input.oid)?;
        validate_number(input.number)?;
        load_current_check(context, &input.oid, input.number)
    }
}

pub(crate) struct GetCheckCreateSubmission;

impl Read for GetCheckCreateSubmission {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 11;
    type Input = CheckSubmissionKey;
    type Output = Option<CheckRunDetail>;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
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

impl Read for GetCheckUpdateSubmission {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 12;
    type Input = CheckSubmissionKey;
    type Output = Option<CheckRunDetail>;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
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

mod storage;
use storage::{
    check_run_from_row, create_submission_digest, insert_check_version, load_check_version,
    load_current_check, update_submission_digest, validate_check_report, validate_oid,
    validate_plain,
};

#[cfg(test)]
mod tests;
