//! One Cell's actor: dispatcher, admission, lifecycle, requests, and runtime administration.
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use tokio::{
    sync::{Semaphore, mpsc, oneshot},
    task::JoinSet,
};

mod handle;
use state::*;
use task::*;
mod admission;
mod lifecycle;
mod requests;
mod runtime;
mod state;
mod task;
mod tasks;

use lifecycle::*;
use requests::*;

pub use handle::CellHandle;
use handle::{CellAdmission, WorkAdmission};

use crate::Error;
use crate::cell::catalog::{CatalogEntry, CatalogProof, CatalogRole};
use crate::cell::executor::{MAX_PENDING_PUBLICATIONS, PENDING_PUBLICATION_HIGH_WATER_BYTES};
use crate::cell::executor::{
    MigrationOutcome, MutationIdentity, PendingCommit, Resolution, StoredOutcome,
};
use crate::cell::worker::{CellReservation, Handler, Initializer, QueryHandler, WorkerState};
use crate::cell::worker::{SqlWorkerPool, WorkerExecution};
use crate::control::authority::{CellAuthority, VersionedControl};
use crate::control::{Owner, Transition};
use crate::coordination::{
    AdmissionKind, CoordinationDecision, CoordinationEffect, CoordinationInput, CoordinationState,
    RejectReason, Residency,
};
use crate::fleet::eviction::{EvictionObservation, EvictionState, select_victims};
use crate::fleet::pressure::{
    MovementBudget, MovementPermit, PressureClassifier, PressureSample, PressureState,
};
use crate::fleet::resource::{
    ACTIVE_CELL_NATIVE_BYTES as ACTIVE_CELL_NATIVE_BYTES_USIZE, LedgerDiskAdmission,
    LedgerHostResourceAdmission, ResourceCost, ResourceLedger, ResourceReservation,
};
use crate::identity::{ApplicationId, CellId, CellTarget, Digest, SessionId};
use crate::node::durability::NodeDurability;
use crate::node::lease::NodeLeaseGuard;
use crate::primitives::effects::InboxDelivery;
use crate::publication::CellPublisher;
use crate::publication::{CellDurabilitySubmitter, NodeDurabilitySlot, PendingDurability};
use crate::registry::MigrationPlan;

const INGRESS_REQUESTS: usize = 1_024;
const CELL_REQUESTS: usize = 64;
// A repository attribution read may reserve a 1 MiB result plus its encoded
// request. Allow a normal burst of concurrent reads; node-wide retained-byte
// admission remains the aggregate safety ceiling.
const CELL_BYTES: usize = 16 * 1024 * 1024;
const RENEWAL_SCAN: std::time::Duration = std::time::Duration::from_millis(100);
const MAX_RENEWALS_IN_FLIGHT: usize = 32;
const SQL_WALL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);
const HYDRATION_TICK: std::time::Duration = std::time::Duration::from_millis(100);
const COMPACTION_QUIET: std::time::Duration = std::time::Duration::from_millis(250);
const COMPACTION_RETRY: std::time::Duration = std::time::Duration::from_secs(1);
const HYDRATION_PAGES_PER_STEP: u32 = 64;
const FLEET_PUBLICATION_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

const fn bounded_u32(value: usize) -> u32 {
    if value > u32::MAX as usize {
        u32::MAX
    } else {
        value as u32
    }
}

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Conservative per-active-Cell reservation for actor state and native tasks.
///
/// This is admission accounting rather than an RSS guarantee. Embedders must
/// qualify the estimate against their compiled registry and workload.
pub const ACTIVE_CELL_NATIVE_BYTES: u64 = ACTIVE_CELL_NATIVE_BYTES_USIZE as u64;

/// Node-wide dispatcher for bounded per-Cell command mailboxes.
#[derive(Clone)]
pub struct CellRuntime {
    inner: Arc<RuntimeInner>,
}

/// Opaque node-wide byte reservation held until it is dropped.
#[must_use = "dropping the reservation immediately releases its capacity"]
pub struct NodeByteReservation {
    _reservation: ResourceReservation,
}

/// Opaque node-wide worker-job reservation held until a primitive job exits.
#[must_use = "dropping the reservation immediately releases its capacity"]
pub struct NodeJobReservation {
    _reservation: ResourceReservation,
}

