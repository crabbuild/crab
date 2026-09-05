use std::{io, io::Write as _, sync::Arc, time::Duration};

use axum::{
    Extension,
    body::Body,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use crab_remote_git::{
    ArchiveEntry, EntryKind, Error as RemoteError, OperationKind, RepositoryOptions, Revision,
    RevisionError,
};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::{CancellationToken, DropGuard};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

use crate::{auth::Principal, server::Server};

const ARCHIVE_DURATION: Duration = Duration::from_secs(10 * 60);
const ARCHIVE_RESPONSE_BYTES: u64 = 3 * 1024 * 1024 * 1024;
const ENTRY_CHANNEL_CAPACITY: usize = 2;
const OUTPUT_CHANNEL_CAPACITY: usize = 8;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Parameters {
    rev: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("repository not found")]
    NotFound,
    #[error("archive transfer capacity is busy")]
    Busy,
    #[error("repository service failed")]
    Service(#[from] crate::Error),
    #[error("remote archive read failed")]
    Remote(#[from] RemoteError),
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let busy = matches!(&self, Self::Busy);
        let should_log = !matches!(&self, Self::NotFound | Self::Busy);
        let (status, code, message) = match &self {
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                "repository_not_found",
                "Repository not found",
            ),
            Self::Busy => (
                StatusCode::TOO_MANY_REQUESTS,
                "busy",
                "Repository transfer capacity is busy. Retry shortly",
            ),
            Self::Remote(RemoteError::EmptyRepository) => (
                StatusCode::NOT_FOUND,
                "empty_repository",
                "This repository has no commits yet",
            ),
            Self::Remote(RemoteError::Revision {
                reason: RevisionError::NotFound | RevisionError::NotReachable,
            }) => (
                StatusCode::NOT_FOUND,
                "not_found",
                "The requested revision is not available",
            ),
            Self::Remote(RemoteError::Revision { .. }) => (
                StatusCode::BAD_REQUEST,
                "invalid_revision",
                "The requested revision is invalid",
            ),
            Self::Remote(RemoteError::InvalidLimit { .. }) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "configuration",
                "Repository archive limits are invalid",
            ),
            Self::Remote(RemoteError::RepositoryIndexing { .. }) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "indexing",
                "Repository metadata is still being indexed. Retry shortly",
            ),
            Self::Remote(RemoteError::Cancelled) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "cancelled",
                "The archive was cancelled",
            ),
            Self::Remote(RemoteError::Timeout { .. }) => (
                StatusCode::GATEWAY_TIMEOUT,
                "timeout",
                "The repository archive timed out",
            ),
            Self::Remote(RemoteError::LimitExceeded { .. }) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "read_limit",
                "This archive exceeds the repository read budget",
            ),
            Self::Service(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "indexing_failed",
                "Repository indexing could not finish. Check storage permissions and retry",
            ),
            Self::Remote(_) => (
                StatusCode::BAD_GATEWAY,
                "remote_read",
                "Repository data could not be read. Check storage and repository health",
            ),
        };
        if should_log {
            tracing::error!(error = ?self, "repository archive request failed");
        }
        let response = axum::Json(serde_json::json!({"error":{"code":code,"message":message}}));
        if busy {
            return (status, [(header::RETRY_AFTER, "1")], response).into_response();
        }
        (status, response).into_response()
    }
}

enum ArchiveMessage {
    Entry(ArchiveEntry),
    Finish,
    Abort { cancelled: bool },
}

struct ChannelWriter {
    sender: mpsc::Sender<io::Result<Bytes>>,
    receiver_closed: bool,
    failed: bool,
    written: u64,
    maximum: u64,
}

