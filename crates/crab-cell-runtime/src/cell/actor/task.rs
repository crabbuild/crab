//! The actor loop, task handlers, activation, and eviction.

use super::admission::*;
use super::*;

pub(super) async fn run(
    mut receiver: mpsc::Receiver<Message>,
    pool: SqlWorkerPool,
    node_lease: Arc<RuntimeNodeLease>,
    unpublished_node_log_bytes: Arc<AtomicU64>,
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
            handle_task(
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
                    handle_message(message, &mut receiver, &pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown, &node_lease, &mut pressure, &mut movement, &mut movement_permits, &mut next_generation);
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
                        handle_task(result, &pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown, &node_lease, &unpublished_node_log_bytes, &mut movement, &mut movement_permits);
                    }
                    break;
                };
                    handle_message(message, &mut receiver, &pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown, &node_lease, &mut pressure, &mut movement, &mut movement_permits, &mut next_generation);
            }
            result = tasks.join_next() => {
                let Some(Ok(result)) = result else {
                    return;
                };
                handle_task(result, &pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown, &node_lease, &unpublished_node_log_bytes, &mut movement, &mut movement_permits);
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
        }
    }
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
            let result = pressure.observe(sample);
            if let Ok(state) = result
                && matches!(state, PressureState::Shedding | PressureState::Critical)
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

pub(super) async fn execute_migration(
    pool: SqlWorkerPool,
    mut publisher: Box<CellPublisher>,
    mut migration: Box<QueuedMigration>,
    interrupt: Arc<crab_ltx::rusqlite::InterruptHandle>,
    generation: u64,
    effect_id: u64,
) -> TaskResult {
    let mut preserve_owner = false;
    let mut unpublished_bytes = 0;
    let deadline = std::time::Instant::now() + SQL_WALL_DEADLINE;
    let operation = pool.migrate(migration.cell, migration.plan, migration.now_ms, deadline);
    tokio::pin!(operation);
    let pending = match tokio::time::timeout_at(deadline.into(), &mut operation).await {
        Ok(result) => result,
        Err(_) => {
            interrupt.interrupt();
            fence_admission(&migration.admission);
            send_migration_reply(&mut migration, Err(Error::Deadline));
            let _ = operation.await;
            let _ = pool.fence(migration.cell).await;
            return TaskResult::Migrated {
                cell: migration.cell,
                generation,
                effect_id,
                publisher,
                migration,
                result: Err(Error::Deadline),
                fenced: true,
                preserve_owner: false,
                unpublished_bytes: 0,
            };
        }
    };
    let result = match pending {
        Ok(pending) => {
            let durability_started = std::time::Instant::now();
            let durability = publisher.submit_migration_durability(&pending).await;
            match durability {
                Err(error) => Err(error),
                Ok(durability) => {
                    let node_logged = durability.is_some();
                    let retained_bytes = pending.retained_bytes();
                    let cell = migration.cell;
                    let early_outcome = MigrationOutcome {
                        code: pending.code(),
                        schema: pending.to_schema(),
                        commit_sequence: pending.commit_sequence(),
                    };
                    let object = async {
                        let prepared = publisher.prepare_migration(&pending).await?;
                        pool.bind_migration_prepared(cell, prepared.clone()).await?;
                        let root = publisher
                            .publish_migration(
                                &prepared,
                                pending.next_due_ms(),
                                pending.code(),
                                pending.to_schema(),
                            )
                            .await?;
                        if let Some(durability) = durability.as_ref() {
                            durability.prove_object().await?;
                        } else {
                            publisher.record_object_proof(durability_started.elapsed());
                        }
                        pool.confirm_migration_published(cell, root).await
                    };
                    tokio::pin!(object);
                    let result = match durability.as_ref() {
                        Some(durability) => {
                            let fleet = durability.prove_fleet();
                            tokio::pin!(fleet);
                            tokio::select! {
                                result = &mut object => result,
                                fleet = &mut fleet => {
                                    if fleet.is_ok() {
                                        let admission = Arc::clone(&migration.successor_admission);
                                        send_migration_reply(
                                            &mut migration,
                                            Ok(MigratedAdmission {
                                                admission,
                                                outcome: early_outcome,
                                            }),
                                        );
                                    }
                                    object.await
                                }
                            }
                        }
                        None => object.await,
                    };
                    preserve_owner = node_logged && result.is_err();
                    if preserve_owner {
                        unpublished_bytes = retained_bytes;
                    }
                    result
                }
            }
        }
        Err(error) => Err(error),
    };
    let fenced = result.is_err();
    if fenced {
        let _ = pool.fence(migration.cell).await;
    }
    TaskResult::Migrated {
        cell: migration.cell,
        generation,
        effect_id,
        publisher,
        migration,
        result,
        fenced,
        preserve_owner,
        unpublished_bytes,
    }
}

