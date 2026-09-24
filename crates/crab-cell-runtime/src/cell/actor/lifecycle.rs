//! Cell lifecycle scheduling for one Cell actor.
//!
//! Activation, bootstrap, background hydration/inventory/compaction, eviction,
//! renewals, transfer inspection, and deactivation all schedule the next step
//! the loop should take.

use super::admission::{fence_active, finish_migration, send_migration_reply};
use super::*;

pub(super) fn start_bounded_evictions(
    limit: usize,
    now_ms: i64,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    movement: &mut MovementBudget,
    movement_permits: &mut HashMap<CellId, MovementPermit>,
) -> usize {
    let mut started = 0;
    for _ in 0..limit {
        let Ok(permit) = movement.try_start(now_ms) else {
            break;
        };
        let mut selected = begin_idle_evictions(1, pool, cells, transitioning, tasks);
        let Some(cell) = selected.pop() else {
            let mut permit = permit;
            movement.complete(&mut permit);
            break;
        };
        movement_permits.insert(cell, permit);
        started += 1;
    }
    started
}

pub(super) fn begin_idle_evictions(
    limit: usize,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
) -> Vec<CellId> {
    let observations = cells
        .iter()
        .map(|(cell, active)| eviction_observation(*cell, active))
        .collect::<Vec<_>>();
    let victims = select_victims(&observations, limit);
    let mut started = Vec::new();
    for cell in victims {
        if begin_idle_cell_eviction(cell, pool, cells, transitioning, tasks, None) {
            started.push(cell);
        }
    }
    started
}

pub(super) fn eviction_observation(cell: CellId, active: &ActiveCell) -> EvictionObservation {
    EvictionObservation {
        cell,
        state: if active.draining() {
            EvictionState::Quiescing
        } else {
            EvictionState::Idle
        },
        last_used_ms: active.last_used_ms,
        cost: ResourceCost::active_cell()
            .with_retained_bytes(usize::try_from(active.publication_bytes).unwrap_or(usize::MAX)),
        busy: active.busy() || active.renewing() || active.transfer.is_some(),
        retained_obligation: active.coordination.publication_count() != 0,
        migrating: active.publisher.is_none(),
        backup_pinned: active.unpublished_node_logs != 0,
        leased_work: !active.queue.is_empty(),
        primitive_obligation: !active.persisted_work.is_transfer_settled(),
        accounting_known: !active.persisted_work.is_unknown(),
    }
}

pub(super) fn transfer_candidate_observation(
    cell: CellId,
    active: &ActiveCell,
) -> EvictionObservation {
    let mut observation = eviction_observation(cell, active);
    if !active.persisted_work.is_unknown() {
        observation.primitive_obligation = false;
    }
    observation
}

pub(super) fn transfer_observation(
    cell: CellId,
    active: &ActiveCell,
    inventory: crate::primitives::maintenance::TransferWorkInventory,
) -> EvictionObservation {
    let mut observation = eviction_observation(cell, active);
    observation.primitive_obligation = !inventory.is_settled();
    observation.accounting_known = true;
    observation
}

pub(super) fn begin_idle_cell_eviction(
    cell: CellId,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    reply: Option<oneshot::Sender<crate::Result<()>>>,
) -> bool {
    let Some(active) = cells.get_mut(&cell) else {
        if let Some(reply) = reply {
            let _ = reply.send(Err(Error::CellNotActive));
        }
        return false;
    };
    let decision = active.coordination.step(CoordinationInput::BeginDrain);
    if matches!(decision, CoordinationDecision::Reject(_)) {
        if let Some(reply) = reply {
            let _ = reply.send(Err(Error::CellDraining));
        }
        return false;
    }
    active.admission.draining.store(true, Ordering::Release);
    active.admission.requests.close();
    active.admission.bytes.close();
    active.drain = reply;
    if matches!(
        schedule(active, true),
        CoordinationDecision::ReadyToDeactivate
    ) {
        start_deactivate(cell, pool, cells, transitioning, tasks);
    }
    true
}
pub(super) async fn activate_restored_and_publish(
    cell: CellId,
    pool: &SqlWorkerPool,
    publisher: &mut CellPublisher,
    activation: RestoredActivation,
) -> crate::Result<Option<crab_ltx::Hydration>> {
    let RestoredActivation {
        database,
        destination,
        incarnation,
        schema,
        root,
        reservation,
    } = activation;
    pool.activate_restored(
        cell,
        database,
        destination,
        incarnation,
        schema,
        root,
        reservation,
    )
    .await?;
    if let Err(error) = publisher.activate().await {
        return match pool.deactivate(cell).await {
            Ok(()) => Err(error),
            Err(cleanup) => Err(cleanup),
        };
    }
    pool.hydration(cell).await
}

