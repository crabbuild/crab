//! Resume a frozen metadata leaf through durable child copies and parent cutover.

use super::*;
use crate::{
    DirectoryCopyReceipt, DirectoryInstall, DirectoryMode, DirectoryPage, DirectoryPageInput,
    DirectorySpec, DirectorySplit, DirectorySplitPublication, FreezeDirectory, InstallDirectory,
    OpenDirectory, PublishDirectorySplit, ReadDirectory, ReadDirectoryPage, RoutePagePartition,
    directory_target,
};

impl CellInitialPartitionProvisioner {
    pub(crate) async fn recover_index_directory_path(
        &self,
        client: &CellClient,
        account_id: &str,
        index_id: &str,
        after: Option<[u8; 16]>,
        nodes: &NodeDirectory,
    ) -> Result<(), StorageError> {
        let mut hash = after.unwrap_or([0; 16]);
        // At a leaf boundary, pagination first checks the previous leaf and then
        // crosses into its neighbour. Restore only those two bounded paths.
        for crossing in 0..2 {
            let mut spec = DirectorySpec::root(index_id.into());
            for _ in 0..128 {
                let (target, _) = self.published_directory(account_id, &spec).await?;
                self.recover_discovered_owner(
                    &target,
                    crate::directory::MODULE,
                    crate::initialize_directory,
                    nodes,
                )
                .await?;
                let state = client
                    .query::<ReadDirectory>(&target, None, Json(()))
                    .await
                    .map_err(cell_error)?
                    .output
                    .0
                    .ok_or_else(|| StorageError::Transient("directory state is missing".into()))?;
                match state.mode {
                    DirectoryMode::Branch(split) => {
                        spec = split
                            .children
                            .into_iter()
                            .find(|child| {
                                child.lower <= hash && child.upper.is_none_or(|upper| hash < upper)
                            })
                            .ok_or_else(|| {
                                StorageError::Internal("directory recovery has no child".into())
                            })?;
                    }
                    DirectoryMode::Leaf | DirectoryMode::Frozen(_) | DirectoryMode::Importing => {
                        break;
                    }
                    DirectoryMode::Retiring { .. } | DirectoryMode::Retired => {
                        return Err(StorageError::Transient(
                            "index directory is retiring".into(),
                        ));
                    }
                }
            }
            let page = crate::read_directory_leaf(
                client,
                account_target(account_id)
                    .map_err(provision_error)?
                    .tenant(),
                index_id,
                hash,
            )
            .await?;
            if crossing == 0
                && let Some(after) = after
                && !page.ranges.iter().any(|range| range.lower > after)
                && let Some(next) = page.spec.upper
            {
                hash = next;
                continue;
            }
            return Ok(());
        }
        Ok(())
    }

