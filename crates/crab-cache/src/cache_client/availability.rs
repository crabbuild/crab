//! Shared endpoint admission; response ownership controls recovery completion.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::{CacheError, Result};

use super::REQUEST_TIMEOUT;

#[derive(Debug, Default)]
pub(super) struct Availability {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    generation: u64,
    retry_at: Option<Instant>,
    probing: bool,
}

pub(super) struct RequestPermit {
    owner: Arc<Availability>,
    generation: u64,
    probe: bool,
    finished: bool,
}

impl Availability {
    pub(super) fn begin(self: &Arc<Self>, now: Instant) -> Result<RequestPermit> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.probing || state.retry_at.is_some_and(|retry_at| now < retry_at) {
            return Err(CacheError::Service {
                reason: "cache service temporarily unavailable; recovery probe deferred".into(),
            });
        }
        let probe = state.retry_at.is_some();
        state.probing = probe;
        Ok(RequestPermit {
            owner: Arc::clone(self),
            generation: state.generation,
            probe,
            finished: false,
        })
    }
}

impl RequestPermit {
    pub(super) fn failed(&mut self) {
        self.finish(false);
    }

    pub(super) fn succeeded(&mut self) {
        self.finish(true);
    }

    fn finish(&mut self, success: bool) {
        if self.finished {
            return;
        }
        self.finished = true;
        let mut state = self
            .owner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // An earlier in-flight request must not clear a newer failure or keep
        // extending its cooldown. Only the current generation may transition.
        if state.generation != self.generation {
            return;
        }
        if success {
            if self.probe {
                state.retry_at = None;
                state.probing = false;
            }
        } else {
            state.generation = state.generation.wrapping_add(1);
            state.retry_at = Some(Instant::now() + REQUEST_TIMEOUT);
            state.probing = false;
        }
    }
}

impl Drop for RequestPermit {
    fn drop(&mut self) {
        // A cancelled or unconsumed probe is inconclusive, not recovery.
        // Reschedule it so dropping a body cannot strand long-lived readers.
        if self.probe && !self.finished {
            self.failed();
        }
    }
}

#[cfg(test)]
mod tests;
