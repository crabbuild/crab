use crab_ltx::{CaptureBatch, ManagedDb, TransactionError, rusqlite::OptionalExtension};

use crate::{CellId, Digest, Error, IncarnationId, RequestId, Result};

const MAX_RESULT_BYTES: usize = 1 << 20;
const MAX_REQUEST_LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_ISSUED_FUTURE_MS: i64 = 5 * 60 * 1000;
const REQUEST_RETENTION_MS: i64 = 24 * 60 * 60 * 1000;

/// Stable caller identity retained across retries and outcome resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MutationIdentity {
    pub request_id: RequestId,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
}

impl MutationIdentity {
    pub(crate) fn validate(self, now_ms: i64) -> Result<()> {
        self.validate_bounds(now_ms)?;
        if self.expires_at_ms <= now_ms {
            return Err(Error::Command("invalid mutation identity lifetime"));
        }
        Ok(())
    }

    fn validate_bounds(self, now_ms: i64) -> Result<()> {
        if now_ms < 0
            || self.issued_at_ms < 0
            || self.expires_at_ms <= self.issued_at_ms
            || self.expires_at_ms - self.issued_at_ms > MAX_REQUEST_LIFETIME_MS
            || self.issued_at_ms > now_ms.saturating_add(MAX_ISSUED_FUTURE_MS)
        {
            return Err(Error::Command("invalid mutation identity lifetime"));
        }
        Ok(())
    }

    pub(crate) fn expired(self, now_ms: i64) -> Result<bool> {
        self.validate_bounds(now_ms)?;
        Ok(self.expires_at_ms <= now_ms)
    }
}

/// Bounded handler decision made inside the application savepoint.
pub enum HandlerOutcome {
    Success(Vec<u8>),
    Rejected(Vec<u8>),
}

/// A durable result stored in `sys_requests`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoredOutcome {
    Success {
        result: Vec<u8>,
        commit_sequence: u64,
    },
    Rejected {
        result: Vec<u8>,
        commit_sequence: u64,
    },
}

/// Authoritative request-ledger observation from the current Cell owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    Committed(StoredOutcome),
    Absent,
    Unknown,
    Expired,
}

impl StoredOutcome {
    #[must_use]
    pub fn commit_sequence(&self) -> u64 {
        match self {
            Self::Success {
                commit_sequence, ..
            }
            | Self::Rejected {
                commit_sequence, ..
            } => *commit_sequence,
        }
    }

    #[must_use]
    pub fn result(&self) -> &[u8] {
        match self {
            Self::Success { result, .. } | Self::Rejected { result, .. } => result,
        }
    }
}

/// Locally committed command retained until immutable upload and control CAS.
#[derive(Clone)]
pub struct PendingCommit {
    identity: MutationIdentity,
    operation_digest: Digest,
    outcome: StoredOutcome,
    logical_time_ms: i64,
    next_due_ms: Option<i64>,
    cuts: CaptureBatch,
    prepared: Option<crab_ltx::RootRef>,
}

impl PendingCommit {
    #[must_use]
    pub fn identity(&self) -> MutationIdentity {
        self.identity
    }

    #[must_use]
    pub fn operation_digest(&self) -> Digest {
        self.operation_digest
    }

    #[must_use]
    pub fn outcome(&self) -> &StoredOutcome {
        &self.outcome
    }

    #[must_use]
    pub fn logical_time_ms(&self) -> i64 {
        self.logical_time_ms
    }

    #[must_use]
    pub fn next_due_ms(&self) -> Option<i64> {
        self.next_due_ms
    }

    #[must_use]
    pub fn cuts(&self) -> &CaptureBatch {
        &self.cuts
    }

    #[must_use]
    pub fn prepared(&self) -> Option<crab_ltx::RootRef> {
        self.prepared
    }
}

/// Immediate executor result; pending output cannot be observed before publication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandExecution {
    Recorded(StoredOutcome),
    Pending,
}

