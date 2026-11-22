use std::path::PathBuf;

use axum::{
    extract::Request,
    http::{HeaderMap, header},
};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::{local_disk, server::Server};

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("Upload exceeded its byte limit")]
    TooLarge,
    #[error("Upload was cancelled")]
    Cancelled,
    #[error("Upload staging is unavailable")]
    Staging(#[from] local_disk::Error),
    #[error("Upload body could not be read")]
    Body(#[from] axum::Error),
    #[error("Upload temporary file operation failed")]
    Io(#[from] std::io::Error),
}

pub(super) struct StagedUpload {
    // Keep the scratch reservation and file alive until object storage finishes.
    _directory: local_disk::StagingDirectory,
    pub(super) path: PathBuf,
    pub(super) size: u64,
    pub(super) digest: String,
    pub(super) hash: [u8; 32],
    pub(super) prefix: Vec<u8>,
}

pub(super) fn staging_bytes(headers: &HeaderMap, maximum: u64) -> Result<u64, Error> {
    let size = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(maximum);
    if size > maximum {
        return Err(Error::TooLarge);
    }
    Ok(size)
}

pub(super) async fn stage(
    server: &Server,
    request: Request,
    staging_bytes: u64,
    cancel: &CancellationToken,
) -> Result<StagedUpload, Error> {
    let directory = server.local_staging.create(staging_bytes, cancel).await?;
    let path = directory.path().join("upload");
    let mut file = tokio::fs::File::create(&path).await?;
    let mut stream = request.into_body().into_data_stream();
    let mut size = 0_u64;
    let mut sha256 = Sha256::new();
    let mut blake3 = blake3::Hasher::new();
    let mut prefix = Vec::with_capacity(12);
    while let Some(chunk) = tokio::select! {
        () = cancel.cancelled() => return Err(Error::Cancelled),
        chunk = stream.next() => chunk,
    } {
        let chunk = chunk?;
        size = size
            .checked_add(chunk.len() as u64)
            .filter(|size| *size <= staging_bytes)
            .ok_or(Error::TooLarge)?;
        sha256.update(&chunk);
        blake3.update(&chunk);
        prefix.extend(chunk.iter().take(12 - prefix.len()).copied());
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);
    Ok(StagedUpload {
        _directory: directory,
        path,
        size,
        digest: format!("{:x}", sha256.finalize()),
        hash: *blake3.finalize().as_bytes(),
        prefix,
    })
}
