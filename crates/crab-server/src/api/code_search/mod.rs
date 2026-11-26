#![expect(
    clippy::result_large_err,
    reason = "handlers and their helpers return rejections as ready axum responses"
)]

use super::{ApiError, display_path, encode_hex, query, read_admission};
use crate::{
    app,
    auth::Principal,
    repository::{Repository, code_index, settings},
    server::Server,
};
use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::Bytes;
use code_index::symbols::Syntax;
use crab_remote_git::{
    EntryKind, Error, GitPath, OperationContext, OperationKind, RemoteGitSnapshot, Revision,
    TreeListingItem, TreeListingRequest,
};
use gix_hash::ObjectId;
use qualifiers::{Facts, QueryPlan, Truth};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{ops::Range, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod evaluate;
mod indexed;
mod qualifiers;
pub(super) mod workspace;

const PAGE_ENTRIES: usize = 128;
const FILE_READ_BATCH: usize = 8;
const MAX_MATCH_LINES: usize = 6;
const MAX_HIGHLIGHT_CHARS: usize = 128;
const MAX_CURSOR_PATH: usize = 8 * 1024;
const CURSOR_CONTEXT: usize = 32;
const RESUME_HEADER: usize = 20 + 8;

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    #[default]
    Any,
    Content,
    Path,
    Symbol,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Parameters {
    q: String,
    rev: Option<String>,
    path: Option<String>,
    #[serde(default)]
    case_sensitive: bool,
    #[serde(default)]
    kind: Kind,
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

impl Page {
    fn for_commit(commit: ObjectId) -> Self {
        Self {
            commit: commit.to_string(),
            items: Vec::new(),
            scanned: 0,
            skipped: 0,
            next: None,
        }
    }
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
    let admission = acquire(&server)?;
    let cancellation = server.cancellation.child_token();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    let terms = params.q.trim();
    let path = params.path.as_deref().unwrap_or("");
    let limit = params.limit.unwrap_or(20);
    if !(1..=40).contains(&limit) {
        return Err(ApiError::Input("limit must be between 1 and 40").into_response());
    }
    let pattern = Pattern::new(terms, path, params.case_sensitive, params.kind)
        .map_err(IntoResponse::into_response)?;
    let context = serde_json::to_vec(&(
        terms,
        params.rev.as_deref(),
        path,
        params.case_sensitive,
        params.kind,
    ))
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
    let lifecycle = pattern.lifecycle(&server, &repo).await?;
    let archive_version = lifecycle.as_ref().map_or(0, |state| state.version);
    check_resume(cursor.as_ref(), archive_version)?;

    let mut scan = scan_repository(
        &server,
        &repo,
        &pattern,
        Window {
            scope: pattern.repository_facts(&repo, lifecycle.as_ref().map(|state| state.archived)),
            rev: params.rev.as_deref(),
            resume: cursor.as_ref(),
            paths: PAGE_ENTRIES,
            results: limit,
            duration: Duration::from_secs(20),
            use_index: pattern.has_symbols
                || params.rev.as_deref().is_none_or(|revision| {
                    revision == "HEAD"
                        || revision.strip_prefix("refs/heads/").unwrap_or(revision)
                            == repo.config.default_branch
                }),
        },
        &cancellation,
    )
    .await
    .map_err(IntoResponse::into_response)?;
    if let Some(after) = scan.after {
        scan.result.next = Some(
            tokens
                .encode(&Resume {
                    commit: scan.commit,
                    archive_version,
                    after,
                })
                .map_err(remote_response)?,
        );
    }
    if let Some(state) = lifecycle {
        check_lifecycle(&server, &repo, state.version).await?;
    }
    // A long scan must not return snippets after the current grant is revoked.
    app::membership(&server, &principal, &repo)
        .await
        .map_err(IntoResponse::into_response)?;
    Ok(read_admission::retain(
        Json(scan.result).into_response(),
        admission,
    ))
}

fn remote_response(error: crab_remote_git::Error) -> Response {
    ApiError::Remote(error).into_response()
}

fn acquire(server: &Server) -> std::result::Result<read_admission::Admission, Response> {
    read_admission::acquire_bulk(server).map_err(|_| {
        (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "1")],
            Json(serde_json::json!({"error":{"code":"busy","message":"Code search is busy. Retry shortly"}}))).into_response()
    })
}

