use super::*;
use crate::api::catalog;

const REPOSITORIES_PER_PAGE: usize = 4;
const PATHS_PER_REPOSITORY: usize = 32;
const MATCHES_PER_PAGE: usize = 20;
const HEADER: usize = 16 + 32;
const CHECKSUM: usize = 32;
const MAX_PAYLOAD: usize = HEADER + 20 + MAX_CURSOR_PATH;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct Parameters {
    q: String,
    directory: Option<String>,
    organization: Option<Uuid>,
    #[serde(default)]
    case_sensitive: bool,
    #[serde(default)]
    kind: Kind,
    cursor: Option<String>,
}

#[derive(Default, Serialize)]
struct Results {
    groups: Vec<Group>,
    scanned: usize,
    skipped: usize,
    verified: usize,
    indexed_repositories: usize,
    scanning_repositories: usize,
    limited_repositories: usize,
    next: Option<String>,
}

#[derive(Serialize)]
struct Group {
    id: Uuid,
    owner: String,
    name: String,
    commit: String,
    files: Vec<FileMatches>,
}

pub(in crate::api) async fn search(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Query(params): Query<Parameters>,
) -> std::result::Result<Response, Response> {
    let actor = app::account(&principal).map_err(IntoResponse::into_response)?;
    let admission = acquire(&server)?;
    let cancel = server.cancellation.child_token();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let terms = params.q.trim();
    let directory = params.directory.as_deref().unwrap_or("");
    let pattern = Pattern::new(terms, directory, params.case_sensitive, params.kind)
        .map_err(IntoResponse::into_response)?;
    let context = serde_json::to_vec(&(
        &actor.issuer,
        &actor.subject,
        terms,
        directory,
        params.case_sensitive,
        params.kind,
        params.organization,
    ))
    .map_err(|error| ApiError::Json(error).into_response())?;
    let tokens = Continuations {
        key: &server.cursor_key,
        context: blake3::hash(&context),
    };
    let result = tokio::time::timeout(Duration::from_secs(30), search_page(
        &server, &principal, &params, &pattern, &tokens, &cancel,
    ))
    .await
    .map_err(|_| {
        (StatusCode::GATEWAY_TIMEOUT, Json(serde_json::json!({
            "error":{"code":"timeout","message":"Code search timed out. Try a directory, repository, or organization scope."}
        }))).into_response()
    })??;
    Ok(read_admission::retain(
        Json(result).into_response(),
        admission,
    ))
}

async fn search_page(
    server: &Server,
    principal: &Principal,
    params: &Parameters,
    pattern: &Pattern,
    tokens: &Continuations<'_>,
    cancel: &CancellationToken,
) -> std::result::Result<Results, Response> {
    let repositories = visible(server, principal, params.organization, pattern)
        .await
        .map_err(IntoResponse::into_response)?;
    let scope = scope_digest(&repositories);
    let cursor = params
        .cursor
        .as_deref()
        .map(|value| tokens.decode(value, scope))
        .transpose()?;
    let start = if let Some(position) = cursor.as_ref() {
        repositories
            .binary_search_by_key(&position.repository, |repo| repo.id)
            .map_err(|_| app::Error::PageChanged.into_response())?
    } else {
        0
    };

    let mut results = Results::default();
    let mut left = MATCHES_PER_PAGE;
    let mut next = None;
    for (index, repo) in repositories
        .iter()
        .enumerate()
        .skip(start)
        .take(REPOSITORIES_PER_PAGE)
    {
        next = repositories.get(index + 1).map(|following| Position {
            repository: following.id,
            scan: None,
        });
        let window = Window {
            rev: None,
            resume: cursor
                .as_ref()
                .filter(|position| position.repository == repo.id)
                .and_then(|position| position.scan.as_ref()),
            paths: PATHS_PER_REPOSITORY,
            results: left,
            duration: Duration::from_secs(6),
            use_index: true,
        };
        let scan = match scan_repository(server, repo, pattern, window, cancel).await {
            Ok(scan) => scan,
            Err(ApiError::Remote(error) | ApiError::Service(crate::Error::Remote(error)))
                if matches!(
                    crate::api::revalidation::shared_cause(&error),
                    Error::EmptyRepository
                        | Error::PathNotFound
                        | Error::PathComponentNotTree { .. }
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error.into_response()),
        };
        match scan.acceleration {
            Acceleration::Indexed => results.indexed_repositories += 1,
            Acceleration::Scanning => results.scanning_repositories += 1,
            Acceleration::Limited => results.limited_repositories += 1,
        }
        results.verified += scan.verified;
        results.scanned += scan.result.scanned;
        results.skipped += scan.result.skipped;
        if !scan.result.items.is_empty() {
            left -= scan.result.items.len();
            results.groups.push(Group {
                id: repo.id,
                owner: repo.config.owner.clone(),
                name: repo.config.name.clone(),
                commit: scan.result.commit,
                files: scan.result.items,
            });
        }
        if let Some(path) = scan.after {
            next = Some(Position {
                repository: repo.id,
                scan: Some((scan.commit, path)),
            });
            break;
        }
        if left == 0 {
            break;
        }
    }
    // Refresh every grant in scope before releasing excerpts or a continuation.
    // The cursor also seals this scope, so previous pages cannot be continued
    // after a repository leaves the user's view.
    let current = visible(server, principal, params.organization, pattern)
        .await
        .map_err(IntoResponse::into_response)?;
    if scope_digest(&current) != scope {
        return Err(app::Error::PageChanged.into_response());
    }
    results.next = next
        .as_ref()
        .map(|position| tokens.encode(scope, position))
        .transpose()
        .map_err(remote_response)?;
    Ok(results)
}

