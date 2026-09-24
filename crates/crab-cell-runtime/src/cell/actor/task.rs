//! The actor loop, message dispatch, request execution, and lifecycle
//! scheduling.
//!
//! Finished background tasks are handled in `tasks.rs`.

use super::admission::*;
use super::*;

pub(super) async fn run(
    mut receiver: mpsc::Receiver<Message>,
    pool: SqlWorkerPool,
    node_lease: Arc<RuntimeNodeLease>,
    unpublished_node_log_bytes: Arc<AtomicU64>,
    telemetry: crate::fleet::telemetry::CellTelemetryHandle,
) {
    let mut cells = HashMap::<CellId, ActiveCell>::new();
    let mut transitioning = HashSet::<CellId>::new();
    let mut tasks = JoinSet::<TaskResult>::new();
    let mut next_generation = 0_u64;
    let mut shutdown = None::<ShutdownState>;
    let mut pressure = match PressureClassifier::new(800, 600, 1_000) {
        Ok(classifier) => classifier,
        Err(_) => return,
    };
    let mut movement = match MovementBudget::new(2, 1_000) {
        Ok(budget) => budget,
        Err(_) => return,
    };
    let mut movement_permits = HashMap::<CellId, MovementPermit>::new();
    let mut renewal_tick = tokio::time::interval(RENEWAL_SCAN);
    renewal_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    renewal_tick.tick().await;
    let mut hydration_tick = tokio::time::interval(HYDRATION_TICK);
    hydration_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    hydration_tick.tick().await;
    let mut pressure_tick = tokio::time::interval(PRESSURE_SAMPLE);
    pressure_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    pressure_tick.tick().await;
    loop {
        if shutdown.as_ref().is_some_and(|state| state.draining) {
            if tasks.is_empty() {
                if !cells.is_empty() || !transitioning.is_empty() {
                    fail_shutdown(
                        &mut shutdown,
                        Error::Control("Cell shutdown left local state without a task"),
                    );
                }
                finish_shutdown(&mut shutdown);
                return;
            }
            let Some(Ok(result)) = tasks.join_next().await else {
                fail_shutdown(&mut shutdown, Error::RuntimeClosed);
                finish_shutdown(&mut shutdown);
                return;
            };
            super::tasks::handle_task(
                result,
                &pool,
                &mut cells,
                &mut transitioning,
                &mut tasks,
                &mut shutdown,
                &node_lease,
                &unpublished_node_log_bytes,
                &mut movement,
                &mut movement_permits,
            );
            continue;
        }
        if tasks.is_empty() {
            tokio::select! {
                message = receiver.recv() => {
                    let Some(message) = message else {
                        if shutdown.is_some() {
                            start_shutdown_drain(
                                &pool,
                                &mut cells,
                                &mut transitioning,
                                &mut tasks,
                                &mut shutdown,
                                &node_lease,
                            );
                            continue;
                        }
                        break;
                    };
                    handle_message(message, &mut receiver, &pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown, &node_lease, &telemetry, &mut pressure, &mut movement, &mut movement_permits, &mut next_generation);
                }
                _ = renewal_tick.tick() => {
                    start_due_renewals(&pool, &mut cells, &mut tasks, &node_lease);
                }
                _ = hydration_tick.tick() => {
                    start_background_hydration(
                        &pool,
                        &mut cells,
                        &mut tasks,
                        &node_lease,
                    );
                    start_background_inventory(&pool, &mut cells, &mut tasks, &node_lease);
                    start_background_compaction(&pool, &mut cells, &mut tasks, &node_lease);
                }
                _ = pressure_tick.tick() => {
                    sample_node_pressure(
                        &pool,
                        &telemetry,
                        &mut pressure,
                        &mut cells,
                        &mut transitioning,
                        &mut tasks,
                        &mut movement,
                        &mut movement_permits,
                    );
                }
            }
            continue;
        }
        tokio::select! {
            message = receiver.recv() => {
                let Some(message) = message else {
                    if shutdown.is_some() {
                        start_shutdown_drain(
                            &pool,
                            &mut cells,
                            &mut transitioning,
                            &mut tasks,
                            &mut shutdown,
                            &node_lease,
                        );
                        continue;
                    }
                    while let Some(result) = tasks.join_next().await {
                        let Ok(result) = result else { return; };
                        super::tasks::handle_task(result, &pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown, &node_lease, &unpublished_node_log_bytes, &mut movement, &mut movement_permits);
                    }
                    break;
                };
                    handle_message(message, &mut receiver, &pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown, &node_lease, &telemetry, &mut pressure, &mut movement, &mut movement_permits, &mut next_generation);
            }
            result = tasks.join_next() => {
                let Some(Ok(result)) = result else {
                    return;
                };
                super::tasks::handle_task(result, &pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown, &node_lease, &unpublished_node_log_bytes, &mut movement, &mut movement_permits);
            }
            _ = renewal_tick.tick() => {
                start_due_renewals(&pool, &mut cells, &mut tasks, &node_lease);
            }
            _ = hydration_tick.tick() => {
                start_background_hydration(
                    &pool,
                    &mut cells,
                    &mut tasks,
                    &node_lease,
                );
                start_background_inventory(&pool, &mut cells, &mut tasks, &node_lease);
                start_background_compaction(&pool, &mut cells, &mut tasks, &node_lease);
            }
            _ = pressure_tick.tick() => {
                sample_node_pressure(
                    &pool,
                    &telemetry,
                    &mut pressure,
                    &mut cells,
                    &mut transitioning,
                    &mut tasks,
                    &mut movement,
                    &mut movement_permits,
                );
            }
        }
    }
}

