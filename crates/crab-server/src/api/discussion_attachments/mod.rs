use std::{sync::Arc, time::Duration};

use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    api::{asset_transfer, representation::image_content_type},
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::collaboration::attachments::{
        Collect, Due, Expire, Finish, Get, MAX_BYTES, ReadInput, Reserve, ReserveOutcome, Upload,
        transferred_id,
    },
    repository::Repository,
    server::Server,
};

const TRANSFER_BUDGET: Duration = Duration::from_secs(120);
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);
const SWEEP_BATCH: usize = 16;

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/discussion-attachments",
            post(upload),
        )
        .route(
            "/api/discussion-attachments/{repository_id}/{id}",
            get(download),
        )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    id: Uuid,
    name: String,
}

#[derive(Serialize)]
struct View {
    url: String,
    name: String,
    inline: bool,
}

fn object_path(repo: &Repository, id: Uuid) -> object_store::path::Path {
    repo.layout
        .repo_path(&format!("discussion-attachments/v1/{}/{id}", repo.id))
}

pub(super) async fn copy_for_issue_transfer(
    server: &Server,
    source: &Repository,
    destination: &Repository,
    request: Uuid,
    upload: &Upload,
) -> Result<Uuid> {
    let cancel = server.cancellation.child_token();
    let _permit = server
        .acquire_transfer(&cancel)
        .await
        .map_err(Error::AttachmentAdmission)?;
    let id = transferred_id(request, upload.id);
    let copy = async {
        let path = object_path(source, upload.id);
        let (bytes, _) = source
            .store
            .get_with_etag_bounded(&path, upload.size)
            .await?;
        if bytes.len() as u64 != upload.size
            || format!("{:x}", Sha256::digest(&bytes)) != upload.digest
        {
            return Err(Error::Storage(crab_storage::StorageError::CorruptObject {
                path: path.to_string(),
                reason: "attachment size or digest does not match the captured manifest".into(),
            }));
        }
        // Stage owns this private ID already. A lost storage reply must
        // recover against the same bytes without overwriting an occupied key.
        destination
            .store
            .put_if_absent_verified(&object_path(destination, id), bytes)
            .await?;
        Ok(id)
    };
    tokio::select! {
        () = cancel.cancelled() => Err(Error::AttachmentTransfer(asset_transfer::Error::Cancelled)),
        result = tokio::time::timeout(TRANSFER_BUDGET, copy) => {
            result.map_err(|_| Error::AttachmentTransfer(asset_transfer::Error::Cancelled))?
        }
    }
}

async fn upload(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(input): Query<Parameters>,
    request: Request,
) -> Result<Json<View>> {
    let repo = app::repository(&server, &principal, &key)?;
    // Discussion writers can attach files with the same Read grant that lets
    // them comment. The native reservation also checks archive state.
    let owner = (&app::actor(&principal)?).into();
    let name = input.name.trim().to_owned();
    if input.id.is_nil()
        || name.is_empty()
        || name.len() > 255
        || matches!(name.as_str(), "." | "..")
        || name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\'))
    {
        return Err(Error::Invalid(
            "Choose a file with a valid name of up to 255 bytes",
        ));
    }
    if request
        .headers()
        .get(header::CONTENT_ENCODING)
        .is_some_and(|value| value != "identity")
    {
        return Err(Error::Invalid(
            "Attachment uploads require identity content encoding",
        ));
    }
    let staging_bytes = asset_transfer::staging_bytes(request.headers(), MAX_BYTES)
        .map_err(Error::AttachmentTransfer)?;
    let cancel = server.cancellation.child_token();
    let permit = server
        .acquire_transfer(&cancel)
        .await
        .map_err(Error::AttachmentAdmission)?;
    let _guard = cancel.clone().drop_guard();
    let worker = Arc::clone(&server);
    let (send, receive) = tokio::sync::oneshot::channel();
    server.receives.spawn(async move {
        let _permit = permit;
        let work = async {
            let staged = asset_transfer::stage(&worker, request, staging_bytes, &cancel)
                .await
                .map_err(Error::AttachmentTransfer)?;
            let upload = Upload {
                id: input.id,
                owner,
                name,
                size: staged.size,
                digest: staged.digest,
                raster: image_content_type(&staged.prefix).map(str::to_owned),
            };
            app::membership(&worker, &principal, &repo).await?;
            let cell = app::repository_cell(&worker, &repo)?;
            let outcome = app::command_output(
                cell.client
                    .command::<Reserve>(&cell.target, app::mutation_identity()?, upload.clone())
                    .await,
            )?;
            match outcome {
                ReserveOutcome::Unavailable => return Err(Error::NotFound),
                ReserveOutcome::Conflict => return Err(Error::RequestConflict),
                ReserveOutcome::Ready => {}
                ReserveOutcome::Pending => {
                    // Replay of this UUID can only write the identical bytes.
                    // Expiry seals the native record before deleting this key.
                    repo.store
                        .put_multipart_file_retry(
                            &object_path(&repo, upload.id),
                            &staged.path,
                            staged.size,
                            staged.hash,
                            8 * 1024 * 1024,
                            &cancel,
                            None,
                        )
                        .await?;
                    app::membership(&worker, &principal, &repo).await?;
                    if !app::command_output(
                        cell.client
                            .command::<Finish>(
                                &cell.target,
                                app::mutation_identity()?,
                                upload.clone(),
                            )
                            .await,
                    )? {
                        return Err(Error::Invalid(
                            "The attachment is no longer available. Upload it again.",
                        ));
                    }
                }
            }
            app::membership(&worker, &principal, &repo).await?;
            Ok(Json(View {
                url: format!("/api/discussion-attachments/{}/{}", repo.id, upload.id),
                name: upload.name,
                inline: upload.raster.is_some(),
            }))
        };
        tokio::pin!(work);
        let result = tokio::select! {
            result = &mut work => result,
            () = tokio::time::sleep(TRANSFER_BUDGET) => {
                // Keep polling the cooperative cancellation path so providers
                // can abort an unfinished multipart upload before dropping it.
                cancel.cancel();
                work.await
            }
        };
        let _ = send.send(result);
    });
    receive
        .await
        .map_err(|_| Error::AttachmentTransfer(asset_transfer::Error::Cancelled))?
}