async fn visible(
    server: &Server,
    principal: &Principal,
    organization: Option<Uuid>,
    pattern: &Pattern,
) -> app::Result<Vec<Arc<Repository>>> {
    let mut repositories: Vec<_> = catalog::visible_matching(server, principal, |repo| {
        organization.is_none_or(|id| repo.organization == Some(id))
            && pattern.filters.repository_matches(repo)
    })
    .await?
    .into_iter()
    .map(|row| row.repository)
    .collect();
    // Stable UUID order lets a continuation locate the same repository without
    // relying on an ACL-sensitive ordinal supplied by the browser.
    repositories.sort_unstable_by_key(|repo| repo.id);
    Ok(repositories)
}

fn scope_digest(repositories: &[Arc<Repository>]) -> blake3::Hash {
    let mut hash = blake3::Hasher::new();
    for repo in repositories {
        hash.update(repo.id.as_bytes());
        hash.update(repo.config.owner.as_bytes());
        hash.update(b"\0");
        hash.update(repo.config.name.as_bytes());
        hash.update(b"\0");
    }
    hash.finalize()
}

struct Position {
    repository: Uuid,
    scan: Option<(ObjectId, GitPath)>,
}

struct Continuations<'a> {
    key: &'a [u8; 32],
    context: blake3::Hash,
}

impl Continuations<'_> {
    fn seal(&self, data: &[u8]) -> blake3::Hash {
        let mut mac = blake3::Hasher::new_keyed(self.key);
        // Old continuations could refer to the same text with literal semantics.
        mac.update(b"crab-workspace-code-search-v2\0");
        mac.update(self.context.as_bytes());
        mac.update(data);
        mac.finalize()
    }

    fn encode(&self, scope: blake3::Hash, position: &Position) -> crab_remote_git::Result<String> {
        let mut data = Vec::new();
        data.extend(position.repository.as_bytes());
        data.extend(scope.as_bytes());
        if let Some((oid, path)) = &position.scan {
            let path = path.as_bytes();
            if path.len() > MAX_CURSOR_PATH {
                return Err(Error::LimitExceeded {
                    limit: "code search cursor path",
                    actual: path.len() as u64,
                    maximum: MAX_CURSOR_PATH as u64,
                });
            }
            data.extend(oid.as_bytes());
            data.extend(path);
        }
        data.extend(self.seal(&data).as_bytes());
        Ok(URL_SAFE_NO_PAD.encode(data))
    }

    fn decode(&self, value: &str, scope: blake3::Hash) -> std::result::Result<Position, Response> {
        let invalid = || ApiError::Input("Invalid cursor for this code search").into_response();
        if value.len() > (MAX_PAYLOAD + CHECKSUM).div_ceil(3) * 4 {
            return Err(invalid());
        }
        let decoded = URL_SAFE_NO_PAD.decode(value).map_err(|_| invalid())?;
        if decoded.len() < HEADER + CHECKSUM || decoded.len() > MAX_PAYLOAD + CHECKSUM {
            return Err(invalid());
        }
        let (data, mac) = decoded.split_at(decoded.len() - CHECKSUM);
        if self.seal(data) != *mac {
            return Err(invalid());
        }
        if scope != data[16..HEADER] {
            return Err(app::Error::PageChanged.into_response());
        }
        let repository = Uuid::from_slice(&data[..16]).map_err(|_| invalid())?;
        let resume = &data[HEADER..];
        let scan = if resume.is_empty() {
            None
        } else if resume.len() > 20 {
            let oid: [u8; 20] = resume[..20].try_into().map_err(|_| invalid())?;
            let path =
                GitPath::new(Bytes::copy_from_slice(&resume[20..])).map_err(remote_response)?;
            Some((ObjectId::from(oid), path))
        } else {
            return Err(invalid());
        };
        Ok(Position { repository, scan })
    }
}
