use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    },
    time::Duration,
};

use crab_cell_runtime::{
    ActivityRunOutcome, ApplicationIdentity, BlockingActivityPool, BlockingActivityReservation,
    CellAuthority, CellCatalog, CellId, CellTarget, DueCellScan, EffectRunOutcome, InvocationError,
    MaintenanceTickOutcome, MaintenanceTickRequest, MutationIdentity, NodeDirectory, Registry,
    RequestId, SchedulerFleet, SessionId, preferred_scanner,
};
use crab_storage::CellStorageLayout;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::RepositoryCellRouter;

const SCAN_INTERVAL: Duration = Duration::from_secs(1);
const MAX_LIVE_NODES: usize = 10_000;
const MAX_DUE_PER_CYCLE: usize = 128;
const EFFECT_LEASE_MS: u32 = 30_000;
const ACTIVITY_LEASE_MS: u32 = 30_000;
const MAX_ACTIVITY_JOBS: usize = 16;
const SCHEDULER_STALE_AFTER_MS: i64 = 15_000;
const NODE_COLLECTION_INTERVAL_MS: i64 = 60_000;
const NODE_COLLECTION_LIMIT: usize = 128;

/// Shared scanner progress used by enrollment, readiness and metrics.
#[derive(Clone)]
pub(crate) struct SchedulerStatus {
    progress: Arc<AtomicU64>,
    last_completed_ms: Arc<AtomicI64>,
    completed: Arc<AtomicBool>,
}

impl SchedulerStatus {
    pub(crate) fn new(now_ms: i64) -> crate::Result<Self> {
        if now_ms < 0 {
            return Err(crab_cell_runtime::Error::Control("negative scheduler status time").into());
        }
        Ok(Self {
            progress: Arc::new(AtomicU64::new(1)),
            last_completed_ms: Arc::new(AtomicI64::new(now_ms)),
            completed: Arc::new(AtomicBool::new(false)),
        })
    }

    pub(crate) fn progress(&self) -> u64 {
        self.progress.load(Ordering::Acquire)
    }

    pub(crate) fn lag_ms(&self, now_ms: i64) -> u64 {
        u64::try_from(now_ms.saturating_sub(self.last_completed_ms.load(Ordering::Acquire)))
            .unwrap_or(0)
    }

    pub(crate) fn is_healthy(&self, now_ms: i64) -> bool {
        let last = self.last_completed_ms.load(Ordering::Acquire);
        self.completed.load(Ordering::Acquire)
            && now_ms >= last
            && now_ms - last < SCHEDULER_STALE_AFTER_MS
    }

    pub(crate) fn mark_completed(&self, now_ms: i64) {
        self.last_completed_ms.store(now_ms, Ordering::Release);
        self.completed.store(true, Ordering::Release);
        let _ = self
            .progress
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |progress| {
                progress.checked_add(1)
            });
    }
}

/// Fleet-assigned scanner for compiled Cell maintenance, activities and effects.
pub(crate) struct RepositoryCellScheduler {
    identity: ApplicationIdentity,
    catalog: CellCatalog,
    authority: CellAuthority,
    directory: NodeDirectory,
    router: RepositoryCellRouter,
    session: SessionId,
    status: SchedulerStatus,
    fleet: SchedulerFleet,
    registry: Arc<Registry>,
    scans: HashMap<u8, DueCellScan>,
    next_shard: u8,
    activity_admission: Arc<tokio::sync::Semaphore>,
    blocking_activities: Option<BlockingActivityPool>,
    activity_cells: Arc<Mutex<HashSet<CellId>>>,
    activity_jobs: tokio::task::JoinSet<()>,
    last_node_collection_ms: i64,
}

