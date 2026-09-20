use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use crab_remote_git::{GitPath, RemoteGitRepository, RepositoryOptions};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{current_branches, review_thread_storage as thread_storage, storage as pull_storage};
use crate::{
    api,
    app::{self, Error, Result},
    auth::{Identity, Principal},
    cells::repository::{PullReviewThreadListInput, PullReviewThreadSide},
    server::{Repository, Server},
};

const MAX_NUMBER: u64 = 9_007_199_254_740_991;
const MAX_LINE_RANGE: u64 = 200;

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/threads",
            get(list).post(create),
        )
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/threads/{thread}",
            get(detail).patch(edit),
        )
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/threads/{thread}/replies",
            get(list_replies).post(create_reply),
        )
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/threads/{thread}/replies/{reply}",
            get(reply_detail).patch(edit_reply),
        )
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ListParameters {
    before: Option<u64>,
    limit: Option<usize>,
    path_hex: Option<String>,
    resolved: Option<bool>,
    outdated: Option<bool>,
}

impl ListParameters {
    fn limit(&self) -> Result<u8> {
        let limit = self.limit.unwrap_or(30);
        if !(1..=50).contains(&limit)
            || self
                .before
                .is_some_and(|value| value == 0 || value > MAX_NUMBER)
        {
            return Err(Error::Invalid("Invalid review thread page"));
        }
        u8::try_from(limit).map_err(|_| Error::Invalid("Invalid review thread page"))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewThread {
    request_id: String,
    body: String,
    suggested_text: Option<String>,
    base_oid: String,
    head_oid: String,
    path_hex: String,
    side: PullReviewThreadSide,
    start_line: u64,
    end_line: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreadEdit {
    version: u64,
    body: Option<String>,
    suggested_text: Option<Option<String>>,
    resolved: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewReply {
    request_id: String,
    body: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyEdit {
    version: u64,
    body: String,
}

struct Comparison {
    base_oid: String,
    head_oid: String,
    repository: Option<RemoteGitRepository>,
}

async fn load_pull(
    server: &Server,
    repo: &Repository,
    actor: &Identity,
    number: u64,
) -> Result<pull_storage::PullRequest> {
    pull_storage::pull(server, repo, actor, number)
        .await?
        .ok_or(Error::NotFound)
}

async fn comparison(
    server: &Server,
    repo: &Repository,
    pull: &pull_storage::PullRequest,
) -> Result<Comparison> {
    let repository = repo
        .open_current(server, RepositoryOptions::default(), &server.cancellation)
        .await
        .ok();
    let (base_oid, head_oid) = pull
        .merge
        .as_ref()
        .map(|merge| (merge.base_oid.clone(), merge.head_oid.clone()))
        .or_else(|| {
            current_branches(repository.as_ref()?, pull)
                .or_else(|| Some((pull.base_oid.clone(), pull.head_oid.clone())))
        })
        .ok_or(Error::Invalid("Pull request comparison is unavailable"))?;
    Ok(Comparison {
        base_oid,
        head_oid,
        repository,
    })
}

fn parse_path(value: &str) -> Result<(Vec<u8>, GitPath)> {
    if value.is_empty()
        || value.len() > 2 * 1024
        || !value.len().is_multiple_of(2)
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(Error::Invalid("Invalid review path"));
    }
    let bytes = (0..value.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&value[index..index + 2], 16)
                .map_err(|_| Error::Invalid("Invalid review path"))
        })
        .collect::<Result<Vec<_>>>()?;
    let path = GitPath::new(bytes.clone()).map_err(|_| Error::Invalid("Invalid review path"))?;
    if path.is_root() {
        return Err(Error::Invalid("Review comments require a file path"));
    }
    Ok((bytes, path))
}

fn path_hex(path: &[u8]) -> String {
    api::encode_hex(path)
}

fn validate_text(value: &str, required: bool) -> Result<()> {
    app::body(value, required)?;
    if value.contains('\0') {
        return Err(Error::Invalid(
            "Review text must be valid UTF-8 without NUL characters",
        ));
    }
    Ok(())
}

fn line_count(blob: &crab_remote_git::Blob) -> Result<u64> {
    if blob.bytes.contains(&0) {
        return Err(Error::Invalid("Review comments require a UTF-8 text file"));
    }
    let text = std::str::from_utf8(&blob.bytes)
        .map_err(|_| Error::Invalid("Review comments require a UTF-8 text file"))?;
    if blob.bytes.len() > 1024 * 1024 {
        return Err(Error::Invalid(
            "Review comments require a file of at most 1 MiB",
        ));
    }
    let lines = text.lines().count();
    u64::try_from(lines).map_err(|_| Error::Invalid("Review line range is too large"))
}

fn validate_range(start_line: u64, end_line: u64, blob: &crab_remote_git::Blob) -> Result<()> {
    if start_line == 0
        || end_line < start_line
        || end_line - start_line >= MAX_LINE_RANGE
        || end_line > line_count(blob)?
    {
        return Err(Error::Invalid(
            "Review line range is outside the selected file",
        ));
    }
    Ok(())
}

fn blob_oid(blob: Option<&crab_remote_git::Blob>) -> Option<String> {
    blob.map(|blob| blob.metadata.oid.to_string())
}

fn comparison_status(
    thread: &thread_storage::PullReviewThread,
    comparison: &Comparison,
    old_blob_oid: Option<&str>,
    new_blob_oid: Option<&str>,
    anchors_checked: bool,
) -> (bool, bool) {
    let anchor_matches = !anchors_checked
        || match thread.side {
            PullReviewThreadSide::Old => thread.old_blob_oid.as_deref() == old_blob_oid,
            PullReviewThreadSide::New => thread.new_blob_oid.as_deref() == new_blob_oid,
        };
    let outdated = thread.base_oid != comparison.base_oid
        || thread.head_oid != comparison.head_oid
        || !anchor_matches;
    let vanished = anchors_checked
        && match thread.side {
            PullReviewThreadSide::Old => old_blob_oid.is_none(),
            PullReviewThreadSide::New => new_blob_oid.is_none(),
        };
    (outdated, vanished)
}

fn thread_view(
    thread: &thread_storage::PullReviewThread,
    actor: &Identity,
    can_reply: bool,
    can_resolve: bool,
    comparison: &Comparison,
    old_blob_oid: Option<&str>,
    new_blob_oid: Option<&str>,
    anchors_checked: bool,
) -> Value {
    let (outdated, vanished) = comparison_status(
        thread,
        comparison,
        old_blob_oid,
        new_blob_oid,
        anchors_checked,
    );
    json!({
        "number": thread.number,
        "author": thread.author.name,
        "body": thread.body,
        "suggested_text": thread.suggested_text,
        "base_oid": thread.base_oid,
        "head_oid": thread.head_oid,
        "path": api::display_path(&thread.path),
        "path_hex": path_hex(&thread.path),
        "old_blob_oid": thread.old_blob_oid,
        "new_blob_oid": thread.new_blob_oid,
        "side": thread.side,
        "start_line": thread.start_line,
        "end_line": thread.end_line,
        "resolved": thread.resolved,
        "resolved_by": thread.resolved_by.as_ref().map(|author| author.name.clone()),
        "resolved_at": thread.resolved_at,
        "outdated": outdated,
        "vanished": vanished,
        "current": !outdated,
        "version": thread.version,
        "created_at": thread.created_at,
        "updated_at": thread.updated_at,
        "can_edit": same_author(&thread.author, actor),
        "can_reply": can_reply,
        "can_resolve": can_resolve,
    })
}

fn reply_view(reply: &thread_storage::PullReviewReply, actor: &Identity) -> Value {
    json!({
        "number": reply.number,
        "author": reply.author.name,
        "body": reply.body,
        "version": reply.version,
        "created_at": reply.created_at,
        "updated_at": reply.updated_at,
        "can_edit": same_author(&reply.author, actor),
    })
}

fn same_author(left: &Identity, right: &Identity) -> bool {
    left.issuer == right.issuer && left.subject == right.subject
}

async fn current_blobs(
    server: &Server,
    comparison: &Comparison,
    path: &GitPath,
) -> Result<(Option<String>, Option<String>)> {
    let Some(repository) = comparison.repository.as_ref() else {
        return Ok((None, None));
    };
    let blobs = api::comparison_blobs(
        repository,
        &comparison.base_oid,
        &comparison.head_oid,
        path,
        &server.cancellation,
    )
    .await
    .map_err(|error| Error::Repository(crate::Error::Remote(error)))?;
    Ok((blob_oid(blobs.old.as_ref()), blob_oid(blobs.new.as_ref())))
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, u64)>,
    Query(params): Query<ListParameters>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let actor = app::actor(&principal)?;
    let pull = load_pull(&server, repo, &actor, app::number(id)?).await?;
    let comparison = comparison(&server, repo, &pull).await?;
    let path = params
        .path_hex
        .as_deref()
        .map(parse_path)
        .transpose()?
        .map(|(bytes, _)| bytes);
    let page = thread_storage::threads(
        &server,
        repo,
        &actor,
        PullReviewThreadListInput {
            pull: pull.number,
            before: params.before,
            limit: params.limit()?,
            path,
            resolved: params.resolved,
            comparison_base_oid: Some(comparison.base_oid.clone()),
            comparison_head_oid: Some(comparison.head_oid.clone()),
            outdated: params.outdated,
        },
    )
    .await?
    .ok_or(Error::NotFound)?;
    let can_resolve = principal.can_write(&repo.config) || same_author(&pull.author, &actor);
    let can_reply = principal.can_read(&repo.config);
    let items = page
        .0
        .iter()
        .map(|thread| {
            thread_view(
                thread,
                &actor,
                can_reply,
                can_resolve,
                &comparison,
                None,
                None,
                false,
            )
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"items": items, "next": page.1})))
}