pub(super) async fn execute_command(
    pool: SqlWorkerPool,
    durability: CellDurabilitySubmitter,
    mut command: Box<QueuedCommand>,
    interrupt: Arc<crab_ltx::rusqlite::InterruptHandle>,
    generation: u64,
    effect_id: u64,
) -> TaskResult {
    let deadline = std::time::Instant::now() + SQL_WALL_DEADLINE;
    let execution = match command.handler.take() {
        Some(handler) => {
            let cell = command.cell;
            let queued_operation = command.operation;
            let now_ms = command.now_ms;
            let max_result_bytes = command.max_result_bytes;
            let worker_pool = pool.clone();
            let operation = async move {
                match queued_operation {
                    QueuedOperation::Mutation {
                        identity,
                        operation_digest,
                    } => {
                        worker_pool
                            .execute_until(
                                cell,
                                identity,
                                operation_digest,
                                now_ms,
                                max_result_bytes,
                                deadline,
                                handler,
                            )
                            .await
                    }
                    QueuedOperation::Effect { delivery } => {
                        worker_pool
                            .deliver_effect(
                                cell,
                                delivery,
                                now_ms,
                                max_result_bytes,
                                deadline,
                                handler,
                            )
                            .await
                    }
                }
            };
            tokio::pin!(operation);
            match tokio::time::timeout_at(deadline.into(), &mut operation).await {
                Ok(result) => result,
                Err(_) => {
                    interrupt.interrupt();
                    fence_admission(&command.admission);
                    let error = command.operation.unknown(Error::Deadline);
                    send_command_reply(&mut command, Err(error));
                    let _ = operation.await;
                    let _ = pool.fence(command.cell).await;
                    return TaskResult::Executed {
                        cell: command.cell,
                        generation,
                        effect_id,
                        command,
                        result: Err(Error::Deadline),
                        fenced: true,
                    };
                }
            }
        }
        None => Err(Error::Fenced),
    };
    let (result, must_fence) = match execution {
        Ok(WorkerExecution::Recorded(outcome)) => (Ok(CommandTaskResult::Recorded(outcome)), false),
        Ok(WorkerExecution::Pending(pending)) => {
            let retained_bytes = usize::try_from(pending.retained_bytes())
                .map_err(|_| Error::Capacity("pending publication bytes"));
            let result = retained_bytes.and_then(|retained_bytes| {
                pool.resource_ledger()
                    .try_reserve(ResourceCost::zero().with_retained_bytes(retained_bytes))
                    .map_err(|error| match error {
                        Error::Capacity(_) => Error::Capacity("pending publication bytes"),
                        error => error,
                    })
            });
            let result = match result {
                Ok(retained_reservation) => durability
                    .submit(pending.outcome().commit_sequence(), pending.cuts())
                    .await
                    .map(|durability| CommandTaskResult::Pending {
                        pending,
                        durability,
                        retained_reservation,
                    }),
                Err(error) => Err(error),
            };
            (result, true)
        }
        Err(error) => (
            Err(error),
            !matches!(pool.state(command.cell).await, Ok(WorkerState::Ready)),
        ),
    };
    let fenced = must_fence && result.is_err();
    if fenced {
        let _ = pool.fence(command.cell).await;
    }
    let result = if fenced {
        result.map_err(|source| command.operation.unknown(source))
    } else {
        result
    };
    TaskResult::Executed {
        cell: command.cell,
        generation,
        effect_id,
        command,
        result,
        fenced,
    }
}

