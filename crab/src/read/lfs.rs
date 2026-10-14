use super::{CrabError, LfsPointer, Result, SnapshotReader, check_cancelled};
use bytes::Bytes;
use crab_lfs::LfsObjectStore;
use futures_util::{Stream, StreamExt as _};
use std::path::Path;
use tokio::io::AsyncWriteExt as _;

impl SnapshotReader {
    pub(super) async fn download_lfs_to_path(
        &self,
        path: &str,
        pointer: &LfsPointer,
        destination: &Path,
    ) -> Result<u64> {
        let stream = self.lfs_stream(path, pointer).await?;
        tokio::pin!(stream);
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        let (file, temporary) = tempfile::Builder::new()
            .prefix(".crab-lfs-")
            .tempfile_in(parent)?
            .into_parts();
        let mut file = tokio::fs::File::from_std(file);
        let result = async {
            while let Some(chunk) = stream.next().await {
                file.write_all(&chunk?).await?;
            }
            Ok::<_, CrabError>(())
        }
        .await;
        // Flush joins queued filesystem writes even after source cancellation.
        // The sibling path stays private until verified EOF and writer success.
        let flushed = file.flush().await;
        drop(file);
        flushed?;
        result?;
        check_cancelled(&self.repo.inner.cancel)?;
        temporary
            .persist(destination)
            .map_err(|error| CrabError::Io(error.error))?;
        Ok(pointer.size)
    }

    pub(super) async fn write_lfs_to_writer<W: std::io::Write>(
        &self,
        path: &str,
        pointer: &LfsPointer,
        mut writer: W,
    ) -> Result<u64> {
        let stream = self.lfs_stream(path, pointer).await?;
        tokio::pin!(stream);
        while let Some(chunk) = stream.next().await {
            writer.write_all(&chunk?)?;
        }
        check_cancelled(&self.repo.inner.cancel)?;
        writer.flush()?;
        Ok(pointer.size)
    }

    async fn lfs_stream(
        &self,
        path: &str,
        pointer: &LfsPointer,
    ) -> Result<impl Stream<Item = Result<Bytes>>> {
        let remote = self.repo.inner.remote().await?;
        let store = LfsObjectStore::new(
            remote.caching_store.origin().clone(),
            remote.router.repo_prefix(),
        );
        let cancel = self.repo.inner.cancel.clone();
        let (_, _, stream) = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(CrabError::Cancelled),
            result = store.get_stream_with_session(&pointer.oid, pointer.size, None, &remote.lfs_reads) => {
                result.map_err(|source| lfs_error(path, source))?
            }
        };
        let path = path.to_owned();
        Ok(futures_util::stream::try_unfold(
            (stream, cancel, path),
            |(mut stream, cancel, path)| async move {
                let next = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(CrabError::Cancelled),
                    next = stream.next() => next,
                };
                match next {
                    Some(chunk) => Ok(Some((
                        chunk.map_err(|source| lfs_error(&path, source))?,
                        (stream, cancel, path),
                    ))),
                    None => Ok(None),
                }
            },
        ))
    }
}

fn lfs_error(path: &str, source: crab_lfs::LfsError) -> CrabError {
    match source {
        // Tagged v1.2.4 selective materialization exposes LFS integrity through
        // CorruptObject. Keep its diagnostic while shared LFS owns verification.
        crab_lfs::LfsError::ObjectCorrupt { oid } => CrabError::CorruptObject {
            path: path.to_owned(),
            reason: format!("LFS object SHA-256 or size does not match the pointer: oid {oid}"),
        },
        source => source.into(),
    }
}