/// Feeds this node's own reservation ledger into the pressure classifier.
///
/// A ledger that cannot be read, or a node without configured limits, yields no
/// sample: pressure policy must never stop the actor loop.
fn sample_node_pressure(
    pool: &SqlWorkerPool,
    telemetry: &crate::fleet::telemetry::CellTelemetryHandle,
    pressure: &mut PressureClassifier,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    movement: &mut MovementBudget,
    movement_permits: &mut HashMap<CellId, MovementPermit>,
) {
    let Ok(snapshot) = pool.resource_ledger().snapshot() else {
        return;
    };
    let Ok(sample) = super::runtime::pressure_sample(snapshot, unix_millis()) else {
        return;
    };
    let _ = classify_pressure_sample(
        sample,
        telemetry,
        pressure,
        pool,
        cells,
        transitioning,
        tasks,
        movement,
        movement_permits,
    );
}

/// Classifies one sample and starts bounded shedding when it demands it.
///
/// The external observation message and the actor's own ledger sample share
/// this path, so both keep one hysteresis and one movement budget.
#[expect(
    clippy::too_many_arguments,
    reason = "the shedding path owns actor state, tasks, the movement budget, and its sink separately"
)]
fn classify_pressure_sample(
    sample: PressureSample,
    telemetry: &crate::fleet::telemetry::CellTelemetryHandle,
    pressure: &mut PressureClassifier,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    movement: &mut MovementBudget,
    movement_permits: &mut HashMap<CellId, MovementPermit>,
) -> crate::Result<PressureState> {
    let state = pressure.observe(sample)?;
    telemetry.pressure_state(state);
    if matches!(state, PressureState::Shedding | PressureState::Critical)
        && movement_permits.len() < 2
    {
        let _ = start_bounded_evictions(
            1,
            sample.at_ms,
            pool,
            cells,
            transitioning,
            tasks,
            movement,
            movement_permits,
        );
    }
    Ok(state)
}