impl io::Write for ChannelWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.receiver_closed || self.failed {
            return Ok(bytes.len());
        }
        let next = self
            .written
            .checked_add(bytes.len() as u64)
            .filter(|next| *next <= self.maximum);
        let Some(next) = next else {
            // ZipWriter finalizes on drop. Accept those cleanup writes after
            // returning the one limit error so finalization cannot log to stderr.
            self.failed = true;
            return Err(io::Error::other("ZIP response exceeds its byte limit"));
        };
        self.written = next;
        if self
            .sender
            .blocking_send(Ok(Bytes::copy_from_slice(bytes)))
            .is_err()
        {
            self.receiver_closed = true;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) async fn download(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
    Query(parameters): Query<Parameters>,
) -> Result<Response, Error> {
    let entry = server
        .repositories
        .get(&(owner, name))
        .filter(|entry| principal.can_read(&entry.config))
        .ok_or(Error::NotFound)?;
    let permit = Arc::clone(&server.git_admission)
        .try_acquire_owned()
        .map_err(|_| Error::Busy)?;
    let cancellation = server.cancellation.child_token();
    let guard = cancellation.clone().drop_guard();
    let repository = entry
        .open_current(&server, archive_options(server.options)?, &cancellation)
        .await?;
    let revision = match parameters.rev.as_deref() {
        Some(value) => Revision::parse(value)?,
        None => Revision::Reference(
            repository
                .refs()
                .head
                .as_ref()
                .ok_or(RemoteError::EmptyRepository)?
                .name
                .clone(),
        ),
    };
    let operation = repository
        .operation(OperationKind::Archive, &cancellation)
        .await?;
    let snapshot = repository.snapshot(&revision, &operation).await?;
    let commit = snapshot.commit_oid();
    let stream = snapshot.archive_stream(operation)?;
    let prefix = format!("{}-{}", entry.config.name, &commit.to_string()[..7]);
    let filename = format!("{prefix}.zip");
    let (entries_tx, entries_rx) = mpsc::channel(ENTRY_CHANNEL_CAPACITY);
    let (output_tx, output_rx) = mpsc::channel(OUTPUT_CHANNEL_CAPACITY);
    spawn_archive_reader(stream, entries_tx, cancellation);
    spawn_zip_writer(prefix, entries_rx, output_tx);
    let body = response_body(output_rx, permit, guard);
    Ok((
        [
            (header::CONTENT_TYPE, "application/zip".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
            (header::CACHE_CONTROL, "private, no-cache".to_owned()),
            (
                header::HeaderName::from_static("x-crab-commit"),
                commit.to_string(),
            ),
        ],
        body,
    )
        .into_response())
}

fn archive_options(options: RepositoryOptions) -> Result<RepositoryOptions, RemoteError> {
    let mut operation = options.operation_limits();
    operation.max_duration = ARCHIVE_DURATION;
    operation.max_response_bytes = ARCHIVE_RESPONSE_BYTES;
    operation.max_fetched_bytes = operation.max_fetched_bytes.max(ARCHIVE_RESPONSE_BYTES);
    operation.max_inflated_bytes = operation.max_inflated_bytes.max(ARCHIVE_RESPONSE_BYTES);
    operation.max_logical_objects = operation.max_logical_objects.max(300_000);
    operation.max_storage_requests = operation.max_storage_requests.max(600_000);
    RepositoryOptions::new(options.object_limits(), operation)
}

fn spawn_archive_reader(
    mut stream: crab_remote_git::ArchiveStream,
    sender: mpsc::Sender<ArchiveMessage>,
    cancellation: CancellationToken,
) {
    tokio::spawn(async move {
        use futures_util::StreamExt as _;

        while let Some(result) = stream.next().await {
            match result {
                Ok(entry) => {
                    if sender.send(ArchiveMessage::Entry(entry)).await.is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let cancelled =
                        cancellation.is_cancelled() || matches!(&error, RemoteError::Cancelled);
                    if !cancelled {
                        tracing::error!(error = ?error, "repository archive traversal failed");
                    }
                    let _ = sender.send(ArchiveMessage::Abort { cancelled }).await;
                    return;
                }
            }
        }
        let _ = sender.send(ArchiveMessage::Finish).await;
    });
}

fn spawn_zip_writer(
    prefix: String,
    receiver: mpsc::Receiver<ArchiveMessage>,
    sender: mpsc::Sender<io::Result<Bytes>>,
) {
    tokio::task::spawn_blocking(move || {
        let failure_sender = sender.clone();
        if let Err(error) = write_zip(
            &prefix,
            receiver,
            ChannelWriter {
                sender,
                receiver_closed: false,
                failed: false,
                written: 0,
                maximum: ARCHIVE_RESPONSE_BYTES,
            },
        ) {
            if error.kind() != io::ErrorKind::Interrupted {
                tracing::error!(error = %error, "repository ZIP encoding failed");
            }
            let _ = failure_sender.blocking_send(Err(error));
        }
    });
}

fn response_body(
    receiver: mpsc::Receiver<io::Result<Bytes>>,
    permit: tokio::sync::OwnedSemaphorePermit,
    guard: DropGuard,
) -> Body {
    let stream = futures_util::stream::unfold(
        (receiver, permit, guard),
        |(mut receiver, permit, guard)| async move {
            receiver
                .recv()
                .await
                .map(|item| (item, (receiver, permit, guard)))
        },
    );
    Body::from_stream(stream)
}

fn write_zip(
    prefix: &str,
    mut receiver: mpsc::Receiver<ArchiveMessage>,
    output: ChannelWriter,
) -> io::Result<()> {
    let mut writer = ZipWriter::new_stream(output);
    writer
        .add_directory(
            format!("{prefix}/"),
            SimpleFileOptions::default().unix_permissions(0o755),
        )
        .map_err(zip_error)?;
    loop {
        match receiver.blocking_recv() {
            Some(ArchiveMessage::Entry(entry)) => write_entry(&mut writer, prefix, entry)?,
            Some(ArchiveMessage::Finish) => {
                writer.finish().map_err(zip_error)?;
                return Ok(());
            }
            Some(ArchiveMessage::Abort { cancelled }) => {
                writer.finish().map_err(zip_error)?;
                if cancelled {
                    return Ok(());
                }
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "repository archive traversal failed",
                ));
            }
            None => return Err(io::Error::other("repository archive traversal stopped")),
        }
    }
}

