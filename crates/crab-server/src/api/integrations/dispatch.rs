use base64::{Engine, engine::general_purpose::STANDARD};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::git;
use stored::dispatches::{self, Current};

const PATH: &str = "/ci/queue/";

pub(crate) fn ci_git_path(method: &Method, path: &str) -> bool {
    path.strip_prefix(PATH).is_some_and(|tail| {
        let segments: Vec<_> = tail.split('/').take(7).collect();
        match (method, segments.as_slice()) {
            (&Method::GET, [_, _, _, checkout, "info", "refs"])
            | (&Method::POST, [_, _, _, checkout, "git-upload-pack"]) => checkout.ends_with(".git"),
            _ => false,
        }
    })
}

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/ci/queue/{repository}/{connection}/{oid}/{checkout}/info/refs",
            get(advertise),
        )
        .route(
            "/ci/queue/{repository}/{connection}/{oid}/{checkout}/git-upload-pack",
            post(fetch).layer(DefaultBodyLimit::max(git::MAX_BODY_BYTES)),
        )
}

pub(super) fn fetch_secret(
    root: &[u8; 32],
    repository: Uuid,
    connection: &stored::Connection,
) -> Result<Option<String>> {
    let Some(config) = connection.dispatch.as_ref() else {
        return Ok(None);
    };
    let mac = fetch_mac(root, repository, connection, config.id)?;
    Ok(Some(crate::api::encode_hex(&mac.finalize().into_bytes())))
}

fn fetch_mac(
    root: &[u8; 32],
    repository: Uuid,
    connection: &stored::Connection,
    generation: Uuid,
) -> Result<Hmac<Sha256>> {
    // Separate the Git read credential from the connection's webhook key.
    let mut mac = Hmac::<Sha256>::new_from_slice(root)
        .map_err(|_| Error::CellContract("Invalid queue checkout key"))?;
    mac.update(b"crab github actions queue checkout v1\0");
    mac.update(repository.as_bytes());
    mac.update(connection.id.as_bytes());
    mac.update(connection.credential.as_bytes());
    mac.update(generation.as_bytes());
    Ok(mac)
}

async fn current(
    server: &Server,
    repo: &Repository,
    connection: Uuid,
    oid: &str,
) -> Result<Option<Current>> {
    let cell = app::repository_cell(server, repo)?;
    app::query_output(
        cell.client
            .query::<dispatches::Get>(
                cell.target,
                None,
                dispatches::Key {
                    connection,
                    oid: oid.to_owned(),
                },
            )
            .await,
    )
}

pub(super) async fn authorize_report(
    server: &Server,
    repo: &Repository,
    row: &stored::Connection,
    run: &WorkflowRun,
) -> Result<Option<(String, Uuid)>> {
    let Some(config) = &row.dispatch else {
        return Ok(None);
    };
    let github = server
        .github_actions
        .as_ref()
        .filter(|github| github.permits(repo.id, row, config))
        .ok_or(Error::CommitNotFound)?;
    let Some((oid, id)) = run
        .display_title
        .as_deref()
        .and_then(|title| title.strip_prefix("Crab queue "))
        .and_then(|title| title.split_once(' '))
    else {
        return Ok(None);
    };
    let id = Uuid::parse_str(id).map_err(|_| Error::CommitNotFound)?;
    let commit = statuses::parse_oid(oid)?;
    if commit.to_string() != oid
        || run.head_repository.as_ref().map(|head| head.id) != Some(row.github_repository)
        || run.head_branch.as_deref() != Some(config.branch.as_str())
    {
        return Err(Error::CommitNotFound);
    }
    let dispatch = current(server, repo, row.id, oid)
        .await?
        .and_then(|current| current.dispatch)
        .filter(|dispatch| dispatch.id == id)
        .ok_or(Error::CommitNotFound)?;
    match dispatch.run {
        Some(bound) if bound == run.id => (),
        Some(_) => return Err(Error::CommitNotFound),
        None => {
            // The signature identifies GitHub and the connection, but only the
            // dispatching App can recover a lost response by the run title.
            let actor = github.actor().await.map_err(|_| Error::AutoMergeChanged)?;
            if run
                .actor
                .as_ref()
                .is_none_or(|value| !value.login.eq_ignore_ascii_case(actor))
            {
                return Err(Error::CommitNotFound);
            }
        }
    }
    if !pulls::candidate::require_check_target(server, repo, dispatch.pull, commit).await? {
        return Err(Error::CommitNotFound);
    }
    Ok(Some((oid.to_owned(), id)))
}

