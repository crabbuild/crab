use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use bytes::Bytes;
use std::time::{Duration, Instant};

use crate::{Error, Result};

const OWNER_BYTES: usize = 128;

pub(crate) struct ResidentBudget {
    maximum: usize,
    used: AtomicUsize,
    metrics: Arc<dyn crate::RemoteGitMetrics>,
}

/// Reserved resident bytes for one retained read allocation.
///
/// Reserve before allocating. Binding transfers the charge to the returned
/// Bytes, whose clones and slices retain it until their last owner drops.
#[must_use]
pub struct ReadReservation {
    budget: Arc<ResidentBudget>,
    amount: usize,
    started: Instant,
}

pub(crate) struct Retained<T> {
    pub(crate) value: T,
    _reservation: ReadReservation,
}

impl<T> Retained<T> {
    pub(crate) fn new(value: T, reservation: ReadReservation) -> Self {
        Self {
            value,
            _reservation: reservation,
        }
    }
}

impl<T> std::ops::Deref for Retained<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Retained<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.fmt(f)
    }
}

struct ReservedBytes {
    bytes: Bytes,
    _reservation: ReadReservation,
}

impl AsRef<[u8]> for ReservedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl ResidentBudget {
    pub(crate) fn new(maximum: usize, metrics: Arc<dyn crate::RemoteGitMetrics>) -> Arc<Self> {
        Arc::new(Self {
            maximum,
            used: AtomicUsize::new(0),
            metrics,
        })
    }

    pub(crate) fn reserve(self: &Arc<Self>, bytes: usize) -> Result<ReadReservation> {
        let amount = bytes.checked_add(OWNER_BYTES).ok_or(Error::LimitExceeded {
            limit: "resident read bytes",
            actual: u64::MAX,
            maximum: self.maximum as u64,
        })?;
        self.acquire(amount)?;
        Ok(ReadReservation {
            budget: self.clone(),
            amount,
            started: Instant::now(),
        })
    }

    fn acquire(&self, amount: usize) -> Result<()> {
        self.used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(amount)
                    .filter(|total| *total <= self.maximum)
            })
            .map_err(|used| Error::LimitExceeded {
                limit: "resident read bytes",
                actual: (used as u64).saturating_add(amount as u64),
                maximum: self.maximum as u64,
            })?;
        self.observe(amount, None);
        Ok(())
    }

    fn observe(&self, amount: usize, duration: Option<Duration>) {
        self.metrics.record(crate::MetricObservation {
            kind: crate::MetricKind::ResidentBytes,
            value: amount as u64,
            duration,
            outcome: None,
            cache: None,
        });
    }

    pub(crate) fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    pub(crate) fn maximum(&self) -> usize {
        self.maximum
    }

    pub(crate) fn fits(&self, payload: usize) -> bool {
        payload
            .checked_add(OWNER_BYTES)
            .is_some_and(|bytes| bytes <= self.maximum)
    }
}

impl ReadReservation {
    pub(crate) fn grow(&mut self, bytes: usize) -> Result<()> {
        self.budget.acquire(bytes)?;
        // This guard is part of the admitted total, whose checked sum cannot
        // exceed the usize-sized capacity.
        self.amount += bytes;
        Ok(())
    }

    /// Bind bytes no larger than the admitted payload and retain their charge.
    ///
    /// Returns a resource limit error if the supplied payload exceeds admission.
    pub fn bind(self, bytes: Bytes) -> Result<Bytes> {
        let maximum = self.amount - OWNER_BYTES;
        if bytes.len() > maximum {
            return Err(Error::LimitExceeded {
                limit: "resident read bytes",
                actual: bytes.len() as u64,
                maximum: maximum as u64,
            });
        }
        // Bytes owns this guard through the last clone or slice, including
        // transport-held frames after the handler has returned.
        Ok(Bytes::from_owner(ReservedBytes {
            bytes,
            _reservation: self,
        }))
    }
}

impl Drop for ReadReservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.amount, Ordering::AcqRel);
        self.budget
            .observe(self.amount, Some(self.started.elapsed()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returned_clones_and_slices_retain_the_byte_reservation() {
        let budget = ResidentBudget::new(1024, Arc::new(crate::NoopMetrics));
        let bytes = budget
            .reserve(32)
            .unwrap()
            .bind(Bytes::from(vec![1; 32]))
            .unwrap();
        let clone = bytes.clone();
        let slice = clone.slice(2..4);
        drop(bytes);
        drop(clone);
        assert_eq!(budget.used(), 32 + OWNER_BYTES);
        drop(slice);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn holding_output_cannot_deadlock_a_second_admission() {
        let budget = ResidentBudget::new(160, Arc::new(crate::NoopMetrics));
        let held = budget.reserve(32).unwrap();
        assert!(matches!(
            budget.reserve(1),
            Err(Error::LimitExceeded {
                limit: "resident read bytes",
                maximum: 160,
                ..
            })
        ));
        drop(held);
        assert!(budget.reserve(32).is_ok());
    }

    #[test]
    fn small_buffers_charge_owner_overhead_and_overflow_is_rejected() {
        let budget = ResidentBudget::new(3 * (1 + OWNER_BYTES), Arc::new(crate::NoopMetrics));
        let held = (0..3)
            .map(|_| budget.reserve(1).unwrap())
            .collect::<Vec<_>>();
        assert!(budget.reserve(1).is_err());
        assert!(budget.reserve(usize::MAX).is_err());
        drop(held);
        assert_eq!(budget.used(), 0);
    }
}