async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id, thread)): Path<(String, String, u64, u64)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let actor = app::actor(&principal)?;
    let pull = load_pull(&server, repo, &actor, app::number(id)?).await?;
    let comparison = comparison(&server, repo, &pull).await?;
    let thread = thread_storage::thread(&server, repo, &actor, pull.number, app::number(thread)?)
        .await?
        .ok_or(Error::NotFound)?;
    let (_, path) = parse_path(&path_hex(&thread.path))?;
    let (old_blob_oid, new_blob_oid) = current_blobs(&server, &comparison, &path).await?;
    let can_resolve = principal.can_write(&repo.config) || same_author(&pull.author, &actor);
    let can_reply = principal.can_read(&repo.config);
    Ok(Json(thread_view(
        &thread,
        &actor,
        can_reply,
        can_resolve,
        &comparison,
        old_blob_oid.as_deref(),
        new_blob_oid.as_deref(),
        comparison.repository.is_some(),
    )))
}

async fn create(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, u64)>,
    input: std::result::Result<Json<NewThread>, JsonRejection>,
) -> Result<impl IntoResponse> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let Json(input) = input?;
    let actor = app::actor(&principal)?;
    let pull = load_pull(&server, repo, &actor, app::number(id)?).await?;
    if pull.state != pull_storage::PullState::Open {
        return Err(Error::Invalid("Closed pull requests cannot be reviewed"));
    }
    validate_text(&input.body, true)?;
    if let Some(suggested_text) = &input.suggested_text {
        validate_text(suggested_text, false)?;
    }
    let request_id = app::submission(&input.request_id)?;
    if let Some(existing) =
        thread_storage::thread_submission(&server, repo, &actor, pull.number, &request_id).await?
    {
        if !same_author(&existing.author, &actor)
            || existing.body != input.body
            || existing.suggested_text != input.suggested_text
            || existing.base_oid != input.base_oid
            || existing.head_oid != input.head_oid
            || existing.path != parse_path(&input.path_hex)?.0
            || existing.side != input.side
            || existing.start_line != input.start_line
            || existing.end_line != input.end_line
        {
            return Err(Error::RequestConflict);
        }
        let comparison = comparison(&server, repo, &pull).await?;
        return Ok((
            StatusCode::CREATED,
            Json(thread_view(
                &existing,
                &actor,
                principal.can_read(&repo.config),
                principal.can_write(&repo.config) || same_author(&pull.author, &actor),
                &comparison,
                None,
                None,
                false,
            )),
        ));
    }
    let comparison = comparison(&server, repo, &pull).await?;
    if input.base_oid != comparison.base_oid || input.head_oid != comparison.head_oid {
        return Err(Error::Conflict);
    }
    let (path_bytes, path) = parse_path(&input.path_hex)?;
    let Some(repository) = comparison.repository.as_ref() else {
        return Err(Error::Invalid(
            "Pull request branches must exist before commenting",
        ));
    };
    let blobs = api::comparison_blobs(
        repository,
        &comparison.base_oid,
        &comparison.head_oid,
        &path,
        &server.cancellation,
    )
    .await
    .map_err(|error| Error::Repository(crate::Error::Remote(error)))?;
    if !blobs.changed {
        return Err(Error::Invalid("Review comments must target a changed file"));
    }
    let anchor = match input.side {
        PullReviewThreadSide::Old => blobs.old.as_ref().ok_or(Error::Invalid(
            "The selected old-side line no longer exists",
        ))?,
        PullReviewThreadSide::New => blobs.new.as_ref().ok_or(Error::Invalid(
            "The selected new-side line no longer exists",
        ))?,
    };
    validate_range(input.start_line, input.end_line, anchor)?;
    if input.side == PullReviewThreadSide::Old && input.suggested_text.is_some() {
        return Err(Error::Invalid("Suggestions can only target added lines"));
    }
    let thread = thread_storage::create_thread(
        &server,
        repo,
        pull.number,
        thread_storage::NewPullReviewThread {
            author: actor.clone(),
            request_id,
            body: input.body,
            suggested_text: input.suggested_text,
            base_oid: comparison.base_oid.clone(),
            head_oid: comparison.head_oid.clone(),
            path: path_bytes,
            old_blob_oid: blob_oid(blobs.old.as_ref()),
            new_blob_oid: blob_oid(blobs.new.as_ref()),
            side: input.side,
            start_line: input.start_line,
            end_line: input.end_line,
        },
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(thread_view(
            &thread,
            &actor,
            principal.can_read(&repo.config),
            principal.can_write(&repo.config) || same_author(&pull.author, &actor),
            &comparison,
            blob_oid(blobs.old.as_ref()).as_deref(),
            blob_oid(blobs.new.as_ref()).as_deref(),
            true,
        )),
    ))
}

