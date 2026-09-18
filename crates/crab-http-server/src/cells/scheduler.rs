use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    },
    time::Duration,
};

use crab_cell_runtime::{
    ActivityRunOutcome, ApplicationIdentity, BlockingActivityPool, BlockingActivityReservation,
    CatalogProof, CatalogShardScan, CellAuthority, CellCatalog, CellId, CellTarget, ControlState,
    DueCellScan, EffectRunOutcome, FencedNodeSession, InvocationError, MaintenanceTickOutcome,
    MaintenanceTickRequest, MigrationFailure, MigrationProgressAttempt, MigrationProgressStore,
    MutationIdentity, NodeDirectory, NodeLogRecovery, NodeLogTransport, RecoveryCoordinator,
    RecoveryManifestStore, Registry, ReleaseState, ReleaseStore, RequestId, SchedulerFleet,
    SessionId, preferred_scanner, recoverable_cells,
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
const MAX_MIGRATION_JOBS: usize = 16;
const MAX_MIGRATION_SCANS_PER_CYCLE: usize = 128;
const SCHEDULER_STALE_AFTER_MS: i64 = 15_000;
const NODE_COLLECTION_INTERVAL_MS: i64 = 60_000;
const NODE_COLLECTION_LIMIT: usize = 128;
const MAX_NODE_RECOVERY_JOBS: usize = 2;
const MAX_NODE_RECOVERY_CELLS: usize = 10_000;
const RECOVERY_CLAIM_HEARTBEAT: Duration = Duration::from_secs(10);
const RECOVERY_CLAIM_REFRESH_TIMEOUT: Duration = Duration::from_secs(5);
const RECOVERY_CLAIM_STORAGE_TIMEOUT: Duration = Duration::from_secs(5);

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
    releases: ReleaseStore,
    migration_progress: MigrationProgressStore,
    directory: NodeDirectory,
    router: RepositoryCellRouter,
    session: SessionId,
    status: SchedulerStatus,
    fleet: SchedulerFleet,
    registry: Arc<Registry>,
    scans: HashMap<u8, DueCellScan>,
    migration_operation: Option<RequestId>,
    migration_scans: HashMap<u8, MigrationShardScan>,
    migration_cells: Arc<Mutex<HashSet<CellId>>>,
    migration_jobs: tokio::task::JoinSet<crate::Result<()>>,
    node_log_transport: Option<Arc<dyn NodeLogTransport>>,
    metrics: Option<crate::metrics::Metrics>,
    recovery_sessions: Arc<Mutex<HashSet<SessionId>>>,
    recovery_jobs: tokio::task::JoinSet<crate::Result<SessionId>>,
    recovery_manifests: RecoveryManifestStore,
    recovery_disk: crab_cell_runtime::DiskBudget,
    next_migration_shard: u8,
    next_shard: u8,
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
            authority: CellAuthority::new(layout.clone()),
            releases: ReleaseStore::new(layout.clone(), identity)?,
            migration_progress: MigrationProgressStore::new(layout.clone(), identity)?,
            directory,
            router,
            session,
            status,
            fleet: SchedulerFleet::default(),
            registry,
            scans: HashMap::new(),
            migration_operation: None,
            migration_scans: HashMap::new(),
            migration_cells: Arc::new(Mutex::new(HashSet::new())),
            migration_jobs: tokio::task::JoinSet::new(),
            node_log_transport: None,
            metrics: None,
            recovery_sessions: Arc::new(Mutex::new(HashSet::new())),
            recovery_jobs: tokio::task::JoinSet::new(),
            recovery_manifests: RecoveryManifestStore::new(
                layout.clone(),
                super::repository_replica_limits(),
            ),
            recovery_disk: crab_cell_runtime::DiskBudget::new(512 << 20),
            next_migration_shard: 0,
            next_shard: 0,
            blocking_activities,
            activity_cells: Arc::new(Mutex::new(HashSet::new())),
            activity_jobs: tokio::task::JoinSet::new(),
            last_node_collection_ms: 0,
        })
    }

    #[must_use]
    pub(crate) fn with_node_recovery(mut self, transport: Arc<dyn NodeLogTransport>) -> Self {
        self.node_log_transport = Some(transport);
        self
    }

    pub(crate) fn with_node_recovery_disk(
        mut self,
        recovery_disk: crab_cell_runtime::DiskBudget,
    ) -> Self {
        self.recovery_disk = recovery_disk;
        self
    }

    pub(crate) fn with_metrics(mut self, metrics: crate::metrics::Metrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    pub(crate) async fn run(mut self, cancellation: CancellationToken) -> crate::Result<()> {
        loop {
            if cancellation.is_cancelled() {
                break;
            }
            self.reap_activity_jobs();
            self.reap_migration_jobs();
            self.reap_recovery_jobs();
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
        self.migration_jobs.abort_all();
        self.recovery_jobs.abort_all();
        while self.activity_jobs.join_next().await.is_some() {}
        while self.migration_jobs.join_next().await.is_some() {}
        while self.recovery_jobs.join_next().await.is_some() {}
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
        self.reap_migration_jobs();
        self.reap_recovery_jobs();
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
        if preferred_scanner(0, &nodes)? == Some(self.session) {
            self.schedule_node_recovery(now_ms).await?;
        }
        let start = self.next_shard;
        let mut assigned = Vec::new();
        for offset in 0_u16..=u8::MAX.into() {
            let shard = start.wrapping_add(offset as u8);
            if preferred_scanner(shard, &nodes)? == Some(self.session) {
                assigned.push(shard);
            } else {
                self.scans.remove(&shard);
                self.migration_scans.remove(&shard);
            }
        }

        self.schedule_migrations(&assigned).await?;

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

    async fn schedule_migrations(&mut self, assigned: &[u8]) -> crate::Result<()> {
        let operation = match self.releases.load().await? {
            Some(release)
                if release.record().state() == ReleaseState::Activating
                    && release.record().desired() == Some(self.registry.release_digest()) =>
            {
                release.record().operation()
            }
            _ => {
                self.migration_operation = None;
                self.migration_scans.clear();
                self.next_migration_shard = 0;
                return Ok(());
            }
        };
        if self.migration_operation != Some(operation) {
            self.migration_operation = Some(operation);
            self.migration_scans.clear();
            self.next_migration_shard = 0;
        }

        let mut remaining = MAX_MIGRATION_JOBS.saturating_sub(self.migration_jobs.len());
        if remaining == 0 {
            return Ok(());
        }
        let mut scans = MAX_MIGRATION_SCANS_PER_CYCLE;
        let mut migration_shards = assigned.to_vec();
        migration_shards
            .sort_unstable_by_key(|shard| shard.wrapping_sub(self.next_migration_shard));
        for shard in migration_shards {
            while remaining != 0 && scans != 0 {
                scans -= 1;
                let proof = self.next_migration_entry(shard).await?;
                let Some(proof) = proof else {
                    self.migration_scans.remove(&shard);
                    break;
                };
                let cell = proof.entry().cell();
                let Some(control) = self.authority.load(cell).await? else {
                    continue;
                };
                if control.value().state == ControlState::Tombstoned
                    || self.registry.is_current_cell(
                        proof.entry().namespace(),
                        proof.entry().role(),
                        control.value().code,
                        control.value().schema,
                    )
                {
                    continue;
                }
                let Some(reservation) = self.reserve_migration(cell)? else {
                    continue;
                };
                let target = CellTarget::new(
                    self.identity.tenant(),
                    self.identity.application(),
                    proof.entry().namespace(),
                    proof.entry().partition(),
                )?;
                let target_version = self
                    .registry
                    .current_cell_version(proof.entry().namespace(), proof.entry().role())
                    .ok_or(crab_cell_runtime::Error::Registry(
                        "cataloged Cell namespace has no current release version",
                    ))?;
                let attempt = MigrationProgressAttempt::new(
                    operation,
                    self.registry.release_digest(),
                    self.session,
                    cell,
                    (control.value().code, control.value().schema),
                    target_version,
                )?;
                let router = self.router.clone();
                let progress = self.migration_progress.clone();
                self.migration_jobs.spawn(async move {
                    let _reservation = reservation;
                    let result = router.migrate_target(target).await;
                    let recorded = match &result {
                        Ok(()) => progress.completed(attempt, super::unix_now_ms()?).await,
                        Err(error) => {
                            progress
                                .failed(attempt, migration_failure(error), super::unix_now_ms()?)
                                .await
                        }
                    };
                    recorded?;
                    result
                });
                remaining -= 1;
            }
            if remaining == 0 || scans == 0 {
                self.next_migration_shard = shard;
                break;
            }
            self.next_migration_shard = shard.wrapping_add(1);
        }
        Ok(())
    }

    async fn next_migration_entry(&mut self, shard: u8) -> crate::Result<Option<CatalogProof>> {
        if !self.migration_scans.contains_key(&shard) {
            self.migration_scans.insert(
                shard,
                MigrationShardScan::new(self.catalog.scan_shard(shard).await?),
            );
        }
        self.migration_scans
            .get_mut(&shard)
            .ok_or(crab_cell_runtime::Error::Control(
                "release migration shard cursor disappeared",
            ))?
            .next()
            .await
            .map_err(Into::into)
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
            && let Some(job) = self.router.reserve_primitive_job()?
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
                drop(job);
                if registry.has_effect_runner(target.namespace())
                    && let Err(error) = run_effect(&registry, &router, cell).await
                {
                    tracing::warn!(error = %error, "Cell effect was not resolved");
                }
                if release_after && let Err(error) = router.drain_local_target(&target).await {
                    tracing::warn!(error = %error, "Workflow scheduler Cell release failed");
                }
                drop(activity_cell);
                drop(activity_bytes);
            });
            return Ok(());
        }
        if !self.registry.has_effect_runner(cell.target.namespace()) {
            return self.release_after(&cell.target, release_after).await;
        }
        let target = cell.target.clone();
        let Some(job) = self.router.reserve_primitive_job()? else {
            return self.release_after(&target, release_after).await;
        };
        let result = match self
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
        };
        drop(job);
        result
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

    fn reap_migration_jobs(&mut self) {
        while let Some(result) = self.migration_jobs.try_join_next() {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(error = %error, "Cell release migration failed"),
                Err(error) if !error.is_cancelled() => {
                    tracing::warn!(error = %error, "Cell release migration task failed");
                }
                Err(_) => {}
            }
        }
    }

    fn reap_recovery_jobs(&mut self) {
        while let Some(result) = self.recovery_jobs.try_join_next() {
            match result {
                Ok(Ok(session)) => {
                    tracing::info!(?session, "sealed recovered Cell node log");
                }
                Ok(Err(error)) => tracing::warn!(error = %error, "Cell node-log recovery failed"),
                Err(error) if !error.is_cancelled() => {
                    tracing::warn!(error = %error, "Cell node-log recovery task failed");
                }
                Err(_) => {}
            }
        }
        if let Some(metrics) = &self.metrics {
            metrics.update_recovery_states(self.recovery_jobs.len(), 0);
        }
    }

    async fn schedule_node_recovery(&mut self, now_ms: i64) -> crate::Result<()> {
        let Some(transport) = self.node_log_transport.as_ref() else {
            return Ok(());
        };
        let available = MAX_NODE_RECOVERY_JOBS.saturating_sub(self.recovery_jobs.len());
        if available == 0 {
            return Ok(());
        }
        let candidates = self
            .directory
            .recovery_candidates(self.session, now_ms, available)
            .await?;
        for session in candidates {
            let Some(reservation) = self.reserve_recovery(session)? else {
                continue;
            };
            let directory = self.directory.clone();
            let catalog = self.catalog.clone();
            let authority = self.authority.clone();
            let manifests = self.recovery_manifests.clone();
            let transport = Arc::clone(transport);
            let claimant = self.session;
            let metrics = self.metrics.clone();
            let recovery_disk = self.recovery_disk.clone();
            self.recovery_jobs.spawn(async move {
                let _reservation = reservation;
                let started = std::time::Instant::now();
                let result = recover_node_session(
                    directory,
                    catalog,
                    authority,
                    manifests,
                    transport,
                    recovery_disk,
                    session,
                    claimant,
                )
                .await;
                if let Some(metrics) = metrics {
                    metrics.record_recovery_finished(
                        started.elapsed(),
                        result.as_ref().err().map(|error| match error {
                            crate::Error::Storage(_) => {
                                crate::metrics::RecoveryFailureReason::Storage
                            }
                            crate::Error::Cell(crab_cell_runtime::Error::Capacity(_)) => {
                                crate::metrics::RecoveryFailureReason::Capacity
                            }
                            crate::Error::Cell(crab_cell_runtime::Error::Fenced) => {
                                crate::metrics::RecoveryFailureReason::Fenced
                            }
                            _ => crate::metrics::RecoveryFailureReason::Other,
                        }),
                    );
                }
                result?;
                Ok(session)
            });
        }
        if let Some(metrics) = &self.metrics {
            metrics.update_recovery_states(self.recovery_jobs.len(), 0);
        }
        Ok(())
    }

    fn reserve_migration(&self, cell: CellId) -> crate::Result<Option<MigrationCellReservation>> {
        let Some(job) = self.router.reserve_primitive_job()? else {
            return Ok(None);
        };
        let mut cells = self
            .migration_cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !cells.insert(cell) {
            drop(job);
            return Ok(None);
        }
        Ok(Some(MigrationCellReservation {
            cell,
            cells: Arc::clone(&self.migration_cells),
            _job: job,
        }))
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

    fn reserve_recovery(
        &self,
        session: SessionId,
    ) -> crate::Result<Option<RecoverySessionReservation>> {
        let Some(job) = self.router.reserve_primitive_job()? else {
            return Ok(None);
        };
        let mut sessions = self
            .recovery_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !sessions.insert(session) {
            drop(job);
            return Ok(None);
        }
        Ok(Some(RecoverySessionReservation {
            session,
            sessions: Arc::clone(&self.recovery_sessions),
            _job: job,
        }))
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

struct MigrationShardScan {
    scan: CatalogShardScan,
    entries: VecDeque<CatalogProof>,
}

impl MigrationShardScan {
    fn new(scan: CatalogShardScan) -> Self {
        Self {
            scan,
            entries: VecDeque::new(),
        }
    }

    async fn next(&mut self) -> crab_cell_runtime::Result<Option<CatalogProof>> {
        loop {
            if let Some(entry) = self.entries.pop_front() {
                return Ok(Some(entry));
            }
            let Some(page) = self.scan.next_page().await? else {
                return Ok(None);
            };
            self.entries.extend(page.entries().iter().cloned());
        }
    }
}

struct ActivityCellReservation {
    cell: CellId,
    cells: Arc<Mutex<HashSet<CellId>>>,
}

struct MigrationCellReservation {
    cell: CellId,
    cells: Arc<Mutex<HashSet<CellId>>>,
    _job: crab_cell_runtime::NodeJobReservation,
}

struct RecoverySessionReservation {
    session: SessionId,
    sessions: Arc<Mutex<HashSet<SessionId>>>,
    _job: crab_cell_runtime::NodeJobReservation,
}

impl Drop for MigrationCellReservation {
    fn drop(&mut self) {
        self.cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.cell);
    }
}

impl Drop for ActivityCellReservation {
    fn drop(&mut self) {
        self.cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.cell);
    }
}

