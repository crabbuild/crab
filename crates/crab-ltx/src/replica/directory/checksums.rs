use std::path::{Path, PathBuf};

use futures_util::{StreamExt as _, stream};

use super::{Header, Verification, read_node, verify_branch, verify_leaf};
use crate::{CrabError, Host, Limits, Result, environment::FileIo, pages::PageChecksums};

const LEAF_READS_IN_FLIGHT: usize = 8;

pub(in crate::replica) async fn load_checksums(
    verification: Verification<'_>,
    root: [u8; 32],
    height: u32,
    destination: &Path,
    limits: Limits,
) -> Result<PageChecksums> {
    if height > 3 || verification.database_pages == 0 {
        return Err(CrabError::LTXCorrupted);
    }
    if u64::from(verification.database_pages) * 8 > limits.max_database_bytes {
        return Err(CrabError::Limit(crate::LimitKind::ChecksumFileBytes));
    }
    let mut writer = ChecksumWriter::create(verification.host, destination).await?;
    let result = async {
        let mut pending = vec![vec![(root, height, None)]];
        let mut previous_page = 0u32;
        let mut seen = 0u64;
        let mut checksum = crate::CHECKSUM_FLAG;
        while let Some(batch) = pending.pop() {
            // Batch only sibling leaves. Internal nodes remain depth first so
            // prefetched branches cannot reorder page coverage or checksums.
            // At most eight reads use the shared host I/O admission.
            let mut reads = stream::iter(batch.into_iter().map(|(digest, remaining, expected)| {
                let verification = &verification;
                async move {
                    read_node(verification, digest)
                        .await
                        .map(|bytes| (bytes, remaining, expected))
                }
            }))
            .buffered(LEAF_READS_IN_FLIGHT);
            while let Some(read) = reads.next().await {
                let (bytes, remaining, expected) = read?;
                let header = Header::parse(&bytes)?;
                if (remaining == 0) != (header.kind == 0) {
                    return Err(CrabError::LTXCorrupted);
                }
                if header.kind == 0 {
                    let (aggregate, entries) = verify_leaf(
                        &bytes,
                        &header,
                        verification.page_size,
                        verification.database_pages,
                        verification.extents,
                    )?;
                    if expected.is_some_and(|value| value != aggregate) {
                        return Err(CrabError::ChecksumMismatch);
                    }
                    for entry in entries {
                        let expected_page = previous_page
                            .checked_add(1)
                            .ok_or(CrabError::LTXCorrupted)?;
                        let lock = crate::ltx::lock_pgno(verification.page_size);
                        if expected_page == lock {
                            writer.append(0).await?;
                            previous_page = lock;
                        }
                        if entry.page
                            != previous_page
                                .checked_add(1)
                                .ok_or(CrabError::LTXCorrupted)?
                        {
                            return Err(CrabError::LTXCorrupted);
                        }
                        writer.append(entry.checksum).await?;
                        checksum = crate::CHECKSUM_FLAG | (checksum ^ entry.checksum);
                        previous_page = entry.page;
                        seen += 1;
                    }
                    continue;
                }
                let (aggregate, children) = verify_branch(&bytes, &header)?;
                if expected.is_some_and(|value| value != aggregate) {
                    return Err(CrabError::ChecksumMismatch);
                }
                let next = remaining.checked_sub(1).ok_or(CrabError::LTXCorrupted)?;
                let batch_size = if next == 0 { super::FANOUT } else { 1 };
                pending.extend(children.chunks(batch_size).rev().map(|children| {
                    children
                        .iter()
                        .map(|child| (child.digest, next, Some(child.aggregate)))
                        .collect()
                }));
            }
        }
        let lock = crate::ltx::lock_pgno(verification.page_size);
        if previous_page < verification.database_pages {
            if previous_page
                .checked_add(1)
                .ok_or(CrabError::LTXCorrupted)?
                != lock
                || lock != verification.database_pages
            {
                return Err(CrabError::LTXCorrupted);
            }
            writer.append(0).await?;
        }
        let expected =
            u64::from(verification.database_pages) - u64::from(lock <= verification.database_pages);
        if seen != expected {
            return Err(CrabError::LTXCorrupted);
        }
        writer.flush().await?;
        let page_size = verification.page_size;
        let database_pages = verification.database_pages;
        writer
            .run(move |output| {
                let mut file = output.file.take().ok_or(CrabError::LTXCorrupted)?;
                file.sync_all()?;
                drop(file);
                output.host.filesystem.sync_parent(&output.path)?;
                PageChecksums::from_file(
                    crate::LtxHost {
                        // Only activation work retains admission. The immutable
                        // checksum base lives with the writer after delivery.
                        facilities: output
                            .host
                            .clone()
                            .without_recovery()
                            .without_dirty()
                            .without_scratch(),
                        max_database_bytes: limits.max_database_bytes,
                        max_file_bytes: limits.max_database_bytes,
                    },
                    &output.path,
                    page_size,
                    database_pages,
                    checksum,
                )
            })
            .await
    }
    .await;
    if result.is_ok() {
        // Disarm after delivery: cancellation of a dispatched final sync must
        // still remove its undelivered activation file.
        if let Some(output) = &mut writer.output {
            output.keep = true;
        }
    } else {
        let _ = writer
            .run(|output| {
                output.remove();
                Ok(())
            })
            .await;
    }
    result
}