pub(super) async fn prove_command(
    pool: SqlWorkerPool,
    command: Box<QueuedCommand>,
    outcome: StoredOutcome,
    commit_sequence: u64,
    durability: Option<PendingDurability>,
    mut object: oneshot::Receiver<crate::Result<()>>,
    generation: u64,
    effect_id: u64,
) -> TaskResult {
    let proof = match durability {
        Some(durability) => {
            let fleet_or_object = durability.prove();
            tokio::pin!(fleet_or_object);
            tokio::select! {
                object = &mut object => match receive_publication_proof(object) {
                    Ok(()) => Ok(()),
                    // Object publication failure does not invalidate an
                    // independently fsynced follower proof for this cut.
                    Err(_) => fleet_or_object.await.map(|_| ()),
                },
                result = &mut fleet_or_object => match result {
                    Ok(()) => Ok(()),
                    // Losing the follower path does not invalidate the same
                    // cut's object publication, which remains the fallback.
                    Err(_) => receive_publication_proof(object.await),
                }
            }
        }
        None => receive_publication_proof(object.await),
    };
    let result = match proof {
        Ok(()) => pool
            .confirm_durable(command.cell, commit_sequence)
            .await
            .map(|()| outcome),
        Err(error) => Err(error),
    };
    let fenced = result.is_err();
    if fenced {
        let _ = pool.fence(command.cell).await;
    }
    let result = if fenced {
        result.map_err(|source| command.operation.unknown(source))
    } else {
        result
    };
    TaskResult::Proven {
        cell: command.cell,
        generation,
        effect_id,
        command,
        result,
        fenced,
    }
}

pub(super) fn receive_publication_proof(
    result: std::result::Result<crate::Result<()>, oneshot::error::RecvError>,
) -> crate::Result<()> {
    result.map_err(|_| Error::RuntimeClosed)?
}

