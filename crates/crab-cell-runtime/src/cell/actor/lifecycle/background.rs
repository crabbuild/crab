//! Background hydration, inventory, and compaction starters.

use super::*;

pub(in crate::cell::actor) fn start_background_hydration(
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

pub(in crate::cell::actor) fn start_background_inventory(
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

pub(in crate::cell::actor) fn start_background_compaction(
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
