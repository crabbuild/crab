use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{get, post},
};
use cellule_runtime::{Committed, InvocationError, MutationIdentity, Observed, RequestId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    app::{self, Error, Result},
    auth::{Identity, Principal},
    cells::{
        RepositoryCell,
        repository::{
            CheckAnnotationRecord, CheckOutputRecord, CheckReportInput, CheckRunDetail,
            CheckRunKey, CheckRunRecord, CheckStepRecord, CheckSubmissionKey, CreateCheckRun,
            CreateCheckRunInput, CreateCheckRunOutcome, GetCheckCreateSubmission, GetCheckRun,
            GetCheckUpdateSubmission, ListCheckRuns, ListCheckRunsInput, RepositoryAuthor,
            UpdateCheckRun, UpdateCheckRunInput, UpdateCheckRunOutcome,
        },
    },
    server::{Repository, Server},
    statuses,
};

const MAX_STEPS: usize = 50;
const MAX_ANNOTATIONS: usize = 50;
const MAX_OUTPUT_BYTES: usize = 192 * 1024;
const MAX_NUMBER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CheckStatus {
    Queued,
    InProgress,
    Completed,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CheckConclusion {
    ActionRequired,
    Cancelled,
    Failure,
    Neutral,
    Skipped,
    Success,
    TimedOut,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CheckStep {
    name: String,
    status: CheckStatus,
    conclusion: Option<CheckConclusion>,
    log: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum AnnotationLevel {
    Notice,
    Warning,
    Failure,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CheckAnnotation {
    path: String,
    start_line: u64,
    end_line: u64,
    level: AnnotationLevel,
    title: Option<String>,
    message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CheckOutput {
    title: String,
    summary: String,
    text: Option<String>,
    #[serde(default)]
    steps: Vec<CheckStep>,
    #[serde(default)]
    annotations: Vec<CheckAnnotation>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckRun {
    pub number: u64,
    request_id: String,
    pub author: Identity,
    pub oid: String,
    pub name: String,
    pub status: CheckStatus,
    pub conclusion: Option<CheckConclusion>,
    pub details_url: Option<String>,
    pub output_title: String,
    pub version: u64,
    pub started_at: Option<u64>,
    pub completed_at: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct NewCheckRun {
    request_id: String,
    head_sha: String,
    name: String,
    status: CheckStatus,
    conclusion: Option<CheckConclusion>,
    details_url: Option<String>,
    output: CheckOutput,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CheckEdit {
    request_id: String,
    version: u64,
    status: CheckStatus,
    conclusion: Option<CheckConclusion>,
    details_url: Option<String>,
    output: CheckOutput,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListParams {
    limit: Option<usize>,
    before: Option<u64>,
}

pub(crate) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/{owner}/{name}/check-runs", post(create))
        .route(
            "/api/repos/{owner}/{name}/commits/{oid}/check-runs",
            get(list),
        )
        .route(
            "/api/repos/{owner}/{name}/commits/{oid}/check-runs/{number}",
            get(detail).patch(edit),
        )
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

fn validate_plain(value: &str, maximum: usize, message: &'static str) -> Result<()> {
    if value.trim() != value
        || value.is_empty()
        || value.chars().count() > maximum
        || value.chars().any(char::is_control)
    {
        return Err(Error::Invalid(message));
    }
    Ok(())
}

fn validate_markdown(value: &str, maximum: usize, required: bool) -> Result<()> {
    if value.len() > maximum || value.contains('\0') || (required && value.trim().is_empty()) {
        return Err(Error::Invalid(
            "Check output exceeds its size limit or contains NUL bytes",
        ));
    }
    Ok(())
}

fn validate_state(status: CheckStatus, conclusion: Option<CheckConclusion>) -> Result<()> {
    if (status == CheckStatus::Completed) != conclusion.is_some() {
        return Err(Error::Invalid(
            "Completed checks require a conclusion; queued and in-progress checks cannot have one",
        ));
    }
    Ok(())
}

fn validate_output(output: &CheckOutput) -> Result<()> {
    validate_plain(
        &output.title,
        200,
        "Check output titles must contain 1–200 characters without controls",
    )?;
    validate_markdown(&output.summary, 32 * 1024, true)?;
    if let Some(text) = &output.text {
        validate_markdown(text, 64 * 1024, false)?;
    }
    if output.steps.len() > MAX_STEPS {
        return Err(Error::Invalid("A check run supports at most 50 steps"));
    }
    for step in &output.steps {
        validate_plain(
            &step.name,
            100,
            "Check step names must contain 1–100 characters without controls",
        )?;
        validate_state(step.status, step.conclusion)?;
        if let Some(log) = &step.log {
            validate_markdown(log, 64 * 1024, false)?;
        }
    }
    if output.annotations.len() > MAX_ANNOTATIONS {
        return Err(Error::Invalid(
            "A check run supports at most 50 annotations",
        ));
    }
    for annotation in &output.annotations {
        validate_plain(
            &annotation.path,
            1_024,
            "Annotation paths must contain 1–1024 characters without controls",
        )?;
        if annotation.start_line == 0
            || annotation.end_line < annotation.start_line
            || annotation.end_line > MAX_NUMBER
        {
            return Err(Error::Invalid(
                "Annotation line ranges must be positive, ordered and safe JSON integers",
            ));
        }
        if let Some(title) = &annotation.title {
            validate_plain(
                title,
                200,
                "Annotation titles must contain 1–200 characters without controls",
            )?;
        }
        validate_markdown(&annotation.message, 4 * 1024, true)?;
    }
    if serde_json::to_vec(output)?.len() > MAX_OUTPUT_BYTES {
        return Err(Error::Invalid(
            "Check output must fit within 192 KiB after encoding",
        ));
    }
    Ok(())
}

fn validate_report(
    status: CheckStatus,
    conclusion: Option<CheckConclusion>,
    details_url: Option<&str>,
    output: &CheckOutput,
) -> Result<()> {
    validate_state(status, conclusion)?;
    statuses::validate_target(details_url)?;
    validate_output(output)
}

fn run_view(run: &CheckRun) -> Value {
    json!({
        "id":run.number,
        "head_sha":run.oid,
        "name":run.name,
        "status":run.status,
        "conclusion":run.conclusion,
        "details_url":run.details_url,
        "output_title":run.output_title,
        "author":run.author.name,
        "version":run.version,
        "started_at":run.started_at,
        "completed_at":run.completed_at,
        "created_at":run.created_at,
        "updated_at":run.updated_at,
    })
}

fn detail_view(run: &CheckRun, output: &CheckOutput) -> Result<Value> {
    let mut value = run_view(run);
    value["output"] = serde_json::to_value(output)?;
    Ok(value)
}

async fn create(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    input: std::result::Result<Json<NewCheckRun>, JsonRejection>,
) -> Result<impl IntoResponse> {
    let repo = app::repository(&server, &principal, &key)?;
    let repo = repo.as_ref();
    if !principal.can_write(&repo.config) {
        return Err(Error::CheckPermission);
    }
    let Json(mut input) = input?;
    let submission_id = submission_id(&input.request_id)?;
    validate_plain(
        &input.name,
        100,
        "Check names must contain 1–100 characters without controls",
    )?;
    validate_report(
        input.status,
        input.conclusion,
        input.details_url.as_deref(),
        &input.output,
    )?;
    let parsed_oid = statuses::parse_oid(&input.head_sha)?;
    input.request_id = Uuid::from_bytes(submission_id).to_string();
    input.head_sha = parsed_oid.to_string();
    let actor = app::actor(&principal)?;
    let read = route(&server, repo, &actor, "repository.read").await?;
    if let Some(saved) = query_output(
        read.client
            .query::<GetCheckCreateSubmission>(
                &read.target,
                None,
                CheckSubmissionKey { submission_id },
            )
            .await,
    )? {
        if !same_create(&saved, &actor, &input)? {
            return Err(Error::RequestConflict);
        }
        let routed = route(&server, repo, &actor, "repository.check.create").await?;
        let detail = created_check(command_output(
            routed
                .client
                .command::<CreateCheckRun>(
                    &routed.target,
                    mutation_identity()?,
                    create_input(submission_id, &actor, input),
                )
                .await,
        )?)?;
        let (run, output) = check_detail(detail)?;
        return Ok((StatusCode::CREATED, Json(detail_view(&run, &output)?)));
    }

    input.head_sha = statuses::require_commit(&server, repo, parsed_oid).await?;
    if !principal.can_write(&repo.config) {
        return Err(Error::CheckPermission);
    }
    let routed = route(&server, repo, &actor, "repository.check.create").await?;
    let detail = created_check(command_output(
        routed
            .client
            .command::<CreateCheckRun>(
                &routed.target,
                mutation_identity()?,
                create_input(submission_id, &actor, input),
            )
            .await,
    )?)?;
    let (run, output) = check_detail(detail)?;
    Ok((StatusCode::CREATED, Json(detail_view(&run, &output)?)))
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, oid)): Path<(String, String, String)>,
    Query(params): Query<ListParams>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let oid = statuses::require_commit(&server, repo, statuses::parse_oid(&oid)?).await?;
    let limit = params.limit.unwrap_or(30);
    if !(1..=50).contains(&limit)
        || params
            .before
            .is_some_and(|before| before == 0 || before > MAX_NUMBER)
    {
        return Err(Error::Invalid(
            "Check run pages require limit 1–50 and a positive before cursor",
        ));
    }
    let actor = app::actor(&principal)?;
    let routed = route(&server, repo, &actor, "repository.read").await?;
    let page = query_output(
        routed
            .client
            .query::<ListCheckRuns>(
                &routed.target,
                None,
                ListCheckRunsInput {
                    oid: oid.clone(),
                    before: params.before,
                    limit: u8::try_from(limit)
                        .map_err(|_| Error::CellContract("check page limit overflowed"))?,
                },
            )
            .await,
    )?;
    let runs = page
        .runs
        .into_iter()
        .map(check_run)
        .collect::<Result<Vec<_>>>()?;
    Ok(Json(json!({
        "sha":oid,
        "items":runs.iter().map(run_view).collect::<Vec<_>>(),
        "next":page.next,
    })))
}

async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, oid, number)): Path<(String, String, String, u64)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let oid = statuses::require_commit(&server, repo, statuses::parse_oid(&oid)?).await?;
    let actor = app::actor(&principal)?;
    let routed = route(&server, repo, &actor, "repository.read").await?;
    let detail = query_output(
        routed
            .client
            .query::<GetCheckRun>(
                &routed.target,
                None,
                CheckRunKey {
                    oid,
                    number: app::number(number)?,
                },
            )
            .await,
    )?
    .ok_or(Error::CheckNotFound)?;
    let (run, output) = check_detail(detail)?;
    Ok(Json(detail_view(&run, &output)?))
}

async fn edit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, oid, number)): Path<(String, String, String, u64)>,
    input: std::result::Result<Json<CheckEdit>, JsonRejection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    if !principal.can_write(&repo.config) {
        return Err(Error::CheckPermission);
    }
    let parsed_oid = statuses::parse_oid(&oid)?;
    let oid = parsed_oid.to_string();
    let number = app::number(number)?;
    let Json(mut input) = input?;
    let submission_id = submission_id(&input.request_id)?;
    input.version = app::number(input.version)?;
    input.request_id = Uuid::from_bytes(submission_id).to_string();
    validate_report(
        input.status,
        input.conclusion,
        input.details_url.as_deref(),
        &input.output,
    )?;
    let actor = app::actor(&principal)?;
    let read = route(&server, repo, &actor, "repository.read").await?;
    if let Some(saved) = query_output(
        read.client
            .query::<GetCheckUpdateSubmission>(
                &read.target,
                None,
                CheckSubmissionKey { submission_id },
            )
            .await,
    )? {
        if !same_update(&saved, &actor, &oid, number, &input)? {
            return Err(Error::RequestConflict);
        }
        let routed = route(&server, repo, &actor, "repository.check.update").await?;
        let detail = updated_check(command_output(
            routed
                .client
                .command::<UpdateCheckRun>(
                    &routed.target,
                    mutation_identity()?,
                    update_input(submission_id, &actor, oid, number, input),
                )
                .await,
        )?)?;
        let (run, output) = check_detail(detail)?;
        return Ok(Json(detail_view(&run, &output)?));
    }

    let oid = statuses::require_commit(&server, repo, parsed_oid).await?;
    if !principal.can_write(&repo.config) {
        return Err(Error::CheckPermission);
    }
    let routed = route(&server, repo, &actor, "repository.check.update").await?;
    let detail = updated_check(command_output(
        routed
            .client
            .command::<UpdateCheckRun>(
                &routed.target,
                mutation_identity()?,
                update_input(submission_id, &actor, oid, number, input),
            )
            .await,
    )?)?;
    let (run, output) = check_detail(detail)?;
    Ok(Json(detail_view(&run, &output)?))
}

pub(crate) async fn latest(
    server: &Server,
    repo: &Repository,
    principal: &Identity,
    oid: &str,
) -> Result<Vec<CheckRun>> {
    let routed = route(server, repo, principal, "repository.read").await?;
    let page = query_output(
        routed
            .client
            .query::<ListCheckRuns>(
                &routed.target,
                None,
                ListCheckRunsInput {
                    oid: oid.to_owned(),
                    before: None,
                    limit: 100,
                },
            )
            .await,
    )?;
    let mut latest = Vec::new();
    for record in page.runs {
        let run = check_run(record)?;
        if latest
            .iter()
            .all(|current: &CheckRun| !statuses::same_context(&current.name, &run.name))
        {
            latest.push(run);
        }
    }
    latest.sort_by_cached_key(|run| run.name.to_lowercase());
    Ok(latest)
}

impl CheckRun {
    pub(crate) fn requirement_state(&self) -> statuses::StatusState {
        match (self.status, self.conclusion) {
            (CheckStatus::Queued | CheckStatus::InProgress, _) => statuses::StatusState::Pending,
            (
                CheckStatus::Completed,
                Some(
                    CheckConclusion::Success | CheckConclusion::Neutral | CheckConclusion::Skipped,
                ),
            ) => statuses::StatusState::Success,
            (CheckStatus::Completed, _) => statuses::StatusState::Failure,
        }
    }
}

fn create_input(
    submission_id: [u8; 16],
    actor: &Identity,
    input: NewCheckRun,
) -> CreateCheckRunInput {
    CreateCheckRunInput {
        submission_id,
        author: repository_author(actor),
        oid: input.head_sha,
        name: input.name,
        report: report(
            input.status,
            input.conclusion,
            input.details_url,
            input.output,
        ),
    }
}

fn update_input(
    submission_id: [u8; 16],
    actor: &Identity,
    oid: String,
    number: u64,
    input: CheckEdit,
) -> UpdateCheckRunInput {
    UpdateCheckRunInput {
        submission_id,
        actor: repository_author(actor),
        oid,
        number,
        version: input.version,
        report: report(
            input.status,
            input.conclusion,
            input.details_url,
            input.output,
        ),
    }
}

fn report(
    status: CheckStatus,
    conclusion: Option<CheckConclusion>,
    details_url: Option<String>,
    output: CheckOutput,
) -> CheckReportInput {
    CheckReportInput {
        status: status_code(status),
        conclusion: conclusion.map(conclusion_code),
        details_url,
        output: output_record(output),
    }
}

fn output_record(output: CheckOutput) -> CheckOutputRecord {
    CheckOutputRecord {
        title: output.title,
        summary: output.summary,
        text: output.text,
        steps: output
            .steps
            .into_iter()
            .map(|step| CheckStepRecord {
                name: step.name,
                status: status_code(step.status),
                conclusion: step.conclusion.map(conclusion_code),
                log: step.log,
            })
            .collect(),
        annotations: output
            .annotations
            .into_iter()
            .map(|annotation| CheckAnnotationRecord {
                path: annotation.path,
                start_line: annotation.start_line,
                end_line: annotation.end_line,
                level: annotation_level_code(annotation.level),
                title: annotation.title,
                message: annotation.message,
            })
            .collect(),
    }
}

fn check_detail(detail: CheckRunDetail) -> Result<(CheckRun, CheckOutput)> {
    Ok((check_run(detail.run)?, check_output(detail.output)?))
}

fn check_run(record: CheckRunRecord) -> Result<CheckRun> {
    Ok(CheckRun {
        number: record.number,
        request_id: Uuid::from_bytes(record.create_submission_id).to_string(),
        author: Identity {
            issuer: record.author.issuer,
            subject: record.author.subject,
            name: record.author.name,
        },
        oid: record.oid,
        name: record.name,
        status: check_status(record.status)?,
        conclusion: record.conclusion.map(check_conclusion).transpose()?,
        details_url: record.details_url,
        output_title: record.output_title,
        version: record.version,
        started_at: record.started_at_ms,
        completed_at: record.completed_at_ms,
        created_at: record.created_at_ms,
        updated_at: record.updated_at_ms,
    })
}

fn check_output(output: CheckOutputRecord) -> Result<CheckOutput> {
    Ok(CheckOutput {
        title: output.title,
        summary: output.summary,
        text: output.text,
        steps: output
            .steps
            .into_iter()
            .map(|step| {
                Ok(CheckStep {
                    name: step.name,
                    status: check_status(step.status)?,
                    conclusion: step.conclusion.map(check_conclusion).transpose()?,
                    log: step.log,
                })
            })
            .collect::<Result<Vec<_>>>()?,
        annotations: output
            .annotations
            .into_iter()
            .map(|annotation| {
                Ok(CheckAnnotation {
                    path: annotation.path,
                    start_line: annotation.start_line,
                    end_line: annotation.end_line,
                    level: annotation_level(annotation.level)?,
                    title: annotation.title,
                    message: annotation.message,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    })
}

fn same_create(saved: &CheckRunDetail, actor: &Identity, input: &NewCheckRun) -> Result<bool> {
    let (run, output) = check_detail(saved.clone())?;
    Ok(same_author(&run.author, actor)
        && run.request_id == input.request_id
        && run.oid == input.head_sha
        && run.name == input.name
        && run.status == input.status
        && run.conclusion == input.conclusion
        && run.details_url == input.details_url
        && output == input.output)
}

fn same_update(
    saved: &CheckRunDetail,
    actor: &Identity,
    oid: &str,
    number: u64,
    input: &CheckEdit,
) -> Result<bool> {
    let (run, output) = check_detail(saved.clone())?;
    Ok(same_author(&run.author, actor)
        && run.oid == oid
        && run.number == number
        && input.version.checked_add(1) == Some(run.version)
        && run.status == input.status
        && run.conclusion == input.conclusion
        && run.details_url == input.details_url
        && output == input.output)
}

fn same_author(left: &Identity, right: &Identity) -> bool {
    left.issuer == right.issuer && left.subject == right.subject
}

fn status_code(status: CheckStatus) -> u8 {
    match status {
        CheckStatus::Queued => 0,
        CheckStatus::InProgress => 1,
        CheckStatus::Completed => 2,
    }
}

fn check_status(status: u8) -> Result<CheckStatus> {
    match status {
        0 => Ok(CheckStatus::Queued),
        1 => Ok(CheckStatus::InProgress),
        2 => Ok(CheckStatus::Completed),
        _ => Err(Error::CellContract("Cell returned an invalid check status")),
    }
}

fn conclusion_code(conclusion: CheckConclusion) -> u8 {
    match conclusion {
        CheckConclusion::ActionRequired => 0,
        CheckConclusion::Cancelled => 1,
        CheckConclusion::Failure => 2,
        CheckConclusion::Neutral => 3,
        CheckConclusion::Skipped => 4,
        CheckConclusion::Success => 5,
        CheckConclusion::TimedOut => 6,
    }
}

fn check_conclusion(conclusion: u8) -> Result<CheckConclusion> {
    match conclusion {
        0 => Ok(CheckConclusion::ActionRequired),
        1 => Ok(CheckConclusion::Cancelled),
        2 => Ok(CheckConclusion::Failure),
        3 => Ok(CheckConclusion::Neutral),
        4 => Ok(CheckConclusion::Skipped),
        5 => Ok(CheckConclusion::Success),
        6 => Ok(CheckConclusion::TimedOut),
        _ => Err(Error::CellContract(
            "Cell returned an invalid check conclusion",
        )),
    }
}

fn annotation_level_code(level: AnnotationLevel) -> u8 {
    match level {
        AnnotationLevel::Notice => 0,
        AnnotationLevel::Warning => 1,
        AnnotationLevel::Failure => 2,
    }
}

fn annotation_level(level: u8) -> Result<AnnotationLevel> {
    match level {
        0 => Ok(AnnotationLevel::Notice),
        1 => Ok(AnnotationLevel::Warning),
        2 => Ok(AnnotationLevel::Failure),
        _ => Err(Error::CellContract(
            "Cell returned an invalid annotation level",
        )),
    }
}

fn created_check(outcome: CreateCheckRunOutcome) -> Result<CheckRunDetail> {
    match outcome {
        CreateCheckRunOutcome::Created(detail) => Ok(*detail),
        CreateCheckRunOutcome::RequestConflict => Err(Error::RequestConflict),
        CreateCheckRunOutcome::RunLimit => {
            Err(Error::Invalid("A commit supports at most 100 check runs"))
        }
    }
}

fn updated_check(outcome: UpdateCheckRunOutcome) -> Result<CheckRunDetail> {
    match outcome {
        UpdateCheckRunOutcome::Updated(detail) => Ok(*detail),
        UpdateCheckRunOutcome::RequestConflict => Err(Error::RequestConflict),
        UpdateCheckRunOutcome::NotFound => Err(Error::CheckNotFound),
        UpdateCheckRunOutcome::Forbidden => Err(Error::Forbidden),
        UpdateCheckRunOutcome::Conflict => Err(Error::Conflict),
        UpdateCheckRunOutcome::InvalidTransition => Err(Error::Invalid(
            "Completed checks are immutable and in-progress checks cannot return to queued",
        )),
    }
}

async fn route(
    server: &Server,
    repository: &Repository,
    principal: &Identity,
    action: &'static str,
) -> Result<RepositoryCell> {
    let router = server.repository_cells().ok_or(Error::CellUnavailable)?;
    router
        .route(repository.id, principal, action)
        .await
        .map_err(|error| match error {
            crate::Error::Cell(source) => Error::Cell(source),
            source => Error::Repository(source),
        })
}

fn repository_author(identity: &Identity) -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: identity.issuer.clone(),
        subject: identity.subject.clone(),
        name: identity.name.clone(),
    }
}

fn submission_id(value: &str) -> Result<[u8; 16]> {
    Uuid::parse_str(&app::submission(value)?)
        .map(Uuid::into_bytes)
        .map_err(|_| Error::Invalid("Submission ID must be a UUID"))
}

fn mutation_identity() -> Result<MutationIdentity> {
    let now_ms = crate::cells::unix_now_ms().map_err(Error::Repository)?;
    let expires_at_ms = now_ms
        .checked_add(60_000)
        .ok_or(Error::CellContract("Cell request expiry overflowed"))?;
    Ok(MutationIdentity {
        request_id: RequestId::from_bytes(Uuid::now_v7().into_bytes()),
        issued_at_ms: now_ms,
        expires_at_ms,
    })
}

fn command_output<T>(result: std::result::Result<Committed<T>, InvocationError<T>>) -> Result<T> {
    match result {
        Ok(committed) => Ok(committed.output),
        Err(InvocationError::Rejected(committed)) => Ok(committed.output),
        Err(InvocationError::Pending(_)) => Err(Error::CellPending),
        Err(InvocationError::InvalidPublishedResult { source, .. }) => Err(Error::Cell(*source)),
        Err(InvocationError::NotStarted(source)) => Err(Error::Cell(source)),
    }
}

fn query_output<T>(result: std::result::Result<Observed<T>, InvocationError<T>>) -> Result<T> {
    match result {
        Ok(observed) => Ok(observed.output),
        Err(InvocationError::Rejected(_)) => Err(Error::CellContract(
            "Cell query returned a durable rejection",
        )),
        Err(InvocationError::Pending(_)) => Err(Error::CellContract(
            "Cell query returned pending mutation evidence",
        )),
        Err(InvocationError::InvalidPublishedResult { source, .. }) => Err(Error::Cell(*source)),
        Err(InvocationError::NotStarted(source)) => Err(Error::Cell(source)),
    }
}
