use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Process-local admission budget for upload spool bytes.
#[derive(Debug)]
pub(crate) struct SpoolBudget {
    limit: u64,
    reserved: AtomicU64,
}

impl SpoolBudget {
    pub(crate) fn new(limit: u64) -> Self {
        Self {
            limit,
            reserved: AtomicU64::new(0),
        }
    }

    fn try_reserve(&self, bytes: u64) -> bool {
        let mut reserved = self.reserved.load(Ordering::Acquire);
        loop {
            let Some(next) = reserved.checked_add(bytes) else {
                return false;
            };
            if next > self.limit {
                return false;
            }
            match self.reserved.compare_exchange_weak(
                reserved,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(current) => reserved = current,
            }
        }
    }

    #[cfg(test)]
    fn reserved(&self) -> u64 {
        self.reserved.load(Ordering::Acquire)
    }
}

/// Reservation released when the corresponding upload leaves the gateway.
#[derive(Debug)]
pub(crate) struct SpoolReservation {
    budget: Arc<SpoolBudget>,
    bytes: u64,
}

impl SpoolReservation {
    pub(crate) fn acquire(budget: Arc<SpoolBudget>, bytes: u64) -> Option<Self> {
        if !budget.try_reserve(bytes) {
            return None;
        }
        Some(Self { budget, bytes })
    }

    pub(crate) fn extend(&mut self, bytes: u64) -> bool {
        let Some(next) = self.bytes.checked_add(bytes) else {
            return false;
        };
        if !self.budget.try_reserve(bytes) {
            return false;
        }
        self.bytes = next;
        true
    }
}

impl Drop for SpoolReservation {
    fn drop(&mut self) {
        self.budget.reserved.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[derive(Debug)]
struct RateState {
    last_refill: Instant,
    tokens: f64,
}

/// Process-local token bucket for request admission.
#[derive(Debug)]
pub(crate) struct RequestRateLimiter {
    rate_per_second: f64,
    burst: f64,
    state: Mutex<RateState>,
}

impl RequestRateLimiter {
    pub(crate) fn new(rate_per_second: u64, burst: u64) -> Self {
        let rate_per_second = rate_per_second as f64;
        let burst = burst as f64;
        Self {
            rate_per_second,
            burst,
            state: Mutex::new(RateState {
                last_refill: Instant::now(),
                tokens: burst,
            }),
        }
    }

    /// Consumes one request token or returns a conservative retry delay.
    pub(crate) fn retry_after(&self) -> Option<Duration> {
        let now = Instant::now();
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let elapsed = now.duration_since(state.last_refill).as_secs_f64();
        state.tokens = (state.tokens + elapsed * self.rate_per_second).min(self.burst);
        state.last_refill = now;
        if state.tokens >= 1.0 {
            state.tokens -= 1.0;
            return None;
        }
        Some(Duration::from_secs(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_are_bounded_and_released() {
        let budget = Arc::new(SpoolBudget::new(10));
        let mut first = SpoolReservation::acquire(Arc::clone(&budget), 6).expect("reservation");
        assert!(SpoolReservation::acquire(Arc::clone(&budget), 5).is_none());
        assert!(!first.extend(5));
        assert_eq!(budget.reserved(), 6);
        assert!(first.extend(4));
        assert_eq!(budget.reserved(), 10);
        drop(first);
        assert_eq!(budget.reserved(), 0);
    }

    #[test]
    fn request_rate_limiter_rejects_until_tokens_refill() {
        let limiter = RequestRateLimiter::new(1, 1);
        assert!(limiter.retry_after().is_none());
        assert_eq!(limiter.retry_after(), Some(Duration::from_secs(1)));
    }
}