    async fn existing_directory_client(
        &self,
        client: &CellClient,
        account_id: &str,
        spec: &DirectorySpec,
    ) -> Result<CellClient, StorageError> {
        // Published paths authorize restoration, never a replacement empty root.
        // Once verified, shared admission routes to live peers or fences expired
        // owners before moving authority; it does not force local ownership.
        let (target, _) = self.published_directory(account_id, spec).await?;
        self.provision_range(&target, client).await
    }

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
        let parent_client = self
            .existing_directory_client(client, account_id, spec)
            .await?;
        let state = parent_client
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
                let split = parent_client
                    .command::<FreezeDirectory>(&parent, mutation_identity()?, Json(state.version))
                    .await
                    .map_err(cell_error)?
                    .output
                    .0
                    .ok_or_else(|| StorageError::Transient("directory cannot split yet".into()))?;
                (split, false)
            }
            DirectoryMode::Importing | DirectoryMode::Retiring { .. } | DirectoryMode::Retired => {
                return Err(StorageError::Transient("directory copy is not open".into()));
            }
        };
        if !published {
            let mut copies: [Vec<RoutePagePartition>; 2] = [Vec::new(), Vec::new()];
            let mut hash = spec.lower;
            loop {
                let page = parent_client
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
                let target = directory_target(account_id, child).map_err(provision_error)?;
                let child_client = self.provision_range(&target, client).await?;
                let installed = child_client
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
            let published = parent_client
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
            let child_client = self
                .existing_directory_client(client, account_id, child)
                .await?;
            let target = directory_target(account_id, child).map_err(provision_error)?;
            let opened = child_client
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

impl CellInitialPartitionProvisioner {
    /// Advance distributed directory retirement before bounded account cleanup.
    pub async fn continue_table_deletion(
        &self,
        client: &CellClient,
        account_id: &str,
        table_id: &str,
    ) -> Result<bool, StorageError> {
        let account = account_target(account_id).map_err(provision_error)?;
        let pending = client
            .query::<crate::ReadPendingDirectoryRetirement>(&account, None, Json(table_id.into()))
            .await
            .map_err(cell_error)?
            .output
            .0;
        if let Some(pending) = pending {
            let spec = pending.spec;
            if !pending.published {
                // The account's durable creation intent authorizes this root.
                // Unlike a published tree, it may never have been installed.
                let target = directory_target(account_id, &spec).map_err(provision_error)?;
                self.reclaim_retired_ranges(client, &account, None).await?;
                let client = self.provision_range(&target, client).await?;
                client
                    .command::<crate::RetireDirectory>(
                        &target,
                        mutation_identity()?,
                        Json(spec.clone()),
                    )
                    .await
                    .map_err(cell_error)?;
            }
            if !self
                .retire_directory_step(client, account_id, &spec)
                .await?
            {
                return Ok(false);
            }
            let directory = directory_target(account_id, &spec).map_err(provision_error)?;
            let observed = client
                .query::<ReadDirectory>(&directory, None, Json(()))
                .await
                .map_err(cell_error)?;
            if !observed
                .output
                .0
                .is_some_and(|state| state.spec == spec && state.mode == DirectoryMode::Retired)
            {
                return Err(StorageError::Transient(
                    "directory retirement is not complete".into(),
                ));
            }
            client
                .command::<crate::RecordTableDirectoryRetirement>(
                    &account,
                    mutation_identity()?,
                    Json(crate::TableDirectoryRetirement {
                        table_id: table_id.into(),
                        index_id: spec.table_id,
                        sequence: observed.receipt.commit_sequence,
                    }),
                )
                .await
                .map_err(cell_error)?;
        }
        Ok(client
            .command::<crate::ContinueTableDeletion>(
                &account,
                mutation_identity()?,
                Json(table_id.into()),
            )
            .await
            .map_err(cell_error)?
            .output
            .0)
    }

    /// Advance one generation-fenced directory retirement by a bounded path.
    ///
    /// Call only after the table lifecycle authority has fenced the generation.
    /// Returns true only when every planned descendant is durably retired.
    pub async fn retire_directory_step(
        &self,
        client: &CellClient,
        account_id: &str,
        root: &DirectorySpec,
    ) -> Result<bool, StorageError> {
        let mut current = root.clone();
        let mut parent = None;
        let mut published = true;
        for _ in 0..128 {
            let target = directory_target(account_id, &current).map_err(provision_error)?;
            let current_client = if published {
                self.existing_directory_client(client, account_id, &current)
                    .await?
            } else {
                // The retiring parent retains the unpublished split intent. A
                // missing copy needs a terminal fence against delayed installers.
                let current_client = self.provision_range(&target, client).await?;
                current_client
                    .command::<crate::RetireDirectory>(
                        &target,
                        mutation_identity()?,
                        Json(current.clone()),
                    )
                    .await
                    .map_err(cell_error)?;
                current_client
            };
            let observed = current_client
                .query::<ReadDirectory>(&target, None, Json(()))
                .await
                .map_err(cell_error)?;
            let state = observed.output.0.ok_or_else(|| {
                StorageError::Transient("published directory state is missing".into())
            })?;
            if state.spec != current {
                return Err(StorageError::Internal(
                    "directory retirement scope changed".into(),
                ));
            }
            let (mode, sequence) = if matches!(
                state.mode,
                DirectoryMode::Retiring { .. } | DirectoryMode::Retired
            ) {
                (state.mode, observed.receipt.commit_sequence)
            } else {
                let retired = current_client
                    .command::<crate::RetireDirectory>(
                        &target,
                        mutation_identity()?,
                        Json(current.clone()),
                    )
                    .await
                    .map_err(cell_error)?;
                (
                    retired.output.0.ok_or_else(|| {
                        StorageError::Transient("directory retirement was rejected".into())
                    })?,
                    retired.receipt.commit_sequence,
                )
            };
            match mode {
                DirectoryMode::Retired => {
                    let Some(parent) = parent else {
                        return Ok(true);
                    };
                    let parent_client = self
                        .existing_directory_client(client, account_id, &parent)
                        .await?;
                    let target = directory_target(account_id, &parent).map_err(provision_error)?;
                    let recorded = parent_client
                        .command::<crate::RecordDirectoryRetirement>(
                            &target,
                            mutation_identity()?,
                            Json(crate::DirectoryRetirementReceipt {
                                parent: parent.clone(),
                                child_id: current.node_id,
                                sequence,
                            }),
                        )
                        .await
                        .map_err(cell_error)?;
                    return Ok(parent == *root && recorded.output.0);
                }
                DirectoryMode::Retiring {
                    children,
                    acknowledged,
                    published: children_published,
                } => {
                    let next = children
                        .into_iter()
                        .enumerate()
                        .find(|(position, _)| acknowledged & (1 << position) == 0)
                        .map(|(_, child)| child)
                        .ok_or_else(|| {
                            StorageError::Internal(
                                "directory retirement has no pending child".into(),
                            )
                        })?;
                    parent = Some(current);
                    current = next;
                    published = children_published;
                }
                _ => {
                    return Err(StorageError::Internal(
                        "directory retirement did not fence writes".into(),
                    ));
                }
            }
        }
        Err(StorageError::Internal(
            "directory retirement exceeded depth bound".into(),
        ))
    }
}
