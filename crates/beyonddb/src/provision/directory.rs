//! Resume a frozen metadata leaf through durable child copies and parent cutover.

use super::*;
use crate::{
    DirectoryCopyReceipt, DirectoryInstall, DirectoryMode, DirectoryPage, DirectoryPageInput,
    DirectorySpec, DirectorySplit, DirectorySplitPublication, FreezeDirectory, InstallDirectory,
    OpenDirectory, PublishDirectorySplit, ReadDirectory, ReadDirectoryPage, RoutePagePartition,
    directory_target,
};

impl CellInitialPartitionProvisioner {
    /// Split an existing metadata leaf, or finish its previously published split.
    ///
    /// The caller must authorize this directory through its table generation.
    /// Only the parent publishes routing; children stay fenced until that commit.
    pub async fn split_directory(
        &self,
        client: &CellClient,
        account_id: &str,
        spec: &DirectorySpec,
    ) -> Result<DirectorySplit, StorageError> {
        let parent = directory_target(account_id, spec).map_err(provision_error)?;
        let state = client
            .query::<ReadDirectory>(&parent, None, Json(()))
            .await
            .map_err(cell_error)?
            .output
            .0
            .ok_or_else(|| StorageError::Transient("directory is not installed".into()))?;
        if state.spec != *spec {
            return Err(StorageError::Internal(
                "directory scope differs from traversal".into(),
            ));
        }
        let (split, published) = match state.mode {
            DirectoryMode::Branch(split) => (split, true),
            DirectoryMode::Frozen(split) => (split, false),
            DirectoryMode::Leaf => {
                let split = client
                    .command::<FreezeDirectory>(&parent, mutation_identity()?, Json(state.version))
                    .await
                    .map_err(cell_error)?
                    .output
                    .0
                    .ok_or_else(|| StorageError::Transient("directory cannot split yet".into()))?;
                (split, false)
            }
            DirectoryMode::Importing => {
                return Err(StorageError::Transient("directory copy is not open".into()));
            }
        };
        if !published {
            let mut copies: [Vec<RoutePagePartition>; 2] = [Vec::new(), Vec::new()];
            let mut hash = spec.lower;
            loop {
                let page = client
                    .query::<ReadDirectoryPage>(
                        &parent,
                        None,
                        Json(DirectoryPageInput {
                            hash,
                            expected_version: Some(split.version),
                        }),
                    )
                    .await
                    .map_err(cell_error)?
                    .output
                    .0;
                let DirectoryPage::Leaf { ranges, .. } = page else {
                    // Another driver may already have cut over. Its durable
                    // branch remains the authority; retry from that state.
                    return Err(StorageError::Transient(
                        "directory changed during copy".into(),
                    ));
                };
                let last = ranges
                    .last()
                    .ok_or_else(|| StorageError::Internal("empty directory copy page".into()))?;
                let next = last.upper;
                for range in ranges {
                    let side = usize::from(range.lower >= split.children[1].lower);
                    copies[side].push(range);
                }
                if next == spec.upper {
                    break;
                }
                let next = next.filter(|next| *next > hash).ok_or_else(|| {
                    StorageError::Internal("directory copy made no progress".into())
                })?;
                hash = next;
            }
            let mut receipts = Vec::with_capacity(2);
            for (index, (child, ranges)) in split.children.iter().zip(copies).enumerate() {
                self.admit_directory(account_id, child).await?;
                let target = directory_target(account_id, child).map_err(provision_error)?;
                let installed = client
                    .command::<InstallDirectory>(
                        &target,
                        mutation_identity()?,
                        Json(DirectoryInstall {
                            spec: child.clone(),
                            ranges,
                            source: Some(split.clone()),
                        }),
                    )
                    .await
                    .map_err(cell_error)?;
                if !installed.output.0 {
                    return Err(StorageError::Transient(
                        "directory child copy was rejected".into(),
                    ));
                }
                receipts.push(DirectoryCopyReceipt {
                    cell_id: *target.cell_id().as_bytes(),
                    sequence: installed.receipt.commit_sequence,
                    fingerprint: split.fingerprints[index],
                });
            }
            let receipts = receipts.try_into().map_err(|_| {
                StorageError::Internal("directory copy receipts are incomplete".into())
            })?;
            let published = client
                .command::<PublishDirectorySplit>(
                    &parent,
                    mutation_identity()?,
                    Json(DirectorySplitPublication {
                        split: split.clone(),
                        receipts,
                    }),
                )
                .await
                .map_err(cell_error)?;
            if !published.output.0 {
                return Err(StorageError::Transient(
                    "directory split publication was rejected".into(),
                ));
            }
        }
        for child in &split.children {
            // A published parent never authorizes bootstrapping a missing child.
            // Restore its verified root before acknowledging that it is open.
            self.admit_existing_directory(account_id, child).await?;
            let target = directory_target(account_id, child).map_err(provision_error)?;
            let opened = client
                .command::<OpenDirectory>(&target, mutation_identity()?, Json(split.clone()))
                .await
                .map_err(cell_error)?;
            if !opened.output.0 {
                return Err(StorageError::Transient(
                    "directory child opening was rejected".into(),
                ));
            }
        }
        Ok(split)
    }
}
