use super::{ApiError, display_path, encode_hex, query, read_admission};
use crate::{app, auth::Principal, server::Server};
use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::Bytes;
use crab_remote_git::{
    ContentClassification, EntryKind, Error, GitPath, OperationContext, OperationKind,
    RemoteGitSnapshot, Revision, TreeListingItem, TreeListingRequest,
};
use gix_hash::ObjectId;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

const PAGE_ENTRIES: usize = 128;
const FILE_READ_BATCH: usize = 8;
const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_MATCH_LINES: usize = 6;
const MAX_CURSOR_PATH: usize = 8 * 1024;
const CURSOR_HEADER: usize = 20 + 32;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Parameters {
    q: String,
    rev: Option<String>,
    path: Option<String>,
    #[serde(default)]
    case_sensitive: bool,
    cursor: Option<String>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct Page {
    commit: String,
    items: Vec<FileMatches>,
    scanned: usize,
    skipped: usize,
    next: Option<String>,
}

#[derive(Serialize)]
struct FileMatches {
    path: String,
    path_hex: String,
    lines: Vec<Excerpt>,
    more_lines: bool,
}

#[derive(Serialize)]
struct Excerpt {
    line: usize,
    before: String,
    matched: String,
    after: String,
}

pub(super) async fn search(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(params): Query<Parameters>,
) -> std::result::Result<Response, Response> {
    let repo = app::repository(&server, &principal, &key).map_err(IntoResponse::into_response)?;
    let admission = read_admission::acquire_bulk(&server).map_err(|_| {
        (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "1")],
            Json(serde_json::json!({"error":{"code":"busy","message":"Code search is busy. Retry shortly"}}))).into_response()
    })?;
    let cancellation = server.cancellation.child_token();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    let terms = params.q.trim();
    if terms.is_empty() || terms.chars().count() > 128 || terms.chars().any(char::is_control) {
        return Err(ApiError::Input("Enter 1–128 characters to search for").into_response());
    }
    let path = params.path.as_deref().unwrap_or("");
    if path.len() > 1024
        || path.chars().any(char::is_control)
        || path.split('/').any(|part| matches!(part, "." | ".."))
    {
        return Err(ApiError::Input("Enter a repository directory path").into_response());
    }
    let directory = GitPath::new(path.as_bytes().to_vec()).map_err(remote_response)?;
    let limit = params.limit.unwrap_or(20);
    if !(1..=40).contains(&limit) {
        return Err(ApiError::Input("limit must be between 1 and 40").into_response());
    }
    let matcher = RegexBuilder::new(&regex::escape(terms))
        .case_insensitive(!params.case_sensitive)
        .build()
        .map_err(|_| ApiError::Input("The search term could not be compiled").into_response())?;
    let context = serde_json::to_vec(&(terms, params.rev.as_deref(), path, params.case_sensitive))
        .map_err(|error| ApiError::Json(error).into_response())?;
    let fingerprint = blake3::hash(&context);
    let tokens = Tokens {
        key: &server.cursor_key,
        repository: repo.id,
        fingerprint,
    };
    let cursor = params
        .cursor
        .as_deref()
        .map(|value| tokens.decode(value))
        .transpose()
        .map_err(IntoResponse::into_response)?;

    let opened = repo
        .revalidate(&server, &cancellation)
        .await
        .map_err(|error| ApiError::Service(error).into_response())?;
    let repository = &opened.git;
    let revision = match &cursor {
        Some((oid, _)) => Revision::Commit(*oid),
        None => query::revision(repository, params.rev.as_deref()).map_err(remote_response)?,
    };
    let mut limits = server.options.operation_limits();
    limits.max_duration = limits.max_duration.min(Duration::from_secs(20));
    limits.max_logical_objects = limits.max_logical_objects.min(12_000);
    limits.max_fetched_bytes = limits.max_fetched_bytes.min(192 * 1024 * 1024);
    limits.max_inflated_bytes = limits.max_inflated_bytes.min(128 * 1024 * 1024);
    limits.max_response_bytes = 40 * 1024 * 1024;
    let operation = repository
        .operation_with_limits(OperationKind::Content, &cancellation, limits)
        .await
        .map_err(remote_response)?;
    let found = async {
        let snapshot = repository.snapshot(&revision, &operation).await?;
        let after = cursor.as_ref().map(|(_, path)| path);
        let mut page = inspect(&snapshot, &directory, after, limit, &matcher, &operation).await?;
        if let Some(after) = page.after.take() {
            page.result.next = Some(tokens.encode(snapshot.commit_oid(), &after)?);
        }
        Ok(page.result)
    }
    .await;
    let page = operation.finish(found).await.map_err(remote_response)?;
    // A long scan must not return snippets after the current grant is revoked.
    app::membership(&server, &principal, &repo)
        .await
        .map_err(IntoResponse::into_response)?;
    Ok(read_admission::retain(
        Json(page).into_response(),
        admission,
    ))
}

