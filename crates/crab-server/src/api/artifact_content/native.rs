use std::{io, sync::Arc};

use axum::{
    body::Body,
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use crab_coordination::{
    GcReadBarrier,
    read_retention::{ReadRetentionLease, RetainedReadRoot},
    read_retention_runtime::{ReadRetentionCapture, RetainedRead},
};
use crab_metadata::capsule_protocol::{
    MAX_CATALOG_BYTES, PointerCatalogLookup, store_read_capture,
};
use crab_read::{ArtifactPointer, ReadError, capsule_protocol::CapsuleRepositoryView};
use crab_storage::{Store, StoreLayout};
use futures_util::StreamExt;
use uuid::Uuid;

use crate::{
    api::{ApiError, read_admission},
    git::lfs,
    repository::Repository,
    server::Server,
    transfer_admission,
};

// The hydrator keeps its metadata files in private transfer staging. Payload
// caching stays disabled so previews cannot grow an unaccounted cache.
const CACHE_METADATA_BYTES: u64 = 8 * 1024 * 1024;

pub(super) struct Capture {
    admission: ReadRetentionCapture,
    repository: GcReadBarrier,
    global: GcReadBarrier,
}

impl Capture {
    pub(super) async fn observe(
        server: &Server,
        layout: &StoreLayout<Store>,
    ) -> Result<Self, ReadError> {
        let admission = server.content_retention.capture()?;
        let store = layout.store().inner();
        let (repository, global) = tokio::try_join!(
            GcReadBarrier::observe(Arc::clone(store), layout.repo_prefix()),
            GcReadBarrier::observe(Arc::clone(store), layout.global_prefix()),
        )?;
        Ok(Self {
            admission,
            repository,
            global,
        })
    }

    async fn validate(&self) -> Result<(), ReadError> {
        tokio::try_join!(self.repository.validate(), self.global.validate())?;
        Ok(())
    }

    async fn retain(
        self,
        layout: &StoreLayout<Store>,
        view: &CapsuleRepositoryView,
        lookup: &PointerCatalogLookup,
    ) -> Result<RetainedRead, ReadError> {
        let owner = blake3::hash(Uuid::now_v7().as_bytes()).to_hex().to_string();
        let (hash, size) = store_read_capture(
            layout,
            &owner,
            view.root(),
            view.capsule_run_pointers(),
            lookup,
        )
        .await?;
        let lease = ReadRetentionLease::create(
            Arc::clone(layout.store().inner()),
            layout.read_retention_claim_path(&owner),
            RetainedReadRoot::new(hash, size)?,
        )
        .await?;
        let retained = self.admission.retain(lease)?;
        // A sweep that crossed the capture may have missed the new claim.
        tokio::try_join!(self.repository.validate(), self.global.validate())?;
        retained.pin()?;
        Ok(retained)
    }
}

pub(super) async fn download(
    server: &Server,
    entry: &Repository,
    capture: Capture,
    view: CapsuleRepositoryView,
    pointer: ArtifactPointer,
    method: Method,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let hash = pointer.hash();
    let size = pointer.size();
    let etag = format!("\"blake3-{hash}\"");
    let requested = if method == Method::GET {
        match lfs::requested_range(headers, size, &etag) {
            Ok(range) => range,
            Err(_) => {
                return Ok((
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    [
                        (header::ACCEPT_RANGES, "bytes".to_owned()),
                        (header::CONTENT_RANGE, format!("bytes */{size}")),
                    ],
                )
                    .into_response());
            }
        }
    } else {
        None
    };
    let catalog = view.pointer_catalog_lookup(&entry.layout, MAX_CATALOG_BYTES)?;
    let range = requested.clone().unwrap_or(0..size);
    let cancel = server.cancellation.child_token();
    let guard = cancel.clone().drop_guard();
    let artifact = pointer.bind(entry.layout.clone(), catalog).await?;
    let body = if method == Method::HEAD || range.is_empty() {
        capture.validate().await?;
        if range.is_empty() && hash != blake3::hash(b"") {
            return Err(ReadError::HashMismatch {
                requested: hash.to_hex().to_string(),
                actual: blake3::hash(b"").to_hex().to_string(),
            }
            .into());
        }
        Body::empty()
    } else {
        let permit = match server.acquire_transfer(&cancel).await {
            Ok(permit) => Arc::new(permit),
            Err(transfer_admission::Error::Busy) => return Ok(read_admission::busy()),
            Err(transfer_admission::Error::Cancelled) => return Err(ReadError::Cancelled.into()),
            Err(transfer_admission::Error::Coordination(error)) => {
                return Err(ReadError::from(error).into());
            }
        };
        let retained = capture
            .retain(&entry.layout, &view, artifact.catalog())
            .await?;
        let staging = server
            .local_staging
            .create(CACHE_METADATA_BYTES, &cancel)
            .await
            .map_err(io::Error::other)
            .map_err(ReadError::from)?;
        let cache_admission = Arc::clone(&permit);
        let stream_range = range.clone();
        let stream_cancel = cancel.clone();
        let ((stream, work), staging) = tokio::task::spawn_blocking(move || {
            let _admission = cache_admission;
            let prepared = artifact.prepare_range(
                &staging.path().join("cache"),
                retained,
                stream_range,
                &stream_cancel,
            )?;
            Ok::<_, ReadError>((prepared, staging))
        })
        .await
        .map_err(|error| ReadError::Io(io::Error::other(error)))??;
        let worker_permit = Arc::clone(&permit);
        server.receives.spawn(async move {
            let _owned = (staging, worker_permit);
            work.await;
        });
        let deadline = tokio::time::Instant::now() + crate::git::receive::REQUEST_BUDGET;
        Body::from_stream(
            stream
                .take_until(async move {
                    tokio::select! {
                        () = cancel.cancelled() => {}
                        () = tokio::time::sleep_until(deadline) => {}
                    }
                })
                .map(move |item| {
                    let _owned = (&permit, &guard);
                    item
                }),
        )
    };
    let mut response = (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::ACCEPT_RANGES, "bytes".to_owned()),
            (
                header::CONTENT_LENGTH,
                (range.end - range.start).to_string(),
            ),
            (header::ETAG, etag),
        ],
        body,
    )
        .into_response();
    if let Some(range) = requested {
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
        let value = format!("bytes {}-{}/{size}", range.start, range.end - 1);
        if let Ok(value) = value.parse() {
            response.headers_mut().insert(header::CONTENT_RANGE, value);
        }
    }
    Ok(response)
}