impl RepositoryCellScheduler {
    pub(crate) fn new(
        identity: ApplicationIdentity,
        layout: CellStorageLayout,
        directory: NodeDirectory,
        router: RepositoryCellRouter,
        session: SessionId,
        status: SchedulerStatus,
    ) -> crate::Result<Self> {
        let registry = router.registry();
        let blocking_activities = registry
            .has_blocking_activities()
            .then(BlockingActivityPool::for_system)
            .transpose()?;
        Ok(Self {
            identity,
            catalog: CellCatalog::new(layout.clone(), identity.tenant()),
            authority: CellAuthority::new(layout),
            directory,
            router,
            session,
            status,
            fleet: SchedulerFleet::default(),
            registry,
            scans: HashMap::new(),
            next_shard: 0,
            activity_admission: Arc::new(tokio::sync::Semaphore::new(
                std::thread::available_parallelism()
                    .map_or(1, |count| count.get())
                    .min(MAX_ACTIVITY_JOBS),
            )),
            blocking_activities,
            activity_cells: Arc::new(Mutex::new(HashSet::new())),
            activity_jobs: tokio::task::JoinSet::new(),
            last_node_collection_ms: 0,
        })
    }

    pub(crate) async fn run(mut self, cancellation: CancellationToken) -> crate::Result<()> {
        loop {
            if cancellation.is_cancelled() {
                break;
            }
            self.reap_activity_jobs();
            match self.scan_once().await {
                Ok(()) => self.status.mark_completed(super::unix_now_ms()?),
                Err(error) => {
                    tracing::warn!(error = %error, "Cell scheduler scan failed");
                }
            }
            tokio::select! {
                () = cancellation.cancelled() => {}
                () = tokio::time::sleep(SCAN_INTERVAL) => {}
            }
        }
        self.activity_jobs.abort_all();
        while self.activity_jobs.join_next().await.is_some() {}
        if let Some(pool) = &self.blocking_activities {
            pool.shutdown().await?;
        }
        Ok(())
    }

    async fn scan_once(&mut self) -> crate::Result<()> {
        self.scan_once_bounded(MAX_DUE_PER_CYCLE).await
    }

    async fn scan_once_bounded(&mut self, cycle_limit: usize) -> crate::Result<()> {
        if cycle_limit == 0 || cycle_limit > MAX_DUE_PER_CYCLE {
            return Err(
                crab_cell_runtime::Error::Control("scheduler cycle limit is invalid").into(),
            );
        }
        let now_ms = super::unix_now_ms()?;
        let advertisements = self.directory.live(now_ms, MAX_LIVE_NODES).await?;
        let nodes =
            self.fleet
                .eligible_sessions(&advertisements, now_ms, SCHEDULER_STALE_AFTER_MS)?;
        if preferred_scanner(0, &nodes)? == Some(self.session)
            && now_ms.saturating_sub(self.last_node_collection_ms) >= NODE_COLLECTION_INTERVAL_MS
        {
            let removed = self
                .directory
                .collect_stale(now_ms, NODE_COLLECTION_LIMIT)
                .await?;
            self.last_node_collection_ms = now_ms;
            tracing::debug!(removed, "collected stale Cell node advertisements");
        }
        let start = self.next_shard;
        let mut assigned = Vec::new();
        for offset in 0_u16..=u8::MAX.into() {
            let shard = start.wrapping_add(offset as u8);
            if preferred_scanner(shard, &nodes)? == Some(self.session) {
                assigned.push(shard);
            } else {
                self.scans.remove(&shard);
            }
        }

        let mut remaining = cycle_limit;
        let mut exhausted = HashSet::new();
        let mut next_shard = start.wrapping_add(1);
        for shard in &assigned {
            let (attempted, complete) = self.scan_shard(*shard, now_ms, 1).await?;
            remaining -= attempted;
            if complete {
                exhausted.insert(*shard);
            }
            if remaining == 0 {
                next_shard = shard.wrapping_add(1);
                break;
            }
        }
        if remaining != 0 {
            for shard in assigned {
                if exhausted.contains(&shard) {
                    continue;
                }
                let (attempted, _) = self.scan_shard(shard, now_ms, remaining).await?;
                remaining -= attempted;
                if remaining == 0 {
                    next_shard = shard.wrapping_add(1);
                    break;
                }
            }
        }
        self.next_shard = next_shard;
        Ok(())
    }

