use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, oneshot};

use super::{
    Message, QueuedCommand, QueuedOperation, QueuedQuery, QueuedResolve, ResolveOperation,
    RuntimeInner,
};
use crate::{
    CatalogProof, CellId, Digest, Error, InboxDelivery, IncarnationId, MutationIdentity,
    Resolution, StoredOutcome,
};

const MAX_OPERATION_BYTES: usize = 1024 * 1024;
const MAX_RESULT_BYTES: usize = 1024 * 1024;

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
    _request: OwnedSemaphorePermit,
    _cell_bytes: OwnedSemaphorePermit,
    _node_bytes: OwnedSemaphorePermit,
}

impl CellHandle {
    #[must_use]
    pub const fn catalog(&self) -> &CatalogProof {
        &self.catalog
    }

    #[must_use]
    pub const fn cell_id(&self) -> CellId {
        self.cell
    }

    #[must_use]
    pub const fn incarnation(&self) -> IncarnationId {
        self.incarnation
    }

    #[must_use]
    pub const fn code(&self) -> Digest {
        self.code
    }

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
            ) -> crate::Result<crate::HandlerOutcome>
            + Send
            + 'static,
    {
        let admission = self.reserve_work(operation_bytes, max_result_bytes)?;
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::Execute(Box::new(QueuedCommand {
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
            ) -> crate::Result<crate::HandlerOutcome>
            + Send
            + 'static,
    {
        let admission = self.reserve_work(operation_bytes, max_result_bytes)?;
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::Execute(Box::new(QueuedCommand {
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
        response.await.map_err(|_| Error::RuntimeClosed)?
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

    fn reserve_work(
        &self,
        operation_bytes: usize,
        max_result_bytes: usize,
    ) -> crate::Result<WorkAdmission> {
        if self.inner.shutting_down.load(Ordering::Acquire) {
            return Err(Error::RuntimeClosed);
        }
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
            _node_bytes: try_many(
                self.inner.node_bytes.clone(),
                reservation_bytes,
                "node retained bytes",
            )?,
        };
        if self.admission.fenced.load(Ordering::Acquire) {
            return Err(Error::Fenced);
        }
        if self.admission.draining.load(Ordering::Acquire) {
            return Err(Error::CellDraining);
        }
        Ok(admission)
    }
}

fn try_one(
    semaphore: Arc<Semaphore>,
    resource: &'static str,
) -> crate::Result<OwnedSemaphorePermit> {
    semaphore
        .try_acquire_owned()
        .map_err(|error| admission_error(error, resource))
}

fn try_many(
    semaphore: Arc<Semaphore>,
    permits: usize,
    resource: &'static str,
) -> crate::Result<OwnedSemaphorePermit> {
    let permits = u32::try_from(permits).map_err(|_| Error::Capacity(resource))?;
    semaphore
        .try_acquire_many_owned(permits)
        .map_err(|error| admission_error(error, resource))
}

fn admission_error(error: TryAcquireError, resource: &'static str) -> Error {
    match error {
        TryAcquireError::Closed => Error::CellDraining,
        TryAcquireError::NoPermits => Error::Capacity(resource),
    }
}
