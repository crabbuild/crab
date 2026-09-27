//! Reclaim obsolete range residency while retaining durable history.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use crab_cell_runtime::cell::{actor::CellHandle, catalog::CatalogProof};
use crab_cell_runtime::client::{CellClient, ReadPolicy};
use crab_cell_runtime::control::{
    Control, ControlState, Owner,
    authority::{CellAuthority, VersionedControl},
};
use crab_cell_runtime::identity::{CellId, CellTarget};
use crab_cell_runtime::recovery::manifest::RecoveryManifestStore;
use crab_ltx::CellReplica;
use extenddb_storage::error::StorageError;

use super::{CellInitialPartitionProvisioner, provision_error};
use crate::backend::cell_error;
use crate::{
    DescribeTableById, DirectoryPartitionInput, GlobalIndexState, Json, PartitionState,
    PublishedPartitionInput, PublishedPartitionOutcome, ReadDirectoryRange, ReadDirectoryTransfer,
    ReadGlobalIndexPartition, ReadGlobalIndexState, ReadPartitionSplitPlan, ReadPartitionState,
    ReadPublishedPartition,
};

impl CellInitialPartitionProvisioner {
    // Admission is held. Metadata and existing published owners must remain
    // reachable when durable Cells outnumber resident slots. New range creation
    // still requires free capacity so placement can choose another node.
    pub(super) async fn release_range_for_admission(
        &self,
        target: &CellTarget,
    ) -> Result<(), StorageError> {
        if ![
            crate::NAMESPACE,
            crate::credentials::NAMESPACE,
            crate::directory::NAMESPACE,
        ]
        .contains(&target.namespace())
            && CellAuthority::new(self.layout.clone())
                .load(target.cell_id())
                .await
                .map_err(provision_error)?
                .is_none_or(|observed| observed.value().root.is_none())
        {
            return Ok(());
        }
        let ranges: HashSet<_> = self
            .runtime
            .active_cell_targets()
            .await
            .map_err(provision_error)?
            .into_iter()
            .filter(|target| {
                [crate::DATA_NAMESPACE, crate::global_index::NAMESPACE]
                    .contains(&target.namespace())
            })
            .map(|target| target.cell_id())
            .collect();
        let candidate = self
            .runtime
            .idle_transfer_candidates()
            .await
            .map_err(provision_error)?
            .into_iter()
            .filter(|(cell, _, _, _)| *cell != target.cell_id() && ranges.contains(cell))
            .min_by_key(|(_, _, last_used, _)| *last_used);
        if let Some((cell, generation, _, _)) = candidate {
            // Release changes residency only. Durable items, intents, and range
            // fences restore through ordinary owner resolution; busy work stays.
            self.release_capacity(cell, generation).await?;
        }
        Ok(())
    }

    pub(crate) async fn reclaim_placement_capacity(
        &self,
        target: &CellTarget,
    ) -> Result<(), StorageError> {
        if self.runtime.stats().active_cells() < self.runtime.stats().active_cell_capacity() {
            return Ok(());
        }
        let _admission = self.admission.lock().await;
        // Placement samples the local pool before activation. Apply the same
        // reclamation policy here so a full pool cannot hide a restorable root.
        self.reclaim_settled_capacity(target).await
    }