async fn edit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id, thread)): Path<(String, String, u64, u64)>,
    input: std::result::Result<Json<ThreadEdit>, JsonRejection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let Json(input) = input?;
    if input.body.is_none() && input.suggested_text.is_none() && input.resolved.is_none() {
        return Err(Error::Invalid("A review thread update cannot be empty"));
    }
    let actor = app::actor(&principal)?;
    if let Some(body) = &input.body {
        validate_text(body, true)?;
    }
    if let Some(Some(suggested_text)) = &input.suggested_text {
        validate_text(suggested_text, false)?;
    }
    let pull = load_pull(&server, repo, &actor, app::number(id)?).await?;
    let number = app::number(thread)?;
    let updated = thread_storage::update_thread(
        &server,
        repo,
        pull.number,
        number,
        thread_storage::PullReviewThreadEdit {
            actor: actor.clone(),
            can_resolve: principal.can_write(&repo.config) || same_author(&pull.author, &actor),
            version: input.version,
            body: input.body,
            suggested_text: input.suggested_text,
            resolved: input.resolved,
        },
    )
    .await?;
    let comparison = comparison(&server, repo, &pull).await?;
    Ok(Json(thread_view(
        &updated,
        &actor,
        principal.can_read(&repo.config),
        principal.can_write(&repo.config) || same_author(&pull.author, &actor),
        &comparison,
        None,
        None,
        false,
    )))
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ReplyListParameters {
    before: Option<u64>,
    limit: Option<usize>,
}