pub(super) async fn bootstrap_and_publish(
    cell: CellId,
    pool: &SqlWorkerPool,
    publisher: &mut CellPublisher,
    activation: BootstrapActivation,
) -> crate::Result<Option<crab_ltx::Hydration>> {
    let BootstrapActivation {
        replica,
        destination,
        incarnation,
        schema,
        initialize,
        reservation,
    } = activation;
    let bootstrap = pool.bootstrap(
        cell,
        replica,
        destination,
        incarnation,
        schema,
        initialize,
        reservation,
    );
    tokio::pin!(bootstrap);
    let mut renewal_error = None;
    let bootstrap = loop {
        tokio::select! {
            result = &mut bootstrap => break result,
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(publisher.renewal_at())) => {
                if let Err(error) = publisher.renew().await {
                    renewal_error = Some(error);
                    break bootstrap.await;
                }
            }
        }
    };
    if let Some(error) = renewal_error {
        return match bootstrap {
            Ok(_) => match pool.deactivate(cell).await {
                Ok(()) => Err(error),
                Err(cleanup) => Err(cleanup),
            },
            Err(bootstrap) => Err(bootstrap),
        };
    }
    let bootstrap = bootstrap?;
    let publication = async {
        let prepared = publisher.prepare_initial(&bootstrap.cuts).await?;
        publisher
            .publish_prepared(&prepared, bootstrap.next_due_ms)
            .await?;
        pool.confirm_bootstrap_published(cell, bootstrap.cuts)
            .await?;
        Ok(())
    }
    .await;
    if let Err(error) = publication {
        return match pool.deactivate(cell).await {
            Ok(()) => Err(error),
            Err(cleanup) => Err(cleanup),
        };
    }
    Ok(None)
}

pub(super) fn start_background_hydration(
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    tasks: &mut JoinSet<TaskResult>,
    node_lease: &RuntimeNodeLease,
) {
    if node_lease.check().is_err() {
        for active in cells.values_mut() {
            active.coordination.step(CoordinationInput::Fence);
            fence_active(active);
        }
        return;
    }
    let resources = pool.resource_ledger();
    let candidates = cells
        .iter_mut()
        .filter_map(|(cell, active)| {
            let reservation = resources
                .try_reserve(ResourceCost::zero().with_hydration_jobs(1))
                .ok()?;
            match active.coordination.step(CoordinationInput::BeginHydration {
                queue_empty: active.queue.is_empty(),
                publication_idle: active.coordination.publication_count() == 0,
                lease_live: node_lease.check().is_ok(),
            }) {
                CoordinationDecision::Started => {}
                CoordinationDecision::Fence => {
                    drop(reservation);
                    fence_active(active);
                    return None;
                }
                _ => {
                    drop(reservation);
                    return None;
                }
            }
            let effect_id = active.begin_task(CoordinationEffect::Hydration);
            Some((*cell, active.generation, effect_id, reservation))
        })
        .collect::<Vec<_>>();

    for (cell, generation, effect_id, reservation) in candidates {
        let pool = pool.clone();
        tasks.spawn(async move {
            let _reservation = reservation;
            let deadline = std::time::Instant::now() + SQL_WALL_DEADLINE;
            let result = tokio::time::timeout_at(
                deadline.into(),
                pool.hydrate(cell, HYDRATION_PAGES_PER_STEP, deadline),
            )
            .await
            .map_err(|_| Error::Deadline)
            .and_then(|result| result);
            if result.is_err() {
                let _ = pool.fence(cell).await;
            }
            TaskResult::Hydrated {
                cell,
                generation,
                effect_id,
                result,
            }
        });
    }
}

