use std::sync::Arc;
use std::time::Instant;

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use crab_remote_git::{
    Blob, Commit, EntryKind, Error, GitPath, HistoryTraversal, OperationKind, PageCursor,
    PageRequest, RemoteGitRepository, RemoteGitSnapshot, Revision, RevisionError, TreeEntry,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{auth::Principal, server::Server};

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Action {
    Refs,
    Commit,
    Commits,
    Tree,
    Search,
    Blob,
    Asset,
    File,
    Changes,
    Diff,
    Blame,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Parameters {
    rev: Option<String>,
    base: Option<String>,
    path: Option<String>,
    path_hex: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
    q: Option<String>,
}

#[derive(Debug, thiserror::Error)]
enum ApiError {
    #[error("{0}")]
    Input(&'static str),
    #[error("remote Git operation failed")]
    Remote(#[from] Error),
    #[error("repository service failed")]
    Service(#[from] crate::Error),
    #[error("JSON response encoding failed")]
    Json(#[from] serde_json::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let Self::Service(crate::Error::Maintenance(error)) = &self {
            tracing::error!(error = ?error, "repository maintenance request failed");
        }
        if let Self::Service(crate::Error::Worker(error)) = &self {
            tracing::error!(error = %error, "repository maintenance task failed");
        }
        let (status, code, message) = match &self {
            Self::Input(message) => (StatusCode::BAD_REQUEST, "invalid_request", *message),
            Self::Remote(error) | Self::Service(crate::Error::Remote(error)) => match error {
                Error::EmptyRepository => (
                    StatusCode::NOT_FOUND,
                    "empty_repository",
                    "This repository has no commits yet",
                ),
                Error::PathNotFound
                | Error::SnapshotUnavailable
                | Error::Revision {
                    reason: RevisionError::NotFound | RevisionError::NotReachable,
                } => (
                    StatusCode::NOT_FOUND,
                    "not_found",
                    "The requested path or revision is not available",
                ),
                Error::InvalidCursor { .. } => (
                    StatusCode::CONFLICT,
                    "stale_cursor",
                    "This page has changed. Reload the first page to continue",
                ),
                Error::InvalidPath { .. }
                | Error::InvalidLimit { .. }
                | Error::Revision { .. }
                | Error::EntryNotBlob { .. }
                | Error::PathComponentNotTree { .. } => (
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "Invalid path, revision, or entry kind",
                ),
                Error::BlameUnsupported { .. } => (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "blame_unsupported",
                    "Blame requires an ordinary UTF-8 text file",
                ),
                Error::LimitExceeded { .. } => (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "read_limit",
                    "This request exceeds the repository read budget",
                ),
                Error::Timeout { .. } => (
                    StatusCode::GATEWAY_TIMEOUT,
                    "timeout",
                    "The repository read timed out. Try a smaller request",
                ),
                Error::Cancelled => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "cancelled",
                    "The read was cancelled",
                ),
                Error::RepositoryIndexing { .. } => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "indexing",
                    "Repository metadata is still being indexed. Retry shortly",
                ),
                _ => (
                    StatusCode::BAD_GATEWAY,
                    "remote_read",
                    "Repository data could not be read. Check storage and repository health",
                ),
            },
            Self::Service(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "indexing_failed",
                "Repository indexing could not finish. Check storage permissions and server logs, then retry",
            ),
            Self::Json(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "encoding",
                "The response could not be encoded",
            ),
        };
        (
            status,
            Json(json!({"error": {"code": code, "message": message}})),
        )
            .into_response()
    }
}

enum Payload {
    Json(Value),
    Blob(Blob),
    Asset(Blob),
}

pub(crate) async fn read(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, action)): Path<(String, String, Action)>,
    Query(params): Query<Parameters>,
) -> Response {
    let started = Instant::now();
    let Some(entry) = server
        .repositories
        .get(&(owner, name))
        .filter(|entry| principal.can_read(&entry.config))
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({"error": {"code":"repository_not_found", "message":"Repository not found"}}),
            ),
        )
            .into_response();
    };
    let Ok(_permit) = server.admission.try_acquire() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "1")],
            Json(json!({"error": {"code":"busy", "message":"The server is busy. Retry shortly"}})),
        )
            .into_response();
    };
    let cancellation = server.cancellation.child_token();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    let mut open_ms = 0.0;
    let mut generation = None;
    let result = async {
        let limit = params.limit.unwrap_or(100);
        if !(1..=200).contains(&limit) {
            return Err(ApiError::Input("limit must be between 1 and 200"));
        }
        let path = match (&params.path, &params.path_hex) {
            (Some(_), Some(_)) => return Err(ApiError::Input("use path or path_hex, not both")),
            (_, Some(value)) => GitPath::new(decode_hex(value)?)?,
            (Some(value), _) => GitPath::new(value.as_bytes().to_vec())?,
            _ => GitPath::root(),
        };
        if params.base.is_some() && !path.is_root() && matches!(action, Action::Commits) {
            return Err(ApiError::Input("base cannot be combined with path history"));
        }
        let cursor = params
            .cursor
            .as_deref()
            .map(|value| decode_cursor(&server.cursor_key, value))
            .transpose()?;
        let page = PageRequest::new(limit, cursor)?;
        let search_query = if matches!(action, Action::Search) {
            let query = params
                .q
                .as_deref()
                .map(str::trim)
                .filter(|query| !query.is_empty())
                .ok_or(ApiError::Input("q must contain a file name or path"))?;
            if query.chars().count() > 128 || query.chars().any(char::is_control) {
                return Err(ApiError::Input(
                    "q must contain at most 128 non-control characters",
                ));
            }
            Some(query)
        } else {
            None
        };
        let timer = Instant::now();
        let repository = entry.open(&server, &cancellation).await;
        open_ms = timer.elapsed().as_secs_f64() * 1000.0;
        let repository = repository?;
        generation = Some(repository.generation());
        let payload = execute(
            &server,
            &repository,
            action,
            &params,
            path,
            page,
            search_query,
            &cancellation,
        )
        .await?;
        match payload {
            Payload::Json(value) => {
                let bytes = serde_json::to_vec(&value)?;
                if bytes.len() > 8 * 1024 * 1024 {
                    return Err(ApiError::Remote(Error::LimitExceeded {
                        limit: "HTTP response bytes",
                        actual: bytes.len() as u64,
                        maximum: 8 * 1024 * 1024,
                    }));
                }
                Ok(([("content-type", "application/json")], bytes).into_response())
            }
            Payload::Blob(blob) => Ok((
                [
                    ("content-type", "application/octet-stream".to_owned()),
                    ("content-disposition", "attachment".to_owned()),
                    ("x-crab-blob-oid", blob.metadata.oid.to_string()),
                ],
                blob.bytes,
            )
                .into_response()),
            Payload::Asset(blob) => Ok(asset_response(blob)),
        }
    }
    .await;
    let response = match result {
        Ok(value) => value,
        Err(error) => error.into_response(),
    };
    let total = started.elapsed().as_secs_f64() * 1000.0;
    (
        [
            (
                "server-timing",
                format!(
                    "open;dur={open_ms:.3}, read;dur={:.3}, total;dur={total:.3}",
                    total - open_ms
                ),
            ),
            (
                "x-crab-generation",
                generation
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
            ),
        ],
        response,
    )
        .into_response()
}

