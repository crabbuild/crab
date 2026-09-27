//! Bounded traversal from a generation root to its current metadata leaf.

use super::*;
use crab_cell_runtime::client::{CellClient, InvocationError, ReadPolicy};
use extenddb_storage::error::StorageError;

/// One current leaf page and the immutable interval whose membership it samples.
pub struct DirectoryLeafPage {
    pub spec: DirectorySpec,
    pub version: u64,
    pub ranges: Vec<RoutePagePartition>,
}

/// Resolve a logical position through at most 128 directory nodes.
///
/// The caller must authorize the root through the table generation. Unavailable
/// nodes fail the traversal; they never imply missing membership or deletion.
pub async fn read_directory_leaf(
    client: &CellClient,
    tenant: TenantId,
    table_id: &str,
    hash: [u8; 16],
) -> std::result::Result<DirectoryLeafPage, StorageError> {
    let client = client.clone().with_read_policy(ReadPolicy::CurrentOwner);
    let mut spec = DirectorySpec::root(table_id.into());
    let mut publication = None;
    for _ in 0..128 {
        let target = target_for_tenant(tenant, &spec)
            .map_err(|error| StorageError::Internal(error.to_string()))?;
        let mut page = client
            .query::<ReadDirectoryPage>(
                &target,
                None,
                Json(DirectoryPageInput {
                    hash,
                    expected_version: None,
                }),
            )
            .await
            .map_err(crate::backend::cell_error)?
            .output
            .0;
        if page == DirectoryPage::Unavailable
            && let Some(split) = publication.take()
        {
            // A published parent is the durable authorization to finish opening
            // its copied child. This repairs a lost controller after cutover;
            // unavailable/missing roots are never bootstrapped by traversal.
            client
                .command::<OpenDirectory>(
                    &target,
                    crate::backend::mutation_identity()?,
                    Json(split),
                )
                .await
                .map_err(crate::backend::cell_error)?;
            page = client
                .query::<ReadDirectoryPage>(
                    &target,
                    None,
                    Json(DirectoryPageInput {
                        hash,
                        expected_version: None,
                    }),
                )
                .await
                .map_err(crate::backend::cell_error)?
                .output
                .0;
        }
        match page {
            DirectoryPage::Leaf { version, ranges } => {
                return Ok(DirectoryLeafPage {
                    spec,
                    version,
                    ranges,
                });
            }
            DirectoryPage::Redirect(split) => {
                let child = split
                    .children
                    .iter()
                    .find(|child| child.contains(hash))
                    .cloned()
                    .ok_or_else(|| {
                        StorageError::Internal("directory redirect misses logical position".into())
                    })?;
                if child.table_id != spec.table_id
                    || child.depth != spec.depth + 1
                    || child.lower < spec.lower
                    || spec
                        .upper
                        .is_some_and(|upper| child.upper.is_none_or(|end| end > upper))
                {
                    return Err(StorageError::Internal(
                        "directory redirect escaped its parent".into(),
                    ));
                }
                publication = Some(*split);
                spec = child;
            }
            DirectoryPage::Unavailable | DirectoryPage::Changed => {
                return Err(StorageError::Transient(
                    "directory is not available for traversal".into(),
                ));
            }
        }
    }
    Err(StorageError::Internal(
        "directory traversal exceeded depth bound".into(),
    ))
}

// Both controllers validate the retained full plan and child fingerprints before
// publication. A competing controller can finish and remove that plan meanwhile;
// accept its result only when no transfer remains and both exact children route.
pub(crate) async fn publish_directory_transfer(
    client: &CellClient,
    account_id: &str,
    directory: &CellTarget,
    plan: &DirectoryTransfer,
) -> std::result::Result<(), StorageError> {
    let changed = || StorageError::Transient("split publication state changed".into());
    match client
        .command::<PublishDirectoryTransfer>(
            directory,
            crate::backend::mutation_identity()?,
            Json(plan.clone()),
        )
        .await
    {
        Ok(result) if result.output.0 => {}
        Ok(_) | Err(InvocationError::Rejected(_)) => {
            let pending = client
                .query::<ReadDirectoryTransfer>(
                    directory,
                    None,
                    Json(DirectoryPartitionInput {
                        table_id: plan.table_id().into(),
                        partition_id: plan.directory_change().source.partition_id,
                    }),
                )
                .await
                .map_err(crate::backend::cell_error)?;
            if pending.output.0.is_some() {
                return Err(changed());
            }
        }
        Err(error) => return Err(crate::backend::cell_error(error)),
    }
    if crate::split_route_state(client, account_id, plan).await? != crate::SplitRouteState::After {
        return Err(changed());
    }
    Ok(())
}