fn write_entry(
    writer: &mut ZipWriter<zip::write::StreamWriter<ChannelWriter>>,
    prefix: &str,
    entry: ArchiveEntry,
) -> io::Result<()> {
    let name = format!("{prefix}/{}", archive_path(entry.path.as_bytes()));
    let permissions = entry.mode.raw() & 0o777;
    match entry.kind {
        EntryKind::Tree | EntryKind::Submodule => writer
            .add_directory(name, SimpleFileOptions::default().unix_permissions(0o755))
            .map_err(zip_error),
        EntryKind::Symlink => {
            let target = entry
                .bytes
                .ok_or_else(|| io::Error::other("archive symlink has no target"))?;
            match std::str::from_utf8(&target) {
                Ok(target) => writer
                    .add_symlink(
                        name,
                        target,
                        SimpleFileOptions::default().unix_permissions(permissions),
                    )
                    .map_err(zip_error),
                Err(_) => write_regular(writer, name, permissions, &target),
            }
        }
        EntryKind::Blob => {
            let bytes = entry
                .bytes
                .ok_or_else(|| io::Error::other("archive blob has no bytes"))?;
            write_regular(writer, name, permissions, &bytes)
        }
    }
}

fn write_regular(
    writer: &mut ZipWriter<zip::write::StreamWriter<ChannelWriter>>,
    name: String,
    permissions: u32,
    bytes: &[u8],
) -> io::Result<()> {
    writer
        .start_file(
            name,
            SimpleFileOptions::default()
                .compression_method(CompressionMethod::Deflated)
                .unix_permissions(permissions),
        )
        .map_err(zip_error)?;
    writer.write_all(bytes)
}

fn archive_path(path: &[u8]) -> String {
    path.split(|byte| *byte == b'/')
        .map(archive_component)
        .collect::<Vec<_>>()
        .join("/")
}

fn archive_component(component: &[u8]) -> String {
    if matches!(component, b"." | b"..") {
        return component.iter().map(|_| "%2E").collect();
    }
    match std::str::from_utf8(component) {
        Ok(text) => {
            let mut safe = String::new();
            for character in text.chars() {
                if character == '%' || character == '\\' || character.is_control() {
                    for byte in character.to_string().bytes() {
                        safe.push_str(&format!("%{byte:02X}"));
                    }
                } else {
                    safe.push(character);
                }
            }
            safe
        }
        Err(_) => component
            .iter()
            .map(|byte| format!("%{byte:02X}"))
            .collect(),
    }
}

fn zip_error(error: zip::result::ZipError) -> io::Error {
    io::Error::other(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_paths_cannot_escape_the_repository_root() {
        assert_eq!(
            archive_path(b".././.\\name/%file"),
            "%2E%2E/%2E/.%5Cname/%25file"
        );
        assert_eq!(archive_path(b"valid/\xffname"), "valid/%FF%6E%61%6D%65");
    }

    #[test]
    fn archive_response_writer_enforces_encoded_byte_limit_once() {
        let (sender, _receiver) = mpsc::channel(1);
        let mut writer = ChannelWriter {
            sender,
            receiver_closed: false,
            failed: false,
            written: 0,
            maximum: 1,
        };

        assert!(writer.write_all(b"too large").is_err());
        assert!(writer.write_all(b"cleanup").is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn closed_response_still_finishes_zip_cleanup() {
        let (messages, receiver) = mpsc::channel(1);
        messages.send(ArchiveMessage::Finish).await.unwrap();
        drop(messages);
        let (output, response) = mpsc::channel(1);
        drop(response);

        tokio::task::spawn_blocking(move || {
            write_zip(
                "repo-1111111",
                receiver,
                ChannelWriter {
                    sender: output,
                    receiver_closed: false,
                    failed: false,
                    written: 0,
                    maximum: ARCHIVE_RESPONSE_BYTES,
                },
            )
        })
        .await
        .unwrap()
        .unwrap();
    }

    #[tokio::test]
    async fn dropping_response_cancels_and_releases_transfer() {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = Arc::clone(&semaphore).acquire_owned().await.unwrap();
        let cancellation = CancellationToken::new();
        let (_sender, receiver) = mpsc::channel(1);
        let body = response_body(receiver, permit, cancellation.clone().drop_guard());

        drop(body);

        assert!(cancellation.is_cancelled());
        assert_eq!(semaphore.available_permits(), 1);
    }
}