async fn download(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((repository_id, id)): Path<(Uuid, Uuid)>,
) -> Result<Response> {
    let repo = server
        .repositories
        .by_id(repository_id)
        .ok_or(Error::NotFound)?;
    let grant = app::membership(&server, &principal, &repo).await?;
    let actor = grant
        .principal
        .member_identity(repo.id)
        .as_ref()
        .map(Into::into);
    let cell = app::repository_cell(&server, &repo)?;
    let upload = app::query_output(
        cell.client
            .query::<Get>(&cell.target, None, ReadInput { id, actor })
            .await,
    )?
    .ok_or(Error::NotFound)?;
    let cancel = server.cancellation.child_token();
    let permit = server
        .acquire_transfer(&cancel)
        .await
        .map_err(Error::AttachmentAdmission)?;
    let guard = cancel.clone().drop_guard();
    let deadline = tokio::time::Instant::now() + TRANSFER_BUDGET;
    let path = object_path(&repo, id);
    let (metadata, _, stream) = tokio::select! {
        () = cancel.cancelled() => return Err(Error::AttachmentTransfer(asset_transfer::Error::Cancelled)),
        result = tokio::time::timeout_at(deadline, repo.store.get_stream(&path, None)) => {
            result.map_err(|_| Error::AttachmentTransfer(asset_transfer::Error::Cancelled))??
        }
    };
    app::membership(&server, &grant.principal, &repo).await?;
    if metadata.size != upload.size {
        return Err(Error::Storage(crab_storage::StorageError::CorruptObject {
            path: metadata.location.to_string(),
            reason: "attachment size does not match metadata".into(),
        }));
    }
    let disposition = if upload.raster.is_some() {
        "inline"
    } else {
        "attachment"
    };
    let encoded: String = url::form_urlencoded::byte_serialize(upload.name.as_bytes()).collect();
    let stream = stream
        .take_until(async move {
            tokio::select! {
                () = cancel.cancelled() => {}
                () = tokio::time::sleep_until(deadline) => {}
            }
        })
        .map(move |chunk| {
            let _ = (&permit, &guard);
            chunk
        });
    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                upload
                    .raster
                    .unwrap_or_else(|| "application/octet-stream".into()),
            ),
            (header::CONTENT_LENGTH, upload.size.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!(
                    "{disposition}; filename*=UTF-8''{}",
                    encoded.replace('+', "%20")
                ),
            ),
            (header::CACHE_CONTROL, "private, no-store".into()),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

async fn collect_repository(server: &Server, repo: &Repository) -> Result<()> {
    let cell = app::repository_cell(server, repo)?;
    if !app::query_output(cell.client.query::<Due>(&cell.target, None, ()).await)? {
        return Ok(());
    }
    let expired = app::command_output(
        cell.client
            .command::<Expire>(&cell.target, app::mutation_identity()?, ())
            .await,
    )?;
    for id in expired {
        if server.cancellation.is_cancelled() {
            return Ok(());
        }
        match repo.store.delete(&object_path(repo, id)).await {
            Ok(()) | Err(crab_storage::StorageError::NotFound { .. }) => {}
            Err(error) => {
                tracing::warn!(repository_id = %repo.id, attachment_id = %id, %error, "attachment deletion will be retried");
                continue;
            }
        }
        app::command_output(
            cell.client
                .command::<Collect>(&cell.target, app::mutation_identity()?, id)
                .await,
        )?;
    }
    Ok(())
}

pub(crate) async fn sweep(server: Arc<Server>) {
    let mut cursor = 0;
    loop {
        tokio::select! {
            () = server.cancellation.cancelled() => return,
            () = tokio::time::sleep(SWEEP_INTERVAL) => {}
        }
        let repositories = server.repositories.values();
        if repositories.is_empty() || server.cells().is_none() {
            continue;
        }
        let count = repositories.len().min(SWEEP_BATCH);
        for offset in 0..count {
            let repo = &repositories[(cursor + offset) % repositories.len()];
            tokio::select! {
                () = server.cancellation.cancelled() => return,
                result = collect_repository(&server, repo) => {
                    if let Err(error) = result {
                        tracing::warn!(repository_id = %repo.id, %error, "attachment expiry sweep failed");
                    }
                }
            }
        }
        cursor = (cursor + count) % repositories.len();
    }
}

#[cfg(test)]
mod tests;
