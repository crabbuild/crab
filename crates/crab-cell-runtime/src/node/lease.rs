//! Node lease guard and its renewal window.
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::{Error, Result};

const MAX_NODE_LEASE_MS: i64 = 60_000;

/// Process-wide monotonic guard for one published node-session lease.
///
/// Only a successful authoritative create or refresh may advance the local
/// deadline. Expiry is terminal: a late object-store response cannot revive
/// dispatch or authorize a durability proof in the old process.
#[derive(Clone)]
pub struct NodeLeaseGuard {
    inner: Arc<LeaseInner>,
}

struct LeaseInner {
    state: Mutex<LeaseState>,
    changed: Notify,
}

struct LeaseState {
    deadline: tokio::time::Instant,
    fenced: bool,
}

impl NodeLeaseGuard {
    /// Starts a watchdog from one successfully published lease observation.
    pub fn new(now_ms: i64, expires_at_ms: i64) -> Result<Self> {
        let remaining = lease_remaining(now_ms, expires_at_ms)?;
        let runtime = tokio::runtime::Handle::try_current().map_err(Error::RuntimeStart)?;
        let inner = Arc::new(LeaseInner {
            state: Mutex::new(LeaseState {
                deadline: tokio::time::Instant::now() + remaining,
                fenced: false,
            }),
            changed: Notify::new(),
        });
        runtime.spawn(watchdog(Arc::clone(&inner)));
        Ok(Self { inner })
    }

    /// Advances the deadline after an authoritative refresh succeeds.
    pub fn renew(&self, now_ms: i64, expires_at_ms: i64) -> Result<()> {
        let remaining = lease_remaining(now_ms, expires_at_ms)?;
        let now = tokio::time::Instant::now();
        let next = now
            .checked_add(remaining)
            .ok_or(Error::Node("node lease deadline overflow"))?;
        let mut state = self.lock()?;
        if state.fenced || now >= state.deadline {
            state.fenced = true;
            drop(state);
            self.inner.changed.notify_waiters();
            return Err(Error::Fenced);
        }
        if next <= state.deadline {
            return Err(Error::Node("node lease deadline did not advance"));
        }
        state.deadline = next;
        drop(state);
        self.inner.changed.notify_waiters();
        Ok(())
    }

    /// Fails once the exact process can no longer prove a live session lease.
    pub fn check(&self) -> Result<()> {
        let mut state = self.lock()?;
        if state.fenced || tokio::time::Instant::now() >= state.deadline {
            state.fenced = true;
            drop(state);
            self.inner.changed.notify_waiters();
            return Err(Error::Fenced);
        }
        Ok(())
    }

    /// Returns the current lease time remaining, or zero after fencing.
    #[must_use]
    pub fn remaining(&self) -> std::time::Duration {
        self.inner
            .state
            .lock()
            .ok()
            .filter(|state| !state.fenced)
            .map_or(std::time::Duration::ZERO, |state| {
                state
                    .deadline
                    .saturating_duration_since(tokio::time::Instant::now())
            })
    }

    /// Permanently closes the guard and wakes dispatch, output, and shutdown waiters.
    pub fn fence(&self) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.fenced = true;
        }
        self.inner.changed.notify_waiters();
    }

    /// Waits until expiry or an explicit terminal fence.
    pub async fn wait_fenced(&self) {
        loop {
            let notified = self.inner.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.lock().map_or(true, |state| state.fenced) {
                return;
            }
            notified.await;
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, LeaseState>> {
        self.inner
            .state
            .lock()
            .map_err(|_| Error::Node("node lease guard poisoned"))
    }
}

async fn watchdog(inner: Arc<LeaseInner>) {
    loop {
        let notified = inner.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let deadline = match inner.state.lock() {
            Ok(state) if state.fenced => return,
            Ok(state) => state.deadline,
            Err(_) => return,
        };
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => {
                if let Ok(mut state) = inner.state.lock()
                    && tokio::time::Instant::now() >= state.deadline
                {
                    state.fenced = true;
                    drop(state);
                    inner.changed.notify_waiters();
                    return;
                }
            }
            () = &mut notified => {}
        }
    }
}

fn lease_remaining(now_ms: i64, expires_at_ms: i64) -> Result<std::time::Duration> {
    let remaining_ms = expires_at_ms
        .checked_sub(now_ms)
        .filter(|remaining| (1..=MAX_NODE_LEASE_MS).contains(remaining))
        .ok_or(Error::Node("node lease bounds are invalid"))?;
    Ok(std::time::Duration::from_millis(remaining_ms as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn expiry_is_terminal_and_late_renewal_cannot_revive_the_process() {
        let guard = NodeLeaseGuard::new(1_000, 1_030).unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(1), guard.wait_fenced())
            .await
            .unwrap();

        assert!(matches!(guard.check(), Err(Error::Fenced)));
        assert!(matches!(guard.renew(1_020, 1_050), Err(Error::Fenced)));
    }

    #[tokio::test]
    async fn authoritative_renewal_moves_the_monotonic_deadline_forward() {
        let guard = NodeLeaseGuard::new(1_000, 1_050).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        guard.renew(1_020, 1_100).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;

        guard.check().unwrap();
        guard.fence();
        guard.wait_fenced().await;
    }

    #[tokio::test]
    async fn invalid_or_nonadvancing_lease_is_rejected() {
        assert!(NodeLeaseGuard::new(10, 10).is_err());
        assert!(NodeLeaseGuard::new(0, MAX_NODE_LEASE_MS + 1).is_err());

        let guard = NodeLeaseGuard::new(1_000, 1_100).unwrap();
        assert!(guard.renew(1_050, 1_060).is_err());
        guard.fence();
    }
}