pub(super) fn start_background_inventory(
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    tasks: &mut JoinSet<TaskResult>,
    node_lease: &RuntimeNodeLease,
) {
    let candidates = cells
        .iter_mut()
        .filter_map(|(cell, active)| {
            let decision = active.coordination.step(CoordinationInput::BeginInventory {
                queue_empty: active.queue.is_empty(),
                publication_idle: active.coordination.publication_count() == 0,
                inventory_unknown: active.persisted_work.is_unknown(),
                refreshing: active.inventory_refreshing,
                lease_live: node_lease.check().is_ok(),
            });
            if matches!(decision, CoordinationDecision::Fence) {
                fence_active(active);
                return None;
            }
            if !matches!(decision, CoordinationDecision::Started) {
                return None;
            }
            let effect_id = active.begin_task(CoordinationEffect::Inventory);
            active.inventory_refreshing = true;
            Some((*cell, active.generation, active.role, effect_id))
        })
        .collect::<Vec<_>>();

    for (cell, generation, role, effect_id) in candidates {
        let pool = pool.clone();
        tasks.spawn(async move {
            let deadline = std::time::Instant::now() + SQL_WALL_DEADLINE;
            let result =
                tokio::time::timeout_at(deadline.into(), pool.persisted_work_inventory(cell, role))
                    .await
                    .map_err(|_| Error::Deadline)
                    .and_then(|result| result);
            TaskResult::InventoryRefreshed {
                cell,
                generation,
                effect_id,
                result,
            }
        });
    }
}

pub(super) fn start_background_compaction(
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    tasks: &mut JoinSet<TaskResult>,
    node_lease: &RuntimeNodeLease,
) {
    let now = std::time::Instant::now();
    for (cell, active) in cells {
        if now.duration_since(active.last_work_at) < COMPACTION_QUIET
            || now < active.compaction_retry_at
        {
            continue;
        }
        let due = active
            .publisher
            .as_ref()
            .is_some_and(CellPublisher::compaction_due);
        let decision = active
            .coordination
            .step(CoordinationInput::BeginCompaction {
                queue_empty: active.queue.is_empty(),
                publication_idle: active.coordination.publication_count() == 0,
                publisher_ready: active.publisher.is_some(),
                due,
                lease_live: node_lease.check().is_ok(),
            });
        if matches!(decision, CoordinationDecision::Fence) {
            fence_active(active);
            continue;
        }
        if !matches!(decision, CoordinationDecision::Started) {
            continue;
        }
        let Some(mut publisher) = active.publisher.take() else {
            active
                .coordination
                .step(CoordinationInput::FinishCompaction { fenced: true });
            fence_active(active);
            continue;
        };
        let cell = *cell;
        let generation = active.generation;
        let effect_id = active.begin_task(CoordinationEffect::Compaction);
        let pool = pool.clone();
        tasks.spawn(async move {
            let started = std::time::Instant::now();
            let result = publisher.compact_one_quiet().await;
            tracing::debug!(
                elapsed_ms = started.elapsed().as_millis(),
                promoted = matches!(result, Ok(Some(true))),
                retry = matches!(result, Ok(None)),
                succeeded = result.is_ok(),
                "Cell LTX quiet compaction completed"
            );
            if result.is_err() {
                let _ = pool.fence(cell).await;
            }
            TaskResult::Compacted {
                cell,
                generation,
                effect_id,
                publisher: Box::new(publisher),
                result,
            }
        });
    }
}

pub(super) fn schedule(active: &mut ActiveCell, lease_live: bool) -> CoordinationDecision {
    let publication_blocked = active.queue.front().is_some_and(|work| {
        matches!(work, QueuedWork::Command(_))
            && (active.coordination.publication_count() >= MAX_PENDING_PUBLICATIONS
                || active.publication_bytes >= PENDING_PUBLICATION_HIGH_WATER_BYTES)
    });
    active.coordination.step(CoordinationInput::Schedule {
        queue_empty: active.queue.is_empty(),
        publisher_ready: active.publisher.is_some(),
        publication_blocked,
        lease_live,
    })
}

