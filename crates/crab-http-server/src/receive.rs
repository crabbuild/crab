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
    #[error("receive metadata failed")]
    Metadata(#[from] crab_metadata::error::MetadataError),
    #[error("receive storage failed")]
    Storage(#[from] crab_storage::StorageError),
    #[error("receive coordination failed")]
    Coordination(#[from] crab_coordination::CoordinationError),
    #[error("receive publication failed")]
    Write(#[from] crab_write::WriteError),
    #[error("pointer content rejected")]
    Dependency(#[source] Box<crab_read::dependency_proof::DependencyProofError>),
    #[error("receive failed and its remote reader also failed to close")]
    Close {
        #[source]
        operation: Box<ReceiveError>,
        close: crab_remote_git::Error,
    },
}

impl IntoResponse for ReceiveError {
    fn into_response(self) -> Response {
        tracing::error!(error = ?self, "Git receive failed");
        let (status, message) = match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "Repository not found"),
            Self::Forbidden => (StatusCode::FORBIDDEN, "Write access required"),
            Self::Busy => (
                StatusCode::TOO_MANY_REQUESTS,
                "Git transfers are busy; retry shortly",
            ),
            Self::Request(message) => (StatusCode::BAD_REQUEST, message),
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