/// Point-in-time node admission usage for one embedded Cell runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellRuntimeStats {
    active_cells: usize,
    active_cell_capacity: usize,
    resident_bytes: usize,
    resident_capacity_bytes: usize,
    file_descriptors: usize,
    file_descriptor_capacity: usize,
    retained_bytes: usize,
    retained_capacity_bytes: usize,
    worker_jobs: usize,
    worker_job_capacity: usize,
    primitive_jobs: usize,
    primitive_job_capacity: usize,
    hydration_jobs: usize,
    hydration_job_capacity: usize,
    io_slots: usize,
    io_slot_capacity: usize,
    blocking_jobs: usize,
    blocking_job_capacity: usize,
    recovery_jobs: usize,
    recovery_job_capacity: usize,
    dirty_jobs: usize,
    dirty_job_capacity: usize,
    scratch_units: usize,
    scratch_unit_capacity: usize,
    local_disk_reserved_bytes: u64,
    local_disk_capacity_bytes: u64,
    unpublished_node_log_bytes: u64,
}

impl CellRuntimeStats {
    /// Returns the number of admitted Cell actors.
    #[must_use]
    pub const fn active_cells(self) -> usize {
        self.active_cells
    }

    /// Returns the maximum number of Cell actors admitted by this runtime.
    #[must_use]
    pub const fn active_cell_capacity(self) -> usize {
        self.active_cell_capacity
    }

    /// Returns resident native bytes reserved by active Cells.
    #[must_use]
    pub const fn resident_bytes(self) -> usize {
        self.resident_bytes
    }

    /// Returns the resident native-byte ceiling.
    #[must_use]
    pub const fn resident_capacity_bytes(self) -> usize {
        self.resident_capacity_bytes
    }

    /// Returns file descriptors reserved by active Cells in the shared ledger.
    #[must_use]
    pub const fn file_descriptors(self) -> usize {
        self.file_descriptors
    }

    /// Returns the active-Cell file-descriptor ceiling in the shared ledger.
    #[must_use]
    pub const fn file_descriptor_capacity(self) -> usize {
        self.file_descriptor_capacity
    }

    /// Returns the bytes currently reserved by node-wide native work.
    #[must_use]
    pub const fn retained_bytes(self) -> usize {
        self.retained_bytes
    }

    /// Returns the node-wide native-work byte capacity.
    #[must_use]
    pub const fn retained_capacity_bytes(self) -> usize {
        self.retained_capacity_bytes
    }

    /// Returns SQL jobs currently admitted by the shared ledger.
    #[must_use]
    pub const fn worker_jobs(self) -> usize {
        self.worker_jobs
    }

    /// Returns the SQL-job ceiling in the shared ledger.
    #[must_use]
    pub const fn worker_job_capacity(self) -> usize {
        self.worker_job_capacity
    }

    /// Returns primitive jobs currently admitted by the shared ledger.
    #[must_use]
    pub const fn primitive_jobs(self) -> usize {
        self.primitive_jobs
    }

    /// Returns the primitive-job ceiling in the shared ledger.
    #[must_use]
    pub const fn primitive_job_capacity(self) -> usize {
        self.primitive_job_capacity
    }

    /// Returns background hydration jobs currently admitted by the shared ledger.
    #[must_use]
    pub const fn hydration_jobs(self) -> usize {
        self.hydration_jobs
    }

    /// Returns the background hydration-job ceiling in the shared ledger.
    #[must_use]
    pub const fn hydration_job_capacity(self) -> usize {
        self.hydration_job_capacity
    }

    /// Returns the active-Cell count in the bounded placement wire shape.
    #[must_use]
    pub const fn placement_active_cells(self) -> u32 {
        bounded_u32(self.active_cells)
    }

    /// Returns the active-Cell ceiling in the bounded placement wire shape.
    #[must_use]
    pub const fn placement_active_cell_capacity(self) -> u32 {
        bounded_u32(self.active_cell_capacity)
    }