pub(super) fn start_next(
    active: &mut ActiveCell,
    pool: &SqlWorkerPool,
    tasks: &mut JoinSet<TaskResult>,
    node_lease: &RuntimeNodeLease,
) {
    let decision = schedule(active, node_lease.check().is_ok());
    if matches!(decision, CoordinationDecision::Fence) {
        fence_active(active);
        return;
    }
    if !matches!(decision, CoordinationDecision::StartQueuedWork) {
        return;
    }
    let Some(work) = active.queue.pop_front() else {
        return;
    };
    if matches!(&work, QueuedWork::Command(_) | QueuedWork::Migration(_)) {
        // Durable command outcomes, effects, Queue rows, and Workflow runs
        // remain release obligations until a fresh inventory proves otherwise.
        active.persisted_work = crate::primitives::maintenance::PersistedWorkInventory::unknown();
    }
    let generation = active.generation;
    active.last_used_ms = unix_millis();
    active.last_work_at = std::time::Instant::now();
    let kind = match &work {
        QueuedWork::Command(_) => AdmissionKind::Command,
        QueuedWork::Query(_) => AdmissionKind::Query,
        QueuedWork::Resolve(_) => AdmissionKind::Resolve,
        QueuedWork::Migration(_) => AdmissionKind::Migration,
    };
    if !matches!(
        active.coordination.step(CoordinationInput::BeginWork {
            kind,
            publisher_ready: active.publisher.is_some(),
        }),
        CoordinationDecision::Started
    ) {
        active.queue.push_front(work);
        return;
    }
    let pool = pool.clone();
    let interrupt = active.interrupt.clone();
    match work {
        QueuedWork::Command(command) => {
            let durability = active.durability_submitter.clone();
            let effect_id = active.begin_task(CoordinationEffect::Work(kind));
            tasks.spawn(async move {
                execute_command(pool, durability, command, interrupt, generation, effect_id).await
            });
        }
        QueuedWork::Query(query) => {
            let effect_id = active.begin_task(CoordinationEffect::Work(kind));
            tasks.spawn(async move {
                execute_query(pool, query, interrupt, generation, effect_id).await
            });
        }
        QueuedWork::Resolve(resolve) => {
            let effect_id = active.begin_task(CoordinationEffect::Work(kind));
            tasks.spawn(async move {
                execute_resolve(pool, resolve, interrupt, generation, effect_id).await
            });
        }
        QueuedWork::Migration(migration) => {
            let Some(publisher) = active.publisher.take() else {
                let mut migration = migration;
                send_migration_reply(&mut migration, Err(Error::Fenced));
                finish_migration(active, true);
                return;
            };
            let effect_id = active.begin_task(CoordinationEffect::Work(kind));
            tasks.spawn(async move {
                execute_migration(
                    pool,
                    Box::new(publisher),
                    migration,
                    interrupt,
                    generation,
                    effect_id,
                )
                .await
            });
        }
    }
}
pub(super) fn start_transfer_inspection(
    cell: CellId,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    tasks: &mut JoinSet<TaskResult>,
    node_lease: &RuntimeNodeLease,
) {
    let Some(active) = cells.get_mut(&cell) else {
        return;
    };
    if active.transfer.is_none()
        || active.inventory_refreshing
        || !active.queue.is_empty()
        || !active.coordination.can_deactivate()
    {
        return;
    }
    if node_lease.check().is_err() {
        fence_active(active);
        return;
    }
    let generation = active.generation;
    let role = active.role;
    let effect_id = active.begin_task(CoordinationEffect::Inventory);
    active.inventory_refreshing = true;
    let pool = pool.clone();
    tasks.spawn(async move {
        let deadline = std::time::Instant::now() + SQL_WALL_DEADLINE;
        let result = tokio::time::timeout_at(
            deadline.into(),
            pool.transfer_work_inventory(cell, role, unix_millis()),
        )
        .await
        .map_err(|_| Error::Deadline)
        .and_then(|result| result);
        TaskResult::TransferPreflight {
            cell,
            generation,
            effect_id,
            result,
        }
    });
}

pub(super) fn start_due_renewals(
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    tasks: &mut JoinSet<TaskResult>,
    node_lease: &RuntimeNodeLease,
) {
    let active_renewals = cells.values().filter(|active| active.renewing()).count();
    let mut available = MAX_RENEWALS_IN_FLIGHT.saturating_sub(active_renewals);
    if available == 0 {
        return;
    }
    let now = std::time::Instant::now();
    for (cell, active) in cells.iter_mut() {
        if available == 0 {
            break;
        }
        if active
            .publisher
            .as_ref()
            .is_none_or(|publisher| !publisher.renewal_due(now))
        {
            continue;
        }
        match active.coordination.step(CoordinationInput::BeginRenewal {
            queue_empty: active.queue.is_empty(),
            publication_idle: active.coordination.publication_count() == 0,
            lease_live: node_lease.check().is_ok(),
        }) {
            CoordinationDecision::Started => {}
            CoordinationDecision::Fence => {
                fence_active(active);
                continue;
            }
            _ => continue,
        }
        let Some(mut publisher) = active.publisher.take() else {
            active
                .coordination
                .step(CoordinationInput::FinishRenewal { fenced: true });
            continue;
        };
        let generation = active.generation;
        let effect_id = active.begin_task(CoordinationEffect::Renewal);
        available -= 1;
        let cell = *cell;
        let pool = pool.clone();
        tasks.spawn(async move {
            let result = publisher.renew().await;
            if result.is_err() {
                let _ = pool.fence(cell).await;
            }
            TaskResult::Renewed {
                cell,
                generation,
                effect_id,
                publisher: Box::new(publisher),
                result,
            }
        });
    }
}