/// Single-threaded SQL owner for one active Cell.
///
/// The actor must not call `execute` while `pending` is present. Dropping a
/// caller does not remove pending cuts or their result. Only `confirm_published`
/// releases that result after an authoritative root matches the local commit.
pub struct CellExecutor {
    db: ManagedDb,
    cell: CellId,
    incarnation: IncarnationId,
    schema: u32,
    pending: Option<PendingCommit>,
    fenced: bool,
}

impl CellExecutor {
    pub(crate) fn interrupt_handle(&self) -> crab_ltx::rusqlite::InterruptHandle {
        self.db.interrupt_handle()
    }

    #[must_use]
    pub fn new(db: ManagedDb, cell: CellId, incarnation: IncarnationId, schema: u32) -> Self {
        Self {
            db,
            cell,
            incarnation,
            schema,
            pending: None,
            fenced: false,
        }
    }

    pub(crate) fn bootstrap(
        mut db: ManagedDb,
        cell: CellId,
        incarnation: IncarnationId,
        schema: u32,
        initialize: impl FnOnce(&crab_ltx::rusqlite::Transaction<'_>) -> Result<()>,
    ) -> Result<(Self, CaptureBatch, Option<i64>)> {
        let initialized = db.transaction_with(|transaction| {
            crate::schema::install_runtime_schema_in(transaction, cell, incarnation, schema)?;
            initialize(transaction)?;
            crate::scheduler_next_due_ms(transaction, 0)
        });
        let next_due_ms = match initialized {
            Ok(next_due_ms) => next_due_ms,
            Err(error) => {
                let error = transaction_error_with_io(&db, error);
                let _ = db.close();
                return Err(error);
            }
        };
        let cuts = match db.capture() {
            Ok(cuts) if !cuts.segments.is_empty() => cuts,
            Ok(_) => {
                let _ = db.close();
                return Err(Error::Control("bootstrap produced no LTX cut"));
            }
            Err(error) => {
                let _ = db.close();
                return Err(error.into());
            }
        };
        Ok((Self::new(db, cell, incarnation, schema), cuts, next_due_ms))
    }

    pub(crate) fn from_restored(
        mut db: ManagedDb,
        cell: CellId,
        incarnation: IncarnationId,
        schema: u32,
        root: crab_ltx::RootRef,
    ) -> Result<Self> {
        let expected_sequence = match i64::try_from(root.commit_sequence) {
            Ok(sequence) => sequence,
            Err(_) => {
                let _ = db.close();
                return Err(Error::Control("root commit sequence exceeds SQLite range"));
            }
        };
        if root.cell != *cell.as_bytes()
            || root.incarnation != *incarnation.as_bytes()
            || db.position() != root.position
        {
            let _ = db.close();
            return Err(Error::Control(
                "restored SQLite position does not match root",
            ));
        }
        let verification = db.transaction_with(|transaction| {
            let metadata = transaction.query_row(
                "SELECT cell_id, incarnation, commit_sequence, logical_time_ms, schema_version FROM sys_meta WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, u32>(4)?,
                    ))
                },
            )?;
            let latest_request = transaction.query_row(
                "SELECT COALESCE(MAX(commit_sequence), 0) FROM sys_requests",
                [],
                |row| row.get::<_, i64>(0),
            )?;
            if metadata.0.as_slice() != cell.as_bytes()
                || metadata.1.as_slice() != incarnation.as_bytes()
                || metadata.2 != expected_sequence
                || metadata.3 < 0
                || metadata.4 != schema
                || latest_request > metadata.2
            {
                return Err(Error::Control(
                    "restored SQLite metadata does not match authoritative root",
                ));
            }
            Ok(())
        });
        if let Err(error) = verification {
            let error = transaction_error_with_io(&db, error);
            let _ = db.close();
            return Err(error);
        }
        Ok(Self::new(db, cell, incarnation, schema))
    }

    /// Executes one accepted command or returns its already published result.
    pub fn execute(
        &mut self,
        identity: MutationIdentity,
        operation_digest: Digest,
        now_ms: i64,
        max_result_bytes: usize,
        handler: impl FnOnce(&crab_ltx::rusqlite::Transaction<'_>) -> Result<HandlerOutcome>,
    ) -> Result<CommandExecution> {
        if self.fenced {
            return Err(Error::Fenced);
        }
        if self.pending.is_some() {
            return Err(Error::PendingPublication);
        }
        if max_result_bytes > MAX_RESULT_BYTES {
            return Err(Error::Command("result limit exceeds 1 MiB"));
        }
        identity.validate(now_ms)?;
        let cell = self.cell;
        let incarnation = self.incarnation;
        let schema = self.schema;
        let transaction = self.db.transaction_with(|transaction| {
            let meta = transaction.query_row(
                "SELECT cell_id, incarnation, commit_sequence, logical_time_ms, schema_version FROM sys_meta WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, u32>(4)?,
                    ))
                },
            )?;
            if meta.0.as_slice() != cell.as_bytes()
                || meta.1.as_slice() != incarnation.as_bytes()
                || meta.2 < 0
                || meta.3 < 0
                || meta.4 != schema
            {
                return Err(Error::Command("runtime schema identity mismatch"));
            }

            let existing = transaction
                .query_row(
                    "SELECT operation_digest, outcome, result, commit_sequence FROM sys_requests WHERE request_id = ?1",
                    [identity.request_id.as_bytes().as_slice()],
                    |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, Vec<u8>>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    },
                )
                .optional()?;
            if let Some((digest, outcome, result, sequence)) = existing {
                if digest.as_slice() != operation_digest.as_bytes() {
                    return Err(Error::RequestConflict);
                }
                let outcome = stored_outcome(outcome, result, sequence)?;
                if outcome.result().len() > max_result_bytes {
                    return Err(Error::Command("stored result exceeds command limit"));
                }
                return Ok(TransactionResult::Recorded(outcome));
            }

            let sequence = meta
                .2
                .checked_add(1)
                .filter(|value| *value > 0)
                .ok_or(Error::Command("commit sequence overflow"))?;
            let logical_time_ms = now_ms.max(meta.3);
            transaction.execute_batch("SAVEPOINT application")?;
            let decision = handler(transaction)?;
            let (outcome, result) = match decision {
                HandlerOutcome::Success(result) => {
                    transaction.execute_batch("RELEASE application")?;
                    (1, result)
                }
                HandlerOutcome::Rejected(result) => {
                    transaction.execute_batch(
                        "ROLLBACK TO application; RELEASE application",
                    )?;
                    (2, result)
                }
            };
            if result.len() > max_result_bytes {
                return Err(Error::Command("handler result exceeds command limit"));
            }
            let retain_until_ms = identity
                .expires_at_ms
                .checked_add(REQUEST_RETENTION_MS)
                .ok_or(Error::Command("request retention overflow"))?;
            transaction.execute(
                "INSERT INTO sys_requests(request_id, operation_digest, outcome, result, commit_sequence, expires_at_ms, retain_until_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                (
                    identity.request_id.as_bytes().as_slice(),
                    operation_digest.as_bytes().as_slice(),
                    outcome,
                    result.as_slice(),
                    sequence,
                    identity.expires_at_ms,
                    retain_until_ms,
                ),
            )?;
            if transaction.execute(
                "UPDATE sys_meta SET commit_sequence = ?1, logical_time_ms = ?2 WHERE singleton = 1",
                (sequence, logical_time_ms),
            )? != 1
            {
                return Err(Error::Command("runtime metadata row missing"));
            }
            let next_due_ms = crate::scheduler_next_due_ms(transaction, logical_time_ms)?;
            Ok(TransactionResult::Committed {
                outcome: stored_outcome(outcome, result, sequence)?,
                logical_time_ms,
                next_due_ms,
            })
        });

        if let Some(error) = self.db.take_io_error() {
            self.fenced = true;
            return Err(ltx_error(error));
        }
        let transaction = match transaction {
            Ok(value) => value,
            Err(error) => match error {
                TransactionError::Operation(error) => return Err(error),
                error => {
                    self.fenced = true;
                    return Err(transaction_error(error));
                }
            },
        };
        match transaction {
            TransactionResult::Recorded(outcome) => Ok(CommandExecution::Recorded(outcome)),
            TransactionResult::Committed {
                outcome,
                logical_time_ms,
                next_due_ms,
            } => {
                let cuts = match self.db.capture() {
                    Ok(cuts) => cuts,
                    Err(error) => {
                        self.fenced = true;
                        return Err(error.into());
                    }
                };
                self.pending = Some(PendingCommit {
                    identity,
                    operation_digest,
                    outcome,
                    logical_time_ms,
                    next_due_ms,
                    cuts,
                    prepared: None,
                });
                Ok(CommandExecution::Pending)
            }
        }
    }

    /// Runs one bounded read only when no unpublished local commit exists.
    pub fn query(
        &mut self,
        max_result_bytes: usize,
        handler: impl FnOnce(&crab_ltx::rusqlite::Connection) -> Result<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        if self.fenced {
            return Err(Error::Fenced);
        }
        if self.pending.is_some() {
            return Err(Error::PendingPublication);
        }
        if max_result_bytes > MAX_RESULT_BYTES {
            return Err(Error::Command("result limit exceeds 1 MiB"));
        }
        let result = self.db.query_with(handler);
        if let Some(error) = self.db.take_io_error() {
            self.fenced = true;
            return Err(ltx_error(error));
        }
        match result {
            Ok(result) if result.len() <= max_result_bytes => Ok(result),
            Ok(_) => Err(Error::Command("query result exceeds command limit")),
            Err(crab_ltx::QueryError::Operation(error)) => Err(error),
            Err(crab_ltx::QueryError::Sqlite(error)) => {
                self.fenced = true;
                Err(error.into())
            }
            Err(crab_ltx::QueryError::State(error)) => {
                self.fenced = true;
                Err(error.into())
            }
        }
    }

    /// Resolves one identity against the current published SQLite state.
    pub fn resolve(
        &mut self,
        identity: MutationIdentity,
        operation_digest: Digest,
        now_ms: i64,
        max_result_bytes: usize,
    ) -> Result<Resolution> {
        if self.fenced {
            return Ok(Resolution::Unknown);
        }
        if self.pending.is_some() {
            return Ok(Resolution::Unknown);
        }
        if identity.expired(now_ms)? {
            return Ok(Resolution::Expired);
        }
        if max_result_bytes > MAX_RESULT_BYTES {
            return Err(Error::Command("result limit exceeds 1 MiB"));
        }
        let result = self.db.query_with(|connection| {
            connection
                .query_row(
                    "SELECT operation_digest, outcome, result, commit_sequence FROM sys_requests WHERE request_id = ?1",
                    [identity.request_id.as_bytes().as_slice()],
                    |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, Vec<u8>>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    },
                )
                .optional()
        });
        if let Some(error) = self.db.take_io_error() {
            self.fenced = true;
            return Err(ltx_error(error));
        }
        let existing = match result {
            Ok(existing) => existing,
            Err(crab_ltx::QueryError::Operation(error)) => return Err(error.into()),
            Err(crab_ltx::QueryError::Sqlite(error)) => {
                self.fenced = true;
                return Err(error.into());
            }
            Err(crab_ltx::QueryError::State(error)) => {
                self.fenced = true;
                return Err(error.into());
            }
        };
        let Some((digest, outcome, result, sequence)) = existing else {
            return Ok(Resolution::Absent);
        };
        if digest.as_slice() != operation_digest.as_bytes() {
            return Err(Error::RequestConflict);
        }
        let outcome = stored_outcome(outcome, result, sequence)?;
        if outcome.result().len() > max_result_bytes {
            return Err(Error::Command("stored result exceeds command limit"));
        }
        Ok(Resolution::Committed(outcome))
    }

    #[must_use]
    pub fn pending(&self) -> Option<&PendingCommit> {
        self.pending.as_ref()
    }

    /// Pins the one immutable proposal that may satisfy the pending commit.
    pub fn bind_prepared(&mut self, prepared: &crab_ltx::PreparedRoot) -> Result<()> {
        let pending = self.pending.as_mut().ok_or(Error::PendingPublication)?;
        let root = prepared.root();
        if root.cell != *self.cell.as_bytes()
            || root.incarnation != *self.incarnation.as_bytes()
            || root.position != pending.cuts.position
            || root.commit_sequence != pending.outcome.commit_sequence()
            || prepared.verified().schema() != self.schema
            || pending.prepared.is_some_and(|existing| existing != root)
        {
            return Err(Error::Command(
                "prepared root does not match pending commit",
            ));
        }
        pending.prepared = Some(root);
        Ok(())
    }

    /// Releases the stored result only after the published root proves inclusion.
    pub fn confirm_published(&mut self, root: &crab_ltx::RootRef) -> Result<StoredOutcome> {
        let pending = self.pending.as_ref().ok_or(Error::PendingPublication)?;
        if pending.prepared.as_ref() != Some(root) {
            return Err(Error::Command(
                "published root does not match prepared commit",
            ));
        }
        self.pending
            .take()
            .map(|pending| pending.outcome)
            .ok_or(Error::PendingPublication)
    }

    /// Closes a drained executor; pending or fenced state requires recovery.
    pub fn close(self) -> Result<()> {
        if self.pending.is_some() || self.fenced {
            return Err(Error::Fenced);
        }
        self.db.close()?;
        Ok(())
    }

    pub(crate) fn fence(&mut self) {
        self.fenced = true;
    }

    pub(crate) fn drained(&self) -> bool {
        self.pending.is_none() && !self.fenced
    }

    pub(crate) fn worker_state(&self) -> crate::worker::WorkerState {
        if self.fenced {
            crate::worker::WorkerState::Fenced
        } else if self.pending.is_some() {
            crate::worker::WorkerState::Pending
        } else {
            crate::worker::WorkerState::Ready
        }
    }
}