impl ReplyListParameters {
    fn limit(&self) -> Result<u8> {
        let limit = self.limit.unwrap_or(50);
        if !(1..=50).contains(&limit)
            || self
                .before
                .is_some_and(|value| value == 0 || value > MAX_NUMBER)
        {
            return Err(Error::Invalid("Invalid review reply page"));
        }
        u8::try_from(limit).map_err(|_| Error::Invalid("Invalid review reply page"))
    }
}

async fn list_replies(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id, thread)): Path<(String, String, u64, u64)>,
    Query(params): Query<ReplyListParameters>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let actor = app::actor(&principal)?;
    let pull = load_pull(&server, repo, &actor, app::number(id)?).await?;
    let page = thread_storage::replies(
        &server,
        repo,
        &actor,
        crate::cells::repository::PullReviewReplyListInput {
            pull: pull.number,
            thread: app::number(thread)?,
            before: params.before,
            limit: params.limit()?,
        },
    )
    .await?
    .ok_or(Error::NotFound)?;
    Ok(Json(json!({
        "items": page.0.iter().map(|reply| reply_view(reply, &actor)).collect::<Vec<_>>(),
        "next": page.1,
    })))
}

async fn create_reply(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id, thread)): Path<(String, String, u64, u64)>,
    input: std::result::Result<Json<NewReply>, JsonRejection>,
) -> Result<impl IntoResponse> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let Json(input) = input?;
    validate_text(&input.body, true)?;
    let actor = app::actor(&principal)?;
    let pull = load_pull(&server, repo, &actor, app::number(id)?).await?;
    let thread = app::number(thread)?;
    let request_id = app::submission(&input.request_id)?;
    if let Some(existing) =
        thread_storage::reply_submission(&server, repo, &actor, pull.number, thread, &request_id)
            .await?
    {
        if !same_author(&existing.author, &actor) || existing.body != input.body {
            return Err(Error::RequestConflict);
        }
        return Ok((StatusCode::CREATED, Json(reply_view(&existing, &actor))));
    }
    let reply = thread_storage::create_reply(
        &server,
        repo,
        pull.number,
        thread,
        thread_storage::NewPullReviewReply {
            author: actor.clone(),
            request_id,
            body: input.body,
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(reply_view(&reply, &actor))))
}

async fn reply_detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id, thread, reply)): Path<(String, String, u64, u64, u64)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let actor = app::actor(&principal)?;
    let pull = load_pull(&server, repo, &actor, app::number(id)?).await?;
    let reply = thread_storage::reply(
        &server,
        repo,
        &actor,
        pull.number,
        app::number(thread)?,
        app::number(reply)?,
    )
    .await?
    .ok_or(Error::NotFound)?;
    Ok(Json(reply_view(&reply, &actor)))
}

async fn edit_reply(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id, thread, reply)): Path<(String, String, u64, u64, u64)>,
    input: std::result::Result<Json<ReplyEdit>, JsonRejection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let Json(input) = input?;
    validate_text(&input.body, true)?;
    let actor = app::actor(&principal)?;
    let pull = load_pull(&server, repo, &actor, app::number(id)?).await?;
    let reply = thread_storage::update_reply(
        &server,
        repo,
        pull.number,
        app::number(thread)?,
        app::number(reply)?,
        actor.clone(),
        input.version,
        input.body,
    )
    .await?;
    Ok(Json(reply_view(&reply, &actor)))
}