async fn execute(
    server: &Server,
    repository: &RemoteGitRepository,
    action: Action,
    params: &Parameters,
    path: GitPath,
    page: PageRequest,
    search_query: Option<&str>,
    cancellation: &CancellationToken,
) -> crab_remote_git::Result<Payload> {
    if matches!(action, Action::Refs) {
        let refs = repository.refs();
        return Ok(Payload::Json(json!({
            "generation": repository.generation(), "packs": repository.pack_count(),
            "head": refs.head.as_ref().map(|head| json!({"name":head.name, "oid":head.target.to_string()})),
            "unborn_head": refs.unborn_head,
            "refs": refs.entries.iter().map(|entry| json!({"name":entry.name,"oid":entry.target.to_string(),"peeled":entry.peeled.map(|oid|oid.to_string())})).collect::<Vec<_>>(),
        })));
    }
    let revision = match params.rev.as_deref() {
        Some(value) => Revision::parse(value)?,
        None => Revision::Reference(
            repository
                .refs()
                .head
                .as_ref()
                .ok_or_else(|| {
                    if repository.refs().is_empty() {
                        Error::EmptyRepository
                    } else {
                        Error::Revision {
                            reason: RevisionError::NotFound,
                        }
                    }
                })?
                .name
                .clone(),
        ),
    };
    let path_history = !path.is_root() && matches!(action, Action::Commit | Action::Commits);
    let kind = match action {
        Action::Refs => OperationKind::Repository,
        Action::Commit if path_history => OperationKind::PathHistory,
        Action::Commit => OperationKind::Commit,
        Action::Commits if path_history => OperationKind::PathHistory,
        Action::Commits => OperationKind::History,
        Action::Tree | Action::Search => OperationKind::Tree,
        Action::Blob | Action::Asset | Action::File => OperationKind::Content,
        Action::Changes => OperationKind::Compare,
        Action::Diff => OperationKind::Diff,
        Action::Blame => OperationKind::Blame,
    };
    let operation = repository.operation(kind, cancellation).await?;
    let result = async {
        let snapshot = repository.snapshot(&revision, &operation).await?;
        let commit = snapshot.commit(&operation).await?;
        let mut value = match action {
            Action::Refs => commit_json(&commit),
            Action::Commit if path_history => {
                let result = snapshot
                    .path_history(
                        &path,
                        HistoryTraversal::FirstParent,
                        &PageRequest::new(1, None)?,
                        &operation,
                    )
                    .await?;
                let latest = result.items.first().ok_or(Error::PathNotFound)?;
                path_history_json(latest)
            }
            Action::Commit => commit_json(&commit),
            Action::Commits => {
                let (items, next) = if path_history {
                    let result = snapshot
                        .path_history(&path, HistoryTraversal::FirstParent, &page, &operation)
                        .await?;
                    (
                        result.items.iter().map(path_history_json).collect::<Vec<_>>(),
                        result.next,
                    )
                } else if let Some(base) = params.base.as_deref() {
                    let base = repository
                        .snapshot(&Revision::parse(base)?, &operation)
                        .await?;
                    let result = snapshot.ahead_history(&base, &page, &operation).await?;
                    (
                        result.items.iter().map(commit_json).collect::<Vec<_>>(),
                        result.next,
                    )
                } else {
                    let result = snapshot
                        .history(HistoryTraversal::FirstParent, &page, &operation)
                        .await?;
                    (
                        result.items.iter().map(commit_json).collect::<Vec<_>>(),
                        result.next,
                    )
                };
                json!({"items":items,"next":next.map(|cursor|encode_cursor(&server.cursor_key,cursor))})
            }
            Action::Tree => {
                let result = snapshot.list_directory(&path, &page, &operation).await?;
                json!({"items":result.items.iter().map(entry_json).collect::<Vec<_>>(), "next":result.next.map(|cursor|encode_cursor(&server.cursor_key,cursor))})
            }
            Action::Search => {
                let query = search_query.ok_or(Error::InternalInvariant {
                    invariant: "validated repository search had no query",
                })?;
                let mut matches = Vec::new();
                for entry in snapshot.list_tree_recursive(&operation).await? {
                    if entry.kind == EntryKind::Tree {
                        continue;
                    }
                    let Some(score) = search_score(&display_path(entry.path.as_bytes()), query)
                    else {
                        continue;
                    };
                    matches
                        .try_reserve(1)
                        .map_err(|source| Error::Allocation {
                            requested: std::mem::size_of::<((u8, usize), TreeEntry)>(),
                            source,
                        })?;
                    matches.push((score, entry));
                }
                matches.sort_by(|left, right| {
                    left.0.cmp(&right.0).then_with(|| {
                        left.1.path.as_bytes().cmp(right.1.path.as_bytes())
                    })
                });
                let truncated = matches.len() > page.limit();
                matches.truncate(page.limit());
                json!({"items":matches.iter().map(|(_,entry)|entry_json(entry)).collect::<Vec<_>>(),"truncated":truncated})
            }
            Action::Blob => return Ok(Payload::Blob(snapshot.read_blob(&path, &operation).await?)),
            Action::Asset => {
                return Ok(Payload::Asset(snapshot.read_blob(&path, &operation).await?));
            }
            Action::File => content_json(&snapshot.read_blob(&path, &operation).await?),
            Action::Changes | Action::Diff => {
                let base_revision = params.base.as_deref().map(Revision::parse).transpose()?
                    .or_else(|| commit.parents.first().copied().map(Revision::Commit));
                let base = match base_revision {
                    Some(revision) => Some(repository.snapshot(&revision, &operation).await?),
                    None => None,
                };
                if matches!(action, Action::Diff) {
                    let old = match &base { Some(base) => optional_blob(base, &path, &operation).await?, None => None };
                    let new = optional_blob(&snapshot, &path, &operation).await?;
                    if old.is_none() && new.is_none() { return Err(Error::PathNotFound); }
                    json!({"base":base.as_ref().map(|base|base.commit_oid().to_string()),"old":old.as_ref().map(content_json),"new":new.as_ref().map(content_json),"path":display_path(path.as_bytes()),"path_hex":encode_hex(path.as_bytes())})
                } else if let Some(base) = &base {
                    let result = snapshot.compare(base, &operation).await?;
                    json!({"base":result.base.to_string(),"changes":result.changes.iter().map(|change|json!({"path":display_path(change.path.as_bytes()),"path_hex":encode_hex(change.path.as_bytes()),"kind":format!("{:?}",change.kind),"old":change.old.as_ref().map(entry_json),"new":change.new.as_ref().map(entry_json)})).collect::<Vec<_>>()})
                } else {
                    let mut pending = vec![GitPath::root()];
                    let mut changes = Vec::new();
                    while let Some(directory) = pending.pop() {
                        let mut cursor = None;
                        loop {
                            let entries = snapshot.list_directory(&directory, &PageRequest::new(200,cursor)?, &operation).await?;
                            for entry in entries.items {
                                if entry.kind == EntryKind::Tree { pending.push(entry.path); }
                                else { changes.push(json!({"path":display_path(entry.path.as_bytes()),"path_hex":encode_hex(entry.path.as_bytes()),"kind":"Added","old":null,"new":entry_json(&entry)})); }
                            }
                            cursor = entries.next;
                            if cursor.is_none() { break; }
                        }
                    }
                    json!({"base":null,"changes":changes})
                }
            }
            Action::Blame => {
                let blame = snapshot.blame(&path, &operation).await?;
                json!({"ranges":blame.ranges.iter().map(|range|json!({"start":range.start,"lines":range.lines,"commit":commit_json(&range.commit)})).collect::<Vec<_>>()})
            }
        };
        value["commit"] = json!(snapshot.commit_oid().to_string());
        value["generation"] = json!(repository.generation());
        Ok(Payload::Json(value))
    }.await;
    operation.finish(result).await
}