pub(super) fn start_deactivate(
    cell: CellId,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
) {
    let Some(active) = cells.remove(&cell) else {
        return;
    };
    transitioning.insert(cell);
    let generation = active.generation;
    let pool = pool.clone();
    tasks.spawn(async move {
        let result = async {
            pool.deactivate(cell).await?;
            let mut publisher = active.publisher.ok_or(Error::Fenced)?;
            publisher.release().await
        }
        .await;
        TaskResult::Deactivated {
            cell,
            generation,
            reply: active.drain,
            shutdown_drain: active.coordination.is_shutdown(),
            result,
        }
    });
}

pub(super) fn start_fenced_deactivate(
    cell: CellId,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    preserve_owner: bool,
) {
    let Some(active) = cells.remove(&cell) else {
        return;
    };
    transitioning.insert(cell);
    let generation = active.generation;
    let pool = pool.clone();
    tasks.spawn(async move {
        let result = async {
            pool.discard(cell).await?;
            // An unpublished node-log cut must keep its owner record so
            // takeover seals and replays it instead of treating the Cell as idle.
            if preserve_owner {
                return Ok(());
            }
            let mut publisher = active.publisher.ok_or(Error::Fenced)?;
            publisher.release_after_fence().await
        }
        .await;
        TaskResult::Deactivated {
            cell,
            generation,
            reply: active.drain,
            shutdown_drain: active.coordination.is_shutdown(),
            result,
        }
    });
}

pub(super) async fn cleanup_failed_activation(
    cell: CellId,
    pool: &SqlWorkerPool,
    publisher: &mut CellPublisher,
) -> crate::Result<()> {
    match pool.deactivate(cell).await {
        Ok(()) | Err(Error::CellNotActive) => {}
        Err(error) => return Err(error),
    }
    if publisher.control().value().root.is_some() {
        publisher.release().await
    } else {
        Ok(())
    }
}

pub(super) async fn rollback_failed_acquisition(
    authority: &CellAuthority,
    claimed: &VersionedControl,
    replica: &crab_ltx::CellReplica,
    node_lease: Option<NodeLeaseGuard>,
) -> crate::Result<()> {
    let current = authority
        .load(claimed.value().cell)
        .await?
        .ok_or(Error::Fenced)?;
    if current.value().state == crate::control::ControlState::Idle
        && current.value().owner.is_none()
    {
        return Ok(());
    }
    if current.value().epoch != claimed.value().epoch
        || current.value().owner != claimed.value().owner
        || current.value().root != claimed.value().root
        || current.value().recovery != claimed.value().recovery
        || current.value().code != claimed.value().code
        || current.value().schema != claimed.value().schema
        || current.value().recovery.is_some()
    {
        return Ok(());
    }
    let mut publisher =
        CellPublisher::new(replica.clone(), authority.clone(), current, PathBuf::new());
    if let Some(node_lease) = node_lease {
        publisher = publisher.with_node_lease(node_lease);
    }
    match publisher.release().await {
        Ok(_) => Ok(()),
        Err(error) => {
            let latest = authority
                .load(claimed.value().cell)
                .await?
                .ok_or(Error::Fenced)?;
            if (latest.value().state == crate::control::ControlState::Idle
                && latest.value().owner.is_none())
                || latest.value().epoch != claimed.value().epoch
                || latest.value().owner != claimed.value().owner
                || latest.value().root != claimed.value().root
                || latest.value().recovery != claimed.value().recovery
                || latest.value().code != claimed.value().code
                || latest.value().schema != claimed.value().schema
            {
                Ok(())
            } else {
                Err(error)
            }
        }
    }
}

pub(super) fn start_orphan_deactivate(
    cell: CellId,
    pool: &SqlWorkerPool,
    mut publisher: CellPublisher,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    shutdown_drain: bool,
    generation: u64,
) {
    let pool = pool.clone();
    tasks.spawn(async move {
        let result = async {
            pool.deactivate(cell).await?;
            publisher.release().await
        }
        .await;
        TaskResult::Deactivated {
            cell,
            generation,
            reply: None,
            shutdown_drain,
            result,
        }
    });
    transitioning.insert(cell);
}
