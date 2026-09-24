//! The worker thread loop and its command handler.
//!
//! One OS thread owns each shard of Cells: it receives `WorkerCommand`s,
//! runs them against the shard's executors, and fences a Cell whose native
//! callback panicked instead of leaving a poisoned executor behind.

use super::*;

pub(super) fn run_worker(mut receiver: mpsc::Receiver<WorkerCommand>) {
    let mut cells = HashMap::new();
    while let Some(command) = receiver.blocking_recv() {
        match command {
            WorkerCommand::Reserved {
                command,
                reservation,
            } => {
                run_worker_command(*command, &mut cells, Some(reservation));
            }
            command => run_worker_command(command, &mut cells, None),
        }
    }
}

fn run_worker_command(
    command: WorkerCommand,
    cells: &mut HashMap<CellId, ActiveCell>,
    mut reservation: Option<WorkerJobReservation>,
) {
    match command {
        WorkerCommand::Reserved {
            command,
            reservation,
        } => {
            run_worker_command(*command, cells, Some(reservation));
        }
        WorkerCommand::Activate {
            cell,
            executor,
            reservation,
            reply,
        } => {
            let result = match cells.entry(cell) {
                std::collections::hash_map::Entry::Occupied(_) => Err(Error::CellAlreadyActive),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(ActiveCell {
                        executor: *executor,
                        _reservation: reservation,
                    });
                    Ok(())
                }
            };
            let _ = reply.send(result);
        }
        WorkerCommand::ActivateRestored {
            cell,
            database,
            destination,
            incarnation,
            schema,
            root,
            reservation,
            reply,
        } => {
            let result = match cells.entry(cell) {
                std::collections::hash_map::Entry::Occupied(_) => Err(Error::CellAlreadyActive),
                std::collections::hash_map::Entry::Vacant(entry) => (*database)
                    .open_writable(&destination)
                    .map_err(Error::from)
                    .and_then(|db| CellExecutor::from_restored(db, cell, incarnation, schema, root))
                    .map(|executor| {
                        entry.insert(ActiveCell {
                            executor,
                            _reservation: reservation,
                        });
                    }),
            };
            let _ = reply.send(result);
        }
        WorkerCommand::Bootstrap(bootstrap) => {
            let WorkerBootstrap {
                cell,
                replica,
                destination,
                incarnation,
                schema,
                initialize,
                reservation,
                reply,
            } = *bootstrap;
            let result = match catch_unwind(AssertUnwindSafe(|| match cells.entry(cell) {
                std::collections::hash_map::Entry::Occupied(_) => Err(Error::CellAlreadyActive),
                std::collections::hash_map::Entry::Vacant(entry) => replica
                    .open_new(&destination)
                    .map_err(Error::from)
                    .and_then(|db| {
                        CellExecutor::bootstrap(db, cell, incarnation, schema, initialize)
                    })
                    .map(|(executor, cuts, next_due_ms)| {
                        entry.insert(ActiveCell {
                            executor,
                            _reservation: reservation,
                        });
                        BootstrapExecution { cuts, next_due_ms }
                    }),
            })) {
                Ok(result) => result,
                Err(_) => Err(Error::NativePanic),
            };
            let _ = reply.send(result);
        }
        WorkerCommand::Execute {
            cell,
            identity,
            operation_digest,
            now_ms,
            max_result_bytes,
            deadline,
            handler,
            reply,
        } => {
            let result = run_native_callback(cells, cell, deadline, move |active| {
                match active.executor.execute(
                    identity,
                    operation_digest,
                    now_ms,
                    max_result_bytes,
                    handler,
                )? {
                    CommandExecution::Recorded(outcome) => Ok(WorkerExecution::Recorded(outcome)),
                    CommandExecution::Pending => active
                        .executor
                        .latest_pending()
                        .cloned()
                        .map(Box::new)
                        .map(WorkerExecution::Pending)
                        .ok_or(Error::Fenced),
                }
            });
            drop(reservation.take());
            let _ = reply.send(result);
        }
        WorkerCommand::Migrate {
            cell,
            plan,
            now_ms,
            deadline,
            reply,
        } => {
            let result = run_native_callback(cells, cell, deadline, move |active| {
                active.executor.migrate(plan, now_ms)?;
                active
                    .executor
                    .pending_migration()
                    .cloned()
                    .ok_or(Error::Fenced)
            });
            drop(reservation.take());
            let _ = reply.send(result);
        }
        WorkerCommand::DeliverEffect {
            cell,
            delivery,
            now_ms,
            max_result_bytes,
            deadline,
            handler,
            reply,
        } => {
            let result = run_native_callback(cells, cell, deadline, move |active| {
                match active
                    .executor
                    .deliver_effect(delivery, now_ms, max_result_bytes, handler)?
                {
                    CommandExecution::Recorded(outcome) => Ok(WorkerExecution::Recorded(outcome)),
                    CommandExecution::Pending => active
                        .executor
                        .latest_pending()
                        .cloned()
                        .map(Box::new)
                        .map(WorkerExecution::Pending)
                        .ok_or(Error::Fenced),
                }
            });
            drop(reservation.take());
            let _ = reply.send(result);
        }
        WorkerCommand::Query {
            cell,
            max_result_bytes,
            deadline,
            handler,
            reply,
        } => {
            let result = run_native_callback(cells, cell, deadline, move |active| {
                active.executor.query(max_result_bytes, handler)
            });
            drop(reservation.take());
            let _ = reply.send(result);
        }
        WorkerCommand::Hydrate {
            cell,
            pages,
            deadline,
            reply,
        } => {
            let result = run_native_callback(cells, cell, deadline, move |active| {
                active.executor.hydrate_step(pages)
            });
            drop(reservation.take());
            let _ = reply.send(result);
        }
        WorkerCommand::Hydration { cell, reply } => {
            let result = cells
                .get(&cell)
                .ok_or(Error::CellNotActive)
                .and_then(|active| active.executor.hydration());
            let _ = reply.send(result);
        }
        WorkerCommand::PersistedWork { cell, role, reply } => {
            let result = cells
                .get_mut(&cell)
                .ok_or(Error::CellNotActive)
                .and_then(|active| active.executor.persisted_work_inventory(role));
            drop(reservation.take());
            let _ = reply.send(result);
        }
        WorkerCommand::TransferWork {
            cell,
            role,
            now_ms,
            reply,
        } => {
            let result = cells
                .get_mut(&cell)
                .ok_or(Error::CellNotActive)
                .and_then(|active| active.executor.transfer_work_inventory(role, now_ms));
            drop(reservation.take());
            let _ = reply.send(result);
        }
        WorkerCommand::Resolve {
            cell,
            identity,
            operation_digest,
            now_ms,
            max_result_bytes,
            deadline,
            reply,
        } => {
            let result = crab_ltx::with_paged_io_deadline(deadline, || {
                cells
                    .get_mut(&cell)
                    .ok_or(Error::CellNotActive)
                    .and_then(|cell| {
                        cell.executor
                            .resolve(identity, operation_digest, now_ms, max_result_bytes)
                    })
            });
            drop(reservation.take());
            let _ = reply.send(result);
        }
        WorkerCommand::ResolveEffect {
            cell,
            delivery,
            now_ms,
            max_result_bytes,
            deadline,
            reply,
        } => {
            let result = crab_ltx::with_paged_io_deadline(deadline, || {
                cells
                    .get_mut(&cell)
                    .ok_or(Error::CellNotActive)
                    .and_then(|cell| {
                        cell.executor
                            .resolve_effect(delivery, now_ms, max_result_bytes)
                    })
            });
            drop(reservation.take());
            let _ = reply.send(result);
        }
        WorkerCommand::BindPrepared {
            cell,
            prepared,
            reply,
        } => {
            let result = cells
                .get_mut(&cell)
                .ok_or(Error::CellNotActive)
                .and_then(|cell| cell.executor.bind_prepared(&prepared));
            let _ = reply.send(result);
        }
        WorkerCommand::BindMigrationPrepared {
            cell,
            prepared,
            reply,
        } => {
            let result = cells
                .get_mut(&cell)
                .ok_or(Error::CellNotActive)
                .and_then(|cell| cell.executor.bind_migration_prepared(&prepared));
            let _ = reply.send(result);
        }
        WorkerCommand::Pending { cell, reply } => {
            let result = cells
                .get(&cell)
                .ok_or(Error::CellNotActive)
                .map(|cell| cell.executor.pending().cloned());
            let _ = reply.send(result);
        }
        WorkerCommand::ConfirmPublished { cell, root, reply } => {
            let result = cells
                .get_mut(&cell)
                .ok_or(Error::CellNotActive)
                .and_then(|cell| cell.executor.confirm_published(&root));
            let _ = reply.send(result);
        }
        WorkerCommand::ConfirmDurable {
            cell,
            commit_sequence,
            reply,
        } => {
            let result = cells
                .get_mut(&cell)
                .ok_or(Error::CellNotActive)
                .and_then(|cell| cell.executor.confirm_durable(commit_sequence));
            let _ = reply.send(result);
        }
        WorkerCommand::ConfirmBootstrapPublished { cell, cuts, reply } => {
            let result = cells
                .get_mut(&cell)
                .ok_or(Error::CellNotActive)
                .and_then(|cell| cell.executor.confirm_bootstrap_published(&cuts));
            let _ = reply.send(result);
        }
        WorkerCommand::ConfirmMigrationPublished { cell, root, reply } => {
            let result = cells
                .get_mut(&cell)
                .ok_or(Error::CellNotActive)
                .and_then(|cell| cell.executor.confirm_migration_published(&root));
            let _ = reply.send(result);
        }
        WorkerCommand::Fence { cell, reply } => {
            let result = cells
                .get_mut(&cell)
                .ok_or(Error::CellNotActive)
                .map(|cell| cell.executor.fence());
            let _ = reply.send(result);
        }
        WorkerCommand::State { cell, reply } => {
            let result = cells
                .get(&cell)
                .ok_or(Error::CellNotActive)
                .map(|cell| cell.executor.worker_state());
            let _ = reply.send(result);
        }
        WorkerCommand::InterruptHandle { cell, reply } => {
            let result = cells
                .get(&cell)
                .ok_or(Error::CellNotActive)
                .map(|cell| cell.executor.interrupt_handle());
            let _ = reply.send(result);
        }
        WorkerCommand::Deactivate { cell, reply } => {
            let result = match cells.get(&cell) {
                None => Err(Error::CellNotActive),
                Some(cell) if !cell.executor.drained() => Err(Error::PendingPublication),
                Some(_) => cells
                    .remove(&cell)
                    .ok_or(Error::CellNotActive)
                    .and_then(|cell| cell.executor.close()),
            };
            let _ = reply.send(result);
        }
        WorkerCommand::Discard { cell, reply } => {
            let result = cells
                .remove(&cell)
                .ok_or(Error::CellNotActive)
                .and_then(|cell| cell.executor.discard());
            let _ = reply.send(result);
        }
    }
}

fn run_native_callback<T>(
    cells: &mut HashMap<CellId, ActiveCell>,
    cell: CellId,
    deadline: Instant,
    callback: impl FnOnce(&mut ActiveCell) -> Result<T>,
) -> Result<T> {
    let result = catch_unwind(AssertUnwindSafe(|| {
        crab_ltx::with_paged_io_deadline(deadline, || {
            cells
                .get_mut(&cell)
                .ok_or(Error::CellNotActive)
                .and_then(callback)
        })
    }));
    match result {
        Ok(result) => result,
        Err(_) => {
            if let Some(active) = cells.get_mut(&cell) {
                active.executor.fence();
            }
            Err(Error::NativePanic)
        }
    }
}