struct Pattern {
    query: Arc<QueryPlan>,
    directory: GitPath,
    uses_text: bool,
    has_symbols: bool,
    uses_archive: bool,
}

impl Pattern {
    fn new(terms: &str, path: &str, case_sensitive: bool, kind: Kind) -> Result<Self, ApiError> {
        let query = QueryPlan::parse(terms, kind, case_sensitive)?;
        if path.len() > 1024
            || path.chars().any(char::is_control)
            || path.split('/').any(|part| matches!(part, "." | ".."))
        {
            return Err(ApiError::Input("Enter a repository directory path"));
        }
        Ok(Self {
            uses_text: query.leaves.iter().any(qualifiers::Predicate::uses_text),
            has_symbols: query.leaves.iter().any(qualifiers::Predicate::uses_symbols),
            uses_archive: query
                .leaves
                .iter()
                .any(|term| matches!(term, qualifiers::Predicate::Archived)),
            query: Arc::new(query),
            directory: GitPath::new(path.as_bytes().to_vec())?,
        })
    }

    async fn lifecycle(
        &self,
        server: &Server,
        repo: &Repository,
    ) -> Result<Option<settings::RepositoryLifecycle>, Response> {
        if !self.uses_archive {
            return Ok(None);
        }
        load_lifecycle(server, repo).await.map(Some)
    }
}

async fn load_lifecycle(
    server: &Server,
    repo: &Repository,
) -> Result<settings::RepositoryLifecycle, Response> {
    settings::load_lifecycle(server, repo)
        .await
        .map_err(IntoResponse::into_response)
}

async fn check_lifecycle(server: &Server, repo: &Repository, version: u64) -> Result<(), Response> {
    if load_lifecycle(server, repo).await?.version != version {
        return Err(app::Error::PageChanged.into_response());
    }
    Ok(())
}

fn check_resume(resume: Option<&Resume>, version: u64) -> Result<(), Response> {
    if resume.is_some_and(|cursor| cursor.archive_version != version) {
        return Err(app::Error::PageChanged.into_response());
    }
    Ok(())
}

struct Window<'a> {
    scope: Facts,
    rev: Option<&'a str>,
    resume: Option<&'a Resume>,
    paths: usize,
    results: usize,
    duration: Duration,
    use_index: bool,
}

struct Scan {
    commit: ObjectId,
    result: Page,
    after: Option<GitPath>,
    acceleration: Acceleration,
    verified: usize,
}

enum Acceleration {
    Indexed,
    Scanning,
    Limited,
}

async fn scan_repository(
    server: &Server,
    repo: &Repository,
    pattern: &Pattern,
    window: Window<'_>,
    cancellation: &CancellationToken,
) -> Result<Scan, ApiError> {
    let opened = repo.revalidate(server, cancellation).await?;
    let repository = &opened.git;
    let revision = match window.resume {
        Some(resume) => Revision::Commit(resume.commit),
        None => query::revision(repository, window.rev)?,
    };
    let mut limits = server.options.operation_limits();
    limits.max_duration = limits.max_duration.min(window.duration);
    limits.max_logical_objects = limits.max_logical_objects.min(12_000);
    limits.max_fetched_bytes = limits.max_fetched_bytes.min(192 * 1024 * 1024);
    limits.max_inflated_bytes = limits.max_inflated_bytes.min(128 * 1024 * 1024);
    limits.max_response_bytes = 40 * 1024 * 1024;
    let kind = if !pattern.uses_text {
        OperationKind::Tree
    } else {
        OperationKind::Content
    };
    let operation = repository
        .operation_with_limits(kind, cancellation, limits)
        .await?;
    let found = async {
        let snapshot = repository.snapshot(&revision, &operation).await?;
        let mut acceleration = Acceleration::Scanning;
        if pattern.query.evaluate(&window.scope).0 == Truth::No {
            let commit = snapshot.commit_oid();
            return Ok(Scan {
                commit,
                result: Page::for_commit(commit),
                after: None,
                acceleration,
                verified: 0,
            });
        }
        if window.use_index {
            match code_index::load(server, repo, snapshot.root_tree_oid()).await {
                Ok(Some(index)) => {
                    if let Some(rows) = &index.rows {
                        return indexed::inspect(rows, &snapshot, pattern, &window, &operation).await;
                    }
                    acceleration = Acceleration::Limited;
                }
                Ok(None) => (),
                Err(error) => tracing::debug!(repository_id = %repo.id, ?error, "Code search index could not be read"),
            }
        }
        let mut scan = inspect(&snapshot, pattern, &window, &operation).await?;
        scan.acceleration = acceleration;
        Ok(scan)
    }
    .await;
    Ok(operation.finish(found).await?)
}