fn remote_response(error: crab_remote_git::Error) -> Response {
    ApiError::Remote(error).into_response()
}

struct Scan {
    result: Page,
    after: Option<GitPath>,
}

async fn inspect(
    snapshot: &RemoteGitSnapshot,
    directory: &GitPath,
    after: Option<&GitPath>,
    limit: usize,
    matcher: &Regex,
    operation: &OperationContext,
) -> crab_remote_git::Result<Scan> {
    let mut prefix = directory.as_bytes().to_vec();
    if !directory.is_root() {
        let entry = snapshot
            .entry(directory, operation)
            .await?
            .ok_or(Error::PathNotFound)?;
        if entry.kind != EntryKind::Tree {
            return Err(Error::PathComponentNotTree { actual: entry.kind });
        }
        prefix.push(b'/');
    }
    let request = TreeListingRequest::new(
        prefix,
        after.map(|path| Bytes::copy_from_slice(path.as_bytes())),
        None,
        PAGE_ENTRIES,
    )?;
    let listings = snapshot.list_tree_blobs(&request, operation).await?;
    let entries = listings.items;
    let mut result = Page {
        commit: snapshot.commit_oid().to_string(),
        items: Vec::new(),
        scanned: 0,
        skipped: 0,
        next: None,
    };
    let mut examined = 0;
    'scan: for batch in entries.chunks(FILE_READ_BATCH) {
        let found = futures_util::future::try_join_all(batch.iter().map(|item| async move {
            match item {
                TreeListingItem::Blob(entry) => {
                    file_matches(snapshot, entry, matcher, operation).await
                }
                TreeListingItem::CommonPrefix(_) => Err(Error::InternalInvariant {
                    invariant: "ungrouped code search received a grouped path",
                }),
            }
        }))
        .await?;
        for found in found {
            examined += 1;
            result.scanned += 1;
            match found {
                Some(found) if !found.lines.is_empty() => result.items.push(found),
                Some(_) => {}
                None => result.skipped += 1,
            }
            if result.items.len() == limit {
                break 'scan;
            }
        }
    }
    Ok(Scan {
        result,
        after: if listings.has_more || examined < entries.len() {
            entries
                .get(examined.saturating_sub(1))
                .map(|item| GitPath::new(Bytes::copy_from_slice(item.path())))
                .transpose()?
        } else {
            None
        },
    })
}

async fn file_matches(
    snapshot: &RemoteGitSnapshot,
    entry: &crab_remote_git::TreeEntry,
    matcher: &Regex,
    operation: &OperationContext,
) -> crab_remote_git::Result<Option<FileMatches>> {
    let metadata = operation.read_object_metadata(entry.oid).await?;
    if metadata.size > MAX_FILE_BYTES {
        return Ok(None);
    }
    let blob = snapshot.read_blob(&entry.path, operation).await?;
    if blob.metadata.classification != ContentClassification::OrdinaryGit || blob.bytes.contains(&0)
    {
        return Ok(None);
    }
    let Ok(text) = std::str::from_utf8(&blob.bytes) else {
        return Ok(None);
    };
    let mut hits = text.lines().enumerate().filter_map(|(index, line)| {
        let found = matcher.find(line)?;
        Some(excerpt(index + 1, line, found))
    });
    let lines: Vec<_> = hits.by_ref().take(MAX_MATCH_LINES).collect();
    Ok(Some(FileMatches {
        path: display_path(entry.path.as_bytes()),
        path_hex: encode_hex(entry.path.as_bytes()),
        lines,
        more_lines: hits.next().is_some(),
    }))
}

