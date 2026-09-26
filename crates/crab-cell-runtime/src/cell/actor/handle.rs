use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, oneshot};

use super::admission::new_cell_admission;
use super::state::{
    Message, QueuedCommand, QueuedMigration, QueuedOperation, QueuedQuery, QueuedResolve,
    ResolveOperation, RuntimeInner,
};

use crate::Error;
use crate::cell::actor::MigratedCell;
use crate::cell::catalog::CatalogProof;
use crate::cell::catalog::CatalogRole;
use crate::cell::executor::StoredOutcome;
use crate::cell::executor::{MutationIdentity, Resolution};
use crate::fleet::resource::{ResourceCost, ResourceReservation};
use crate::identity::IncarnationId;
use crate::identity::{CellId, Digest};
use crate::primitives::effects::InboxDelivery;
use crate::primitives::maintenance::PersistedWorkInventory;
use crate::registry::MigrationPlan;

pub(super) const MAX_OPERATION_BYTES: usize = crate::codec::MAX_WIRE_BYTES;
pub(super) const MAX_RESULT_BYTES: usize = crate::codec::MAX_WIRE_BYTES;

/// Cloneable capability for one activated Cell.
#[derive(Clone)]
pub struct CellHandle {
    pub(super) cell: CellId,
    pub(super) incarnation: IncarnationId,
    pub(super) code: Digest,
    pub(super) schema: u32,
    pub(super) catalog: CatalogProof,
    pub(super) inner: Arc<RuntimeInner>,
    pub(super) admission: Arc<CellAdmission>,
}

pub(super) struct CellAdmission {
    pub(super) requests: Arc<Semaphore>,
    pub(super) bytes: Arc<Semaphore>,
    pub(super) draining: AtomicBool,
    pub(super) fenced: AtomicBool,
}

pub(super) struct WorkAdmission {
    pub(super) _request: OwnedSemaphorePermit,
    pub(super) _cell_bytes: OwnedSemaphorePermit,
    pub(super) _node_bytes: ResourceReservation,
}

/// One resident Cell whose published due time has passed.
///
/// A scheduler ticks such a Cell through the handle it already holds, so due
/// work does not wait for a fleet scan to reach it. `expected_commit_sequence`
/// is what the last authoritative publication named: a Tick built from it is a
/// no-op when the Cell committed again in the meantime.
pub struct DueResident {
    handle: CellHandle,
    expected_commit_sequence: u64,
    next_due_ms: i64,
}

impl DueResident {
    pub(super) const fn new(
        handle: CellHandle,
        expected_commit_sequence: u64,
        next_due_ms: i64,
    ) -> Self {
        Self {
            handle,
            expected_commit_sequence,
            next_due_ms,
        }
    }

    /// Returns the resident handle to dispatch the Tick through.
    #[must_use]
    pub const fn handle(&self) -> &CellHandle {
        &self.handle
    }

    /// Returns the commit sequence the last publication named.
    #[must_use]
    pub const fn expected_commit_sequence(&self) -> u64 {
        self.expected_commit_sequence
    }

    /// Returns the due time this Cell published.
    #[must_use]
    pub const fn next_due_ms(&self) -> i64 {
        self.next_due_ms
    }
}

impl CellHandle {
    /// Returns the catalog proof that authorized this activation.
    #[must_use]
    pub const fn catalog(&self) -> &CatalogProof {
        &self.catalog
    }

    /// Returns the Cell this handle addresses.
    #[must_use]
    pub const fn cell_id(&self) -> CellId {
        self.cell
    }

    /// Returns the incarnation the activation belongs to.
    #[must_use]
    pub const fn incarnation(&self) -> IncarnationId {
        self.incarnation
    }

    /// Returns the application code digest the Cell serves.
    #[must_use]
    pub const fn code(&self) -> Digest {
        self.code
    }

    /// Returns the schema version the Cell serves.
    #[must_use]
    pub const fn schema(&self) -> u32 {
        self.schema
    }