    /// Returns aggregate admitted worker, primitive, and hydration jobs.
    #[must_use]
    pub const fn placement_running_jobs(self) -> u32 {
        bounded_u32(
            self.worker_jobs
                .saturating_add(self.primitive_jobs)
                .saturating_add(self.hydration_jobs),
        )
    }

    /// Returns aggregate worker, primitive, and hydration job capacity.
    #[must_use]
    pub const fn placement_job_capacity(self) -> u32 {
        bounded_u32(
            self.worker_job_capacity
                .saturating_add(self.primitive_job_capacity)
                .saturating_add(self.hydration_job_capacity),
        )
    }

    /// Returns bounded object-store I/O operations currently admitted.
    #[must_use]
    pub const fn io_slots(self) -> usize {
        self.io_slots
    }

    /// Returns the object-store I/O operation ceiling.
    #[must_use]
    pub const fn io_slot_capacity(self) -> usize {
        self.io_slot_capacity
    }

    /// Returns blocking host jobs currently admitted.
    #[must_use]
    pub const fn blocking_jobs(self) -> usize {
        self.blocking_jobs
    }

    /// Returns the blocking host-job ceiling.
    #[must_use]
    pub const fn blocking_job_capacity(self) -> usize {
        self.blocking_job_capacity
    }

    /// Returns full recovery cohorts currently admitted.
    #[must_use]
    pub const fn recovery_jobs(self) -> usize {
        self.recovery_jobs
    }

    /// Returns the full recovery-cohort ceiling.
    #[must_use]
    pub const fn recovery_job_capacity(self) -> usize {
        self.recovery_job_capacity
    }

    /// Returns dirty-memory cohorts currently admitted.
    #[must_use]
    pub const fn dirty_jobs(self) -> usize {
        self.dirty_jobs
    }

    /// Returns the dirty-memory cohort ceiling.
    #[must_use]
    pub const fn dirty_job_capacity(self) -> usize {
        self.dirty_job_capacity
    }

    /// Returns temporary scratch units currently admitted.
    #[must_use]
    pub const fn scratch_units(self) -> usize {
        self.scratch_units
    }

    /// Returns the temporary scratch-unit ceiling.
    #[must_use]
    pub const fn scratch_unit_capacity(self) -> usize {
        self.scratch_unit_capacity
    }

    /// Returns the bytes currently reserved in the local replica cache.
    #[must_use]
    pub const fn local_disk_reserved_bytes(self) -> u64 {
        self.local_disk_reserved_bytes
    }

    /// Returns the local replica-cache byte capacity.
    #[must_use]
    pub const fn local_disk_capacity_bytes(self) -> u64 {
        self.local_disk_capacity_bytes
    }

    /// Returns owner bytes submitted to node logs but not covered by object roots.
    #[must_use]
    pub const fn unpublished_node_log_bytes(self) -> u64 {
        self.unpublished_node_log_bytes
    }
}

/// New capability and publication receipt returned by one schema migration.
pub struct MigratedCell {
    pub handle: CellHandle,
    pub outcome: MigrationOutcome,
}