impl Drop for RecoverySessionReservation {
    fn drop(&mut self) {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.session);
    }
}

async fn recover_node_session(
    directory: NodeDirectory,
    catalog: crab_cell_runtime::CellCatalog,
    authority: CellAuthority,
    manifests: RecoveryManifestStore,
    transport: Arc<dyn NodeLogTransport>,
    recovery_disk: crab_cell_runtime::DiskBudget,
    session: SessionId,
    claimant: SessionId,
) -> crate::Result<()> {
    let mut fenced = claim_expired_with_timeout(
        &directory,
        session,
        claimant,
        super::unix_now_ms()?,
        RECOVERY_CLAIM_STORAGE_TIMEOUT,
    )
    .await?;
    tracing::debug!(?session, ?claimant, "claimed expired Cell node log");
    let cells = await_with_claim_heartbeat(
        &directory,
        &mut fenced,
        recoverable_cells(&catalog, &authority, session, MAX_NODE_RECOVERY_CELLS),
    )
    .await?;
    tracing::debug!(
        ?session,
        ?claimant,
        cells = cells.len(),
        "inventoried Cells for node-log recovery"
    );
    let recovery = NodeLogRecovery::from_fenced_with_disk(
        transport,
        &fenced,
        super::repository_replica_limits(),
        recovery_disk,
    )?;
    let coordinator = RecoveryCoordinator::new(recovery, manifests);
    let recovery_fence = fenced.clone();
    let controls = await_with_claim_heartbeat(
        &directory,
        &mut fenced,
        coordinator.recover(recovery_fence, cells),
    )
    .await?;
    tracing::debug!(
        ?session,
        ?claimant,
        controls = controls.len(),
        "pinned recovered Cell overlays"
    );
    coordinator
        .finish(&directory, fenced, controls, super::unix_now_ms()?)
        .await?;
    tracing::debug!(?session, ?claimant, "sealed recovered Cell node log");
    Ok(())
}