    // Admission is held. Terminal directory state is irreversible, so release
    // needs no account lookup that could recursively request another slot.
    pub(super) async fn release_retired_directory(&self) -> Result<bool, StorageError> {
        let stats = self.runtime.stats();
        if stats.active_cells() < stats.active_cell_capacity() {
            return Ok(false);
        }
        let targets: HashMap<_, _> = self
            .runtime
            .active_cell_targets()
            .await
            .map_err(provision_error)?
            .into_iter()
            .filter(|target| target.namespace() == crate::directory::NAMESPACE)
            .map(|target| (target.cell_id(), target))
            .collect();
        let mut candidates = self
            .runtime
            .idle_transfer_candidates()
            .await
            .map_err(provision_error)?;
        candidates.sort_by_key(|(_, _, last_used, _)| *last_used);
        let client = CellClient::local_runtime(
            self.application.registry(),
            self.runtime.clone(),
            self.layout.clone(),
        );
        for (cell, generation, _, _) in candidates {
            let Some(target) = targets.get(&cell) else {
                continue;
            };
            let state = client
                .query::<crate::ReadDirectory>(target, None, Json(()))
                .await
                .map_err(cell_error)?;
            if state
                .output
                .0
                .is_some_and(|state| state.mode == crate::DirectoryMode::Retired)
            {
                // Runtime rechecks residency generation and settled work. The
                // retained root still fences delayed split/open commands.
                self.release_capacity(cell, generation).await?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) async fn restore_idle(
        &self,
        target: &CellTarget,
        proof: CatalogProof,
        observed: VersionedControl,
    ) -> crab_cell_runtime::Result<Option<CellHandle>> {
        let restorable = |control: &Control| {
            control.root.is_some()
                && match control.state {
                    ControlState::Idle => control.owner.is_none(),
                    ControlState::Recovering => control
                        .owner
                        .as_ref()
                        .is_some_and(|owner| owner.session == self.session),
                    _ => false,
                }
        };
        if !restorable(observed.value()) {
            return Ok(None);
        }
        let _admission = self.admission.lock().await;
        let authority = CellAuthority::new(self.layout.clone());
        let observed = authority
            .load(target.cell_id())
            .await?
            .ok_or(crab_cell_runtime::Error::CellNotActive)?;
        if let Some(handle) = self.runtime.local_handle(proof.clone(), &observed).await? {
            return Ok(Some(handle));
        }
        if !restorable(observed.value()) {
            return Ok(None);
        }
        self.reclaim_settled_capacity(target)
            .await
            .map_err(admission_error)?;
        self.activate_published(target, proof, observed)
            .await
            .map(Some)
    }

    // Callers hold admission and have checked Idle or our Recovering claim.
    // Sharing activation keeps request and background recovery on the same root.
    pub(super) fn activate_published<'a>(
        &'a self,
        target: &'a CellTarget,
        proof: CatalogProof,
        observed: VersionedControl,
    ) -> impl Future<Output = crab_cell_runtime::Result<CellHandle>> + Send + 'a {
        // Recovery composes nested activation and root-verification futures.
        // Keep this cold path on the heap so admission callers do not carry
        // its state through every parent poll frame.
        Box::pin(async move {
            let authority = CellAuthority::new(self.layout.clone());
            let replica = CellReplica::new(
                self.layout.clone(),
                *target.cell_id().as_bytes(),
                *observed.value().incarnation.as_bytes(),
                self.replica_limits(target)?,
            )?;
            let destination = self
                .activation_destination(target)
                .map_err(admission_error)?;
            // Reads restore only an existing published root. Initial catalog and
            // authority creation belong exclusively to explicit provisioning.
            let handle = if observed.value().owner.is_some() {
                // A canceled request can leave our ownership CAS published before
                // restoration reaches the actor. Resume that exact root and epoch.
                self.runtime
                    .activate_restored(
                        proof,
                        replica,
                        authority,
                        observed,
                        RecoveryManifestStore::new(
                            self.layout.clone(),
                            self.replica_limits(target)?,
                        )
                        .with_recovery_scratch(self.directory.clone()),
                        destination,
                    )
                    .await?
            } else {
                self.runtime
                    .acquire_idle_restored(
                        proof,
                        replica,
                        authority,
                        observed,
                        destination,
                        Owner {
                            session: self.session,
                            endpoint: self.endpoint.clone(),
                        },
                    )
                    .await?
            };
            self.track_coordinator(target).map_err(admission_error)?;
            Ok(handle)
        })
    }