pub(super) fn start_publication(
    cell: CellId,
    active: &mut ActiveCell,
    pool: &SqlWorkerPool,
    tasks: &mut JoinSet<TaskResult>,
) {
    let Some(mut publisher) = active.publisher.take() else {
        return;
    };
    let Some(publication) = active.publications.pop_front() else {
        active.publisher = Some(publisher);
        return;
    };
    let node_logged = publication.durability.is_some();
    tracing::debug!(
        queue_wait_ms = publication.submitted_at.elapsed().as_millis(),
        pending_publications = active.coordination.publication_count(),
        publication_bytes = active.publication_bytes,
        "Cell LTX publication started"
    );
    let generation = active.generation;
    let effect_id = active.begin_task(CoordinationEffect::Publication);
    // Moving the publisher out of ActiveCell is the serialization token for
    // root preparation and CAS; no second object publisher can overtake it.
    let pool = pool.clone();
    let retained_reservation = publication.retained_reservation;
    tasks.spawn(async move {
        let _retained_reservation = retained_reservation;
        let retained_bytes = publication.pending.retained_bytes();
        let mut publication_proof = Some(publication.proof);
        let fleet_deadline = std::time::Instant::now() + FLEET_PUBLICATION_GRACE;
        let mut retry_delay = std::time::Duration::from_millis(100);
        let result = async {
            let expected = publication.pending.outcome().clone();
            let prepared = loop {
                match publisher.prepare(&publication.pending).await {
                    Ok(prepared) => break prepared,
                    Err(error) if node_logged && is_storage_publication_error(&error) => {
                        if let Some(durability) = publication.durability.as_ref() {
                            wait_for_fleet_proof(durability, fleet_deadline).await?;
                        }
                        if let Some(proof) = publication_proof.take() {
                            let _ = proof.send(Err(error));
                        }
                        if std::time::Instant::now() >= fleet_deadline {
                            return Err(Error::Fenced);
                        }
                        tokio::time::sleep(retry_delay).await;
                        retry_delay = retry_delay
                            .saturating_mul(2)
                            .min(std::time::Duration::from_secs(2));
                    }
                    Err(error) => return Err(error),
                }
            };
            pool.bind_prepared(cell, prepared.clone()).await?;
            let root = loop {
                match publisher
                    .publish_prepared(&prepared, publication.pending.next_due_ms())
                    .await
                {
                    Ok(root) => break root,
                    Err(error) if node_logged && is_storage_publication_error(&error) => {
                        if let Some(durability) = publication.durability.as_ref() {
                            wait_for_fleet_proof(durability, fleet_deadline).await?;
                        }
                        if let Some(proof) = publication_proof.take() {
                            let _ = proof.send(Err(error));
                        }
                        if std::time::Instant::now() >= fleet_deadline {
                            return Err(Error::Fenced);
                        }
                        tokio::time::sleep(retry_delay).await;
                        retry_delay = retry_delay
                            .saturating_mul(2)
                            .min(std::time::Duration::from_secs(2));
                    }
                    Err(error) => return Err(error),
                }
            };
            if let Some(durability) = publication.durability.as_ref() {
                durability.prove_object().await?;
            } else {
                publisher.record_object_proof(publication.submitted_at.elapsed());
            }
            let published = pool.confirm_published(cell, root).await?;
            if published != expected {
                return Err(Error::Control(
                    "published result does not match queued commit",
                ));
            }
            Ok(())
        }
        .await;
        tracing::debug!(
            publication_lag_ms = publication.submitted_at.elapsed().as_millis(),
            succeeded = result.is_ok(),
            "Cell LTX publication completed"
        );
        let fenced = result.is_err();
        if let Some(proof) = publication_proof {
            let _ = proof.send(if fenced { Err(Error::Fenced) } else { Ok(()) });
        }
        if fenced {
            let _ = pool.fence(cell).await;
        }
        TaskResult::Published {
            cell,
            generation,
            effect_id,
            publisher: Box::new(publisher),
            retained_bytes,
            node_logged,
            result,
            fenced,
        }
    });
}

pub(super) fn is_storage_publication_error(error: &Error) -> bool {
    matches!(
        error,
        Error::Storage(_) | Error::Ltx(crab_ltx::CrabError::Storage(_))
    )
}

pub(super) async fn wait_for_fleet_proof(
    durability: &PendingDurability,
    deadline: std::time::Instant,
) -> crate::Result<()> {
    tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        durability.prove_fleet(),
    )
    .await
    .map_err(|_| Error::Fenced)?
}

pub(super) async fn execute_query(
    pool: SqlWorkerPool,
    mut query: Box<QueuedQuery>,
    interrupt: Arc<crab_ltx::rusqlite::InterruptHandle>,
    generation: u64,
    effect_id: u64,
) -> TaskResult {
    let deadline = std::time::Instant::now() + SQL_WALL_DEADLINE;
    let result = match query.handler.take() {
        Some(handler) => {
            let operation = pool.query(query.cell, query.max_result_bytes, deadline, handler);
            tokio::pin!(operation);
            match tokio::time::timeout_at(deadline.into(), &mut operation).await {
                Ok(result) => result,
                Err(_) => {
                    interrupt.interrupt();
                    fence_admission(&query.admission);
                    send_query_reply(&mut query, Err(Error::Deadline));
                    let _ = operation.await;
                    let _ = pool.fence(query.cell).await;
                    return TaskResult::Queried {
                        cell: query.cell,
                        generation,
                        effect_id,
                        query,
                        result: Err(Error::Deadline),
                        fenced: true,
                    };
                }
            }
        }
        None => Err(Error::Fenced),
    };
    let fenced = result.is_err() && !matches!(pool.state(query.cell).await, Ok(WorkerState::Ready));
    if fenced {
        let _ = pool.fence(query.cell).await;
    }
    TaskResult::Queried {
        cell: query.cell,
        generation,
        effect_id,
        query,
        result,
        fenced,
    }
}