fn transaction_error(error: TransactionError<Error>) -> Error {
    match error {
        TransactionError::Operation(error) => error,
        TransactionError::Sqlite(error) => error.into(),
        TransactionError::Capture(error) => error.into(),
    }
}

fn transaction_error_with_io(db: &ManagedDb, error: TransactionError<Error>) -> Error {
    db.take_io_error()
        .map_or_else(|| transaction_error(error), ltx_error)
}

fn ltx_error(error: crab_ltx::CrabError) -> Error {
    match error {
        crab_ltx::CrabError::Deadline => Error::Deadline,
        error => Error::Ltx(error),
    }
}

enum TransactionResult {
    Recorded(StoredOutcome),
    Committed {
        outcome: StoredOutcome,
        logical_time_ms: i64,
        next_due_ms: Option<i64>,
    },
}

fn stored_outcome(outcome: i64, result: Vec<u8>, sequence: i64) -> Result<StoredOutcome> {
    if result.len() > MAX_RESULT_BYTES || sequence <= 0 {
        return Err(Error::Command("invalid stored request outcome"));
    }
    let commit_sequence =
        u64::try_from(sequence).map_err(|_| Error::Command("invalid stored commit sequence"))?;
    match outcome {
        1 => Ok(StoredOutcome::Success {
            result,
            commit_sequence,
        }),
        2 => Ok(StoredOutcome::Rejected {
            result,
            commit_sequence,
        }),
        _ => Err(Error::Command("invalid stored request outcome")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restored_executor_rejects_root_sequence_ahead_of_sqlite_metadata() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("cell.sqlite");
        let cell = CellId::from_bytes([31; 32]);
        let incarnation = IncarnationId::from_bytes([32; 16]);
        let mut connection = crab_ltx::rusqlite::Connection::open(&path).unwrap();
        crate::install_runtime_schema(&mut connection, cell, incarnation, 1).unwrap();
        drop(connection);
        let mut db = ManagedDb::open(&path, crab_ltx::Limits::default()).unwrap();
        db.transaction(|transaction| {
            transaction.execute("UPDATE sys_meta SET logical_time_ms = 1", [])?;
            Ok(())
        })
        .unwrap();
        db.capture().unwrap();
        let root = crab_ltx::RootRef {
            cell: *cell.as_bytes(),
            incarnation: *incarnation.as_bytes(),
            digest: [33; 32],
            position: db.position(),
            commit_sequence: 1,
        };
        assert!(matches!(
            CellExecutor::from_restored(db, cell, incarnation, 1, root),
            Err(Error::Control(
                "restored SQLite metadata does not match authoritative root"
            ))
        ));
    }
}