enum Inspection {
    Match(FileMatches),
    NoMatch,
    Skipped,
}

async fn inspect(
    snapshot: &RemoteGitSnapshot,
    pattern: &Pattern,
    window: &Window<'_>,
    operation: &OperationContext,
) -> crab_remote_git::Result<Scan> {
    let directory = &pattern.directory;
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
        window
            .resume
            .map(|resume| Bytes::copy_from_slice(resume.after.as_bytes())),
        None,
        window.paths,
    )?;
    let listings = snapshot.list_tree_blobs(&request, operation).await?;
    let entries = listings.items;
    let mut result = Page::for_commit(snapshot.commit_oid());
    let mut examined = 0;
    let mut verified = 0;
    'scan: for batch in entries.chunks(FILE_READ_BATCH) {
        let found = futures_util::future::try_join_all(batch.iter().map(|item| async move {
            match item {
                TreeListingItem::Blob(entry) => {
                    pattern.inspect_entry(entry, &window.scope, operation).await
                }
                TreeListingItem::CommonPrefix(_) => Err(Error::InternalInvariant {
                    invariant: "ungrouped code search received a grouped path",
                }),
            }
        }))
        .await?;
        verified += found.iter().filter(|(_, read)| *read).count();
        for (found, _) in found {
            examined += 1;
            result.scanned += 1;
            match found {
                Inspection::Match(found) => result.items.push(found),
                Inspection::NoMatch => {}
                Inspection::Skipped => result.skipped += 1,
            }
            if result.items.len() == window.results {
                break 'scan;
            }
        }
    }
    Ok(Scan {
        commit: snapshot.commit_oid(),
        result,
        acceleration: Acceleration::Scanning,
        verified,
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

fn expression_matches(
    text: &str,
    matcher: &Regex,
    cancel: &CancellationToken,
) -> Option<(Vec<Excerpt>, bool)> {
    let mut excerpts = Vec::new();
    let mut number = 1;
    let mut line_start = 0;
    while line_start <= text.len() {
        if cancel.is_cancelled() {
            return None;
        }
        let Some(found) = matcher.find_at(text, line_start) else {
            break;
        };
        let mut advance = 0;
        for (offset, _) in text[line_start..found.start()].match_indices('\n') {
            advance = offset + 1;
            number += 1;
        }
        line_start += advance;
        loop {
            if excerpts.len() == MAX_MATCH_LINES {
                return Some((excerpts, true));
            }
            let end = text[line_start..]
                .find('\n')
                .map_or(text.len(), |offset| line_start + offset);
            let line = text[line_start..end].trim_end_matches('\r');
            let start = found.start().saturating_sub(line_start).min(line.len());
            let stop = found.end().saturating_sub(line_start).min(line.len());
            excerpts.push(excerpt(number, line, start..stop));

            // Advance by line even for a zero-width match. At most seven
            // searches are needed to fill these excerpts and detect more.
            line_start = end + 1;
            number += 1;
            if line_start >= found.end() {
                break;
            }
        }
    }
    Some((excerpts, false))
}

fn symbol_matches(
    definitions: &[code_index::symbols::Definition],
    text: &str,
    matcher: &Regex,
) -> (Vec<Excerpt>, bool) {
    let mut lines: Vec<Excerpt> = Vec::new();
    for symbol in definitions {
        let Some(found) = matcher.find(&text[symbol.bytes.clone()]) else {
            continue;
        };
        if lines.last().is_some_and(|last| last.line == symbol.line) {
            continue;
        }
        if lines.len() == MAX_MATCH_LINES {
            return (lines, true);
        }
        let start = text[..symbol.bytes.start]
            .rfind('\n')
            .map_or(0, |offset| offset + 1);
        let end = text[symbol.bytes.end..]
            .find('\n')
            .map_or(text.len(), |offset| symbol.bytes.end + offset);
        let column = symbol.bytes.start - start;
        lines.push(excerpt(
            symbol.line,
            text[start..end].trim_end_matches('\r'),
            column + found.start()..column + found.end(),
        ));
    }
    (lines, false)
}

fn excerpt(number: usize, text: &str, found: Range<usize>) -> Excerpt {
    let before = &text[..found.start];
    let from = before
        .char_indices()
        .rev()
        .nth(79)
        .map_or(0, |(index, _)| index);
    let after = &text[found.end..];
    let to = after
        .char_indices()
        .nth(120)
        .map_or(after.len(), |(index, _)| index);
    let matched = &text[found];
    let marked = matched.char_indices().nth(MAX_HIGHLIGHT_CHARS).map_or_else(
        || matched.to_owned(),
        |(end, _)| format!("{}…", &matched[..end]),
    );
    Excerpt {
        line: number,
        before: format!("{}{}", if from > 0 { "…" } else { "" }, &before[from..]),
        matched: marked,
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

struct Resume {
    commit: ObjectId,
    archive_version: u64,
    after: GitPath,
}

impl Resume {
    fn write(&self, data: &mut Vec<u8>) -> crab_remote_git::Result<()> {
        let path = self.after.as_bytes();
        if path.len() > MAX_CURSOR_PATH {
            return Err(crab_remote_git::Error::LimitExceeded {
                limit: "code search cursor path",
                actual: path.len() as u64,
                maximum: MAX_CURSOR_PATH as u64,
            });
        }
        data.extend(self.commit.as_bytes());
        data.extend(self.archive_version.to_be_bytes());
        data.extend(path);
        Ok(())
    }

    fn read(data: &[u8]) -> Result<Self, ApiError> {
        let invalid = || ApiError::Input("Invalid cursor for this code search");
        if data.len() <= RESUME_HEADER {
            return Err(invalid());
        }
        let oid: [u8; 20] = data[..20].try_into().map_err(|_| invalid())?;
        let version = data[20..RESUME_HEADER].try_into().map_err(|_| invalid())?;
        Ok(Self {
            commit: ObjectId::from(oid),
            archive_version: u64::from_be_bytes(version),
            after: GitPath::new(Bytes::copy_from_slice(&data[RESUME_HEADER..]))?,
        })
    }
}

impl Tokens<'_> {
    fn encode(&self, resume: &Resume) -> crab_remote_git::Result<String> {
        let mut payload = Vec::new();
        payload.extend(self.fingerprint.as_bytes());
        resume.write(&mut payload)?;
        payload.extend(self.signature(&payload).as_bytes());
        Ok(URL_SAFE_NO_PAD.encode(payload))
    }

    fn decode(&self, value: &str) -> Result<Resume, ApiError> {
        const MIN: usize = CURSOR_CONTEXT + RESUME_HEADER + 32;
        let invalid = ApiError::Input("Invalid cursor for this code search");
        if value.len() > (MIN + MAX_CURSOR_PATH).div_ceil(3) * 4 {
            return Err(invalid);
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| ApiError::Input("Invalid code search cursor"))?;
        if bytes.len() <= MIN || bytes.len() > MIN + MAX_CURSOR_PATH {
            return Err(invalid);
        }
        let (payload, signature) = bytes.split_at(bytes.len() - 32);
        if self.signature(payload) != *signature || self.fingerprint != payload[..CURSOR_CONTEXT] {
            return Err(invalid);
        }
        Resume::read(&payload[CURSOR_CONTEXT..])
    }

    fn signature(&self, payload: &[u8]) -> blake3::Hash {
        let mut hash = blake3::Hasher::new_keyed(self.key);
        hash.update(b"crab-code-search-v5\0");
        hash.update(self.repository.as_bytes());
        hash.update(payload);
        hash.finalize()
    }
}

#[cfg(test)]
mod tests;