pub(super) fn start_shutdown_drain(
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    shutdown: &mut Option<ShutdownState>,
    node_lease: &RuntimeNodeLease,
) {
    let Some(state) = shutdown.as_mut() else {
        return;
    };
    if state.draining {
        return;
    }
    state.draining = true;
    let mut ready = Vec::new();
    for (cell, active) in cells.iter_mut() {
        if let Some(transfer) = active.transfer.take() {
            active.coordination.step(CoordinationInput::AbortTransfer);
            let _ = transfer.reply.send(Err(Error::CellDraining));
        }
        active.inventory_refreshing = false;
        active.coordination.step(CoordinationInput::BeginShutdown);
        active.admission.draining.store(true, Ordering::Release);
        active.admission.requests.close();
        active.admission.bytes.close();
        if !active.queue.is_empty() {
            start_next(active, pool, tasks, node_lease);
        }
        match schedule(active, node_lease.check().is_ok()) {
            CoordinationDecision::ReadyToDeactivate => {
                ready.push((*cell, false, active.unpublished_node_logs != 0));
            }
            CoordinationDecision::ReadyToDeactivateFenced => {
                ready.push((*cell, true, active.unpublished_node_logs != 0));
            }
            CoordinationDecision::Fence => fence_active(active),
            _ => {}
        }
    }
    for (cell, fenced, preserve_owner) in ready {
        if fenced {
            start_fenced_deactivate(cell, pool, cells, transitioning, tasks, preserve_owner);
        } else {
            start_deactivate(cell, pool, cells, transitioning, tasks);
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the actor adapter passes each independently owned protocol facility explicitly"
)]
pub(super) fn handle_message(
    message: Message,
    receiver: &mut mpsc::Receiver<Message>,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    shutdown: &mut Option<ShutdownState>,
    node_lease: &RuntimeNodeLease,
    telemetry: &crate::fleet::telemetry::CellTelemetryHandle,
    pressure: &mut PressureClassifier,
    movement: &mut MovementBudget,
    movement_permits: &mut HashMap<CellId, MovementPermit>,
    next_generation: &mut u64,
) {
    if !matches!(message, Message::Shutdown { .. }) && node_lease.check().is_err() {
        for active in cells.values_mut() {
            active.coordination.step(CoordinationInput::Fence);
            fence_active(active);
        }
        reject_fenced_message(message);
        return;
    }
    match message {
        Message::Activate {
            cell,
            role,
            activation,
            publisher,
            reply,
        } => {
            if cells.contains_key(&cell) || !transitioning.insert(cell) {
                let _ = reply.send(Err(Error::CellAlreadyActive));
                return;
            }
            *next_generation = next_generation.wrapping_add(1).max(1);
            let generation = *next_generation;
            let admission = new_cell_admission();
            let pool = pool.clone();
            tasks.spawn(async move {
                let mut publisher = publisher;
                let result = match activation {
                    Activation::Restored(activation) => {
                        activate_restored_and_publish(cell, &pool, &mut publisher, *activation)
                            .await
                    }
                    Activation::Bootstrap(activation) => {
                        bootstrap_and_publish(cell, &pool, &mut publisher, *activation).await
                    }
                };
                let (result, persisted_work) = match result {
                    Ok(hydration) => {
                        match pool.interrupt_handle(cell).await {
                            Ok(interrupt) => {
                                let persisted_work =
                                    pool.persisted_work_inventory(cell, role).await;
                                (Ok((Arc::new(interrupt), hydration)), persisted_work)
                            }
                            Err(error) => {
                                let result =
                                    match cleanup_failed_activation(cell, &pool, &mut publisher)
                                        .await
                                    {
                                        Ok(()) => error,
                                        Err(cleanup) => cleanup,
                                    };
                                (Err(result), Err(Error::CellNotActive))
                            }
                        }
                    }
                    Err(error) => {
                        let result =
                            match cleanup_failed_activation(cell, &pool, &mut publisher).await {
                                Ok(()) => error,
                                Err(cleanup) => cleanup,
                            };
                        (Err(result), Err(Error::CellNotActive))
                    }
                };
                TaskResult::Activated {
                    cell,
                    generation,
                    role,
                    publisher,
                    admission,
                    reply,
                    result,
                    persisted_work,
                }
            });
        }
        Message::Execute(mut command) => {
            let Some(active) = cells.get_mut(&command.cell) else {
                send_command_reply(&mut command, Err(Error::CellNotActive));
                return;
            };
            if active.transfer.is_some() {
                send_command_reply(&mut command, Err(Error::CellDraining));
                return;
            }
            match active.coordination.step(CoordinationInput::Admit {
                kind: AdmissionKind::Command,
                admission_matches: Arc::ptr_eq(&active.admission, &command.admission),
            }) {
                CoordinationDecision::Admit => {
                    active.queue.push_back(QueuedWork::Command(command));
                    start_next(active, pool, tasks, node_lease);
                }
                CoordinationDecision::Reject(reason) => {
                    send_command_reply(&mut command, Err(rejection_error(reason)));
                }
                _ => send_command_reply(&mut command, Err(Error::CellNotActive)),
            }
        }
        Message::Query(mut query) => {
            let Some(active) = cells.get_mut(&query.cell) else {
                send_query_reply(&mut query, Err(Error::CellNotActive));
                return;
            };
            if active.transfer.is_some() {
                send_query_reply(&mut query, Err(Error::CellDraining));
                return;
            }
            match active.coordination.step(CoordinationInput::Admit {
                kind: AdmissionKind::Query,
                admission_matches: Arc::ptr_eq(&active.admission, &query.admission),
            }) {
                CoordinationDecision::Admit => {
                    active.queue.push_back(QueuedWork::Query(query));
                    start_next(active, pool, tasks, node_lease);
                }
                CoordinationDecision::Reject(reason) => {
                    send_query_reply(&mut query, Err(rejection_error(reason)));
                }
                _ => send_query_reply(&mut query, Err(Error::CellNotActive)),
            }
        }
        Message::Resolve(mut resolve) => {
            let Some(active) = cells.get_mut(&resolve.cell) else {
                send_resolve_reply(&mut resolve, Err(Error::CellNotActive));
                return;
            };
            if active.transfer.is_some() {
                send_resolve_reply(&mut resolve, Ok(Resolution::Unknown));
                return;
            }
            match active.coordination.step(CoordinationInput::Admit {
                kind: AdmissionKind::Resolve,
                admission_matches: Arc::ptr_eq(&active.admission, &resolve.admission),
            }) {
                CoordinationDecision::Admit => {
                    active.queue.push_back(QueuedWork::Resolve(resolve));
                    start_next(active, pool, tasks, node_lease);
                }
                CoordinationDecision::ResolveUnknown => {
                    send_resolve_reply(&mut resolve, Ok(Resolution::Unknown));
                }
                CoordinationDecision::Reject(reason) => {
                    send_resolve_reply(&mut resolve, Err(rejection_error(reason)));
                }
                _ => send_resolve_reply(&mut resolve, Err(Error::CellNotActive)),
            }
        }
        Message::Migrate(mut migration) => {
            let Some(active) = cells.get_mut(&migration.cell) else {
                send_migration_reply(&mut migration, Err(Error::CellNotActive));
                return;
            };
            let decision = active.coordination.step(CoordinationInput::Admit {
                kind: AdmissionKind::Migration,
                admission_matches: Arc::ptr_eq(&active.admission, &migration.admission),
            });
            if let CoordinationDecision::Reject(reason) = decision {
                send_migration_reply(&mut migration, Err(rejection_error(reason)));
                return;
            }
            if !matches!(decision, CoordinationDecision::Admit) {
                send_migration_reply(&mut migration, Err(Error::CellNotActive));
                return;
            }
            if active.code != migration.plan.from_code()
                || active.schema != migration.plan.from_schema()
            {
                send_migration_reply(
                    &mut migration,
                    Err(Error::Registry("migration plan does not match active Cell")),
                );
                return;
            }
            match active.coordination.step(CoordinationInput::BeginMigration) {
                CoordinationDecision::Started => {}
                CoordinationDecision::Reject(reason) => {
                    send_migration_reply(&mut migration, Err(rejection_error(reason)));
                    return;
                }
                _ => {
                    send_migration_reply(&mut migration, Err(Error::CellDraining));
                    return;
                }
            }
            active.admission = Arc::clone(&migration.successor_admission);
            active.queue.push_back(QueuedWork::Migration(migration));
            start_next(active, pool, tasks, node_lease);
        }
        Message::Lookup {
            cell,
            require_resident,
            reply,
        } => {
            let local = cells.get(&cell).and_then(|active| {
                matches!(
                    active.coordination.lookup(),
                    CoordinationDecision::LocalHandle
                )
                .then_some(active)
                .filter(|active| {
                    active.transfer.is_none()
                        && (!require_resident
                            || active.coordination.residency() == Residency::Resident)
                })
                .map(|active| LocalCell {
                    admission: active.admission.clone(),
                    incarnation: active.incarnation,
                    code: active.code,
                    schema: active.schema,
                })
            });
            let _ = reply.send(local);
        }
        Message::Drain {
            cell,
            admission,
            reply,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                let _ = reply.send(Err(Error::CellNotActive));
                return;
            };
            if !Arc::ptr_eq(&active.admission, &admission) {
                let _ = reply.send(Err(if active.transfer.is_some() {
                    Error::CellDraining
                } else {
                    Error::CellNotActive
                }));
                return;
            }
            if active.transfer.is_some() {
                if let Some(transfer) = active.transfer.take() {
                    active.coordination.step(CoordinationInput::AbortTransfer);
                    let _ = transfer.reply.send(Err(Error::CellDraining));
                }
                let decision = active.coordination.step(CoordinationInput::BeginDrain);
                if let CoordinationDecision::Reject(reason) = decision {
                    let _ = reply.send(Err(rejection_error(reason)));
                    return;
                }
                active.drain = Some(reply);
                if matches!(
                    schedule(active, node_lease.check().is_ok()),
                    CoordinationDecision::ReadyToDeactivate
                ) {
                    start_deactivate(cell, pool, cells, transitioning, tasks);
                }
                return;
            }
            let decision = active.coordination.step(CoordinationInput::BeginDrain);
            if let CoordinationDecision::Reject(reason) = decision {
                let _ = reply.send(Err(rejection_error(reason)));
                return;
            }
            active.drain = Some(reply);
            match schedule(active, node_lease.check().is_ok()) {
                CoordinationDecision::ReadyToDeactivate => {
                    start_deactivate(cell, pool, cells, transitioning, tasks);
                }
                CoordinationDecision::Fence => fence_active(active),
                _ => {}
            }
        }
        Message::EvictIdle { limit, reply } => {
            let count = start_bounded_evictions(
                limit,
                unix_millis(),
                pool,
                cells,
                transitioning,
                tasks,
                movement,
                movement_permits,
            );
            let _ = reply.send(Ok(count));
        }
        Message::IdleTransferCandidates { reply } => {
            if node_lease.check().is_err() {
                let _ = reply.send(Err(Error::Fenced));
                return;
            }
            let candidates = cells
                .iter()
                .filter_map(|(cell, active)| {
                    transfer_candidate_observation(*cell, active)
                        .eligible()
                        .then_some(active)
                        .filter(|active| !active.draining())
                        .map(|active| (*cell, active.generation, active.last_used_ms, active.role))
                })
                .collect();
            let _ = reply.send(Ok(candidates));
        }
        Message::UnreleasedCellCount { reply } => {
            let _ = reply.send(Ok(cells.len().saturating_add(transitioning.len())));
        }
        Message::ReleaseIdleCell {
            cell,
            generation,
            reply,
        } => {
            if node_lease.check().is_err() {
                let _ = reply.send(Err(Error::Fenced));
                return;
            }
            let Some(active) = cells.get(&cell) else {
                let _ = reply.send(Err(Error::CellNotActive));
                return;
            };
            if active.generation != generation
                || active.draining()
                || active.transfer.is_some()
                || active.inventory_refreshing
            {
                let _ = reply.send(Err(Error::CellDraining));
                return;
            }
            let Ok(mut permit) = movement.try_start(unix_millis()) else {
                let _ = reply.send(Err(Error::Capacity("movement budget")));
                return;
            };
            {
                let Some(active) = cells.get_mut(&cell) else {
                    movement.complete(&mut permit);
                    let _ = reply.send(Err(Error::CellNotActive));
                    return;
                };
                if active.transfer.is_some()
                    || active.drain.is_some()
                    || active.inventory_refreshing
                {
                    movement.complete(&mut permit);
                    let _ = reply.send(Err(Error::CellDraining));
                    return;
                }
                let decision =
                    active
                        .coordination
                        .step(CoordinationInput::BeginTransferPreflight {
                            queue_empty: active.queue.is_empty(),
                            publication_idle: active.coordination.publication_count() == 0,
                            lease_live: node_lease.check().is_ok(),
                        });
                match decision {
                    CoordinationDecision::Started => {}
                    CoordinationDecision::Fence => {
                        fence_active(active);
                        movement.complete(&mut permit);
                        let _ = reply.send(Err(Error::Fenced));
                        return;
                    }
                    CoordinationDecision::Reject(reason) => {
                        movement.complete(&mut permit);
                        let _ = reply.send(Err(rejection_error(reason)));
                        return;
                    }
                    CoordinationDecision::Ignored => {
                        movement.complete(&mut permit);
                        let _ = reply.send(Err(Error::CellDraining));
                        return;
                    }
                    _ => {
                        movement.complete(&mut permit);
                        let _ = reply.send(Err(Error::CellDraining));
                        return;
                    }
                }
                active.admission.draining.store(true, Ordering::Release);
                active.admission.requests.close();
                active.admission.bytes.close();
                active.admission = new_cell_admission();
                active.transfer = Some(TransferPreflight { reply });
            };
            movement_permits.insert(cell, permit);
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        Message::ObservePressure { sample, reply } => {
            let result = classify_pressure_sample(
                sample,
                telemetry,
                pressure,
                pool,
                cells,
                transitioning,
                tasks,
                movement,
                movement_permits,
            );
            let _ = reply.send(result);
        }
        Message::Shutdown { reply } => {
            if shutdown.is_some() {
                let _ = reply.send(Err(Error::RuntimeClosed));
                return;
            }
            receiver.close();
            *shutdown = Some(ShutdownState {
                reply,
                draining: false,
                error: None,
            });
        }
    }
}

pub(super) fn reject_fenced_message(message: Message) {
    match message {
        Message::Activate { reply, .. } => {
            let _ = reply.send(Err(Error::Fenced));
        }
        Message::Execute(mut command) => {
            send_command_reply(&mut command, Err(Error::Fenced));
        }
        Message::Query(mut query) => {
            send_query_reply(&mut query, Err(Error::Fenced));
        }
        Message::Resolve(mut resolve) => {
            send_resolve_reply(&mut resolve, Ok(Resolution::Unknown));
        }
        Message::Migrate(mut migration) => {
            send_migration_reply(&mut migration, Err(Error::Fenced));
        }
        Message::Lookup { reply, .. } => {
            let _ = reply.send(None);
        }
        Message::Drain { reply, .. } => {
            let _ = reply.send(Err(Error::Fenced));
        }
        Message::EvictIdle { reply, .. } => {
            let _ = reply.send(Err(Error::Fenced));
        }
        Message::IdleTransferCandidates { reply } => {
            let _ = reply.send(Err(Error::Fenced));
        }
        Message::UnreleasedCellCount { reply } => {
            let _ = reply.send(Err(Error::Fenced));
        }
        Message::ReleaseIdleCell { reply, .. } => {
            let _ = reply.send(Err(Error::Fenced));
        }
        Message::ObservePressure { reply, .. } => {
            let _ = reply.send(Err(Error::Fenced));
        }
        Message::Shutdown { reply } => {
            let _ = reply.send(Err(Error::RuntimeClosed));
        }
    }
}
