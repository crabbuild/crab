//! Bounded pacing for callers that can wait for owner admission.

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use tokio::sync::Semaphore;

use super::{
    CellDescription, CellTarget, CellTransport, EncodedCommand, EncodedObservation, EncodedQuery,
    EncodedResolve, Resolution, StoredOutcome, unix_time_ms,
};
use crate::cell::actor::{CELL_BYTES, CELL_REQUESTS};
use crate::identity::CellId;
use crate::{Error, Result};

struct CellAdmission {
    requests: Semaphore,
    bytes: Semaphore,
}

#[derive(Clone)]
pub(super) struct BackpressureTransport {
    inner: Arc<dyn CellTransport>,
    requests: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    cells: Arc<Mutex<HashMap<CellId, Weak<CellAdmission>>>>,
    max_wait: Duration,
}

impl BackpressureTransport {
    pub(super) fn new(
        inner: Arc<dyn CellTransport>,
        requests: usize,
        bytes: usize,
        max_wait: Duration,
    ) -> Result<Self> {
        if requests == 0
            || requests > Semaphore::MAX_PERMITS
            || bytes == 0
            || bytes > Semaphore::MAX_PERMITS.min(u32::MAX as usize)
            || max_wait.is_zero()
        {
            return Err(Error::Command("invalid client admission bounds"));
        }
        Ok(Self {
            inner,
            requests: Arc::new(Semaphore::new(requests)),
            bytes: Arc::new(Semaphore::new(bytes)),
            cells: Arc::default(),
            max_wait,
        })
    }

    pub(super) async fn invoke<T, F: Future<Output = Result<T>>>(
        &self,
        cell: CellId,
        input_bytes: usize,
        output_bytes: Option<usize>,
        mut attempt: impl FnMut() -> F,
    ) -> Result<T> {
        let _request = self
            .requests
            .try_acquire()
            .map_err(|_| Error::Capacity("client admission requests"))?;
        // Retain the original envelope plus one attempted copy. Owner result
        // reservations remain in the runtime; this bounds waiting input memory.
        let bytes = input_bytes
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(2_048))
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or(Error::Capacity("client admission bytes"))?;
        let _bytes = self
            .bytes
            .try_acquire_many(bytes)
            .map_err(|_| Error::Capacity("client admission bytes"))?;
        let started = tokio::time::Instant::now();
        let cell = if let Some(output_bytes) = output_bytes {
            let reservation = input_bytes
                .checked_add(output_bytes)
                .filter(|bytes| *bytes <= CELL_BYTES)
                .and_then(|bytes| u32::try_from(bytes.max(1)).ok())
                .ok_or(Error::Capacity("client Cell admission bytes"))?;
            let gate = {
                let mut cells = self
                    .cells
                    .lock()
                    .map_err(|_| Error::Control("client admission lock poisoned"))?;
                // Only admitted calls retain a gate. Pruning weak entries bounds
                // this map by the shared call budget even as table IDs change.
                cells.retain(|_, gate| gate.strong_count() != 0);
                match cells.get(&cell).and_then(Weak::upgrade) {
                    Some(gate) => gate,
                    None => {
                        let gate = Arc::new(CellAdmission {
                            requests: Semaphore::new(CELL_REQUESTS),
                            bytes: Semaphore::new(CELL_BYTES),
                        });
                        cells.insert(cell, Arc::downgrade(&gate));
                        gate
                    }
                }
            };
            Some((gate, reservation))
        } else {
            // Describe does not enter the owner mailbox. Queuing it behind
            // writes would add an unrelated wait before every prepared command.
            None
        };
        // Match the owner's existing limits while allowing routing and reads
        // to overlap. FIFO weighted permits keep large calls from starvation.
        let _cell = if let Some((cell, reservation)) = &cell {
            Some(
                tokio::time::timeout(self.max_wait, async {
                    let requests = cell
                        .requests
                        .acquire()
                        .await
                        .map_err(|_| Error::RuntimeClosed)?;
                    let bytes = cell
                        .bytes
                        .acquire_many(*reservation)
                        .await
                        .map_err(|_| Error::RuntimeClosed)?;
                    Ok::<_, Error>((requests, bytes))
                })
                .await
                .map_err(|_| Error::Capacity("client Cell admission wait"))??,
            )
        } else {
            None
        };
        let mut delay = Duration::from_millis(10);
        loop {
            let result = attempt().await;
            let Err(Error::Capacity(_)) = &result else {
                return result;
            };
            let Some(remaining) = self.max_wait.checked_sub(started.elapsed()) else {
                return result;
            };
            if remaining.is_zero() {
                return result;
            }
            // Never cancel an accepted command to enforce an admission timeout.
            // Unknown outcomes pass through unchanged and must be resolved.
            tokio::time::sleep(delay.min(remaining)).await;
            if started.elapsed() >= self.max_wait {
                return result;
            }
            delay = (delay * 2).min(Duration::from_millis(100));
        }
    }
}

impl CellTransport for BackpressureTransport {
    fn describe(
        &self,
        target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = Result<CellDescription>> + Send + 'static>> {
        let transport = self.clone();
        Box::pin(async move {
            transport
                .invoke(target.cell_id(), target.partition().len(), None, || {
                    transport.inner.describe(target.clone())
                })
                .await
        })
    }

    fn command(
        &self,
        command: EncodedCommand,
    ) -> Pin<Box<dyn Future<Output = Result<StoredOutcome>> + Send + 'static>> {
        let transport = self.clone();
        Box::pin(async move {
            transport
                .invoke(
                    command.target.cell_id(),
                    command.input.len(),
                    Some(command.output_limit as usize),
                    || {
                        let mut request = command.clone();
                        let inner = Arc::clone(&transport.inner);
                        async move {
                            request.now_ms = unix_time_ms()?;
                            request.identity.validate(request.now_ms)?;
                            inner.command(request).await
                        }
                    },
                )
                .await
        })
    }

    fn query(
        &self,
        query: EncodedQuery,
    ) -> Pin<Box<dyn Future<Output = Result<EncodedObservation>> + Send + 'static>> {
        let transport = self.clone();
        Box::pin(async move {
            transport
                .invoke(
                    query.target.cell_id(),
                    query.input.len(),
                    Some(query.output_limit as usize),
                    || {
                        let mut request = query.clone();
                        let inner = Arc::clone(&transport.inner);
                        async move {
                            request.now_ms = unix_time_ms()?;
                            inner.query(request).await
                        }
                    },
                )
                .await
        })
    }

    fn resolve(
        &self,
        resolve: EncodedResolve,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution>> + Send + 'static>> {
        let transport = self.clone();
        Box::pin(async move {
            transport
                .invoke(
                    resolve.target.cell_id(),
                    48,
                    Some(resolve.max_result_bytes),
                    || {
                        let mut request = resolve.clone();
                        let inner = Arc::clone(&transport.inner);
                        async move {
                            request.now_ms = unix_time_ms()?;
                            inner.resolve(request).await
                        }
                    },
                )
                .await
        })
    }
}