    pub(crate) async fn reclaim_retired_ranges(
        &self,
        client: &CellClient,
        account: &CellTarget,
        retained_source: Option<CellId>,
    ) -> Result<(), StorageError> {
        let stats = self.runtime.stats();
        if stats.active_cells() < stats.active_cell_capacity() {
            return Ok(());
        }
        let mut retired = HashSet::new();
        let mut tables = HashMap::new();
        let local = CellClient::local_runtime(
            self.application.registry(),
            self.runtime.clone(),
            self.layout.clone(),
        );
        // Missing metadata is permanent only for this exact generation and
        // only at the account owner. A stale replica cannot authorize release.
        let client = client.clone().with_read_policy(ReadPolicy::CurrentOwner);
        for entry in self
            .runtime
            .active_catalog_entries()
            .await
            .map_err(provision_error)?
        {
            if ![crate::DATA_NAMESPACE, crate::global_index::NAMESPACE].contains(&entry.namespace())
            {
                continue;
            }
            let target = CellTarget::new(
                account.tenant(),
                account.application(),
                entry.namespace(),
                entry.partition(),
            )
            .map_err(provision_error)?;
            // An unfinished controller may resume after publication. Keep its
            // sealed export source resident throughout copy/open recovery.
            if target.cell_id() != entry.cell() || retained_source == Some(entry.cell()) {
                continue;
            }
            let (table_id, partition_id, index_id, sealed) =
                if entry.namespace() == crate::DATA_NAMESPACE {
                    let Some(status) = local
                        .query::<ReadPartitionState>(&target, None, Json(()))
                        .await
                        .map_err(cell_error)?
                        .output
                        .0
                    else {
                        continue;
                    };
                    (
                        status.spec.table.id,
                        status.spec.partition_id,
                        None,
                        matches!(status.state, PartitionState::Sealed(_)),
                    )
                } else {
                    let Some(spec) = local
                        .query::<ReadGlobalIndexPartition>(&target, None, Json(()))
                        .await
                        .map_err(cell_error)?
                        .output
                        .0
                    else {
                        continue;
                    };
                    let sealed = matches!(
                        local
                            .query::<ReadGlobalIndexState>(&target, None, Json(()))
                            .await
                            .map_err(cell_error)?
                            .output
                            .0,
                        Some(GlobalIndexState::Sealed(_))
                    );
                    (
                        spec.table.id,
                        spec.partition_id,
                        Some((spec.index.id, spec.lower.unwrap_or([0; 16]))),
                        sealed,
                    )
                };
            let present = match tables.get(&table_id) {
                Some(present) => *present,
                None => {
                    let present = client
                        .query::<DescribeTableById>(account, None, Json(table_id.clone()))
                        .await
                        .map_err(cell_error)?
                        .output
                        .0
                        .is_some();
                    tables.insert(table_id.clone(), present);
                    present
                }
            };
            if present {
                if !sealed {
                    continue;
                }
                let unpublished = if let Some((index_id, lower)) = index_id {
                    let directory =
                        crate::global_index_directory_target(&client, account, &index_id, lower)
                            .await?;
                    let input = DirectoryPartitionInput {
                        table_id: index_id,
                        partition_id,
                    };
                    // Keep the sealed source resident while copying/opening is
                    // pending. Completion removes its durable participant reservation.
                    client
                        .query::<ReadDirectoryTransfer>(&directory, None, Json(input.clone()))
                        .await
                        .map_err(cell_error)?
                        .output
                        .0
                        .is_none()
                        && client
                            .query::<ReadDirectoryRange>(&directory, None, Json(input))
                            .await
                            .map_err(cell_error)?
                            .output
                            .0
                            .is_none()
                } else {
                    let input = PublishedPartitionInput {
                        table_id,
                        partition_id,
                    };
                    client
                        .query::<ReadPartitionSplitPlan>(account, None, Json(input.clone()))
                        .await
                        .map_err(cell_error)?
                        .output
                        .0
                        .is_none()
                        && matches!(
                            client
                                .query::<ReadPublishedPartition>(account, None, Json(input))
                                .await
                                .map_err(cell_error)?
                                .output
                                .0,
                            PublishedPartitionOutcome::Missing
                        )
                };
                if !unpublished {
                    continue;
                }
            }
            // Generation IDs are never reused and sealed sources cannot reopen.
            // Release residency only: replay and recovery retain the durable root.
            retired.insert(entry.cell());
        }
        if retired.is_empty() {
            // Ordinary admission can still reclaim a settled coordinator.
            return Ok(());
        }
        // A just-committed range may await the runtime's inventory refresh.
        // Wait for eligibility; unknown or busy state never authorizes release.
        let candidate = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                // Another admission or maintenance sweep can release these
                // owners while this caller waits for inventory settlement.
                let stats = self.runtime.stats();
                if stats.active_cells() < stats.active_cell_capacity()
                    || !self
                        .runtime
                        .active_cell_targets()
                        .await
                        .map_err(provision_error)?
                        .iter()
                        .any(|target| retired.contains(&target.cell_id()))
                {
                    return Ok(None);
                }
                let candidates = self
                    .runtime
                    .idle_transfer_candidates()
                    .await
                    .map_err(provision_error)?;
                if let Some((cell, generation, _, _)) = candidates
                    .into_iter()
                    .filter(|(cell, _, _, _)| retired.contains(cell))
                    .min_by_key(|(_, _, last_used, _)| *last_used)
                {
                    return Ok::<_, StorageError>(Some((cell, generation)));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .map_err(|_| StorageError::Transient("retired ranges have not settled".into()))??;
        let Some(candidate) = candidate else {
            return Ok(());
        };
        // Metadata lookup may itself restore an idle account. Hold the local
        // admission gate only for release, avoiding recursive admission waits.
        let _admission = self.admission.lock().await;
        let stats = self.runtime.stats();
        if stats.active_cells() < stats.active_cell_capacity() {
            return Ok(());
        }
        self.release_capacity(candidate.0, candidate.1).await
    }

    pub(super) async fn release_capacity(
        &self,
        cell: CellId,
        generation: u64,
    ) -> Result<(), StorageError> {
        let mut release = self
            .runtime
            .release_idle_cell(cell, self.session, generation)
            .await;
        if matches!(release, Err(crab_cell_runtime::Error::Capacity(_))) {
            // Movement credits replenish each second. Retrying preserves the
            // runtime's generation, settled-work, and authority checks.
            tokio::time::sleep(Duration::from_secs(1)).await;
            release = self
                .runtime
                .release_idle_cell(cell, self.session, generation)
                .await;
        }
        release.map_err(|error| match error {
            crab_cell_runtime::Error::Capacity(_) => StorageError::Transient(error.to_string()),
            _ => provision_error(error),
        })
    }
}

fn admission_error(source: StorageError) -> crab_cell_runtime::Error {
    crab_cell_runtime::Error::PeerTransport {
        context: "BeyondDB owner admission",
        source: Box::new(source),
    }
}