async fn claim_expired_with_timeout(
    directory: &NodeDirectory,
    session: SessionId,
    claimant: SessionId,
    now_ms: i64,
    timeout: Duration,
) -> crate::Result<crab_cell_runtime::FencedNodeSession> {
    tokio::time::timeout(timeout, directory.claim_expired(session, claimant, now_ms))
        .await
        .map_err(|_| crab_cell_runtime::Error::Deadline)?
        .map_err(Into::into)
}

async fn await_with_claim_heartbeat<T, F>(
    directory: &NodeDirectory,
    fenced: &mut FencedNodeSession,
    future: F,
) -> crate::Result<T>
where
    F: Future<Output = crab_cell_runtime::Result<T>>,
{
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return result.map_err(Into::into),
            () = tokio::time::sleep(RECOVERY_CLAIM_HEARTBEAT) => {
                let refreshed = tokio::time::timeout(
                    RECOVERY_CLAIM_REFRESH_TIMEOUT,
                    directory.refresh_recovery_claim(fenced, super::unix_now_ms()?),
                )
                .await
                .map_err(|_| crab_cell_runtime::Error::Fenced)??;
                *fenced = refreshed;
            }
        }
    }
}

async fn run_effect(
    registry: &Registry,
    router: &RepositoryCellRouter,
    cell: super::RepositoryCell,
) -> crate::Result<()> {
    let Some(_job) = router.reserve_primitive_job()? else {
        return Ok(());
    };
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
    Ok(())
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

fn migration_failure(error: &crate::Error) -> MigrationFailure {
    match error {
        crate::Error::Cell(crab_cell_runtime::Error::Capacity(_)) => MigrationFailure::Capacity,
        crate::Error::Cell(crab_cell_runtime::Error::Deadline) => MigrationFailure::Deadline,
        crate::Error::Cell(
            crab_cell_runtime::Error::Registry(_)
            | crab_cell_runtime::Error::Control(_)
            | crab_cell_runtime::Error::Release(_),
        )
        | crate::Error::Config(_) => MigrationFailure::Incompatible,
        crate::Error::Cell(
            crab_cell_runtime::Error::CellNotActive
            | crab_cell_runtime::Error::CellDraining
            | crab_cell_runtime::Error::Fenced
            | crab_cell_runtime::Error::RuntimeClosed
            | crab_cell_runtime::Error::PeerTransport { .. }
            | crab_cell_runtime::Error::PeerTransportUnknown { .. }
            | crab_cell_runtime::Error::Storage(_),
        )
        | crate::Error::Storage(_) => MigrationFailure::Unavailable,
        _ => MigrationFailure::Internal,
    }
}

#[cfg(test)]
#[path = "scheduler/tests.rs"]
mod tests;