fn excerpt(number: usize, text: &str, found: regex::Match<'_>) -> Excerpt {
    let before = &text[..found.start()];
    let from = before
        .char_indices()
        .rev()
        .nth(79)
        .map_or(0, |(index, _)| index);
    let after = &text[found.end()..];
    let to = after
        .char_indices()
        .nth(120)
        .map_or(after.len(), |(index, _)| index);
    Excerpt {
        line: number,
        before: format!("{}{}", if from > 0 { "…" } else { "" }, &before[from..]),
        matched: found.as_str().into(),
        after: format!(
            "{}{}",
            &after[..to],
            if to < after.len() { "…" } else { "" }
        ),
    }
}

struct Tokens<'a> {
    key: &'a [u8; 32],
    repository: Uuid,
    fingerprint: blake3::Hash,
}

impl Tokens<'_> {
    fn encode(&self, commit: ObjectId, after: &GitPath) -> crab_remote_git::Result<String> {
        let path = after.as_bytes();
        if path.len() > MAX_CURSOR_PATH {
            return Err(crab_remote_git::Error::LimitExceeded {
                limit: "code search cursor path",
                actual: path.len() as u64,
                maximum: MAX_CURSOR_PATH as u64,
            });
        }
        let mut payload = Vec::with_capacity(CURSOR_HEADER + path.len() + 32);
        payload.extend(commit.as_bytes());
        payload.extend(self.fingerprint.as_bytes());
        payload.extend(path);
        payload.extend(self.signature(&payload).as_bytes());
        Ok(URL_SAFE_NO_PAD.encode(payload))
    }

    fn decode(&self, value: &str) -> Result<(ObjectId, GitPath), ApiError> {
        let invalid = ApiError::Input("Invalid cursor for this code search");
        if value.len() > (CURSOR_HEADER + MAX_CURSOR_PATH + 32).div_ceil(3) * 4 {
            return Err(invalid);
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| ApiError::Input("Invalid code search cursor"))?;
        if bytes.len() <= CURSOR_HEADER + 32 || bytes.len() > CURSOR_HEADER + MAX_CURSOR_PATH + 32 {
            return Err(invalid);
        }
        let (payload, signature) = bytes.split_at(bytes.len() - 32);
        if self.signature(payload) != *signature || self.fingerprint != payload[20..CURSOR_HEADER] {
            return Err(invalid);
        }
        let oid: [u8; 20] = payload[..20]
            .try_into()
            .map_err(|_| ApiError::Input("Invalid code search commit"))?;
        let after = GitPath::new(payload[CURSOR_HEADER..].to_vec())?;
        Ok((ObjectId::from(oid), after))
    }

    fn signature(&self, payload: &[u8]) -> blake3::Hash {
        let mut hash = blake3::Hasher::new_keyed(self.key);
        hash.update(b"crab-code-search-v1\0");
        hash.update(self.repository.as_bytes());
        hash.update(payload);
        hash.finalize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{git, http::Harness};
    use reqwest::Method;
    use serde_json::{Value, json};

    #[tokio::test]
    async fn code_search_pins_its_pages_and_requires_current_repository_access() {
        let h = Harness::new(false).await;
        let alice = h.login().await;
        let token: Value = h
            .write(
                Method::POST,
                "/api/git-token",
                &alice,
                json!({"owner":"team","repository":"private","access":"write"}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let mut remote = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
        remote.set_username("crab").unwrap();
        let secret = token["token"].as_str().unwrap();
        remote.set_password(Some(secret)).unwrap();
        let source = tempfile::tempdir().unwrap();
        let path = source.path();
        git::success(
            path,
            &["init", "--initial-branch=main", "--object-format=sha1", "."],
        )
        .await;
        std::fs::create_dir_all(path.join("src/nested")).unwrap();
        std::fs::write(
            path.join("src/nested/a.rs"),
            b"heading\r\nlet x = \"Caf\xc3\xa9.*\";\r\n",
        )
        .unwrap();
        std::fs::write(path.join("src/z.rs"), "Café.* follows here\n").unwrap();
        std::fs::write(path.join("outside.rs"), "Café.* outside the scope\n").unwrap();
        std::fs::write(path.join("src/binary"), b"\0Caf\xc3\xa9.*").unwrap();
        std::fs::write(path.join("src/large"), "Café.*".repeat(50_000)).unwrap();
        git::success(path, &["add", "."]).await;
        git::success(path, &["commit", "-m", "Searchable commit"]).await;
        let head = git::success(path, &["rev-parse", "HEAD"]).await;
        git::success(path, &["branch", "topic"]).await;
        for branch in ["main", "topic"] {
            let pushed = git::git(path, &["push", remote.as_str(), branch]).await;
            assert!(
                pushed.status.success(),
                "{}",
                String::from_utf8_lossy(&pushed.stderr).replace(secret, "[redacted]")
            );
        }
        *h.provider.mode.lock().await = "member".into();
        let reader = h.login().await;
        let resource = "/api/repos/team/private/code-search";
        let first = format!("{resource}?rev=refs%2Fheads%2Ftopic&path=src&q=CAF%C3%89.*&limit=1");
        let result = h.json(&first, &reader).await;
        assert_eq!(result["commit"], head);
        assert_eq!(result["skipped"], 2);
        assert_eq!(result["items"][0]["path"], "src/nested/a.rs");
        assert_eq!(result["items"][0]["lines"][0]["line"], 2);
        assert_eq!(result["items"][0]["lines"][0]["matched"], "Café.*");
        let cursor = result["next"].as_str().unwrap();
        let mismatched = format!("{first}&case_sensitive=true&cursor={cursor}");
        let status = |url: String| async {
            h.http
                .get(url)
                .header(reqwest::header::COOKIE, &reader)
                .send()
                .await
                .unwrap()
                .status()
        };
        assert_eq!(
            status(format!("{}{mismatched}", h.origin)).await,
            StatusCode::BAD_REQUEST
        );

        git::success(path, &["checkout", "topic"]).await;
        std::fs::write(path.join("src/z.rs"), "Changed on the topic branch\n").unwrap();
        git::success(path, &["commit", "-am", "Advance topic"]).await;
        let pushed = git::git(path, &["push", remote.as_str(), "topic"]).await;
        assert!(
            pushed.status.success(),
            "{}",
            String::from_utf8_lossy(&pushed.stderr).replace(secret, "[redacted]")
        );
        let continuation = format!("{first}&cursor={cursor}");
        let second = h.json(&continuation, &reader).await;
        assert_eq!(second["commit"], head);
        assert_eq!(second["items"][0]["path"], "src/z.rs");
        assert_eq!(second["next"], Value::Null);
        let fresh = h.json(&first, &reader).await;
        assert_ne!(fresh["commit"], head);
        let sensitive = h
            .json(&format!("{first}&case_sensitive=true"), &reader)
            .await;
        assert_eq!(sensitive["items"], json!([]));

        let catalog = h.server.catalog().unwrap();
        let mut access = catalog.membership("team", "private").await.unwrap().members;
        access.retain(|member| member.subject != "bob-id");
        catalog
            .set_members("team", "private", access, true)
            .await
            .unwrap();
        assert_eq!(
            status(format!("{}{continuation}", h.origin)).await,
            StatusCode::NOT_FOUND
        );
        h.close().await;
    }
}
