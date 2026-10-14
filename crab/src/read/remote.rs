use std::sync::Arc;

use bytes::Bytes;
use crab_read::capsule_protocol::CapsuleRepositoryView;
use crab_remote_git::{
    ContentClassification, EntryKind, GitPath, OperationKind, RemoteGitRepository,
    RemoteGitRuntime, RemoteGitSnapshot, RepositoryIdentity, RepositoryOptions, Revision,
    TreeListingItem, TreeListingRequest,
};
use crab_storage::{Store, StoreLayout};
use tokio_util::sync::CancellationToken;

use super::{DownloadEntry, DownloadEntryKind, classify_blob_bytes};
use crate::core::error::{CrabError, Result};

pub(super) struct RemoteSnapshot {
    repository: RemoteGitRepository,
    snapshot: RemoteGitSnapshot,
}

impl RemoteSnapshot {
    pub(super) async fn open(
        view: &CapsuleRepositoryView,
        layout: StoreLayout<Store>,
        runtime: Arc<RemoteGitRuntime>,
        maximum: u64,
        revision: &str,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let bucket = layout.store().bucket_identity();
        let provider = format!("{:?}:{}:{}", bucket.cloud, bucket.host, bucket.container);
        let identity =
            RepositoryIdentity::new(provider, layout.repo_prefix(), 1).map_err(remote_error)?;
        let profile = crab_read::upload_pack_repository_options().map_err(remote_error)?;
        let mut object = profile.object_limits();
        object.max_object_bytes = maximum;
        object.max_packed_entry_bytes = maximum;
        object.max_inflated_entry_bytes = maximum;
        let mut operation = profile.operation_limits();
        operation.max_fetched_bytes = maximum;
        operation.max_inflated_bytes = maximum;
        operation.max_response_bytes = maximum;
        // Explicit complete-file/tree selection retains the transfer profile's
        // repository-size coverage rather than the interactive browse page cap.
        operation.max_entries = operation.max_logical_objects;
        operation.max_history_commits = operation.max_logical_objects;
        let options = RepositoryOptions::new(object, operation).map_err(remote_error)?;
        let repository = view
            .git_repository_from_store(layout, identity, runtime, options, maximum, cancel)
            .await?;
        let revision = Revision::from_oid_hex(revision).map_err(remote_error)?;
        let operation = repository
            .operation(OperationKind::Snapshot, cancel)
            .await
            .map_err(remote_error)?;
        let result = repository.snapshot(&revision, &operation).await;
        let snapshot = operation.finish(result).await.map_err(remote_error)?;
        Ok(Self {
            repository,
            snapshot,
        })
    }

    pub(super) fn commit_oid(&self) -> gix_hash::ObjectId {
        self.snapshot.commit_oid()
    }

    pub(super) async fn blob_bytes(&self, path: &str, cancel: &CancellationToken) -> Result<Bytes> {
        let path_bytes = normalized_path(path)?;
        let operation = self
            .repository
            .operation(OperationKind::Content, cancel)
            .await
            .map_err(remote_error)?;
        let result = async {
            let entry = self
                .snapshot
                .entry(&path_bytes, &operation)
                .await?
                .ok_or(crab_remote_git::Error::PathNotFound)?;
            // Selective materialization never follows symlinks or gitlinks.
            if entry.kind != EntryKind::Blob {
                return Err(crab_remote_git::Error::PathNotFound);
            }
            self.snapshot
                .read_blob(&path_bytes, &operation)
                .await
                .map(|blob| blob.bytes)
        }
        .await;
        match operation.finish(result).await {
            Err(
                crab_remote_git::Error::PathNotFound
                | crab_remote_git::Error::PathComponentNotTree { .. },
            ) => Err(CrabError::NotFound {
                path: path.to_owned(),
            }),
            other => other.map_err(remote_error),
        }
    }

    pub(super) async fn list_entries(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Vec<DownloadEntry>> {
        let operation = self
            .repository
            .operation(OperationKind::Tree, cancel)
            .await
            .map_err(remote_error)?;
        let result = async {
            let mut entries = Vec::new();
            let mut after = None;
            loop {
                let request = TreeListingRequest::new(Bytes::new(), after, None, 1000)?;
                let page = self.snapshot.list_tree_blobs(&request, &operation).await?;
                after = page
                    .items
                    .last()
                    .map(|item| Bytes::copy_from_slice(item.path()));
                for item in page.items {
                    let TreeListingItem::Blob(entry) = item else {
                        continue;
                    };
                    let path = std::str::from_utf8(entry.path.as_bytes())
                        .map_err(|source| {
                            crab_remote_git::Error::Storage(crab_storage::StorageError::Io {
                                source: std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    source,
                                ),
                            })
                        })?
                        .to_owned();
                    let metadata = self.snapshot.blob_metadata(&entry.path, &operation).await?;
                    let value = if metadata.classification == ContentClassification::OrdinaryGit {
                        DownloadEntry {
                            path,
                            size: metadata.git_size,
                            kind: DownloadEntryKind::Git,
                            content_hash: None,
                        }
                    } else {
                        let blob = self.snapshot.read_blob(&entry.path, &operation).await?;
                        classify_blob_bytes(&path, &blob.bytes).map_err(|source| {
                            crab_remote_git::Error::Storage(crab_storage::StorageError::Io {
                                source: std::io::Error::other(source),
                            })
                        })?
                    };
                    entries.push(value);
                }
                if !page.has_more {
                    break;
                }
            }
            Ok::<_, crab_remote_git::Error>(entries)
        }
        .await;
        // The product result must survive until the locator session closes;
        // cleanup errors still propagate even after a successful selection.
        operation.finish(result).await.map_err(remote_error)
    }
}

fn normalized_path(path: &str) -> Result<GitPath> {
    let normalized = path
        .split('/')
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    if normalized.is_empty() {
        return Err(CrabError::NotFound {
            path: path.to_owned(),
        });
    }
    GitPath::new(Bytes::from(normalized)).map_err(remote_error)
}

fn remote_error(source: crab_remote_git::Error) -> CrabError {
    CrabError::from(crab_read::ReadError::RemoteGit(source))
}

#[cfg(test)]
mod tests;
