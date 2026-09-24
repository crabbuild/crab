//! Activation and hydration completions for one Cell actor.

use super::*;

/// Applies an activation result: admit the Cell, or fence and clean up.
#[expect(
    clippy::too_many_arguments,
    reason = "the actor loop hands each protocol facility and finished-task field to the handler explicitly"
)]
pub(super) fn handle_activated(
    context: TaskContext<'_>,
    cell: CellId,
    generation: u64,
    role: CatalogRole,
    publisher: Box<CellPublisher>,
    admission: Arc<CellAdmission>,
    reply: oneshot::Sender<crate::Result<Arc<CellAdmission>>>,
    result: crate::Result<(
        Arc<crab_ltx::rusqlite::InterruptHandle>,
        Option<crab_ltx::Hydration>,
    )>,
    persisted_work: crate::Result<crate::primitives::maintenance::PersistedWorkInventory>,
) {
    let TaskContext {
        pool,
        cells,
        transitioning,
        tasks,
        shutdown,
        node_lease,
        ..
    } = context;
    match result {
        Ok((interrupt, hydration)) => {
            if node_lease.check().is_err() {
                fence_admission(&admission);
                let _ = reply.send(Err(Error::Fenced));
                start_orphan_deactivate(
                    cell,
                    pool,
                    *publisher,
                    transitioning,
                    tasks,
                    false,
                    generation,
                );
                return;
            }
            if shutdown.as_ref().is_some_and(|state| state.draining) {
                admission.draining.store(true, Ordering::Release);
                admission.requests.close();
                admission.bytes.close();
                let _ = reply.send(Err(Error::RuntimeClosed));
                start_orphan_deactivate(
                    cell,
                    pool,
                    *publisher,
                    transitioning,
                    tasks,
                    true,
                    generation,
                );
                return;
            }
            if reply.send(Ok(admission.clone())).is_err() {
                start_orphan_deactivate(
                    cell,
                    pool,
                    *publisher,
                    transitioning,
                    tasks,
                    false,
                    generation,
                );
                return;
            }
            transitioning.remove(&cell);
            let control = publisher.control().value();
            let incarnation = control.incarnation;
            let code = control.code;
            let schema = control.schema;
            let durability_submitter = publisher.durability_submitter();
            let residency = hydration.map_or(Residency::Resident, |progress| {
                if progress.complete() {
                    Residency::Resident
                } else {
                    Residency::Sparse
                }
            });
            let persisted_work = match persisted_work {
                Ok(inventory) => inventory,
                Err(_) => {
                    // An inventory read is a safety precondition for
                    // eviction. Unknown accounting must remain ineligible.
                    crate::primitives::maintenance::PersistedWorkInventory::unknown()
                }
            };
            cells.insert(
                cell,
                ActiveCell {
                    generation,
                    admission,
                    incarnation,
                    code,
                    schema,
                    role,
                    interrupt,
                    publisher: Some(*publisher),
                    durability_submitter,
                    publications: VecDeque::new(),
                    publication_bytes: 0,
                    unpublished_node_logs: 0,
                    queue: VecDeque::new(),
                    coordination: CoordinationState::serving_with_residency(true, residency),
                    persisted_work,
                    inventory_refreshing: false,
                    drain: None,
                    transfer: None,
                    last_used_ms: unix_millis(),
                    last_work_at: std::time::Instant::now(),
                    compaction_retry_at: std::time::Instant::now(),
                },
            );
        }
        Err(error) => {
            transitioning.remove(&cell);
            let error = if node_lease.check().is_err() {
                Error::Fenced
            } else {
                error
            };
            let _ = reply.send(Err(error));
        }
    }
}

/// Applies a hydration result and continues the Cell's schedule.
pub(super) fn handle_hydrated(
    context: TaskContext<'_>,
    cell: CellId,
    generation: u64,
    effect_id: u64,
    result: crate::Result<Option<crab_ltx::Hydration>>,
) {
    let TaskContext {
        pool,
        cells,
        transitioning,
        tasks,
        node_lease,
        ..
    } = context;
    let Some(active) = cells.get_mut(&cell) else {
        return;
    };
    if active.generation != generation
        || !active
            .coordination
            .effect_matches(effect_id, CoordinationEffect::Hydration)
    {
        return;
    }
    active.finish_task(effect_id, CoordinationEffect::Hydration);
    match result {
        Ok(Some(progress)) => {
            active
                .coordination
                .step(CoordinationInput::FinishHydration {
                    complete: progress.complete(),
                    stale: false,
                });
        }
        Ok(None) => {
            active
                .coordination
                .step(CoordinationInput::FinishHydration {
                    complete: true,
                    stale: false,
                });
        }
        Err(_) => {
            let decision = active
                .coordination
                .step(CoordinationInput::FinishHydration {
                    complete: false,
                    stale: true,
                });
            if matches!(decision, CoordinationDecision::Fence) {
                fence_active(active);
            }
        }
    }
    continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
}
