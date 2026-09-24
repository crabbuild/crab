//! Task-completion handling for one Cell actor.
//!
//! Everything here reacts to a finished background task: activation,
//! publication, migration, transfer, hydration, eviction, and drain. The loop
//! itself and the request paths stay in `task.rs`.

use super::admission::{
    fail_shutdown, fence_active, fence_admission, finish_migration, finish_work, rejection_error,
    send_command_reply, send_command_task_reply, send_migration_reply, send_query_reply,
    send_resolve_reply, subtract_unpublished_bytes,
};
use super::*;

#[expect(
    clippy::too_many_arguments,
    reason = "the actor adapter passes each independently owned protocol facility explicitly"
)]
pub(super) fn handle_task(
    result: TaskResult,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    shutdown: &mut Option<ShutdownState>,
    node_lease: &RuntimeNodeLease,
    unpublished_node_log_bytes: &AtomicU64,
    movement: &mut MovementBudget,
    movement_permits: &mut HashMap<CellId, MovementPermit>,
) {
    match result {
        TaskResult::Activated {
            cell,
            generation,
            role,
            publisher,
            admission,
            reply,
            result,
            persisted_work,
        } => match result {
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
        },
        TaskResult::Hydrated {
            cell,
            generation,
            effect_id,
            result,
        } => {
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
        TaskResult::InventoryRefreshed {
            cell,
            generation,
            effect_id,
            result,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                return;
            };
            if active.generation != generation
                || !active
                    .coordination
                    .effect_matches(effect_id, CoordinationEffect::Inventory)
            {
                return;
            }
            active.finish_task(effect_id, CoordinationEffect::Inventory);
            active.inventory_refreshing = false;
            if let Ok(inventory) = result {
                active.persisted_work = inventory;
            }
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::TransferPreflight {
            cell,
            generation,
            effect_id,
            result,
        } => {
            let preflight = {
                let Some(active) = cells.get_mut(&cell) else {
                    return;
                };
                if active.generation != generation
                    || !active
                        .coordination
                        .effect_matches(effect_id, CoordinationEffect::Inventory)
                {
                    return;
                }
                active.finish_task(effect_id, CoordinationEffect::Inventory);
                active.inventory_refreshing = false;
                match active.transfer.take() {
                    Some(transfer) => {
                        let mut ready_to_deactivate = false;
                        let mut fenced = false;
                        let transfer_result = if node_lease.check().is_err() {
                            fenced = true;
                            active.coordination.step(CoordinationInput::Fence);
                            fence_active(active);
                            Err(Error::Fenced)
                        } else {
                            match result {
                                Ok(inventory) => {
                                    if inventory.is_settled()
                                        && active.queue.is_empty()
                                        && active.coordination.can_deactivate()
                                        && transfer_observation(cell, active, inventory).eligible()
                                    {
                                        match active
                                            .coordination
                                            .step(CoordinationInput::ConfirmTransfer)
                                        {
                                            CoordinationDecision::ReadyToDeactivate => {
                                                ready_to_deactivate = true;
                                                Ok(())
                                            }
                                            CoordinationDecision::Started => Ok(()),
                                            CoordinationDecision::Reject(RejectReason::Fenced) => {
                                                fenced = true;
                                                fence_active(active);
                                                Err(Error::Fenced)
                                            }
                                            CoordinationDecision::Reject(reason) => {
                                                Err(rejection_error(reason))
                                            }
                                            _ => Err(Error::CellDraining),
                                        }
                                    } else {
                                        Err(Error::CellDraining)
                                    }
                                }
                                Err(error) => Err(error),
                            }
                        };
                        if transfer_result.is_err() && !fenced {
                            active.coordination.step(CoordinationInput::AbortTransfer);
                        }
                        Some((transfer, ready_to_deactivate, fenced, transfer_result))
                    }
                    None => None,
                }
            };
            let Some((transfer, ready_to_deactivate, _fenced, transfer_result)) = preflight else {
                continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
                return;
            };
            if transfer_result.is_ok() {
                if ready_to_deactivate {
                    if let Some(active) = cells.get_mut(&cell) {
                        active.drain = Some(transfer.reply);
                    }
                    start_deactivate(cell, pool, cells, transitioning, tasks);
                } else if let Some(active) = cells.get_mut(&cell) {
                    active.drain = Some(transfer.reply);
                    continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
                } else {
                    if let Some(mut permit) = movement_permits.remove(&cell) {
                        movement.complete(&mut permit);
                    }
                }
            } else {
                let _ = transfer.reply.send(transfer_result);
                if let Some(mut permit) = movement_permits.remove(&cell) {
                    movement.complete(&mut permit);
                }
                continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
            }
        }
        TaskResult::Executed {
            cell,
            generation,
            effect_id,
            mut command,
            mut result,
            mut fenced,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                // Publication failure may fence and remove the Cell before its proof waiter
                // completes; the accepted command still owns exactly one terminal outcome.
                send_command_task_reply(&mut command, result);
                return;
            };
            if active.generation != generation
                || !active
                    .coordination
                    .effect_matches(effect_id, CoordinationEffect::Work(AdmissionKind::Command))
            {
                send_command_task_reply(&mut command, result);
                return;
            }
            active.finish_task(effect_id, CoordinationEffect::Work(AdmissionKind::Command));
            if node_lease.check().is_err() {
                result = Err(command.operation.unknown(Error::Fenced));
                fenced = true;
            }
            if fenced {
                finish_work(active, true);
                send_command_task_reply(&mut command, result);
                continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
                return;
            }
            match result {
                Ok(CommandTaskResult::Recorded(outcome)) => {
                    finish_work(active, false);
                    send_command_reply(&mut command, Ok(outcome));
                }
                Ok(CommandTaskResult::Pending {
                    pending,
                    durability,
                    retained_reservation,
                }) => {
                    let retained_bytes = pending.retained_bytes();
                    let publication = active
                        .coordination
                        .step(CoordinationInput::BeginPublication);
                    let CoordinationDecision::Started = publication else {
                        drop(retained_reservation);
                        finish_work(active, false);
                        let error = command.operation.unknown(match publication {
                            CoordinationDecision::Reject(reason) => rejection_error(reason),
                            _ => Error::Fenced,
                        });
                        send_command_reply(&mut command, Err(error));
                        continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
                        return;
                    };
                    active.publication_bytes =
                        match active.publication_bytes.checked_add(retained_bytes) {
                            Some(bytes) => bytes,
                            None => {
                                finish_work(active, true);
                                let error = command
                                    .operation
                                    .unknown(Error::Capacity("pending publication bytes"));
                                send_command_reply(&mut command, Err(error));
                                continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
                                return;
                            }
                        };
                    let outcome = pending.outcome().clone();
                    let commit_sequence = outcome.commit_sequence();
                    let (proof, object) = oneshot::channel();
                    active.publications.push_back(QueuedPublication {
                        pending: *pending,
                        durability: durability.clone(),
                        retained_reservation,
                        submitted_at: std::time::Instant::now(),
                        proof,
                    });
                    if durability.is_some() {
                        active.unpublished_node_logs += 1;
                        unpublished_node_log_bytes.fetch_add(retained_bytes, Ordering::AcqRel);
                    }
                    start_publication(cell, active, pool, tasks);
                    let pool = pool.clone();
                    let generation = active.generation;
                    let effect_id = active.begin_task(CoordinationEffect::Proof);
                    tasks.spawn(async move {
                        prove_command(
                            pool,
                            command,
                            outcome,
                            commit_sequence,
                            durability,
                            object,
                            generation,
                            effect_id,
                        )
                        .await
                    });
                }
                Err(error) => {
                    finish_work(active, false);
                    send_command_reply(&mut command, Err(error));
                }
            }
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Proven {
            cell,
            generation,
            effect_id,
            mut command,
            mut result,
            mut fenced,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                // The publication task may fence and remove the actor first; proof owns the
                // caller's final result and must not be rewritten as CellNotActive.
                send_command_reply(&mut command, result);
                return;
            };
            if active.generation != generation
                || !active
                    .coordination
                    .effect_matches(effect_id, CoordinationEffect::Proof)
            {
                send_command_reply(&mut command, result);
                return;
            }
            active.finish_task(effect_id, CoordinationEffect::Proof);
            if node_lease.check().is_err() {
                result = Err(command.operation.unknown(Error::Fenced));
                fenced = true;
            }
            finish_work(active, fenced);
            send_command_reply(&mut command, result);
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Published {
            cell,
            generation,
            effect_id,
            publisher,
            retained_bytes,
            node_logged,
            mut result,
            mut fenced,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                return;
            };
            if active.generation != generation
                || !active
                    .coordination
                    .effect_matches(effect_id, CoordinationEffect::Publication)
            {
                return;
            }
            active.finish_task(effect_id, CoordinationEffect::Publication);
            active.last_work_at = std::time::Instant::now();
            let object_published = result.is_ok();
            if node_lease.check().is_err() {
                result = Err(Error::Fenced);
                fenced = true;
            }
            active.publisher = Some(*publisher);
            active.publication_bytes = active.publication_bytes.saturating_sub(retained_bytes);
            if node_logged && object_published {
                active.unpublished_node_logs = active.unpublished_node_logs.saturating_sub(1);
                subtract_unpublished_bytes(unpublished_node_log_bytes, retained_bytes);
            }
            let decision = active
                .coordination
                .step(CoordinationInput::FinishPublication {
                    fenced,
                    succeeded: result.is_ok(),
                });
            if matches!(decision, CoordinationDecision::Fence) {
                fence_active(active);
            } else {
                start_publication(cell, active, pool, tasks);
            }
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Compacted {
            cell,
            generation,
            effect_id,
            publisher,
            result,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                return;
            };
            if active.generation != generation
                || !active
                    .coordination
                    .effect_matches(effect_id, CoordinationEffect::Compaction)
            {
                return;
            }
            active.finish_task(effect_id, CoordinationEffect::Compaction);
            let fenced = result.is_err() || node_lease.check().is_err();
            active.publisher = Some(*publisher);
            if matches!(result, Ok(None)) {
                active.compaction_retry_at = std::time::Instant::now() + COMPACTION_RETRY;
            }
            let decision = active
                .coordination
                .step(CoordinationInput::FinishCompaction { fenced });
            if matches!(decision, CoordinationDecision::Fence) {
                fence_active(active);
            }
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Queried {
            cell,
            generation,
            effect_id,
            mut query,
            mut result,
            mut fenced,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                // A query accepted before a fence keeps its result even when deactivation wins
                // the actor turn before this completion is delivered.
                send_query_reply(&mut query, result);
                return;
            };
            if active.generation != generation
                || !active
                    .coordination
                    .effect_matches(effect_id, CoordinationEffect::Work(AdmissionKind::Query))
            {
                send_query_reply(&mut query, result);
                return;
            }
            active.finish_task(effect_id, CoordinationEffect::Work(AdmissionKind::Query));
            if node_lease.check().is_err() {
                result = Err(Error::Fenced);
                fenced = true;
            }
            finish_work(active, fenced);
            send_query_reply(&mut query, result);
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Resolved {
            cell,
            generation,
            effect_id,
            mut resolve,
            mut result,
            mut fenced,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                // Resolution is an accepted observation, not a new admission; preserve its
                // unknown/committed result across a concurrent fenced deactivation.
                send_resolve_reply(&mut resolve, result);
                return;
            };
            if active.generation != generation
                || !active
                    .coordination
                    .effect_matches(effect_id, CoordinationEffect::Work(AdmissionKind::Resolve))
            {
                send_resolve_reply(&mut resolve, result);
                return;
            }
            active.finish_task(effect_id, CoordinationEffect::Work(AdmissionKind::Resolve));
            if node_lease.check().is_err() {
                result = Ok(Resolution::Unknown);
                fenced = true;
            }
            finish_work(active, fenced);
            send_resolve_reply(&mut resolve, result);
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Migrated {
            cell,
            generation,
            effect_id,
            publisher,
            mut migration,
            mut result,
            mut fenced,
            preserve_owner,
            unpublished_bytes,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                let result = match result {
                    Ok(_) => Err(Error::Fenced),
                    Err(error) => Err(error),
                };
                send_migration_reply(&mut migration, result);
                return;
            };
            if active.generation != generation
                || !active.coordination.effect_matches(
                    effect_id,
                    CoordinationEffect::Work(AdmissionKind::Migration),
                )
            {
                let result = match result {
                    Ok(_) => Err(Error::Fenced),
                    Err(error) => Err(error),
                };
                send_migration_reply(&mut migration, result);
                return;
            }
            active.finish_task(
                effect_id,
                CoordinationEffect::Work(AdmissionKind::Migration),
            );
            if node_lease.check().is_err() {
                result = Err(Error::Fenced);
                fenced = true;
            }
            active.publisher = Some(*publisher);
            if preserve_owner {
                active.unpublished_node_logs = active.unpublished_node_logs.saturating_add(1);
                unpublished_node_log_bytes.fetch_add(unpublished_bytes, Ordering::AcqRel);
            }
            let completion = finish_migration(active, fenced);
            match result {
                Ok(outcome) if !matches!(completion, CoordinationDecision::Fence) => {
                    active.code = outcome.code;
                    active.schema = outcome.schema;
                    let admission = Arc::clone(&migration.successor_admission);
                    send_migration_reply(
                        &mut migration,
                        Ok(MigratedAdmission { admission, outcome }),
                    );
                }
                Ok(_) => send_migration_reply(&mut migration, Err(Error::Fenced)),
                Err(error) => send_migration_reply(&mut migration, Err(error)),
            }
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Renewed {
            cell,
            generation,
            effect_id,
            publisher,
            mut result,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                return;
            };
            if active.generation != generation
                || !active
                    .coordination
                    .effect_matches(effect_id, CoordinationEffect::Renewal)
            {
                return;
            }
            active.finish_task(effect_id, CoordinationEffect::Renewal);
            if node_lease.check().is_err() {
                result = Err(Error::Fenced);
            }
            active.publisher = Some(*publisher);
            let decision = active.coordination.step(CoordinationInput::FinishRenewal {
                fenced: result.is_err(),
            });
            if matches!(decision, CoordinationDecision::Fence) {
                fence_active(active);
            }
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Deactivated {
            cell,
            generation,
            reply,
            shutdown_drain,
            result,
        } => {
            if cells
                .get(&cell)
                .is_some_and(|active| active.generation != generation)
            {
                return;
            }
            if let Some(mut permit) = movement_permits.remove(&cell) {
                movement.complete(&mut permit);
            }
            transitioning.remove(&cell);
            let runtime_waiting =
                shutdown_drain || shutdown.as_ref().is_some_and(|state| state.draining);
            match (reply, runtime_waiting) {
                (Some(reply), true) => {
                    if result.is_err() {
                        fail_shutdown(
                            shutdown,
                            Error::Control("one or more Cells failed to drain"),
                        );
                    }
                    let _ = reply.send(result);
                }
                (Some(reply), false) => {
                    let _ = reply.send(result);
                }
                (None, true) => {
                    if let Err(error) = result {
                        fail_shutdown(shutdown, error);
                    }
                }
                (None, false) => {}
            }
        }
    }
}