    async fn scan_shard(
        &mut self,
        shard: u8,
        now_ms: i64,
        limit: usize,
    ) -> crate::Result<(usize, bool)> {
        if !self.scans.contains_key(&shard) {
            let scan = DueCellScan::new(&self.catalog, self.authority.clone(), shard).await?;
            self.scans.insert(shard, scan);
        }
        let mut attempted = 0;
        while attempted < limit {
            let batch = {
                let scan = self
                    .scans
                    .get_mut(&shard)
                    .ok_or(crab_cell_runtime::Error::Control(
                        "scheduler shard cursor disappeared",
                    ))?;
                scan.next_batch_bounded(now_ms, limit - attempted).await
            };
            let batch = match batch {
                Ok(Some(batch)) => batch,
                Ok(None) => {
                    self.scans.remove(&shard);
                    return Ok((attempted, true));
                }
                Err(error) => {
                    self.scans.remove(&shard);
                    return Err(error.into());
                }
            };
            for due in batch {
                attempted += 1;
                if let Err(error) = self.process(due).await {
                    tracing::warn!(error = %error, "Cell scheduler item failed");
                }
            }
        }
        Ok((attempted, false))
    }

    async fn process(&mut self, due: crab_cell_runtime::DueCell) -> crate::Result<()> {
        let entry = due.catalog().entry();
        let control = due.control().value();
        if !self.registry.supports_cell(
            entry.namespace(),
            entry.role(),
            control.code,
            control.schema,
        ) {
            return Err(crab_cell_runtime::Error::Registry(
                "due Cell is unsupported by the running release",
            )
            .into());
        }
        let target = CellTarget::new(
            self.identity.tenant(),
            self.identity.application(),
            entry.namespace(),
            entry.partition(),
        )?;
        let expected_commit_sequence = control
            .root
            .as_ref()
            .ok_or(crab_cell_runtime::Error::CellNotActive)?
            .commit_sequence;
        let scheduled = self.router.route_scheduler_target(target.clone()).await?;
        let release_after = scheduled.should_release();
        self.process_cell(scheduled.cell, expected_commit_sequence, release_after)
            .await
    }

    async fn process_cell(
        &mut self,
        cell: super::RepositoryCell,
        expected_commit_sequence: u64,
        release_after: bool,
    ) -> crate::Result<()> {
        let tick = self
            .registry
            .run_maintenance_once(
                cell.client.clone(),
                cell.target.clone(),
                mutation_identity()?,
                MaintenanceTickRequest {
                    expected_commit_sequence,
                },
            )
            .await;
        let processed = match tick {
            Ok(committed) => match committed.output {
                MaintenanceTickOutcome::Applied { processed } => processed,
                MaintenanceTickOutcome::Stale => {
                    return self.release_after(&cell.target, release_after).await;
                }
            },
            Err(InvocationError::Rejected(_)) => {
                return self.release_after(&cell.target, release_after).await;
            }
            Err(error) => {
                tracing::warn!(error = %error, "Cell Tick was not resolved");
                return self.release_after(&cell.target, release_after).await;
            }
        };
        if processed != 0 {
            return self.release_after(&cell.target, release_after).await;
        }
        if self.registry.has_activity_runner(cell.target.namespace())
            && let Ok(permit) = Arc::clone(&self.activity_admission).try_acquire_owned()
            && let Ok(activity_bytes) = self.router.reserve_activity_payloads()
            && let Some(activity_cell) = self.reserve_activity(cell.target.cell_id())
            && let Some(blocking) = self.reserve_blocking_activity(cell.target.namespace())?
        {
            let registry = Arc::clone(&self.registry);
            let router = self.router.clone();
            self.activity_jobs.spawn(async move {
                let target = cell.target.clone();
                match registry
                    .run_activity_once(cell.client.clone(), &target, ACTIVITY_LEASE_MS, blocking)
                    .await
                {
                    Ok(ActivityRunOutcome::Idle { .. })
                    | Ok(ActivityRunOutcome::LeaseLost { .. })
                    | Ok(ActivityRunOutcome::IdentityConflict { .. })
                    | Ok(ActivityRunOutcome::Retrying { .. })
                    | Ok(ActivityRunOutcome::Completed { .. })
                    | Ok(ActivityRunOutcome::Duplicate { .. }) => {}
                    Err(error) => {
                        tracing::warn!(error = %error, "Workflow activity was not resolved");
                    }
                }
                if registry.has_effect_runner(target.namespace()) {
                    run_effect(&registry, &router, cell).await;
                }
                if release_after && let Err(error) = router.drain_local_target(&target).await {
                    tracing::warn!(error = %error, "Workflow scheduler Cell release failed");
                }
                drop(activity_cell);
                drop(activity_bytes);
                drop(permit);
            });
            return Ok(());
        }
        if !self.registry.has_effect_runner(cell.target.namespace()) {
            return self.release_after(&cell.target, release_after).await;
        }
        let target = cell.target.clone();
        match self
            .registry
            .run_effect_once(
                cell.client,
                cell.target,
                self.router.effect_peer_client(),
                EFFECT_LEASE_MS,
            )
            .await
        {
            Ok(EffectRunOutcome::Idle { .. })
            | Ok(EffectRunOutcome::Delivered { .. })
            | Ok(EffectRunOutcome::Retrying { .. })
            | Ok(EffectRunOutcome::Failed { .. })
            | Ok(EffectRunOutcome::LeaseLost { .. }) => {
                self.release_after(&target, release_after).await
            }
            Err(error) => {
                tracing::warn!(error = %error, "Cell effect was not resolved");
                self.release_after(&target, release_after).await
            }
        }
    }

