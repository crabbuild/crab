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
mod acquire;
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
    /// Handle that owns the migrated Cell.
    pub handle: CellHandle,
    /// Outcome the migration published.
    pub outcome: MigrationOutcome,
}

impl CellRuntime {
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