impl CellRuntime {
    /// Resolves an active local owner without exposing the dispatcher's Cell map.
    pub async fn local_handle(
        &self,
        catalog: CatalogProof,
        control: &VersionedControl,
    ) -> crate::Result<Option<CellHandle>> {
        self.ensure_running()?;
        let value = control.value();
        if catalog.entry().cell() != value.cell {
            return Err(Error::Control("scheduler catalog and control differ"));
        }
        if value
            .owner
            .as_ref()
            .is_none_or(|owner| owner.session != self.inner.session)
            || value.root.is_none()
        {
            return Ok(None);
        }
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::Lookup {
                cell: value.cell,
                require_resident: false,
                reply,
            })
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        let Some(local) = response.await.map_err(|_| Error::RuntimeClosed)? else {
            return Ok(None);
        };
        if local.incarnation != value.incarnation
            || local.code != value.code
            || local.schema != value.schema
        {
            return Ok(None);
        }
        Ok(Some(CellHandle {
            cell: value.cell,
            incarnation: value.incarnation,
            code: value.code,
            schema: value.schema,
            catalog,
            inner: self.inner.clone(),
            admission: local.admission,
        }))
    }

    /// Creates, initializes and publishes a new Cell before returning a handle.
    pub async fn bootstrap<F>(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
        destination: PathBuf,
        initialize: F,
    ) -> crate::Result<CellHandle>
    where
        F: for<'connection> FnOnce(
                &crab_ltx::rusqlite::Transaction<'connection>,
            ) -> crate::Result<()>
            + Send
            + 'static,
    {
        self.ensure_acquiring()?;
        self.activation_cell(&catalog, &observed)?;
        if observed.value().state != crate::control::ControlState::Recovering
            || observed.value().root.is_some()
        {
            return Err(Error::Control("bootstrap requires an unpublished control"));
        }
        let reservation = self.inner.pool.reserve_activation()?;
        let incarnation = observed.value().incarnation;
        let schema = observed.value().schema;
        self.activate_inner(
            catalog,
            Activation::Bootstrap(Box::new(BootstrapActivation {
                replica: replica.clone(),
                destination,
                incarnation,
                schema,
                initialize: Box::new(initialize),
                reservation,
            })),
            replica,
            authority,
            observed,
        )
        .await
    }

    /// Takes over an unchanged unpublished owner, initializes and publishes the Cell.
    #[expect(
        clippy::too_many_arguments,
        reason = "the takeover boundary keeps every authority and activation input explicit"
    )]
    pub async fn takeover_unpublished<F>(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        mut observed: VersionedControl,
        takeover: crate::node::NodeTakeoverProof,
        destination: PathBuf,
        owner: Owner,
        initialize: F,
    ) -> crate::Result<CellHandle>
    where
        F: for<'connection> FnOnce(
                &crab_ltx::rusqlite::Transaction<'connection>,
            ) -> crate::Result<()>
            + Send
            + 'static,
    {
        self.ensure_acquiring()?;
        let cell = self.claiming_cell(&catalog, &observed, &owner)?;
        if owner.session != takeover.claimant() {
            return Err(Error::Fenced);
        }
        loop {
            if observed.value().state != crate::control::ControlState::Recovering
                || observed.value().owner.is_none()
                || observed.value().root.is_some()
            {
                return Err(Error::Control(
                    "unpublished takeover requires an active rootless control",
                ));
            }
            if observed.value().owner.as_ref().map(|owner| owner.session)
                != Some(takeover.session())
            {
                return Err(Error::Fenced);
            }
            self.ensure_running()?;
            let current = authority.load(cell).await?.ok_or(Error::Fenced)?;
            if current.value() != observed.value() {
                self.claiming_cell(&catalog, &current, &owner)?;
                observed = current;
                continue;
            }
            let reservation = self.inner.pool.reserve_activation()?;
            let successor = current.value().takeover(owner.clone())?;
            let claimed = match authority
                .transition(&current, successor.clone(), Transition::Takeover)
                .await
            {
                Ok(claimed) => claimed,
                Err(error) => {
                    let latest = authority.load(cell).await?.ok_or(Error::Fenced)?;
                    if latest.value() == &successor {
                        latest
                    } else if matches!(
                        &error,
                        Error::Storage(crab_storage::StorageError::StateConflict { .. })
                    ) {
                        observed = latest;
                        continue;
                    } else {
                        return Err(error);
                    }
                }
            };
            let incarnation = claimed.value().incarnation;
            let schema = claimed.value().schema;
            return self
                .activate_inner(
                    catalog,
                    Activation::Bootstrap(Box::new(BootstrapActivation {
                        replica: replica.clone(),
                        destination,
                        incarnation,
                        schema,
                        initialize: Box::new(initialize),
                        reservation,
                    })),
                    replica,
                    authority,
                    claimed,
                )
                .await;
        }
    }

    /// Cold-opens the exact authoritative root on the Cell's SQL worker.
    pub async fn activate_restored(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
        recovery_store: crate::recovery::manifest::RecoveryManifestStore,
        destination: PathBuf,
    ) -> crate::Result<CellHandle> {
        self.ensure_acquiring()?;
        self.activation_cell(&catalog, &observed)?;
        let replica = self.replica_with_directory_cache(replica, &destination)?;
        let observed = self
            .publish_attached_recovery(&replica, &authority, observed, &recovery_store)
            .await?;
        let reservation = self.inner.pool.reserve_activation()?;
        self.activate_restored_reserved(
            catalog,
            replica,
            authority,
            observed,
            destination,
            reservation,
        )
        .await
    }

    /// Acquires an idle published Cell and restores its exact immutable root.
    pub async fn acquire_idle_restored(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
        destination: PathBuf,
        owner: Owner,
    ) -> crate::Result<CellHandle> {
        self.ensure_acquiring()?;
        let rollback_node_lease = self.inner.node_lease.guard()?;
        self.claiming_cell(&catalog, &observed, &owner)?;
        if observed.value().state != crate::control::ControlState::Idle
            || observed.value().owner.is_some()
            || observed.value().root.is_none()
        {
            return Err(Error::Control(
                "idle acquisition requires a published idle control",
            ));
        }
        let reservation = self.inner.pool.reserve_activation()?;
        let successor = observed.value().takeover(owner)?;
        let claimed = match authority
            .transition(&observed, successor.clone(), Transition::Takeover)
            .await
        {
            Ok(claimed) => claimed,
            Err(error) => {
                let current = authority
                    .load(observed.value().cell)
                    .await?
                    .ok_or(Error::Fenced)?;
                if current.value() != &successor {
                    return Err(error);
                }
                current
            }
        };
        let rollback_authority = authority.clone();
        let rollback_claim = claimed.clone();
        let rollback_replica = replica.clone();
        match self
            .activate_restored_reserved(
                catalog,
                replica,
                authority,
                claimed,
                destination,
                reservation,
            )
            .await
        {
            Ok(handle) => Ok(handle),
            Err(error) => {
                match rollback_failed_acquisition(
                    &rollback_authority,
                    &rollback_claim,
                    &rollback_replica,
                    rollback_node_lease,
                )
                .await
                {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(cleanup),
                }
            }
        }
    }

    /// Takes over an unchanged owner after its exact node session is fenced.
    #[expect(
        clippy::too_many_arguments,
        reason = "the takeover boundary keeps every authority, recovery and activation input explicit"
    )]
    pub async fn takeover_restored(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        mut observed: VersionedControl,
        takeover: crate::node::NodeTakeoverProof,
        recovery_store: crate::recovery::manifest::RecoveryManifestStore,
        destination: PathBuf,
        owner: Owner,
    ) -> crate::Result<CellHandle> {
        self.ensure_acquiring()?;
        let replica = self.replica_with_directory_cache(replica, &destination)?;
        let rollback_node_lease = self.inner.node_lease.guard()?;
        let rollback_authority = authority.clone();
        let rollback_replica = replica.clone();
        let cell = self.claiming_cell(&catalog, &observed, &owner)?;
        if owner.session != takeover.claimant() {
            return Err(Error::Fenced);
        }
        loop {
            if !matches!(
                observed.value().state,
                crate::control::ControlState::Recovering | crate::control::ControlState::Serving
            ) || observed.value().owner.is_none()
                || observed.value().root.is_none()
            {
                return Err(Error::Control(
                    "takeover requires a published control with an active owner",
                ));
            }
            if observed.value().owner.as_ref().map(|owner| owner.session)
                != Some(takeover.session())
            {
                return Err(Error::Fenced);
            }
            self.ensure_running()?;
            let current = authority.load(cell).await?.ok_or(Error::Fenced)?;
            if current.value() != observed.value() {
                self.claiming_cell(&catalog, &current, &owner)?;
                observed = current;
                continue;
            }
            let reservation = self.inner.pool.reserve_activation()?;
            let successor = current.value().takeover(owner.clone())?;
            let claimed = match authority
                .transition(&current, successor.clone(), Transition::Takeover)
                .await
            {
                Ok(claimed) => claimed,
                Err(error) => {
                    let latest = authority.load(cell).await?.ok_or(Error::Fenced)?;
                    if latest.value() == &successor {
                        latest
                    } else if matches!(
                        &error,
                        Error::Storage(crab_storage::StorageError::StateConflict { .. })
                    ) {
                        observed = latest;
                        continue;
                    } else {
                        return Err(error);
                    }
                }
            };
            let recovery_rollback_claim = claimed.clone();
            let claimed = match self
                .publish_attached_recovery(&replica, &authority, claimed, &recovery_store)
                .await
            {
                Ok(claimed) => claimed,
                Err(error) => {
                    match rollback_failed_acquisition(
                        &rollback_authority,
                        &recovery_rollback_claim,
                        &rollback_replica,
                        rollback_node_lease.clone(),
                    )
                    .await
                    {
                        Ok(()) => return Err(error),
                        Err(cleanup) => return Err(cleanup),
                    }
                }
            };
            let rollback_claim = claimed.clone();
            return match self
                .activate_restored_reserved(
                    catalog,
                    replica,
                    authority,
                    claimed,
                    destination,
                    reservation,
                )
                .await
            {
                Ok(handle) => Ok(handle),
                Err(error) => {
                    match rollback_failed_acquisition(
                        &rollback_authority,
                        &rollback_claim,
                        &rollback_replica,
                        rollback_node_lease,
                    )
                    .await
                    {
                        Ok(()) => Err(error),
                        Err(cleanup) => Err(cleanup),
                    }
                }
            };
        }
    }

    async fn publish_attached_recovery(
        &self,
        replica: &crab_ltx::CellReplica,
        authority: &CellAuthority,
        observed: VersionedControl,
        recovery_store: &crate::recovery::manifest::RecoveryManifestStore,
    ) -> crate::Result<VersionedControl> {
        let Some(recovery) = observed.value().recovery.as_ref() else {
            return Ok(observed);
        };
        let overlay = recovery_store
            .load_overlay(
                observed.value().cell,
                observed.value().incarnation,
                recovery,
            )
            .await?;
        let prepared = replica
            .prepare_recovered_overlay(&overlay, observed.value().schema)
            .await?;
        self.ensure_running()?;
        let successor = observed
            .value()
            .publish_recovery(&prepared, observed.value().next_due_ms)?;
        match authority
            .transition(&observed, successor.clone(), Transition::PublishRecovery)
            .await
        {
            Ok(published) => Ok(published),
            Err(error) => {
                let current = authority
                    .load(observed.value().cell)
                    .await?
                    .ok_or(Error::Fenced)?;
                if current.value() == &successor {
                    Ok(current)
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn activate_restored_reserved(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
        destination: PathBuf,
        reservation: CellReservation,
    ) -> crate::Result<CellHandle> {
        // Root verification precedes actor activation, so it must use the same
        // persistent directory cache as the publisher path below.
        let replica = self.replica_with_directory_cache(replica, &destination)?;
        let cell = self.activation_cell(&catalog, &observed)?;
        let control = observed.value();
        let root = control
            .ltx_root()
            .ok_or(Error::Control("activation requires a published root"))?;
        let incarnation = control.incarnation;
        let schema = control.schema;
        let verified = replica.open_root(&root).await?;
        if verified.schema() != schema {
            return Err(Error::Control(
                "immutable root schema does not match control",
            ));
        }
        let database = verified.paged().prepare_writable(&destination).await?;
        let current = authority.load(cell).await?.ok_or(Error::Fenced)?;
        if !current.value().is_same_or_pure_renewal_of(observed.value()) {
            return Err(Error::Fenced);
        }
        self.activate_inner(
            catalog,
            Activation::Restored(Box::new(RestoredActivation {
                database,
                destination,
                incarnation,
                schema,
                root,
                reservation,
            })),
            replica,
            authority,
            current,
        )
        .await
    }

    fn claiming_cell(
        &self,
        catalog: &CatalogProof,
        observed: &VersionedControl,
        owner: &Owner,
    ) -> crate::Result<CellId> {
        let cell = catalog.entry().cell();
        if observed.value().cell != cell {
            return Err(Error::Control("ownership control changed Cell"));
        }
        if owner.session != self.inner.session {
            return Err(Error::Fenced);
        }
        if observed
            .value()
            .owner
            .as_ref()
            .is_some_and(|current| current.session == owner.session)
        {
            return Err(Error::CellAlreadyActive);
        }
        Ok(cell)
    }

    fn activation_cell(
        &self,
        catalog: &CatalogProof,
        observed: &VersionedControl,
    ) -> crate::Result<CellId> {
        let cell = catalog.entry().cell();
        if observed.value().cell != cell {
            return Err(Error::Control("activation control changed Cell"));
        }
        if observed
            .value()
            .owner
            .as_ref()
            .is_none_or(|owner| owner.session != self.inner.session)
        {
            return Err(Error::Fenced);
        }
        Ok(cell)
    }

    fn ensure_running(&self) -> crate::Result<()> {
        if self.inner.shutting_down.load(Ordering::Acquire) {
            return Err(Error::RuntimeClosed);
        }
        self.inner.node_lease.check()
    }

    fn ensure_acquiring(&self) -> crate::Result<()> {
        self.ensure_running()?;
        if !self.inner.accepting_cells.load(Ordering::Acquire) {
            return Err(Error::CellDraining);
        }
        Ok(())
    }

    async fn activate_inner(
        &self,
        catalog: CatalogProof,
        activation: Activation,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
    ) -> crate::Result<CellHandle> {
        let cell = self.activation_cell(&catalog, &observed)?;
        let incarnation = observed.value().incarnation;
        let code = observed.value().code;
        let schema = observed.value().schema;
        let scratch_directory = match &activation {
            Activation::Restored(activation) => activation.destination.parent(),
            Activation::Bootstrap(activation) => activation.destination.parent(),
        }
        .ok_or(Error::Control("Cell activation destination has no parent"))?
        .to_owned();
        let replica = self.replica_with_directory_cache(replica, &scratch_directory)?;
        let (reply, response) = oneshot::channel();
        let mut publisher = CellPublisher::new(replica, authority, observed, scratch_directory);
        if let Some(node_lease) = self.inner.node_lease.guard()? {
            publisher = publisher.with_node_lease(node_lease);
        }
        publisher = publisher.with_node_durability_slot(Arc::clone(&self.inner.node_durability));
        publisher = publisher.with_telemetry(self.inner.telemetry.clone());
        self.inner
            .sender
            .send(Message::Activate {
                cell,
                role: catalog.entry().role(),
                activation,
                publisher: Box::new(publisher),
                reply,
            })
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        let admission = response.await.map_err(|_| Error::RuntimeClosed)??;
        Ok(CellHandle {
            cell,
            incarnation,
            code,
            schema,
            catalog,
            inner: self.inner.clone(),
            admission,
        })
    }

    fn replica_with_directory_cache(
        &self,
        replica: crab_ltx::CellReplica,
        destination: &Path,
    ) -> crate::Result<crab_ltx::CellReplica> {
        let scratch_directory = destination
            .parent()
            .ok_or(Error::Control("Cell activation destination has no parent"))?;
        let host = self
            .inner
            .replica_host
            .clone()
            .with_directory_cache(scratch_directory.join(".crab-cell-directory-cache"));
        Ok(replica.with_host(host))
    }

    /// Resolves a verified resident owner without reading catalog or authority objects.
    pub async fn resident_handle(
        &self,
        target: &CellTarget,
        role: CatalogRole,
    ) -> crate::Result<Option<CellHandle>> {
        self.ensure_running()?;
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::Lookup {
                cell: target.cell_id(),
                require_resident: true,
                reply,
            })
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        let local = match response.await {
            Ok(local) => local,
            Err(_) => {
                self.inner
                    .telemetry
                    .resident_route(crate::fleet::telemetry::ResidentRouteOutcome::Refused);
                return Err(Error::RuntimeClosed);
            }
        };
        let Some(local) = local else {
            self.inner
                .telemetry
                .resident_route(crate::fleet::telemetry::ResidentRouteOutcome::Miss);
            return Ok(None);
        };
        self.inner
            .telemetry
            .resident_route(crate::fleet::telemetry::ResidentRouteOutcome::Hit);
        let entry = CatalogEntry::new(target, role, local.code, local.schema)?;
        Ok(Some(CellHandle {
            cell: target.cell_id(),
            incarnation: local.incarnation,
            code: local.code,
            schema: local.schema,
            catalog: CatalogProof::local(entry),
            inner: self.inner.clone(),
            admission: local.admission,
        }))
    }
}

#[cfg(test)]
mod tests;
