//! Catalog refresh and bounded, fair projection scheduling.

use super::Server;
use crate::{
    Result,
    repository::{MaintenanceSchedule, MaintenanceTrigger},
};
use std::{
    collections::HashMap,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};
use uuid::Uuid;

const PROJECTION_SWEEP_INTERVAL: Duration = Duration::from_secs(5);
const PROJECTION_SWEEP_BATCH: usize = 16;
const PROJECTION_RETRY_BASE: Duration = Duration::from_secs(10);
const PROJECTION_RETRY_MAX: Duration = Duration::from_secs(5 * 60);

pub(super) async fn refresh_catalog(server: Arc<Server>) {
    let Some(catalog) = server.catalog() else {
        return;
    };
    loop {
        tokio::select! {
            () = server.cancellation.cancelled() => return,
            () = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
        let active_version = server.repositories.version();
        let (document, _) = match catalog.load().await {
            Ok(value) => value,
            Err(error) => {
                server.catalog_healthy.store(false, Ordering::Release);
                server.metrics.record_catalog_refresh_failure();
                tracing::warn!(error = ?error, "repository catalog refresh failed");
                continue;
            }
        };
        if let Err(error) = catalog.flush_membership_audit().await {
            tracing::warn!(error = ?error, "membership audit flush failed");
        }
        if document.version < active_version {
            server.catalog_healthy.store(false, Ordering::Release);
            server.metrics.record_catalog_refresh_failure();
            tracing::warn!(
                catalog_version = document.version,
                active_version,
                "repository catalog version moved backwards"
            );
            continue;
        }
        match server.install_catalog(document).await {
            Ok(()) => {
                server.catalog_healthy.store(true, Ordering::Release);
            }
            Err(error) => {
                server.catalog_healthy.store(false, Ordering::Release);
                server.metrics.record_catalog_refresh_failure();
                tracing::warn!(error = ?error, "repository catalog installation failed");
            }
        }
    }
}

struct ProjectionSweepState {
    source_token: String,
    next_attempt: Instant,
    retry_delay: Duration,
}

impl ProjectionSweepState {
    fn record_schedule(&mut self, now: Instant, schedule: MaintenanceSchedule) {
        if schedule == MaintenanceSchedule::Deferred {
            self.next_attempt = now + PROJECTION_SWEEP_INTERVAL;
            return;
        }
        self.next_attempt = now + self.retry_delay;
        self.retry_delay = (self.retry_delay * 2).min(PROJECTION_RETRY_MAX);
    }
}

fn next_projection_sweep_cursor(start: usize, count: usize, repository_count: usize) -> usize {
    let advance = if count == repository_count { 1 } else { count };
    start.saturating_add(advance) % repository_count
}

pub(super) async fn sweep_projections(server: Arc<Server>) -> Result<()> {
    let mut cursor = 0_usize;
    let mut states = HashMap::<Uuid, ProjectionSweepState>::new();
    loop {
        tokio::select! {
            () = server.cancellation.cancelled() => return Ok(()),
            () = tokio::time::sleep(PROJECTION_SWEEP_INTERVAL) => {}
        }
        let repositories = server.repositories.values();
        if repositories.is_empty() {
            continue;
        }
        let start = cursor % repositories.len();
        let count = repositories.len().min(PROJECTION_SWEEP_BATCH);
        for offset in 0..count {
            let repository = Arc::clone(&repositories[(start + offset) % repositories.len()]);
            let now = Instant::now();
            let probe_started = now;
            let source =
                match crate::projection::current_source(&repository, &server, &server.cancellation)
                    .await
                {
                    Ok(source) => source,
                    Err(error) => {
                        server.metrics.record_projection_probe(
                            crate::metrics::ProjectionProbeResult::Error,
                            probe_started.elapsed(),
                        );
                        tracing::warn!(
                            repository_id = %repository.id,
                            error = %error,
                            "Git projection source probe failed"
                        );
                        if let Err(schedule_error) = repository
                            .schedule_maintenance(&server, MaintenanceTrigger::Repair)
                            .await
                        {
                            tracing::warn!(
                                repository_id = %repository.id,
                                error = %schedule_error,
                                "Git projection repair could not be scheduled"
                            );
                        }
                        continue;
                    }
                };
            let state = states
                .entry(repository.id)
                .or_insert_with(|| ProjectionSweepState {
                    source_token: String::new(),
                    next_attempt: now,
                    retry_delay: PROJECTION_RETRY_BASE,
                });
            if state.source_token != source.source_token {
                server.metrics.record_projection_probe(
                    crate::metrics::ProjectionProbeResult::Changed,
                    probe_started.elapsed(),
                );
                server.metrics.update_projection_state(false, 0, 0.0, 0);
                state.source_token = source.source_token.clone();
                state.next_attempt = now;
                state.retry_delay = PROJECTION_RETRY_BASE;
            } else {
                server.metrics.record_projection_probe(
                    crate::metrics::ProjectionProbeResult::Ready,
                    probe_started.elapsed(),
                );
            }
            if now < state.next_attempt {
                continue;
            }
            match repository
                .schedule_maintenance(&server, MaintenanceTrigger::Poll(&source))
                .await
            {
                Ok(schedule) => state.record_schedule(now, schedule),
                Err(error) => {
                    tracing::warn!(
                        repository_id = %repository.id,
                        error = %error,
                        "Git projection maintenance scheduling failed"
                    );
                    state.next_attempt = now + state.retry_delay;
                    state.retry_delay = (state.retry_delay * 2).min(PROJECTION_RETRY_MAX);
                }
            }
        }
        cursor = next_projection_sweep_cursor(start, count, repositories.len());
        states.retain(|repository_id, _| server.repositories.by_id(*repository_id).is_some());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projection_sweep_rotates_when_one_batch_covers_every_repository() {
        assert_eq!(next_projection_sweep_cursor(0, 12, 12), 1);
        assert_eq!(next_projection_sweep_cursor(1, 12, 12), 2);
    }

    #[test]
    fn projection_sweep_retries_capacity_deferral_without_growing_backoff() {
        let now = Instant::now();
        let retry_delay = Duration::from_secs(40);
        let mut state = ProjectionSweepState {
            source_token: "source".into(),
            next_attempt: now,
            retry_delay,
        };

        state.record_schedule(now, MaintenanceSchedule::Deferred);

        assert_eq!(state.next_attempt, now + PROJECTION_SWEEP_INTERVAL);
        assert_eq!(state.retry_delay, retry_delay);
    }
}
