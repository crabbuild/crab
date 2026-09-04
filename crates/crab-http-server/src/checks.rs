use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    app::{self, Error, Result},
    app_storage,
    auth::{Identity, Principal},
    server::{Repository, Server},
    statuses,
};

const ROOT: &str = "app/v1/check-runs";
const MAX_RUNS: usize = 100;
const MAX_STEPS: usize = 50;
const MAX_ANNOTATIONS: usize = 50;
const MAX_OUTPUT_BYTES: usize = 192 * 1024;

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
    output_version: u64,
    pub started_at: Option<u64>,
    pub completed_at: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    oid: String,
    runs: Vec<CheckRun>,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CreateReservation {
    author: Identity,
    input: NewCheckRun,
    run: CheckRun,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UpdateReservation {
    author: Identity,
    oid: String,
    number: u64,
    input: CheckEdit,
    run: CheckRun,
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

fn catalog_path(oid: &str) -> String {
    format!("{ROOT}/commits/{oid}/catalog.json")
}

fn output_path(oid: &str, number: u64, version: u64) -> String {
    format!("{ROOT}/commits/{oid}/outputs/{number:016}/{version:016}.json")
}

fn create_request_path(request_id: &str) -> String {
    format!("{ROOT}/create-requests/{request_id}.json")
}

fn update_request_path(request_id: &str) -> String {
    format!("{ROOT}/update-requests/{request_id}.json")
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
            || annotation.end_line > app_storage::MAX_NUMBER
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

fn same_run(left: &CheckRun, right: &CheckRun) -> bool {
    left.number == right.number
        && left.request_id == right.request_id
        && app_storage::same_author(&left.author, &right.author)
        && left.oid == right.oid
        && left.name == right.name
        && left.status == right.status
        && left.conclusion == right.conclusion
        && left.details_url == right.details_url
        && left.output_title == right.output_title
        && left.version == right.version
        && left.output_version == right.output_version
        && left.started_at == right.started_at
        && left.completed_at == right.completed_at
        && left.created_at == right.created_at
        && left.updated_at == right.updated_at
}

fn same_create(saved: &CreateReservation, author: &Identity, input: &NewCheckRun) -> bool {
    app_storage::same_author(&saved.author, author) && saved.input == *input
}

fn same_update(
    saved: &UpdateReservation,
    author: &Identity,
    oid: &str,
    number: u64,
    input: &CheckEdit,
) -> bool {
    app_storage::same_author(&saved.author, author)
        && saved.oid == oid
        && saved.number == number
        && saved.input == *input
}

async fn output(repo: &Repository, run: &CheckRun) -> Result<CheckOutput> {
    app_storage::read(repo, &output_path(&run.oid, run.number, run.output_version))
        .await?
        .map(|(output, _)| output)
        .ok_or(Error::CheckNotFound)
}

async fn store_output(repo: &Repository, run: &CheckRun, expected: CheckOutput) -> Result<()> {
    let stored = app_storage::create_or_read(
        repo,
        &output_path(&run.oid, run.number, run.output_version),
        expected.clone(),
    )
    .await?;
    if stored != expected {
        return Err(Error::Conflict);
    }
    Ok(())
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

async fn detail_view(repo: &Repository, run: &CheckRun) -> Result<Value> {
    let mut value = run_view(run);
    value["output"] = serde_json::to_value(output(repo, run).await?)?;
    Ok(value)
}

async fn catalog(repo: &Repository, oid: &str) -> Result<Option<Catalog>> {
    let value = app_storage::read::<Catalog>(repo, &catalog_path(oid)).await?;
    match value {
        Some((catalog, _)) if catalog.oid == oid => Ok(Some(catalog)),
        Some(_) => Err(Error::Conflict),
        None => Ok(None),
    }
}

async fn publish_create(repo: &Repository, proposed: &CheckRun) -> Result<CheckRun> {
    let path = catalog_path(&proposed.oid);
    for _ in 0..10 {
        let Some((mut catalog, etag)) = app_storage::read::<Catalog>(repo, &path).await? else {
            let created = app_storage::create_or_read(
                repo,
                &path,
                Catalog {
                    oid: proposed.oid.clone(),
                    runs: vec![proposed.clone()],
                },
            )
            .await?;
            if created.oid != proposed.oid {
                return Err(Error::Conflict);
            }
            if let Some(run) = created
                .runs
                .iter()
                .find(|run| run.number == proposed.number)
            {
                return Ok(run.clone());
            }
            continue;
        };
        if catalog.oid != proposed.oid {
            return Err(Error::Conflict);
        }
        if let Some(run) = catalog
            .runs
            .iter()
            .find(|run| run.number == proposed.number)
        {
            return Ok(run.clone());
        }
        if catalog.runs.len() >= MAX_RUNS {
            return Err(Error::Invalid("A commit supports at most 100 check runs"));
        }
        catalog.runs.push(proposed.clone());
        catalog.runs.sort_by_key(|run| run.number);
        match app_storage::update(repo, &path, &catalog, etag).await {
            Ok(()) => return Ok(proposed.clone()),
            Err(Error::Storage(crab_storage::StorageError::StateConflict { .. })) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(Error::Conflict)
}

async fn publish_update(repo: &Repository, proposed: &CheckRun) -> Result<CheckRun> {
    let path = catalog_path(&proposed.oid);
    for _ in 0..10 {
        let Some((mut catalog, etag)) = app_storage::read::<Catalog>(repo, &path).await? else {
            return Err(Error::CheckNotFound);
        };
        if catalog.oid != proposed.oid {
            return Err(Error::Conflict);
        }
        let current = catalog
            .runs
            .iter_mut()
            .find(|run| run.number == proposed.number)
            .ok_or(Error::CheckNotFound)?;
        if current.version > proposed.version {
            return Ok(proposed.clone());
        }
        if current.version == proposed.version {
            return if same_run(current, proposed) {
                Ok(current.clone())
            } else {
                Err(Error::Conflict)
            };
        }
        if current.version.checked_add(1) != Some(proposed.version) {
            return Err(Error::Conflict);
        }
        *current = proposed.clone();
        match app_storage::update(repo, &path, &catalog, etag).await {
            Ok(()) => return Ok(proposed.clone()),
            Err(Error::Storage(crab_storage::StorageError::StateConflict { .. })) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(Error::Conflict)
}

fn transition(current: CheckStatus, next: CheckStatus) -> Result<()> {
    if current == CheckStatus::Completed
        || (current == CheckStatus::InProgress && next == CheckStatus::Queued)
    {
        return Err(Error::Invalid(
            "Completed checks are immutable and in-progress checks cannot return to queued",
        ));
    }
    Ok(())
}

async fn create(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    input: std::result::Result<Json<NewCheckRun>, JsonRejection>,
) -> Result<impl IntoResponse> {
    let repo = app::repository(&server, &principal, &key)?;
    if !principal.can_write(&repo.config) {
        return Err(Error::CheckPermission);
    }
    let Json(mut input) = input?;
    let request_id = app::submission(&input.request_id)?;
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
    let oid =
        statuses::require_commit(&server, repo, statuses::parse_oid(&input.head_sha)?).await?;
    input.request_id = request_id.clone();
    input.head_sha = oid.clone();
    let author = app::actor(&principal)?;
    let reservation_path = create_request_path(&request_id);
    let saved = match app_storage::read::<CreateReservation>(repo, &reservation_path).await? {
        Some((saved, _)) => saved,
        None => {
            let number = app_storage::reserve_number(repo, ROOT).await?;
            let timestamp = app_storage::now()?;
            let run = CheckRun {
                number,
                request_id: request_id.clone(),
                author: author.clone(),
                oid,
                name: input.name.clone(),
                status: input.status,
                conclusion: input.conclusion,
                details_url: input.details_url.clone(),
                output_title: input.output.title.clone(),
                version: 1,
                output_version: 1,
                started_at: (input.status != CheckStatus::Queued).then_some(timestamp),
                completed_at: (input.status == CheckStatus::Completed).then_some(timestamp),
                created_at: timestamp,
                updated_at: timestamp,
            };
            app_storage::create_or_read(
                repo,
                &reservation_path,
                CreateReservation {
                    author: author.clone(),
                    input: input.clone(),
                    run,
                },
            )
            .await?
        }
    };
    if !same_create(&saved, &author, &input) {
        return Err(Error::RequestConflict);
    }
    if !principal.can_write(&repo.config) {
        return Err(Error::CheckPermission);
    }
    store_output(repo, &saved.run, saved.input.output.clone()).await?;
    let current = publish_create(repo, &saved.run).await?;
    Ok((
        StatusCode::CREATED,
        Json(detail_view(repo, &current).await?),
    ))
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, oid)): Path<(String, String, String)>,
    Query(params): Query<ListParams>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let oid = statuses::require_commit(&server, repo, statuses::parse_oid(&oid)?).await?;
    let limit = params.limit.unwrap_or(30);
    if !(1..=50).contains(&limit) || params.before == Some(0) {
        return Err(Error::Invalid(
            "Check run pages require limit 1–50 and a positive before cursor",
        ));
    }
    let mut runs = catalog(repo, &oid)
        .await?
        .map_or_else(Vec::new, |value| value.runs);
    runs.sort_by_key(|run| std::cmp::Reverse(run.number));
    let mut runs = runs
        .into_iter()
        .filter(|run| params.before.is_none_or(|before| run.number < before))
        .take(limit + 1)
        .collect::<Vec<_>>();
    let next = (runs.len() > limit).then(|| runs[limit - 1].number);
    runs.truncate(limit);
    Ok(Json(json!({
        "sha":oid,
        "items":runs.iter().map(run_view).collect::<Vec<_>>(),
        "next":next,
    })))
}

async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, oid, number)): Path<(String, String, String, u64)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let oid = statuses::require_commit(&server, repo, statuses::parse_oid(&oid)?).await?;
    let number = app::number(number)?;
    let run = catalog(repo, &oid)
        .await?
        .and_then(|catalog| catalog.runs.into_iter().find(|run| run.number == number))
        .ok_or(Error::CheckNotFound)?;
    Ok(Json(detail_view(repo, &run).await?))
}

async fn edit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, oid, number)): Path<(String, String, String, u64)>,
    input: std::result::Result<Json<CheckEdit>, JsonRejection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    if !principal.can_write(&repo.config) {
        return Err(Error::CheckPermission);
    }
    let oid = statuses::require_commit(&server, repo, statuses::parse_oid(&oid)?).await?;
    let number = app::number(number)?;
    let Json(mut input) = input?;
    input.request_id = app::submission(&input.request_id)?;
    validate_report(
        input.status,
        input.conclusion,
        input.details_url.as_deref(),
        &input.output,
    )?;
    let author = app::actor(&principal)?;
    let request_path = update_request_path(&input.request_id);
    let saved = match app_storage::read::<UpdateReservation>(repo, &request_path).await? {
        Some((saved, _)) => saved,
        None => {
            let catalog = catalog(repo, &oid).await?.ok_or(Error::CheckNotFound)?;
            let current = catalog
                .runs
                .iter()
                .find(|run| run.number == number)
                .ok_or(Error::CheckNotFound)?;
            if !app_storage::same_author(&current.author, &author) {
                return Err(Error::Forbidden);
            }
            if current.version != input.version {
                return Err(Error::Conflict);
            }
            transition(current.status, input.status)?;
            let timestamp = app_storage::now()?;
            let run = CheckRun {
                number,
                request_id: current.request_id.clone(),
                author: current.author.clone(),
                oid: oid.clone(),
                name: current.name.clone(),
                status: input.status,
                conclusion: input.conclusion,
                details_url: input.details_url.clone(),
                output_title: input.output.title.clone(),
                version: current.version.checked_add(1).ok_or(Error::Conflict)?,
                output_version: current.version.checked_add(1).ok_or(Error::Conflict)?,
                started_at: current
                    .started_at
                    .or((input.status != CheckStatus::Queued).then_some(timestamp)),
                completed_at: (input.status == CheckStatus::Completed).then_some(timestamp),
                created_at: current.created_at,
                updated_at: timestamp,
            };
            app_storage::create_or_read(
                repo,
                &request_path,
                UpdateReservation {
                    author: author.clone(),
                    oid: oid.clone(),
                    number,
                    input: input.clone(),
                    run,
                },
            )
            .await?
        }
    };
    if !same_update(&saved, &author, &oid, number, &input) {
        return Err(Error::RequestConflict);
    }
    if !principal.can_write(&repo.config) {
        return Err(Error::CheckPermission);
    }
    store_output(repo, &saved.run, saved.input.output.clone()).await?;
    let run = publish_update(repo, &saved.run).await?;
    Ok(Json(detail_view(repo, &run).await?))
}

pub(crate) async fn latest(repo: &Repository, oid: &str) -> Result<Vec<CheckRun>> {
    let Some(catalog) = catalog(repo, oid).await? else {
        return Ok(vec![]);
    };
    let mut latest = Vec::<CheckRun>::new();
    for run in catalog.runs.into_iter().rev() {
        if latest
            .iter()
            .all(|current| !statuses::same_context(&current.name, &run.name))
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