async fn optional_blob(
    snapshot: &RemoteGitSnapshot,
    path: &GitPath,
    operation: &crab_remote_git::OperationContext,
) -> crab_remote_git::Result<Option<Blob>> {
    let Some(entry) = snapshot.entry(path, operation).await? else {
        return Ok(None);
    };
    if !matches!(entry.kind, EntryKind::Blob | EntryKind::Symlink) {
        return Ok(None);
    }
    snapshot.read_blob(path, operation).await.map(Some)
}

fn content_json(blob: &Blob) -> Value {
    let text = if blob.bytes.contains(&0) {
        None
    } else {
        std::str::from_utf8(&blob.bytes).ok()
    };
    json!({"oid":blob.metadata.oid.to_string(),"size":blob.bytes.len(),"mode":format!("{:06o}",blob.metadata.mode.raw()),"classification":format!("{:?}",blob.metadata.classification),"text":text})
}

fn image_content_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn asset_response(blob: Blob) -> Response {
    match image_content_type(&blob.bytes) {
        Some(content_type) => (
            [
                ("content-type", content_type.to_owned()),
                ("x-crab-blob-oid", blob.metadata.oid.to_string()),
            ],
            blob.bytes,
        )
            .into_response(),
        None => (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(json!({"error":{"code":"unsupported_asset","message":"Only PNG, JPEG, GIF and WebP blobs can be displayed inline"}})),
        )
            .into_response(),
    }
}

