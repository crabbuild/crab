use std::sync::Arc;

use axum::{
    Extension,
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use crab_read::{
    UploadPackRequest, plan_upload_pack_catalog, upload_pack_repository_options,
    upload_pack_wire as wire,
};
use crab_remote_git::{RemoteGitRepository, RepositoryRefs};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::io::AsyncWrite;
use tokio_util::{io::ReaderStream, sync::CancellationToken};

use crate::{auth::Principal, server::Server};

// HTTP response EOF delimits commands: Git's remote-curl rejects an on-wire 0002.
// The stdio helper separately emits response-end packets for stateless-connect.

#[derive(Debug, thiserror::Error)]
pub(crate) enum GitError {
    #[error("{0}")]
    Request(&'static str),
    #[error("repository not found")]
    NotFound,
    #[error("Git transport is busy")]
    Busy,
    #[error("Git protocol failed")]
    Wire(#[from] wire::WireError),
    #[error("Git object read failed")]
    Remote(#[from] crab_remote_git::Error),
    #[error("Git fetch planning failed")]
    Read(#[from] crab_read::ReadError),
}

impl IntoResponse for GitError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::Request(message) => (StatusCode::BAD_REQUEST, message),
            Self::NotFound => (StatusCode::NOT_FOUND, "Repository not found"),
            Self::Busy => (
                StatusCode::TOO_MANY_REQUESTS,
                "Git transfers are busy; retry shortly",
            ),
            Self::Wire(_) => (StatusCode::BAD_REQUEST, "Invalid Git protocol-v2 request"),
            Self::Read(crab_read::ReadError::UnauthorizedObject) => (
                StatusCode::FORBIDDEN,
                "Requested object is not reachable from this repository's refs",
            ),
            _ => (
                StatusCode::BAD_GATEWAY,
                "Repository data could not be read; check storage and indexing",
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Discovery {
    service: String,
}

fn require_v2(headers: &HeaderMap) -> Result<(), GitError> {
    if headers
        .get("git-protocol")
        .and_then(|value| value.to_str().ok())
        != Some("version=2")
    {
        return Err(GitError::Request(
            "This server requires Git protocol v2; use git -c protocol.version=2",
        ));
    }
    Ok(())
}

pub(crate) async fn advertise(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<Discovery>,
    headers: HeaderMap,
) -> Result<Response, GitError> {
    if !server
        .repositories
        .get(&(owner, name))
        .is_some_and(|entry| principal.can_read(&entry.config))
    {
        return Err(GitError::NotFound);
    }
    require_v2(&headers)?;
    if query.service != "git-upload-pack" {
        return Err(GitError::Request(
            "Only Git fetch is available; receive-pack is not implemented yet",
        ));
    }
    let cancel = server.cancellation.child_token();
    let mut bytes = Vec::new();
    for line in [
        "version 2\n",
        "agent=crab-http\n",
        "ls-refs=unborn\n",
        "fetch=shallow filter\n",
    ] {
        wire::write_packet(&mut bytes, line.as_bytes(), None, &cancel).await?;
    }
    wire::write_flush(&mut bytes, &cancel).await?;
    Ok((
        [(
            "content-type",
            "application/x-git-upload-pack-advertisement",
        )],
        bytes,
    )
        .into_response())
}

pub(crate) async fn upload_pack(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, GitError> {
    let entry = server
        .repositories
        .get(&(owner, name))
        .filter(|entry| principal.can_read(&entry.config))
        .ok_or(GitError::NotFound)?;
    require_v2(&headers)?;
    if headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        != Some("application/x-git-upload-pack-request")
    {
        return Err(GitError::Request(
            "Expected application/x-git-upload-pack-request",
        ));
    }
    let permit = Arc::clone(&server.git_admission)
        .try_acquire_owned()
        .map_err(|_| GitError::Busy)?;
    let cancel = server.cancellation.child_token();
    let guard = cancel.clone().drop_guard();
    let mut input = body.as_ref();
    let request = wire::read_command_request(&mut input, &cancel)
        .await?
        .ok_or(GitError::Request("Expected one Git command"))?;
    if !input.is_empty() {
        return Err(GitError::Request(
            "Only one command is allowed per HTTP request",
        ));
    }
    let repository = RemoteGitRepository::open(
        entry.store.clone(),
        entry.layout.clone(),
        entry.identity.clone(),
        Arc::clone(&server.runtime),
        upload_pack_repository_options()?,
        &cancel,
    )
    .await?;
    let mut response = Vec::new();
    match request.command.as_str() {
        "ls-refs" => {
            let request = wire::parse_ls_refs(&request.args)?;
            write_refs(&mut response, repository.refs(), &request, &cancel).await?;
            Ok((
                [("content-type", "application/x-git-upload-pack-result")],
                response,
            )
                .into_response())
        }
        "fetch" => {
            let request = wire::parse_fetch(&request.args)?;
            let visibility = repository.catalog_visibility_index(&cancel).await?;
            let refs = repository
                .refs()
                .entries
                .iter()
                .map(|reference| reference.name.clone())
                .collect::<Vec<_>>();
            let semantic = UploadPackRequest {
                wants: request.wants.clone(),
                haves: request.haves.clone(),
                shallow: request.shallow.clone(),
                deepen: request.deepen,
                deepen_relative: request.deepen_relative,
                include_tags: request.include_tags,
                filter: request.filter.clone(),
            };
            let plan =
                plan_upload_pack_catalog(&repository, &visibility, &refs, &semantic, &cancel)
                    .await?;
            if !request.done {
                wire::write_packet(&mut response, b"acknowledgments\n", None, &cancel).await?;
                if plan.common_haves.is_empty() {
                    wire::write_packet(&mut response, b"NAK\n", None, &cancel).await?;
                    wire::write_flush(&mut response, &cancel).await?;
                    return Ok((
                        [("content-type", "application/x-git-upload-pack-result")],
                        response,
                    )
                        .into_response());
                }
                for oid in &plan.common_haves {
                    wire::write_packet(
                        &mut response,
                        format!("ACK {oid}\n").as_bytes(),
                        None,
                        &cancel,
                    )
                    .await?;
                }
                wire::write_packet(&mut response, b"ready\n", None, &cancel).await?;
                wire::write_delimiter(&mut response, &cancel).await?;
            }
            if !plan.shallow.is_empty() || !plan.unshallow.is_empty() {
                wire::write_packet(&mut response, b"shallow-info\n", None, &cancel).await?;
                for (name, oids) in [("shallow", &plan.shallow), ("unshallow", &plan.unshallow)] {
                    for oid in oids {
                        wire::write_packet(
                            &mut response,
                            format!("{name} {oid}\n").as_bytes(),
                            None,
                            &cancel,
                        )
                        .await?;
                    }
                }
                wire::write_delimiter(&mut response, &cancel).await?;
            }
            wire::write_packet(&mut response, b"packfile\n", None, &cancel).await?;
            let (mut writer, reader) = tokio::io::duplex(128 * 1024);
            let transfer_cancel = cancel.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let result = async {
                    wire::write_all_cancellable(&mut writer, &response, &transfer_cancel).await?;
                    let bases = if request.thin_pack {
                        plan.common_haves.as_slice()
                    } else {
                        &[]
                    };
                    let pack = repository
                        .generate_pack_with_bases(&plan.object_ids, bases, &transfer_cancel)
                        .await?;
                    pack.write_sideband(&mut writer, &transfer_cancel).await?;
                    wire::write_flush(&mut writer, &transfer_cancel).await?;
                    Ok::<(), GitError>(())
                }
                .await;
                if result.is_err() && !transfer_cancel.is_cancelled() {
                    let _ = wire::write_packet(
                        &mut writer,
                        b"Repository transfer failed; retry or check storage/index health\n",
                        Some(3),
                        &transfer_cancel,
                    )
                    .await;
                    let _ = wire::write_flush(&mut writer, &transfer_cancel).await;
                }
            });
            // Dropping the HTTP body cancels pack production, including while it is awaiting storage.
            let stream = futures_util::stream::unfold(
                (ReaderStream::new(reader), guard),
                |(mut stream, guard)| async move {
                    stream.next().await.map(|chunk| (chunk, (stream, guard)))
                },
            );
            Ok((
                [("content-type", "application/x-git-upload-pack-result")],
                Body::from_stream(stream),
            )
                .into_response())
        }
        _ => Err(GitError::Request("Unsupported Git protocol-v2 command")),
    }
}

async fn write_refs<W: AsyncWrite + Unpin>(
    writer: &mut W,
    refs: &RepositoryRefs,
    request: &wire::LsRefsRequest,
    cancel: &CancellationToken,
) -> Result<(), GitError> {
    let matches = |name: &str| {
        request.prefixes.is_empty()
            || request
                .prefixes
                .iter()
                .any(|prefix| name.starts_with(prefix))
    };
    if let Some(head) = &refs.head {
        if matches("HEAD") {
            let suffix = if request.symrefs {
                format!(" symref-target:{}", head.name)
            } else {
                String::new()
            };
            wire::write_packet(
                writer,
                format!("{} HEAD{suffix}\n", head.target).as_bytes(),
                None,
                cancel,
            )
            .await?;
        }
    } else if request.unborn
        && matches("HEAD")
        && let Some(target) = &refs.unborn_head
    {
        wire::write_packet(
            writer,
            format!("unborn HEAD symref-target:{target}\n").as_bytes(),
            None,
            cancel,
        )
        .await?;
    }
    for reference in &refs.entries {
        if matches(&reference.name) {
            let suffix = if request.peel {
                reference
                    .peeled
                    .map(|oid| format!(" peeled:{oid}"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            wire::write_packet(
                writer,
                format!("{} {}{suffix}\n", reference.target, reference.name).as_bytes(),
                None,
                cancel,
            )
            .await?;
        }
    }
    wire::write_flush(writer, cancel).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn http_ref_listing_ends_at_flush_without_helper_response_end() {
        let mut response = Vec::new();
        write_refs(
            &mut response,
            &RepositoryRefs::default(),
            &wire::LsRefsRequest::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(response, b"0000");
    }
}