    /// Runs and publishes one command while retaining admission after cancellation.
    pub async fn execute<F>(
        &self,
        identity: MutationIdentity,
        operation_digest: Digest,
        now_ms: i64,
        operation_bytes: usize,
        max_result_bytes: usize,
        handler: F,
    ) -> crate::Result<StoredOutcome>
    where
        F: for<'connection> FnOnce(
                &crab_ltx::rusqlite::Transaction<'connection>,
            )
                -> crate::Result<crate::cell::executor::HandlerOutcome>
            + Send
            + 'static,
    {
        let admission = self.reserve_work(operation_bytes, max_result_bytes)?;
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::Execute(Box::new(QueuedCommand {
                telemetry: self.inner.telemetry.clone(),
                queued_at: std::time::Instant::now(),
                response_proof: None,
                cell: self.cell,
                admission: self.admission.clone(),
                operation: QueuedOperation::Mutation {
                    identity,
                    operation_digest,
                },
                now_ms,
                max_result_bytes,
                handler: Some(Box::new(handler)),
                reply: Some(reply),
                _work: admission,
            })))
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        response.await.map_err(|_| Error::OutcomeUnknown {
            request_id: identity.request_id,
            operation_digest,
            source: Box::new(Error::RuntimeClosed),
        })?
    }

    /// Applies or replays one private destination effect through durable publication.
    pub async fn deliver_effect<F>(
        &self,
        delivery: InboxDelivery,
        now_ms: i64,
        operation_bytes: usize,
        max_result_bytes: usize,
        handler: F,
    ) -> crate::Result<StoredOutcome>
    where
        F: for<'connection> FnOnce(
                &crab_ltx::rusqlite::Transaction<'connection>,
            )
                -> crate::Result<crate::cell::executor::HandlerOutcome>
            + Send
            + 'static,
    {
        let admission = self.reserve_work(operation_bytes, max_result_bytes)?;
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::Execute(Box::new(QueuedCommand {
                telemetry: self.inner.telemetry.clone(),
                queued_at: std::time::Instant::now(),
                response_proof: None,
                cell: self.cell,
                admission: self.admission.clone(),
                operation: QueuedOperation::Effect { delivery },
                now_ms,
                max_result_bytes,
                handler: Some(Box::new(handler)),
                reply: Some(reply),
                _work: admission,
            })))
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        response.await.map_err(|_| Error::EffectOutcomeUnknown {
            effect_id: delivery.effect_id,
            operation_digest: delivery.operation_digest,
            source: Box::new(Error::RuntimeClosed),
        })?
    }

    /// Runs one FIFO-ordered bounded read after all preceding writes publish.
    pub async fn query<F>(
        &self,
        operation_bytes: usize,
        max_result_bytes: usize,
        handler: F,
    ) -> crate::Result<Vec<u8>>
    where
        F: FnOnce(&crab_ltx::rusqlite::Connection) -> crate::Result<Vec<u8>> + Send + 'static,
    {
        let admission = self.reserve_work(operation_bytes, max_result_bytes)?;
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::Query(Box::new(QueuedQuery {
                cell: self.cell,
                admission: self.admission.clone(),
                max_result_bytes,
                handler: Some(Box::new(handler)),
                reply: Some(reply),
                _work: admission,
            })))
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        let result = response.await.map_err(|_| Error::RuntimeClosed)?;
        self.inner.node_lease.check()?;
        result
    }

    /// Reads durable work that can retain a removed release contract.
    pub async fn persisted_work_inventory(
        &self,
        role: CatalogRole,
    ) -> crate::Result<PersistedWorkInventory> {
        let encoded = self
            .query(1, 1, move |connection| {
                crate::primitives::maintenance::inspect_persisted_work(connection, role)
                    .map(PersistedWorkInventory::encode)
            })
            .await?;
        PersistedWorkInventory::decode(&encoded)
    }

    /// Resolves a prior mutation without rerunning its handler.
    pub async fn resolve(
        &self,
        identity: MutationIdentity,
        operation_digest: Digest,
        now_ms: i64,
        max_result_bytes: usize,
    ) -> crate::Result<Resolution> {
        if identity.expired(now_ms)? {
            return Ok(Resolution::Expired);
        }
        if self.admission.fenced.load(Ordering::Acquire) {
            return Ok(Resolution::Unknown);
        }
        let admission = match self.reserve_work(48, max_result_bytes) {
            Ok(admission) => admission,
            Err(Error::Fenced) => return Ok(Resolution::Unknown),
            Err(error) => return Err(error),
        };
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::Resolve(Box::new(QueuedResolve {
                cell: self.cell,
                admission: self.admission.clone(),
                operation: ResolveOperation::Mutation {
                    identity,
                    operation_digest,
                },
                now_ms,
                max_result_bytes,
                reply: Some(reply),
                _work: admission,
            })))
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        match response.await {
            Ok(result) => result,
            Err(_) => Ok(Resolution::Unknown),
        }
    }

    /// Resolves one private destination effect without executing its handler again.
    pub async fn resolve_effect(
        &self,
        delivery: InboxDelivery,
        now_ms: i64,
        max_result_bytes: usize,
    ) -> crate::Result<Resolution> {
        if delivery.expires_at_ms <= now_ms {
            return Ok(Resolution::Expired);
        }
        if self.admission.fenced.load(Ordering::Acquire) {
            return Ok(Resolution::Unknown);
        }
        let admission = match self.reserve_work(72, max_result_bytes) {
            Ok(admission) => admission,
            Err(Error::Fenced) => return Ok(Resolution::Unknown),
            Err(error) => return Err(error),
        };
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::Resolve(Box::new(QueuedResolve {
                cell: self.cell,
                admission: self.admission.clone(),
                operation: ResolveOperation::Effect { delivery },
                now_ms,
                max_result_bytes,
                reply: Some(reply),
                _work: admission,
            })))
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        match response.await {
            Ok(result) => result,
            Err(_) => Ok(Resolution::Unknown),
        }
    }

    /// Drains the old capability and durably proves one registry-verified schema step.
    pub async fn migrate(&self, plan: MigrationPlan, now_ms: i64) -> crate::Result<MigratedCell> {
        if plan.from_code() != self.code
            || plan.from_schema() != self.schema
            || plan.operation_bytes() > MAX_OPERATION_BYTES
        {
            return Err(Error::Registry("migration plan does not match Cell handle"));
        }
        let work = self.reserve_work(plan.operation_bytes(), 0)?;
        if self
            .admission
            .draining
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Error::CellDraining);
        }
        self.admission.requests.close();
        self.admission.bytes.close();
        let successor_admission = new_cell_admission();
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::Migrate(Box::new(QueuedMigration {
                cell: self.cell,
                admission: self.admission.clone(),
                plan,
                now_ms,
                reply: Some(reply),
                successor_admission,
                _work: work,
            })))
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        let migrated = response.await.map_err(|_| Error::Fenced)??;
        Ok(MigratedCell {
            handle: Self {
                cell: self.cell,
                incarnation: self.incarnation,
                code: migrated.outcome.code,
                schema: migrated.outcome.schema,
                catalog: self.catalog.clone(),
                inner: self.inner.clone(),
                admission: migrated.admission,
            },
            outcome: migrated.outcome,
        })
    }

    /// Stops admission, publishes accepted commands, then closes the SQLite handle.
    pub async fn drain(&self) -> crate::Result<()> {
        if self
            .admission
            .draining
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Error::CellDraining);
        }
        self.admission.requests.close();
        self.admission.bytes.close();
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::Drain {
                cell: self.cell,
                admission: self.admission.clone(),
                reply,
            })
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        response.await.map_err(|_| Error::RuntimeClosed)?
    }

    pub(super) fn reserve_work(
        &self,
        operation_bytes: usize,
        max_result_bytes: usize,
    ) -> crate::Result<WorkAdmission> {
        if self.inner.shutting_down.load(Ordering::Acquire) {
            return Err(Error::RuntimeClosed);
        }
        self.inner.node_lease.check()?;
        if operation_bytes > MAX_OPERATION_BYTES || max_result_bytes > MAX_RESULT_BYTES {
            return Err(Error::Capacity("operation or result bytes"));
        }
        let reservation_bytes = operation_bytes
            .checked_add(max_result_bytes)
            .filter(|bytes| *bytes != 0)
            .ok_or(Error::Capacity("mailbox bytes"))?;
        if self.admission.fenced.load(Ordering::Acquire) {
            return Err(Error::Fenced);
        }
        if self.admission.draining.load(Ordering::Acquire) {
            return Err(Error::CellDraining);
        }
        let admission = WorkAdmission {
            _request: try_one(self.admission.requests.clone(), "Cell mailbox requests")?,
            _cell_bytes: try_many(
                self.admission.bytes.clone(),
                reservation_bytes,
                "Cell mailbox bytes",
            )?,
            _node_bytes: self
                .inner
                .resources
                .try_reserve(ResourceCost::zero().with_retained_bytes(reservation_bytes))
                .map_err(|error| match error {
                    Error::Capacity(_) => Error::Capacity("node retained bytes"),
                    error => error,
                })?,
        };
        if self.admission.fenced.load(Ordering::Acquire) {
            return Err(Error::Fenced);
        }
        if self.admission.draining.load(Ordering::Acquire) {
            return Err(Error::CellDraining);
        }
        self.inner.node_lease.check()?;
        Ok(admission)
    }
}

pub(super) fn try_one(
    semaphore: Arc<Semaphore>,
    resource: &'static str,
) -> crate::Result<OwnedSemaphorePermit> {
    semaphore
        .try_acquire_owned()
        .map_err(|error| admission_error(error, resource))
}

pub(super) fn try_many(
    semaphore: Arc<Semaphore>,
    permits: usize,
    resource: &'static str,
) -> crate::Result<OwnedSemaphorePermit> {
    let permits = u32::try_from(permits).map_err(|_| Error::Capacity(resource))?;
    semaphore
        .try_acquire_many_owned(permits)
        .map_err(|error| admission_error(error, resource))
}

pub(super) fn admission_error(error: TryAcquireError, resource: &'static str) -> Error {
    match error {
        TryAcquireError::Closed => Error::CellDraining,
        TryAcquireError::NoPermits => Error::Capacity(resource),
    }
}