pub(in crate::api) async fn start_required(
    server: &Server,
    repo: &Repository,
    pull: u64,
    oid: &str,
    required: Vec<Uuid>,
    cancel: &CancellationToken,
) -> Result<()> {
    let Some(github) = server.github_actions.as_ref() else {
        return Ok(());
    };
    let Some(auth) = server.auth.as_ref() else {
        return Ok(());
    };
    let now = u64::try_from(crate::cells::unix_now_ms().map_err(Error::Cell)?)
        .map_err(|_| Error::CellContract("Cell clock is negative"))?;
    let cell = app::repository_cell(server, repo)?;
    for connection in required {
        if cancel.is_cancelled() {
            break;
        }
        let Some(Current {
            connection: row,
            dispatch,
        }) = current(server, repo, connection, oid).await?
        else {
            continue;
        };
        if dispatch.as_ref().is_some_and(|state| !state.ready(now)) {
            continue;
        }
        let Some(config) = &row.dispatch else {
            continue;
        };
        if !github.permits(repo.id, &row, config) {
            continue;
        }
        let commit = statuses::parse_oid(oid)?;
        if !pulls::candidate::require_check_target(server, repo, pull, commit).await? {
            return Ok(());
        }
        let key = dispatches::Key {
            connection,
            oid: oid.to_owned(),
        };
        let reserved = app::command_output(
            cell.client
                .command::<dispatches::Reserve>(
                    cell.target,
                    app::mutation_identity()?,
                    dispatches::Start {
                        key: key.clone(),
                        id: Uuid::now_v7(),
                        pull,
                        credential: row.credential,
                        generation: config.id,
                    },
                )
                .await,
        )?;
        let Some(reserved) = reserved else { continue };
        let source = format!(
            "{}{PATH}{}/{connection}/{oid}/{}.git",
            auth.origin(),
            repo.id,
            reserved.id
        );
        let launched = tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            outcome = github.launch(&row, config, &reserved, oid, &source) => outcome,
        };
        let run = match launched {
            Ok(run) => Some(run),
            Err(super::github::Error::Rejected) => None,
            Err(super::github::Error::Unconfirmed) => continue,
        };
        let _accepted = app::command_output(
            cell.client
                .command::<dispatches::Confirm>(
                    cell.target,
                    app::mutation_identity()?,
                    dispatches::Finish {
                        key,
                        id: reserved.id,
                        run,
                    },
                )
                .await,
        )?;
    }
    Ok(())
}

pub(in crate::api) async fn view(
    server: &Server,
    repo: &Repository,
    oid: &str,
) -> Result<Vec<Value>> {
    let rows = connections(server, repo).await?;
    let mut results = vec![];
    for row in rows.into_iter().filter(|row| row.dispatch.is_some()) {
        let Some(Current {
            connection,
            dispatch: Some(state),
        }) = current(server, repo, row.id, oid).await?
        else {
            continue;
        };
        let Some(config) = connection.dispatch else {
            continue;
        };
        let run_url = state
            .run
            .map(|id| format!("https://github.com/{}/actions/runs/{id}", config.repository));
        results.push(json!({
            "id": row.id, "name": row.name,
            "state": if state.run.is_some() { "started" } else if state.failed { "failed" } else { "starting" },
            "url": run_url,
        }));
    }
    Ok(results)
}

type Address = (Uuid, Uuid, String, String);

async fn checkout(
    server: &Server,
    (repository, connection, oid, checkout): Address,
    headers: &HeaderMap,
) -> Result<Option<(Arc<Repository>, (String, gix_hash::ObjectId))>> {
    let Some(root) = shared_key(server) else {
        return Ok(None);
    };
    let Some(repo) = server.repositories.by_id(repository) else {
        return Ok(None);
    };
    let token = one_header(headers, "authorization")
        .and_then(|header| header.split_once(' '))
        .filter(|(scheme, encoded)| scheme.eq_ignore_ascii_case("basic") && encoded.len() <= 128)
        .and_then(|(_, encoded)| STANDARD.decode(encoded).ok())
        .and_then(|raw| String::from_utf8(raw).ok())
        .and_then(|raw| {
            raw.strip_prefix("crab-ci:")
                .and_then(|secret| blake3::Hash::from_hex(secret).ok())
        });
    let Some(token) = token else { return Ok(None) };
    let commit = statuses::parse_oid(&oid)?;
    let id = checkout
        .strip_suffix(".git")
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .ok_or(Error::NotFound)?;
    let Some(Current {
        connection,
        dispatch: Some(dispatch),
    }) = current(server, &repo, connection, &oid).await?
    else {
        return Ok(None);
    };
    if dispatch.id != id
        || dispatch.failed
        || !server.github_actions.as_ref().is_some_and(|github| {
            connection
                .dispatch
                .as_ref()
                .is_some_and(|config| github.permits(repo.id, &connection, config))
        })
        || fetch_mac(root, repo.id, &connection, dispatch.generation)?
            .verify_slice(token.as_bytes())
            .is_err()
    {
        return Ok(None);
    }
    if !pulls::candidate::require_check_target(server, &repo, dispatch.pull, commit).await? {
        return Err(Error::NotFound);
    }
    Ok(Some((
        repo,
        (git::receive::merge_candidate_ref(dispatch.pull), commit),
    )))
}

fn challenge() -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        StatusCode::UNAUTHORIZED,
        [("www-authenticate", "Basic realm=\"Crab CI\"")],
        "Queue checkout credential is required",
    )
        .into_response()
}

async fn advertise(
    State(server): State<Arc<Server>>,
    Path(address): Path<Address>,
    axum::extract::Query(discovery): axum::extract::Query<git::Discovery>,
    headers: HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match checkout(&server, address, &headers).await {
        Ok(Some(_)) => git::advertise_read(&server, discovery, &headers)
            .await
            .into_response(),
        Ok(None) => challenge(),
        Err(error) => error.into_response(),
    }
}

async fn fetch(
    State(server): State<Arc<Server>>,
    Path(address): Path<Address>,
    headers: HeaderMap,
    request: axum::extract::Request,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match checkout(&server, address, &headers).await {
        Ok(Some((repo, scope))) => {
            git::upload_pack_read(server, repo, headers, request, Some(scope))
                .await
                .into_response()
        }
        Ok(None) => challenge(),
        Err(error) => error.into_response(),
    }
}
