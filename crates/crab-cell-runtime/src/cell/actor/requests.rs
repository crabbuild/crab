//! Request execution for one Cell actor.
//!
//! Every request path runs on the actor's own task: it drives one queued
//! command, query, migration, or resolve to a terminal coordination decision,
//! publishes the outcome, and hands the cell back through `continue_cell`.

use super::admission::{
    fence_active, fence_admission, send_command_reply, send_migration_reply, send_query_reply,
    send_resolve_reply,
};
use super::*;

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
    let root_sequence_lag = i128::from(publication.pending.outcome().commit_sequence())
        - i128::from(active.published_sequence);
    tracing::debug!(
        queue_wait_ms = publication.submitted_at.elapsed().as_millis(),
        pending_publications = active.coordination.publication_count(),
        publication_bytes = active.publication_bytes,
        root_sequence_lag = %root_sequence_lag,
        "Cell LTX publication started"
    );
    let generation = active.generation;
    let effect_id = active.begin_task(CoordinationEffect::Publication);
    // Moving the publisher out of ActiveCell is the serialization token for
    // root preparation and CAS; no second object publisher can overtake it.
    let pool = pool.clone();
    let retained_reservation = publication.retained_reservation;
    let published_next_due_ms = publication.pending.next_due_ms();
    let published_commit_sequence = publication.pending.outcome().commit_sequence();
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
            next_due_ms: published_next_due_ms,
            commit_sequence: published_commit_sequence,
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
