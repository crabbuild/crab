use std::{sync::Arc, time::Duration};

use axum::{
    Extension,
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use crab_git::receive_wire;
use crab_remote_git::RepositoryOptions;
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::{auth::Principal, server::Server};

mod publish;
mod validate;

const MAX_BODY: u64 = 2 * 1024 * 1024 * 1024;
const REQUEST_BUDGET: Duration = Duration::from_secs(5 * 60);
const REF_UPDATE_BUDGET: Duration = Duration::from_secs(30);
const CONTENT_TYPE: &str = "application/x-git-receive-pack-result";

type Result<T> = std::result::Result<T, ReceiveError>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ReceiveError {
    #[error("{0}")]
    Request(&'static str),
    #[error("repository not found")]
    NotFound,
    #[error("write access required")]
    Forbidden,
    #[error("protected branch requires a pull request")]
    Protected,
    #[error("repository is archived and read-only")]
    Archived,
    #[error("Git transfers are busy")]
    Busy,
    #[error("receive cancelled or deadline exceeded")]
    Cancelled,
    #[error("receive body exceeds 2 GiB")]
    TooLarge,
    #[error("receive body failed")]
    Body(#[from] axum::Error),
    #[error("receive temporary I/O failed")]
    Io(#[from] std::io::Error),
    #[error("receive worker failed")]
    Worker(#[from] tokio::task::JoinError),
    #[error("receive protocol failed")]
    Wire(#[from] receive_wire::ReceiveWireError),
    #[error("incoming pack rejected")]
    Pack(#[from] crab_git::incoming_pack::IncomingPackError),
    #[error("incoming graph rejected")]
    Graph(#[from] crab_git::receive_plan::ReceivePlanError),
    #[error("pack preparation failed")]
    Prepare(#[from] crab_git::incoming_pack::PreparePackError),
    #[error("remote lookup failed")]
    Remote(#[from] crab_remote_git::Error),
    #[error("repository service failed")]
    Service(#[from] crate::Error),
    #[error("repository default branch changed")]
    DefaultBranchChanged,
    #[error("selected branch changed or was deleted")]
    BranchChanged,
    #[error("receive metadata failed")]
    Metadata(#[from] crab_metadata::error::MetadataError),
    #[error("receive storage failed")]
    Storage(#[from] crab_storage::StorageError),
    #[error("repository settings failed")]
    Settings(#[source] Box<crate::app::Error>),
    #[error("receive coordination failed")]
    Coordination(#[from] crab_coordination::CoordinationError),
    #[error("receive publication failed")]
    Write(#[from] crab_write::WriteError),
    #[error("Git object hashing failed")]
    Hash(#[from] gix_hash::hasher::Error),
    #[error("pointer content rejected")]
    Dependency(#[source] Box<crab_read::dependency_proof::DependencyProofError>),
    #[error("receive failed and its remote reader also failed to close")]
    Close {
        #[source]
        operation: Box<ReceiveError>,
        close: crab_remote_git::Error,
    },
}

fn pack_header(kind: gix_object::Kind, size: usize, output: &mut Vec<u8>) {
    let kind = match kind {
        gix_object::Kind::Commit => 1,
        gix_object::Kind::Tree => 2,
        gix_object::Kind::Blob => 3,
        gix_object::Kind::Tag => 4,
    };
    let mut remaining = size >> 4;
    let mut byte = (kind << 4) | (size as u8 & 0x0f);
    while remaining != 0 {
        output.push(byte | 0x80);
        byte = (remaining as u8) & 0x7f;
        remaining >>= 7;
    }
    output.push(byte);
}

fn write_pack(path: &std::path::Path, objects: &[(gix_object::Kind, Vec<u8>)]) -> Result<()> {
    use std::io::Write as _;

    let count = u32::try_from(objects.len())
        .map_err(|_| ReceiveError::Request("Too many generated Git objects"))?;
    let mut bytes = b"PACK\0\0\0\x02".to_vec();
    bytes.extend_from_slice(&count.to_be_bytes());
    for (kind, data) in objects {
        pack_header(*kind, data.len(), &mut bytes);
        let mut encoder =
            flate2::write::ZlibEncoder::new(&mut bytes, flate2::Compression::default());
        encoder.write_all(data)?;
        encoder.finish()?;
    }
    let mut hasher = gix_hash::hasher(gix_hash::Kind::Sha1);
    hasher.update(&bytes);
    let checksum = hasher.try_finalize()?;
    bytes.extend_from_slice(checksum.as_bytes());
    std::fs::write(path, bytes)?;
    Ok(())
}

pub(crate) async fn publish_objects(
    server: Arc<Server>,
    principal: Principal,
    key: (String, String),
    update: crab_git::receive_plan::RefUpdate,
    objects: Vec<(gix_object::Kind, Vec<u8>)>,
    visibility_base: Option<(String, gix_hash::ObjectId)>,
) -> Result<()> {
    publish_generated_objects(
        server,
        principal,
        key,
        update,
        objects,
        visibility_base,
        publish::Publication::NativePush,
    )
    .await
}

pub(crate) async fn publish_pull_request_objects(
    server: Arc<Server>,
    principal: Principal,
    key: (String, String),
    update: crab_git::receive_plan::RefUpdate,
    objects: Vec<(gix_object::Kind, Vec<u8>)>,
) -> Result<()> {
    publish_generated_objects(
        server,
        principal,
        key,
        update,
        objects,
        None,
        publish::Publication::PullRequest,
    )
    .await
}

async fn publish_generated_objects(
    server: Arc<Server>,
    principal: Principal,
    key: (String, String),
    update: crab_git::receive_plan::RefUpdate,
    objects: Vec<(gix_object::Kind, Vec<u8>)>,
    visibility_base: Option<(String, gix_hash::ObjectId)>,
    publication: publish::Publication,
) -> Result<()> {
    let permit = Arc::clone(&server.git_admission)
        .try_acquire_owned()
        .map_err(|_| ReceiveError::Busy)?;
    let cancel = server.cancellation.child_token();
    let worker_cancel = cancel.clone();
    let worker_server = Arc::clone(&server);
    let (send, result) = tokio::sync::oneshot::channel();
    server.receives.spawn(async move {
        let _permit = permit;
        let work = async {
            let directory = tokio::task::spawn_blocking(tempfile::tempdir).await??;
            let path = directory.path().join("generated.pack");
            let pack_path = path.clone();
            tokio::task::spawn_blocking(move || write_pack(&pack_path, &objects)).await??;
            let file = tokio::task::spawn_blocking(move || std::fs::File::open(path)).await??;
            publish::publish_pack(
                &worker_server,
                &principal,
                &key,
                directory,
                std::io::BufReader::new(file),
                update,
                publish::PackPublication {
                    visibility_base,
                    kind: publication,
                },
                &worker_cancel,
            )
            .await
        };
        tokio::pin!(work);
        let completed = tokio::select! {
            result = &mut work => result,
            () = tokio::time::sleep(REQUEST_BUDGET) => {
                worker_cancel.cancel();
                work.await
            }
        };
        if let Err(Err(error)) = send.send(completed) {
            tracing::error!(error = ?error, "disconnected browser Git publication failed");
        }
    });
    result
        .await
        .map_err(|_| ReceiveError::Request("Browser publication worker stopped"))?
}

impl IntoResponse for ReceiveError {
    fn into_response(self) -> Response {
        tracing::error!(error = ?self, "Git receive failed");
        let (status, message) = match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "Repository not found"),
            Self::Forbidden => (StatusCode::FORBIDDEN, "Write access required"),
            Self::Protected => (
                StatusCode::FORBIDDEN,
                "Protected branch requires a pull request",
            ),
            Self::Archived => (
                StatusCode::FORBIDDEN,
                "Repository is archived and read-only",
            ),
            Self::Busy => (
                StatusCode::TOO_MANY_REQUESTS,
                "Git transfers are busy; retry shortly",
            ),
            Self::Request(message) => (StatusCode::BAD_REQUEST, message),
            Self::DefaultBranchChanged | Self::BranchChanged => {
                (StatusCode::CONFLICT, "Repository changed; retry")
            }
            Self::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "Receive body exceeds 2 GiB"),
            Self::Cancelled => (
                StatusCode::REQUEST_TIMEOUT,
                "Receive cancelled or timed out",
            ),
            Self::Wire(_) | Self::Pack(_) | Self::Graph(_) | Self::Body(_) => {
                (StatusCode::BAD_REQUEST, "Invalid Git receive request")
            }
            _ => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Receive could not finish; check remote refs before retrying and inspect server logs",
            ),
        };
        (
            status,
            [("content-type", "text/plain; charset=utf-8")],
            message,
        )
            .into_response()
    }
}

pub(crate) async fn advertise(
    server: &Server,
    principal: &Principal,
    owner: &str,
    name: &str,
) -> Result<Response> {
    let entry = server
        .repositories
        .get(&(owner.to_owned(), name.to_owned()))
        .filter(|entry| principal.can_read(&entry.config))
        .ok_or(ReceiveError::NotFound)?;
    if !principal.can_write(&entry.config) {
        return Err(ReceiveError::Forbidden);
    }
    let repository = entry
        .open_current(server, RepositoryOptions::default(), &server.cancellation)
        .await?;
    let refs = repository
        .refs()
        .entries
        .iter()
        .map(|reference| (reference.name.clone(), reference.target))
        .collect();
    // Smart HTTP wraps the receive advertisement in a service announcement.
    let mut response = b"001f# service=git-receive-pack\n0000".to_vec();
    receive_wire::advertise(&mut response, &refs)?;
    Ok((
        [(
            "content-type",
            "application/x-git-receive-pack-advertisement",
        )],
        response,
    )
        .into_response())
}

pub(crate) async fn receive(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    request: Request,
) -> Result<Response> {
    let name = name
        .strip_suffix(".git")
        .ok_or(ReceiveError::NotFound)?
        .to_owned();
    let entry = server
        .repositories
        .get(&(owner.clone(), name.clone()))
        .filter(|entry| principal.can_read(&entry.config))
        .ok_or(ReceiveError::NotFound)?;
    if !principal.can_write(&entry.config) {
        return Err(ReceiveError::Forbidden);
    }
    if headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        != Some("application/x-git-receive-pack-request")
    {
        return Err(ReceiveError::Request(
            "Expected application/x-git-receive-pack-request",
        ));
    }
    if headers
        .get("content-encoding")
        .is_some_and(|value| value != "identity")
    {
        return Err(ReceiveError::Request(
            "Receive requires identity content encoding",
        ));
    }
    check_cancelled(&server.cancellation)?;
    let permit = Arc::clone(&server.git_admission)
        .try_acquire_owned()
        .map_err(|_| ReceiveError::Busy)?;
    let cancel = server.cancellation.child_token();
    let _guard = cancel.clone().drop_guard();
    let worker_cancel = cancel.clone();
    let worker_server = Arc::clone(&server);
    let (send, result) = tokio::sync::oneshot::channel();
    // The tracker owns the worker even if the HTTP response future disappears.
    // Shutdown cancels and drains it before closing remote readers.
    server.receives.spawn(async move {
        let _permit = permit;
        let work = async {
            let directory = tokio::task::spawn_blocking(tempfile::tempdir).await??;
            let path = directory.path().join("receive");
            let mut file = tokio::fs::File::create(&path).await?;
            let mut stream = request.into_body().into_data_stream();
            let mut size = 0_u64;
            loop {
                let chunk = tokio::select! {
                    () = worker_cancel.cancelled() => return Err(ReceiveError::Cancelled),
                    chunk = stream.next() => chunk,
                };
                let Some(chunk) = chunk else {
                    break;
                };
                let chunk = chunk?;
                size = size
                    .checked_add(chunk.len() as u64)
                    .filter(|size| *size <= MAX_BODY)
                    .ok_or(ReceiveError::TooLarge)?;
                file.write_all(&chunk).await?;
            }
            file.flush().await?;
            drop(file);
            publish::run(
                &worker_server,
                &principal,
                &(owner, name),
                directory,
                &worker_cancel,
            )
            .await
        };
        tokio::pin!(work);
        let completed = tokio::select! {
            result = &mut work => result,
            () = tokio::time::sleep(REQUEST_BUDGET) => {
                worker_cancel.cancel();
                work.await
            }
        };
        if let Err(Err(error)) = send.send(completed) {
            tracing::error!(error = ?error, "disconnected Git receive failed");
        }
    });
    let bytes = result
        .await
        .map_err(|_| ReceiveError::Request("Receive worker stopped"))??;
    Ok(([("content-type", CONTENT_TYPE)], bytes).into_response())
}

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(ReceiveError::Cancelled);
    }
    Ok(())
}

pub(crate) async fn fast_forward(
    server: Arc<Server>,
    principal: Principal,
    key: (String, String),
    name: String,
    old: gix_hash::ObjectId,
    new: gix_hash::ObjectId,
) -> Result<()> {
    publish_ref(
        server,
        principal,
        key,
        RefPublication::Update {
            update: crab_git::receive_plan::RefUpdate {
                name,
                old: Some(old),
                new: Some(new),
            },
            publication: publish::Publication::PullRequest,
        },
    )
    .await
}

pub(crate) async fn create_branch(
    server: Arc<Server>,
    principal: Principal,
    key: (String, String),
    name: String,
    new: gix_hash::ObjectId,
) -> Result<()> {
    create_reference(server, principal, key, name, new).await
}

pub(crate) async fn create_tag(
    server: Arc<Server>,
    principal: Principal,
    key: (String, String),
    name: String,
    new: gix_hash::ObjectId,
) -> Result<()> {
    create_reference(server, principal, key, name, new).await
}

async fn create_reference(
    server: Arc<Server>,
    principal: Principal,
    key: (String, String),
    name: String,
    new: gix_hash::ObjectId,
) -> Result<()> {
    publish_ref(
        server,
        principal,
        key,
        RefPublication::Update {
            update: crab_git::receive_plan::RefUpdate {
                name,
                old: None,
                new: Some(new),
            },
            publication: publish::Publication::NativePush,
        },
    )
    .await
}

pub(crate) async fn delete_branch(
    server: Arc<Server>,
    principal: Principal,
    key: (String, String),
    name: String,
    old: gix_hash::ObjectId,
) -> Result<()> {
    publish_ref(
        server,
        principal,
        key,
        RefPublication::Update {
            update: crab_git::receive_plan::RefUpdate {
                name,
                old: Some(old),
                new: None,
            },
            publication: publish::Publication::NativePush,
        },
    )
    .await
}

pub(crate) async fn set_default_branch(
    server: Arc<Server>,
    principal: Principal,
    key: (String, String),
    expected_head: String,
    branch: String,
    expected_oid: gix_hash::ObjectId,
) -> Result<()> {
    publish_ref(
        server,
        principal,
        key,
        RefPublication::DefaultBranch {
            expected_head,
            branch,
            expected_oid,
        },
    )
    .await
}

enum RefPublication {
    Update {
        update: crab_git::receive_plan::RefUpdate,
        publication: publish::Publication,
    },
    DefaultBranch {
        expected_head: String,
        branch: String,
        expected_oid: gix_hash::ObjectId,
    },
}

async fn publish_ref(
    server: Arc<Server>,
    principal: Principal,
    key: (String, String),
    publication: RefPublication,
) -> Result<()> {
    let permit = Arc::clone(&server.git_admission)
        .try_acquire_owned()
        .map_err(|_| ReceiveError::Busy)?;
    let cancel = server.cancellation.child_token();
    let worker_cancel = cancel.clone();
    let worker_server = Arc::clone(&server);
    let (send, result) = tokio::sync::oneshot::channel();
    server.receives.spawn(async move {
        let _permit = permit;
        let work = async {
            match publication {
                RefPublication::Update {
                    update,
                    publication,
                } => {
                    publish::publish_existing_objects(
                        &worker_server,
                        &principal,
                        &key,
                        update,
                        publication,
                        &worker_cancel,
                    )
                    .await
                }
                RefPublication::DefaultBranch {
                    expected_head,
                    branch,
                    expected_oid,
                } => {
                    publish::publish_default_branch(
                        &worker_server,
                        &principal,
                        &key,
                        &expected_head,
                        &branch,
                        expected_oid,
                        &worker_cancel,
                    )
                    .await
                }
            }
        };
        tokio::pin!(work);
        let completed = tokio::select! {
            result = &mut work => result,
            () = tokio::time::sleep(REF_UPDATE_BUDGET) => {
                worker_cancel.cancel();
                work.await
            }
        };
        if let Err(Err(error)) = send.send(completed) {
            tracing::error!(error = ?error, "disconnected ref publication failed");
        }
    });
    result
        .await
        .map_err(|_| ReceiveError::Request("Ref publication worker stopped"))?
}