fn entry_json(entry: &TreeEntry) -> Value {
    json!({"path":display_path(entry.path.as_bytes()),"path_hex":encode_hex(entry.path.as_bytes()),"oid":entry.oid.to_string(),"mode":format!("{:06o}",entry.mode.raw()),"kind":format!("{:?}",entry.kind)})
}

fn commit_json(commit: &Commit) -> Value {
    json!({"oid":commit.oid.to_string(),"tree":commit.tree.to_string(),"parents":commit.parents.iter().map(ToString::to_string).collect::<Vec<_>>(),"author":String::from_utf8_lossy(&commit.author.name),"author_seconds":commit.author.seconds,"message":String::from_utf8_lossy(&commit.message),"message_hex":encode_hex(&commit.message)})
}

fn path_history_json(entry: &crab_remote_git::PathHistoryEntry) -> Value {
    let mut value = commit_json(&entry.commit);
    value["change_kind"] = json!(format!("{:?}", entry.kind));
    value
}

fn display_path(bytes: &[u8]) -> String {
    bytes
        .split(|byte| *byte == b'/')
        .map(|component| match std::str::from_utf8(component) {
            Ok(text) => text.replace('%', "%25"),
            Err(_) => component
                .iter()
                .map(|byte| format!("%{byte:02X}"))
                .collect(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn search_score(path: &str, query: &str) -> Option<(u8, usize)> {
    let path = path.to_lowercase();
    let query = query.to_lowercase();
    let name = path.rsplit('/').next().unwrap_or(&path);
    if name == query {
        return Some((0, path.len()));
    }
    if name.starts_with(&query) {
        return Some((1, name.len().saturating_sub(query.len())));
    }
    if path.starts_with(&query) {
        return Some((2, path.len().saturating_sub(query.len())));
    }
    if let Some(position) = name.find(&query) {
        return Some((3, position.saturating_add(name.len())));
    }
    if let Some(position) = path.find(&query) {
        return Some((4, position.saturating_add(path.len())));
    }
    fuzzy_cost(&path, &query).map(|cost| (5, cost.saturating_add(path.len())))
}

fn fuzzy_cost(path: &str, query: &str) -> Option<usize> {
    let mut cursor = 0;
    let mut cost = 0usize;
    for character in query.chars() {
        let offset = path[cursor..].find(character)?;
        cost = cost.saturating_add(offset);
        cursor = cursor
            .saturating_add(offset)
            .saturating_add(character.len_utf8());
    }
    Some(cost)
}

fn encode_cursor(key: &[u8; 32], cursor: PageCursor) -> String {
    format!(
        "{}.{}",
        encode_hex(cursor.as_bytes()),
        blake3::keyed_hash(key, cursor.as_bytes()).to_hex()
    )
}

fn decode_cursor(key: &[u8; 32], value: &str) -> std::result::Result<PageCursor, ApiError> {
    let (payload, signature) = value
        .split_once('.')
        .ok_or(ApiError::Input("Invalid cursor"))?;
    let bytes = decode_hex(payload)?;
    let signature = decode_hex(signature)?;
    if blake3::keyed_hash(key, &bytes) != *signature.as_slice() {
        return Err(ApiError::Input("Invalid cursor signature"));
    }
    Ok(PageCursor::from_bytes(bytes)?)
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn decode_hex(value: &str) -> std::result::Result<Vec<u8>, ApiError> {
    if !value.len().is_multiple_of(2)
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        || value.len() > 128 * 1024
    {
        return Err(ApiError::Input("Invalid hex bytes"));
    }
    (0..value.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&value[index..index + 2], 16)
                .map_err(|_| ApiError::Input("Invalid hex bytes"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_remote_git::{BlobMetadata, ContentClassification, EntryMode};
    use http_body_util::BodyExt;

    fn blob(bytes: &[u8]) -> Blob {
        Blob {
            metadata: BlobMetadata {
                oid: gix_hash::ObjectId::from_hex(b"1111111111111111111111111111111111111111")
                    .unwrap(),
                git_size: bytes.len() as u64,
                logical_size: Some(bytes.len() as u64),
                mode: EntryMode::Regular,
                kind: EntryKind::Blob,
                classification: ContentClassification::OrdinaryGit,
            },
            bytes: bytes.to_vec().into(),
        }
    }

    #[test]
    fn cursor_mac_rejects_tampering_and_other_server_keys() {
        // Version-one history payload: version, kind, SHA-1, traversal, skip.
        // Validate through the public decoder before exercising the HTTP envelope.
        let mut payload = vec![0; 31];
        payload[0] = 1;
        payload[1] = 2;
        payload[22] = HistoryTraversal::FirstParent as u8;
        let cursor = PageCursor::from_bytes(payload).unwrap();
        let signed = encode_cursor(&[7; 32], cursor.clone());
        assert_eq!(decode_cursor(&[7; 32], &signed).unwrap(), cursor);
        assert!(decode_cursor(&[8; 32], &signed).is_err());
        assert!(decode_cursor(&[7; 32], &signed.replacen("0102", "0103", 1)).is_err());
    }

    #[test]
    fn display_names_do_not_alias_invalid_utf8_and_literal_escapes() {
        assert_ne!(display_path(b"\xff"), display_path(b"%FF"));
        assert_eq!(display_path(b"dir/\xff"), "dir/%FF");
        assert_eq!(decode_hex(&encode_hex(b"dir/\xff")).unwrap(), b"dir/\xff");
    }

    #[test]
    fn file_search_prioritizes_names_then_paths_and_supports_fuzzy_input() {
        assert_eq!(search_score("src/Main.rs", "main.rs").unwrap().0, 0);
        assert!(
            search_score("src/main_test.rs", "main").unwrap()
                < search_score("main/examples.rs", "main").unwrap()
        );
        assert!(
            search_score("src/engine/chunk_file.rs", "engine").unwrap()
                < search_score("src/engine/chunk_file.rs", "cfr").unwrap()
        );
        assert!(search_score("src/engine/chunk_file.rs", "not-here").is_none());
    }

    #[test]
    fn inline_assets_require_a_supported_raster_signature() {
        assert_eq!(
            image_content_type(b"\x89PNG\r\n\x1a\nrest"),
            Some("image/png")
        );
        assert_eq!(image_content_type(b"\xff\xd8\xffrest"), Some("image/jpeg"));
        assert_eq!(image_content_type(b"GIF89arest"), Some("image/gif"));
        assert_eq!(image_content_type(b"RIFFsizeWEBPrest"), Some("image/webp"));
        assert_eq!(image_content_type(b"<svg><script/></svg>"), None);
        assert_eq!(image_content_type(b"<html>not an image</html>"), None);
    }

    #[tokio::test]
    async fn inline_asset_response_sets_safe_representation_headers() {
        let response = asset_response(blob(b"\x89PNG\r\n\x1a\nrest"));
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "image/png");
        assert!(!response.headers().contains_key("content-disposition"));
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            b"\x89PNG\r\n\x1a\nrest".as_slice()
        );

        let response = asset_response(blob(b"<svg><script/></svg>"));
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
}