struct ChecksumWriter {
    output: Option<ChecksumFile>,
    buffer: Vec<u8>,
}

impl ChecksumWriter {
    async fn create(host: &Host, destination: &Path) -> Result<Self> {
        let destination = destination.to_owned();
        let path = crate::resume::checksum_path(&destination);
        let runtime = tokio::runtime::Handle::try_current().map_err(std::io::Error::other)?;
        let job_host = host.clone();
        let output = host
            .run(move || {
                if job_host.filesystem.exists(&destination)? || job_host.filesystem.exists(&path)? {
                    return Err(CrabError::InvalidState(
                        "writable activation destination already exists",
                    ));
                }
                let file = job_host.filesystem.create(&path)?;
                Ok(ChecksumFile {
                    host: job_host,
                    path,
                    file: Some(file),
                    runtime,
                    keep: false,
                })
            })
            .await??;
        Ok(Self {
            output: Some(output),
            buffer: Vec::with_capacity(64 << 10),
        })
    }

    async fn append(&mut self, checksum: u64) -> Result<()> {
        self.buffer.extend_from_slice(&checksum.to_be_bytes());
        if self.buffer.len() >= 64 << 10 {
            self.flush().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let mut bytes = std::mem::take(&mut self.buffer);
        self.buffer = self
            .run(move |output| {
                output
                    .file
                    .as_mut()
                    .ok_or(CrabError::LTXCorrupted)?
                    .write_all(&bytes)?;
                bytes.clear();
                Ok(bytes)
            })
            .await?;
        Ok(())
    }

    async fn run<T: Send + 'static>(
        &mut self,
        operation: impl FnOnce(&mut ChecksumFile) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let mut output = self.output.take().ok_or(CrabError::LTXCorrupted)?;
        let host = output.host.clone();
        let (output, result) = host
            .run(move || {
                let result = operation(&mut output);
                (output, result)
            })
            .await?;
        self.output = Some(output);
        result
    }
}

struct ChecksumFile {
    host: Host,
    path: PathBuf,
    file: Option<Box<dyn FileIo>>,
    runtime: tokio::runtime::Handle,
    keep: bool,
}

impl ChecksumFile {
    fn remove(&mut self) {
        drop(self.file.take());
        if self.host.filesystem.remove_file(&self.path).is_ok() {
            let _ = self.host.filesystem.sync_parent(&self.path);
        }
        self.keep = true;
    }
}

impl Drop for ChecksumFile {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        let mut cleanup = Self {
            host: self.host.clone(),
            path: std::mem::take(&mut self.path),
            file: self.file.take(),
            runtime: self.runtime.clone(),
            keep: true,
        };
        // Cancellation can occur between network reads or while a file job
        // is queued. Keep the file and dirty admission through admitted cleanup;
        // never block the async worker or release capacity before cleanup ends.
        self.runtime.spawn(async move {
            let host = cleanup.host.clone();
            let _ = host.run(move || cleanup.remove()).await;
        });
    }
}