pub(super) async fn execute_resolve(
    pool: SqlWorkerPool,
    mut resolve: Box<QueuedResolve>,
    interrupt: Arc<crab_ltx::rusqlite::InterruptHandle>,
    generation: u64,
    effect_id: u64,
) -> TaskResult {
    let deadline = std::time::Instant::now() + SQL_WALL_DEADLINE;
    let cell = resolve.cell;
    let resolve_operation = resolve.operation;
    let now_ms = resolve.now_ms;
    let max_result_bytes = resolve.max_result_bytes;
    let worker_pool = pool.clone();
    let operation = async move {
        match resolve_operation {
            ResolveOperation::Mutation {
                identity,
                operation_digest,
            } => {
                worker_pool
                    .resolve(
                        cell,
                        identity,
                        operation_digest,
                        now_ms,
                        max_result_bytes,
                        deadline,
                    )
                    .await
            }
            ResolveOperation::Effect { delivery } => {
                worker_pool
                    .resolve_effect(cell, delivery, now_ms, max_result_bytes, deadline)
                    .await
            }
        }
    };
    tokio::pin!(operation);
    let result = match tokio::time::timeout_at(deadline.into(), &mut operation).await {
        Ok(result) => result,
        Err(_) => {
            interrupt.interrupt();
            fence_admission(&resolve.admission);
            send_resolve_reply(&mut resolve, Ok(Resolution::Unknown));
            let _ = operation.await;
            let _ = pool.fence(resolve.cell).await;
            return TaskResult::Resolved {
                cell: resolve.cell,
                generation,
                effect_id,
                resolve,
                result: Ok(Resolution::Unknown),
                fenced: true,
            };
        }
    };
    let deadline = matches!(result, Err(Error::Deadline));
    let result = if deadline {
        Ok(Resolution::Unknown)
    } else {
        result
    };
    let fenced = deadline
        || result.is_err() && !matches!(pool.state(resolve.cell).await, Ok(WorkerState::Ready));
    if fenced {
        let _ = pool.fence(resolve.cell).await;
    }
    TaskResult::Resolved {
        cell: resolve.cell,
        generation,
        effect_id,
        resolve,
        result,
        fenced,
    }
}

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

pub(super) fn continue_cell(
    cell: CellId,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    node_lease: &RuntimeNodeLease,
) {
    if cells
        .get(&cell)
        .is_some_and(|active| active.transfer.is_some())
    {
        let ready = cells.get(&cell).is_some_and(|active| {
            !active.inventory_refreshing
                && active.queue.is_empty()
                && active.coordination.can_deactivate()
        });
        if ready {
            start_transfer_inspection(cell, pool, cells, tasks, node_lease);
            return;
        }
        if let Some(active) = cells.get_mut(&cell)
            && !active.inventory_refreshing
            && !active.queue.is_empty()
        {
            start_next(active, pool, tasks, node_lease);
        }
        return;
    }
    let Some(active) = cells.get_mut(&cell) else {
        return;
    };
    let decision = schedule(active, node_lease.check().is_ok());
    match decision {
        CoordinationDecision::ReadyToDeactivateFenced => {
            let preserve_owner = active.unpublished_node_logs != 0;
            start_fenced_deactivate(cell, pool, cells, transitioning, tasks, preserve_owner);
        }
        CoordinationDecision::ReadyToDeactivate => {
            start_deactivate(cell, pool, cells, transitioning, tasks);
        }
        CoordinationDecision::StartQueuedWork => {
            start_next(active, pool, tasks, node_lease);
        }
        CoordinationDecision::Ignored
        | CoordinationDecision::Admit
        | CoordinationDecision::ResolveUnknown
        | CoordinationDecision::LocalHandle
        | CoordinationDecision::Reject(_)
        | CoordinationDecision::Started
        | CoordinationDecision::EffectCompleted
        | CoordinationDecision::StaleEffect => {}
        CoordinationDecision::Fence => {
            fence_active(active);
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