    async fn release_after(&self, target: &CellTarget, release_after: bool) -> crate::Result<()> {
        if release_after {
            self.router.drain_local_target(target).await
        } else {
            Ok(())
        }
    }

    fn reap_activity_jobs(&mut self) {
        while let Some(result) = self.activity_jobs.try_join_next() {
            if let Err(error) = result
                && !error.is_cancelled()
            {
                tracing::warn!(error = %error, "Workflow activity task failed");
            }
        }
    }

    fn reserve_activity(&self, cell: CellId) -> Option<ActivityCellReservation> {
        let mut cells = self
            .activity_cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cells.insert(cell).then(|| ActivityCellReservation {
            cell,
            cells: Arc::clone(&self.activity_cells),
        })
    }

    fn reserve_blocking_activity(
        &self,
        namespace: crab_cell_runtime::NamespaceId,
    ) -> crate::Result<Option<Option<BlockingActivityReservation>>> {
        if !self.registry.requires_blocking_activity(namespace) {
            return Ok(Some(None));
        }
        let pool = self
            .blocking_activities
            .as_ref()
            .ok_or(crab_cell_runtime::Error::Registry(
                "blocking activity pool is unavailable",
            ))?;
        Ok(pool.try_reserve()?.map(Some))
    }
}

struct ActivityCellReservation {
    cell: CellId,
    cells: Arc<Mutex<HashSet<CellId>>>,
}

impl Drop for ActivityCellReservation {
    fn drop(&mut self) {
        self.cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.cell);
    }
}

async fn run_effect(
    registry: &Registry,
    router: &RepositoryCellRouter,
    cell: super::RepositoryCell,
) {
    match registry
        .run_effect_once(
            cell.client,
            cell.target,
            router.effect_peer_client(),
            EFFECT_LEASE_MS,
        )
        .await
    {
        Ok(EffectRunOutcome::Idle { .. })
        | Ok(EffectRunOutcome::Delivered { .. })
        | Ok(EffectRunOutcome::Retrying { .. })
        | Ok(EffectRunOutcome::Failed { .. })
        | Ok(EffectRunOutcome::LeaseLost { .. }) => {}
        Err(error) => tracing::warn!(error = %error, "Cell effect was not resolved"),
    }
}

fn mutation_identity() -> crate::Result<MutationIdentity> {
    let now_ms = super::unix_now_ms()?;
    Ok(MutationIdentity {
        request_id: RequestId::from_bytes(Uuid::now_v7().into_bytes()),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms
            .checked_add(60_000)
            .ok_or(crab_cell_runtime::Error::Command(
                "scheduler request expiry overflow",
            ))?,
    })
}

#[cfg(test)]
#[path = "scheduler/tests.rs"]
mod tests;
