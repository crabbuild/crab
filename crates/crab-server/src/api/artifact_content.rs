use std::sync::Arc;

use axum::{
    Extension,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, Method, header},
    response::{IntoResponse, Response},
};
use crab_git::lfs_pointer::LfsPointer;
use crab_remote_git::{ContentClassification, EntryKind, Error, GitPath, OperationKind, Revision};
use serde::Deserialize;

use super::{ApiError, decode_hex, read_admission};
use crate::{app, auth::Principal, git::lfs, server::Server};

mod native;

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Lfs,
    Crab,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Parameters {
    rev: String,
    path_hex: String,
}

pub(super) async fn download(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Extension(kind): Extension<Kind>,
    Path(key): Path<(String, String)>,
    Query(parameters): Query<Parameters>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let Some(entry) = server
        .repositories
        .get(&key)
        .filter(|entry| principal.can_read(entry.id))
    else {
        return Ok(app::Error::NotFound.into_response());
    };
    let revision = Revision::parse(&parameters.rev)?;
    if !matches!(revision, Revision::Commit(_)) {
        return Err(ApiError::Input(
            "Stored file downloads require an exact commit ID",
        ));
    }
    let path = GitPath::new(decode_hex(&parameters.path_hex)?)?;
    let Ok(_admission) = read_admission::acquire_bulk(&server) else {
        return Ok(read_admission::busy());
    };
    let cancellation = server.cancellation.child_token();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    let (git, captured, classification) = match kind {
        Kind::Lfs => (
            entry.revalidate(&server, &cancellation).await?.git,
            None,
            ContentClassification::LfsPointer,
        ),
        Kind::Crab => {
            let capture = native::Capture::observe(&server, &entry.layout).await?;
            let (view, git) = entry
                .open_capsule_repository(&server, server.options, &cancellation)
                .await?;
            (
                git,
                Some((capture, view)),
                ContentClassification::CrabPointer,
            )
        }
    };
    let operation = git.operation(OperationKind::Content, &cancellation).await?;
    let pointer = async {
        let snapshot = git.snapshot(&revision, &operation).await?;
        let metadata = snapshot.blob_metadata(&path, &operation).await?;
        if metadata.kind != EntryKind::Blob || metadata.classification != classification {
            return Err(Error::PathNotFound);
        }
        operation.read_object(metadata.oid).await
    }
    .await;
    let object = operation.finish(pointer).await?;
    if let Err(error) = app::membership(&server, &principal, &entry).await {
        return Ok(error.into_response());
    }
    // The verified commit and path supply the only allowed object identity and size.
    let mut response = if let Some((capture, view)) = captured {
        let pointer = crab_types::pointer::Pointer::parse(&object.data)
            .map_err(crab_read::ReadError::from)?;
        native::download(&server, &entry, capture, view, pointer, method, &headers).await?
    } else {
        let transfer = async {
            let pointer = LfsPointer::parse(&object.data).map_err(lfs::Error::from)?;
            lfs::validate_size(pointer.size, &entry.store)?;
            lfs::download_object(&server, &entry, &pointer, method, &headers).await
        }
        .await;
        transfer.into_response()
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    if response.status().is_success() {
        response.headers_mut().insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment"),
        );
    }
    Ok(response)
}
