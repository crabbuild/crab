//! Single-Cell executor: commands, queries, migrations, and pending publication.
use std::collections::VecDeque;

use crab_ltx::{CaptureBatch, Db, TransactionError, rusqlite::OptionalExtension};

use crate::cell::catalog::CatalogRole;
use crate::identity::{CellId, Digest};
use crate::identity::{IncarnationId, RequestId};
use crate::primitives::maintenance::PersistedWorkInventory;
use crate::primitives::maintenance::TransferWorkInventory;
use crate::{Error, Result};

const MAX_RESULT_BYTES: usize = crate::codec::MAX_WIRE_BYTES;
const MAX_REQUEST_LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_ISSUED_FUTURE_MS: i64 = 5 * 60 * 1000;
const REQUEST_RETENTION_MS: i64 = 24 * 60 * 60 * 1000;
pub(crate) const MAX_PENDING_PUBLICATIONS: usize = 64;
pub(crate) const PENDING_PUBLICATION_HIGH_WATER_BYTES: u64 = 64 << 20;

/// Stable caller identity retained across retries and outcome resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MutationIdentity {
    /// Caller-supplied request identity.
    pub request_id: RequestId,
    /// Logical time the caller created the mutation.
    pub issued_at_ms: i64,
    /// Logical time after which the identity is refused.
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
    /// The handler committed a successful result.
    Success(Vec<u8>),
    /// The handler committed a rejection.
    Rejected(Vec<u8>),
}

/// A durable result stored in `sys_requests`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoredOutcome {
    /// Committed success with the sequence it was stored at.
    Success {
        /// Encoded result bytes.
        result: Vec<u8>,
        /// Commit sequence the outcome was stored at.
        commit_sequence: u64,
    },
    /// Committed rejection with the sequence it was stored at.
    Rejected {
        /// Encoded result bytes.
        result: Vec<u8>,
        /// Commit sequence the outcome was stored at.
        commit_sequence: u64,
    },
}

/// Authoritative request-ledger observation from the current Cell owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// The request has a durable outcome.
    Committed(StoredOutcome),
    /// No outcome is stored for the request.
    Absent,
    /// The owning Cell could not be reached, so the outcome is unknown.
    Unknown,
    /// The request identity expired before an outcome was stored.
    Expired,
}

impl StoredOutcome {
    /// Returns the commit sequence the outcome was stored at.
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

    /// Returns the encoded result bytes.
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
    outcome: StoredOutcome,
    logical_time_ms: i64,
    next_due_ms: Option<i64>,
    cuts: CaptureBatch,
    prepared: Option<crab_ltx::RootRef>,
    durable: bool,
}

/// Locally committed schema step retained until its root and control pair publish.
#[derive(Clone)]
pub struct PendingMigration {
    code: Digest,
    from_schema: u32,
    to_schema: u32,
    digest: Option<Digest>,
    commit_sequence: u64,
    next_due_ms: Option<i64>,
    cuts: CaptureBatch,
    prepared: Option<crab_ltx::RootRef>,
}

impl PendingMigration {
    /// Returns the application code digest the migration installs.
    #[must_use]
    pub const fn code(&self) -> Digest {
        self.code
    }

    /// Returns the schema version the migration starts from.
    #[must_use]
    pub const fn from_schema(&self) -> u32 {
        self.from_schema
    }

    /// Returns the schema version the migration installs.
    #[must_use]
    pub const fn to_schema(&self) -> u32 {
        self.to_schema
    }

    /// Returns the migration plan digest, when the plan declares one.
    #[must_use]
    pub const fn digest(&self) -> Option<Digest> {
        self.digest
    }

    /// Returns the commit sequence the migration committed at.
    #[must_use]
    pub const fn commit_sequence(&self) -> u64 {
        self.commit_sequence
    }

    /// Returns the logical time the owner asked to be renewed by.
    #[must_use]
    pub const fn next_due_ms(&self) -> Option<i64> {
        self.next_due_ms
    }

    /// Returns the captured cuts awaiting publication.
    #[must_use]
    pub const fn cuts(&self) -> &CaptureBatch {
        &self.cuts
    }

    #[must_use]
    pub(crate) fn retained_bytes(&self) -> u64 {
        retained_bytes(&self.cuts)
    }
}

/// Durably proven identity of one completed schema migration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationOutcome {
    /// Application code digest the migration installed.
    pub code: Digest,
    /// Schema version the migration installed.
    pub schema: u32,
    /// Commit sequence the migration committed at.
    pub commit_sequence: u64,
}

impl PendingCommit {
    /// Returns the durable outcome awaiting publication.
    #[must_use]
    pub fn outcome(&self) -> &StoredOutcome {
        &self.outcome
    }

    /// Returns the logical time the commit was made at.
    #[must_use]
    pub fn logical_time_ms(&self) -> i64 {
        self.logical_time_ms
    }

    /// Returns the logical time the owner asked to be renewed by.
    #[must_use]
    pub fn next_due_ms(&self) -> Option<i64> {
        self.next_due_ms
    }

    /// Returns the captured cuts awaiting publication.
    #[must_use]
    pub fn cuts(&self) -> &CaptureBatch {
        &self.cuts
    }

    /// Returns the prepared root, once one has been uploaded.
    #[must_use]
    pub fn prepared(&self) -> Option<crab_ltx::RootRef> {
        self.prepared
    }

    #[must_use]
    pub(crate) fn retained_bytes(&self) -> u64 {
        retained_bytes(&self.cuts)
    }
}

/// Bytes the captured cuts retain until they publish.
fn retained_bytes(cuts: &CaptureBatch) -> u64 {
    cuts.segments
        .iter()
        .map(|segment| segment.info().size_bytes)
        .sum()
}

/// Immediate executor result; pending output cannot be observed before publication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandExecution {
    /// The command has a durable outcome.
    Recorded(StoredOutcome),
    /// The command committed locally and waits for publication.
    Pending,
}

/// Single-threaded SQL owner for one active Cell.
///
/// The actor may continue after the preceding commit has a durability proof.
/// Dropping a caller does not remove queued cuts or their result. Only
/// `confirm_published` releases retained files after an authoritative root
/// matches the oldest local commit.
pub struct CellExecutor {
    db: Db,
    cell: CellId,
    incarnation: IncarnationId,
    schema: u32,
    pending: VecDeque<PendingCommit>,
    pending_bytes: u64,
    published_sequence: u64,
    pending_migration: Option<PendingMigration>,
    fenced: bool,
}

impl CellExecutor {
    pub(crate) fn interrupt_handle(&self) -> crab_ltx::rusqlite::InterruptHandle {
        self.db.interrupt_handle()
    }

    /// Creates the executor for one active Cell at the given schema version.
    #[must_use]
    pub fn new(db: Db, cell: CellId, incarnation: IncarnationId, schema: u32) -> Self {
        Self {
            db,
            cell,
            incarnation,
            schema,
            pending: VecDeque::new(),
            pending_bytes: 0,
            published_sequence: 0,
            pending_migration: None,
            fenced: false,
        }
    }

    pub(crate) fn bootstrap(
        mut db: Db,
        cell: CellId,
        incarnation: IncarnationId,
        schema: u32,
        initialize: impl FnOnce(&crab_ltx::rusqlite::Transaction<'_>) -> Result<()>,
    ) -> Result<(Self, CaptureBatch, Option<i64>)> {
        let initialized = db.transaction_with(|transaction| {
            crate::cell::schema::install_runtime_schema_in(transaction, cell, incarnation, schema)?;
            initialize(transaction)?;
            crate::fleet::scheduler::scheduler_next_due_ms(transaction, 0)
        });
        let next_due_ms = match initialized {
            Ok(next_due_ms) => next_due_ms,
            Err(error) => {
                let error = transaction_error_with_io(&db, error);
                let _ = db.close();
                return Err(error);
            }
        };
        // Bootstrap publishes these exact bytes before activation. Local file
        // durability would duplicate the root publication proof.
        let cuts = match db.capture_deferred() {
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
        mut db: Db,
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
        let verification = db.query_with(|connection| {
            let metadata = connection.query_row(
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
            let latest_ledger = connection.query_row(
                "SELECT COALESCE(MAX(commit_sequence), 0) FROM (SELECT commit_sequence FROM sys_requests UNION ALL SELECT commit_sequence FROM sys_inbox)",
                [],
                |row| row.get::<_, i64>(0),
            )?;
            if metadata.0.as_slice() != cell.as_bytes()
                || metadata.1.as_slice() != incarnation.as_bytes()
                || metadata.2 != expected_sequence
                || metadata.3 < 0
                || metadata.4 != schema
                || latest_ledger > metadata.2
            {
                return Err(Error::Control(
                    "restored SQLite metadata does not match authoritative root",
                ));
            }
            Ok(())
        });
        if let Err(error) = verification {
            let error = db.take_io_error().map_or_else(
                || match error {
                    crab_ltx::QueryError::Operation(error) => error,
                    crab_ltx::QueryError::Sqlite(error) => error.into(),
                    crab_ltx::QueryError::State(error) => error.into(),
                },
                ltx_error,
            );
            let _ = db.close();
            return Err(error);
        }
        let mut executor = Self::new(db, cell, incarnation, schema);
        executor.published_sequence = root.commit_sequence;
        Ok(executor)
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
        if !self.accepts_publication() {
            return Err(Error::PendingPublication);
        }
        if max_result_bytes > MAX_RESULT_BYTES {
            return Err(Error::Command("result exceeds wire limit"));
        }
        identity.validate(now_ms)?;
        let cell = self.cell;
        let incarnation = self.incarnation;
        let schema = self.schema;
        let transaction = self.db.transaction_with(|transaction| {
            let (commit_sequence, prior_logical_time_ms) =
                runtime_metadata(transaction, cell, incarnation, schema)?;

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

            let sequence = commit_sequence
                .checked_add(1)
                .filter(|value| *value > 0)
                .ok_or(Error::Command("commit sequence overflow"))?;
            let logical_time_ms = now_ms.max(prior_logical_time_ms);
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
            let next_due_ms = crate::fleet::scheduler::scheduler_next_due_ms(transaction, logical_time_ms)?;
            Ok(TransactionResult::Committed {
                outcome: stored_outcome(outcome, result, sequence)?,
                logical_time_ms,
                next_due_ms,
            })
        });

        self.finish_transaction(transaction)
    }

    /// Applies or replays one destination inbox operation through normal publication.
    pub fn deliver_effect(
        &mut self,
        delivery: crate::primitives::effects::InboxDelivery,
        now_ms: i64,
        max_result_bytes: usize,
        handler: impl FnOnce(&crab_ltx::rusqlite::Transaction<'_>) -> Result<HandlerOutcome>,
    ) -> Result<CommandExecution> {
        if self.fenced {
            return Err(Error::Fenced);
        }
        if !self.accepts_publication() {
            return Err(Error::PendingPublication);
        }
        if max_result_bytes > MAX_RESULT_BYTES {
            return Err(Error::Command("effect result exceeds wire limit"));
        }
        let cell = self.cell;
        let incarnation = self.incarnation;
        let schema = self.schema;
        let transaction = self.db.transaction_with(|transaction| {
            let (commit_sequence, prior_logical_time_ms) =
                runtime_metadata(transaction, cell, incarnation, schema)?;
            let applied = crate::primitives::effects::inbox_apply(
                transaction,
                now_ms,
                delivery,
                max_result_bytes,
                handler,
            )?;
            let (outcome, duplicate) = match applied {
                crate::primitives::effects::InboxApplyOutcome::Success {
                    result,
                    commit_sequence,
                    duplicate,
                } => (
                    StoredOutcome::Success {
                        result,
                        commit_sequence,
                    },
                    duplicate,
                ),
                crate::primitives::effects::InboxApplyOutcome::Rejected {
                    result,
                    commit_sequence,
                    duplicate,
                } => (
                    StoredOutcome::Rejected {
                        result,
                        commit_sequence,
                    },
                    duplicate,
                ),
                crate::primitives::effects::InboxApplyOutcome::Conflict => return Err(Error::RequestConflict),
                crate::primitives::effects::InboxApplyOutcome::Expired => return Err(Error::EffectExpired),
            };
            if duplicate {
                return Ok(TransactionResult::Recorded(outcome));
            }
            let sequence = commit_sequence
                .checked_add(1)
                .filter(|value| *value > 0)
                .ok_or(Error::Command("commit sequence overflow"))?;
            let published_sequence = u64::try_from(sequence)
                .map_err(|_| Error::Command("effect destination sequence overflow"))?;
            if outcome.commit_sequence() != published_sequence {
                return Err(Error::Command("effect inbox sequence does not follow Cell state"));
            }
            let logical_time_ms = now_ms.max(prior_logical_time_ms);
            if transaction.execute(
                "UPDATE sys_meta SET commit_sequence = ?1, logical_time_ms = ?2 WHERE singleton = 1",
                (sequence, logical_time_ms),
            )? != 1
            {
                return Err(Error::Command("runtime metadata row missing"));
            }
            let next_due_ms = crate::fleet::scheduler::scheduler_next_due_ms(transaction, logical_time_ms)?;
            Ok(TransactionResult::Committed {
                outcome,
                logical_time_ms,
                next_due_ms,
            })
        });
        self.finish_transaction(transaction)
    }

    /// Runs one bounded read from the actor-serialized logical head.
    pub fn query(
        &mut self,
        max_result_bytes: usize,
        handler: impl FnOnce(&crab_ltx::rusqlite::Connection) -> Result<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        if self.fenced {
            return Err(Error::Fenced);
        }
        if !self.logical_head_is_durable() {
            return Err(Error::PendingPublication);
        }
        if max_result_bytes > MAX_RESULT_BYTES {
            return Err(Error::Command("result exceeds wire limit"));
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

    /// Resolves a bounded number of sparse pages without publishing or
    /// competing with the actor's foreground SQL admission.
    pub(crate) fn hydrate_step(&mut self, pages: u32) -> Result<Option<crab_ltx::Hydration>> {
        if self.fenced {
            return Err(Error::Fenced);
        }
        if pages == 0 {
            return Err(Error::Capacity("hydration pages"));
        }
        let Some(_) = self.db.hydration()? else {
            return Ok(None);
        };
        match self.db.hydrate_step(pages) {
            Ok(hydration) => Ok(Some(hydration)),
            Err(error) => {
                self.fenced = true;
                Err(error.into())
            }
        }
    }

    pub(crate) fn hydration(&self) -> Result<Option<crab_ltx::Hydration>> {
        self.db.hydration().map_err(Into::into)
    }

    /// Reads the durable work classes that can block safe owner release.
    pub(crate) fn persisted_work_inventory(
        &mut self,
        role: CatalogRole,
    ) -> Result<PersistedWorkInventory> {
        if self.fenced {
            return Err(Error::Fenced);
        }
        let result = self.db.query_with(|connection| {
            crate::primitives::maintenance::inspect_persisted_work(connection, role)
        });
        if let Some(error) = self.db.take_io_error() {
            self.fenced = true;
            return Err(ltx_error(error));
        }
        match result {
            Ok(inventory) => Ok(inventory),
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

    pub(crate) fn transfer_work_inventory(
        &mut self,
        role: CatalogRole,
        now_ms: i64,
    ) -> Result<TransferWorkInventory> {
        if self.fenced {
            return Err(Error::Fenced);
        }
        let result = self.db.query_with(|connection| {
            crate::primitives::maintenance::inspect_transfer_work(connection, role, now_ms)
        });
        if let Some(error) = self.db.take_io_error() {
            self.fenced = true;
            return Err(ltx_error(error));
        }
        match result {
            Ok(inventory) => Ok(inventory),
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

    /// Resolves one identity against the actor-serialized logical SQLite state.
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
        if !self.logical_head_is_durable() {
            return Ok(Resolution::Unknown);
        }
        if identity.expired(now_ms)? {
            return Ok(Resolution::Expired);
        }
        if max_result_bytes > MAX_RESULT_BYTES {
            return Err(Error::Command("result exceeds wire limit"));
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

    /// Resolves one destination inbox identity from the logical SQLite state.
    pub fn resolve_effect(
        &mut self,
        delivery: crate::primitives::effects::InboxDelivery,
        now_ms: i64,
        max_result_bytes: usize,
    ) -> Result<Resolution> {
        if self.fenced || !self.logical_head_is_durable() {
            return Ok(Resolution::Unknown);
        }
        let result = self.db.query_with(|connection| {
            crate::primitives::effects::inbox_resolve(
                connection,
                now_ms,
                delivery,
                max_result_bytes,
            )
        });
        if let Some(error) = self.db.take_io_error() {
            self.fenced = true;
            return Err(ltx_error(error));
        }
        match result {
            Ok(resolution) => Ok(resolution),
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

    /// Returns the oldest commit awaiting publication.
    #[must_use]
    pub fn pending(&self) -> Option<&PendingCommit> {
        self.pending.front()
    }

    #[must_use]
    pub(crate) fn latest_pending(&self) -> Option<&PendingCommit> {
        self.pending.back()
    }

    /// Marks one actor-ordered logical commit safe to observe before object publication.
    pub(crate) fn confirm_durable(&mut self, commit_sequence: u64) -> Result<()> {
        if commit_sequence <= self.published_sequence {
            return Ok(());
        }
        let index = self
            .pending
            .iter()
            .position(|pending| pending.outcome.commit_sequence() == commit_sequence)
            .ok_or(Error::PendingPublication)?;
        if self
            .pending
            .iter()
            .take(index)
            .any(|pending| !pending.durable)
        {
            return Err(Error::Control("durability proof skipped an earlier commit"));
        }
        let pending = self
            .pending
            .get_mut(index)
            .ok_or(Error::PendingPublication)?;
        pending.durable = true;
        Ok(())
    }

    /// Returns the schema migration awaiting publication.
    #[must_use]
    pub fn pending_migration(&self) -> Option<&PendingMigration> {
        self.pending_migration.as_ref()
    }

    /// Applies one trusted registry migration and retains its captured cut.
    pub fn migrate(&mut self, plan: crate::registry::MigrationPlan, now_ms: i64) -> Result<()> {
        if self.fenced {
            return Err(Error::Fenced);
        }
        if self.has_pending() {
            return Err(Error::PendingPublication);
        }
        let valid_step = match (plan.sql(), plan.digest()) {
            (Some(sql), Some(digest)) => {
                self.schema.checked_add(1) == Some(plan.to_schema())
                    && digest == Digest::from_bytes(*blake3::hash(sql.as_bytes()).as_bytes())
            }
            (None, None) => self.schema == plan.to_schema() && plan.from_code() != plan.to_code(),
            _ => false,
        };
        if now_ms < 0 || plan.from_schema() != self.schema || !valid_step {
            return Err(Error::Registry("invalid Cell migration plan"));
        }
        let cell = self.cell;
        let incarnation = self.incarnation;
        let from_schema = self.schema;
        let to_schema = plan.to_schema();
        let digest = plan.digest();
        let transaction = self.db.transaction_with(|transaction| {
            let (commit_sequence, prior_logical_time_ms) =
                runtime_metadata(transaction, cell, incarnation, from_schema)?;
            let sequence = commit_sequence
                .checked_add(1)
                .filter(|value| *value > 0)
                .ok_or(Error::Command("commit sequence overflow"))?;
            let logical_time_ms = now_ms.max(prior_logical_time_ms);
            if let (Some(sql), Some(digest)) = (plan.sql(), digest) {
                let existing = transaction
                    .query_row(
                        "SELECT digest, applied_sequence FROM sys_migrations WHERE version = ?1",
                        [to_schema],
                        |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?)),
                    )
                    .optional()?;
                if let Some((existing_digest, _)) = existing {
                    return Err(if existing_digest.as_slice() == digest.as_bytes() {
                        Error::Control("migration is recorded ahead of runtime schema")
                    } else {
                        Error::Registry("migration digest conflicts with SQLite history")
                    });
                }
                transaction.execute_batch(sql)?;
                transaction.execute(
                    "INSERT INTO sys_migrations(version, digest, applied_sequence) VALUES (?1, ?2, ?3)",
                    (to_schema, digest.as_bytes().as_slice(), sequence),
                )?;
            }
            if transaction.execute(
                "UPDATE sys_meta SET commit_sequence = ?1, logical_time_ms = ?2, schema_version = ?3 WHERE singleton = 1 AND schema_version = ?4",
                (sequence, logical_time_ms, to_schema, from_schema),
            )? != 1
            {
                return Err(Error::Control("migration metadata changed during execution"));
            }
            let metadata = runtime_metadata(transaction, cell, incarnation, to_schema)?;
            if metadata != (sequence, logical_time_ms) {
                return Err(Error::Control("migration metadata did not validate"));
            }
            let next_due_ms = crate::fleet::scheduler::scheduler_next_due_ms(transaction, logical_time_ms)?;
            Ok((sequence, next_due_ms))
        });
        if let Some(error) = self.db.take_io_error() {
            self.fenced = true;
            return Err(ltx_error(error));
        }
        let (sequence, next_due_ms) = match transaction {
            Ok(value) => value,
            Err(TransactionError::Operation(error)) => return Err(error),
            Err(TransactionError::Admission(error)) => return Err(admission_error(error)),
            Err(error) => {
                self.fenced = true;
                return Err(transaction_error(error));
            }
        };
        // Migration output stays gated on follower or object durability. Keep
        // the local cut readable for publication without serially fsyncing it.
        let cuts = match self.db.capture_deferred() {
            Ok(cuts) if !cuts.segments.is_empty() => cuts,
            Ok(_) => {
                self.fenced = true;
                return Err(Error::Control("migration produced no LTX cut"));
            }
            Err(error) => {
                self.fenced = true;
                return Err(error.into());
            }
        };
        let commit_sequence =
            u64::try_from(sequence).map_err(|_| Error::Command("migration sequence overflow"))?;
        self.schema = to_schema;
        self.pending_migration = Some(PendingMigration {
            code: plan.to_code(),
            from_schema,
            to_schema,
            digest,
            commit_sequence,
            next_due_ms,
            cuts,
            prepared: None,
        });
        Ok(())
    }

    /// Pins the one immutable proposal that may satisfy the pending commit.
    pub fn bind_prepared(&mut self, prepared: &crab_ltx::PreparedRoot) -> Result<()> {
        let pending = self.pending.front_mut().ok_or(Error::PendingPublication)?;
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

    /// Pins the immutable proposal for the pending schema migration.
    pub fn bind_migration_prepared(&mut self, prepared: &crab_ltx::PreparedRoot) -> Result<()> {
        let pending = self
            .pending_migration
            .as_mut()
            .ok_or(Error::PendingPublication)?;
        let root = prepared.root();
        if root.cell != *self.cell.as_bytes()
            || root.incarnation != *self.incarnation.as_bytes()
            || root.position != pending.cuts.position
            || root.commit_sequence != pending.commit_sequence
            || prepared.verified().schema() != pending.to_schema
            || pending.prepared.is_some_and(|existing| existing != root)
        {
            return Err(Error::Command(
                "prepared root does not match pending migration",
            ));
        }
        pending.prepared = Some(root);
        Ok(())
    }

    /// Releases the stored result only after the published root proves inclusion.
    pub fn confirm_published(&mut self, root: &crab_ltx::RootRef) -> Result<StoredOutcome> {
        let pending = self.pending.front().ok_or(Error::PendingPublication)?;
        if pending.prepared.as_ref() != Some(root) {
            return Err(Error::Command(
                "published root does not match prepared commit",
            ));
        }
        self.db.prune_captured(&pending.cuts)?;
        let pending = self.pending.pop_front().ok_or(Error::PendingPublication)?;
        self.pending_bytes = self
            .pending_bytes
            .checked_sub(pending.retained_bytes())
            .ok_or(Error::Control("pending publication accounting underflow"))?;
        self.published_sequence = root.commit_sequence;
        Ok(pending.outcome)
    }

    /// Finalizes local migration state after its exact schema-bearing root publishes.
    pub fn confirm_migration_published(
        &mut self,
        root: &crab_ltx::RootRef,
    ) -> Result<MigrationOutcome> {
        let pending = self
            .pending_migration
            .as_ref()
            .ok_or(Error::PendingPublication)?;
        if pending.prepared.as_ref() != Some(root) {
            return Err(Error::Command(
                "published root does not match prepared migration",
            ));
        }
        self.db.prune_captured(&pending.cuts)?;
        self.pending_migration
            .take()
            .map(|pending| MigrationOutcome {
                code: pending.code,
                schema: pending.to_schema,
                commit_sequence: pending.commit_sequence,
            })
            .ok_or(Error::PendingPublication)
    }

    pub(crate) fn confirm_bootstrap_published(
        &mut self,
        cuts: &crab_ltx::CaptureBatch,
    ) -> Result<()> {
        if self.has_pending() || self.fenced || cuts.segments.is_empty() {
            return Err(Error::PendingPublication);
        }
        self.db.prune_captured(cuts)?;
        Ok(())
    }

    /// Closes a drained executor; pending or fenced state requires recovery.
    pub fn close(self) -> Result<()> {
        if self.has_pending() || self.fenced {
            return Err(Error::Fenced);
        }
        self.db.close()?;
        Ok(())
    }

    /// Closes a fenced executor without treating local pending state as authority.
    ///
    /// The caller must recover only from the authoritative immutable root. Local
    /// database and LTX artifacts remain quarantined for diagnosis.
    pub(crate) fn discard(self) -> Result<()> {
        self.db.close()?;
        Ok(())
    }

    pub(crate) fn fence(&mut self) {
        self.fenced = true;
    }

    pub(crate) fn drained(&self) -> bool {
        !self.has_pending() && !self.fenced
    }

    pub(crate) fn worker_state(&self) -> crate::cell::worker::WorkerState {
        if self.fenced {
            crate::cell::worker::WorkerState::Fenced
        } else if self.has_pending() {
            crate::cell::worker::WorkerState::Pending
        } else {
            crate::cell::worker::WorkerState::Ready
        }
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty() || self.pending_migration.is_some()
    }

    fn accepts_publication(&self) -> bool {
        self.pending_migration.is_none()
            && self.logical_head_is_durable()
            && self.pending.len() < MAX_PENDING_PUBLICATIONS
            && self.pending_bytes < PENDING_PUBLICATION_HIGH_WATER_BYTES
    }

    fn logical_head_is_durable(&self) -> bool {
        self.pending.back().is_none_or(|pending| pending.durable)
    }

    fn finish_transaction(
        &mut self,
        transaction: std::result::Result<TransactionResult, TransactionError<Error>>,
    ) -> Result<CommandExecution> {
        if let Some(error) = self.db.take_io_error() {
            self.fenced = true;
            return Err(ltx_error(error));
        }
        let transaction = match transaction {
            Ok(value) => value,
            Err(TransactionError::Operation(error)) => return Err(error),
            Err(TransactionError::Admission(error)) => return Err(admission_error(error)),
            Err(error) => {
                self.fenced = true;
                return Err(transaction_error(error));
            }
        };
        match transaction {
            TransactionResult::Recorded(outcome) => Ok(CommandExecution::Recorded(outcome)),
            TransactionResult::Committed {
                outcome,
                logical_time_ms,
                next_due_ms,
            } => {
                // The actor cannot expose this commit until the same cut is
                // durable on followers or behind the authoritative root CAS.
                let cuts = match self.db.capture_deferred() {
                    Ok(cuts) => cuts,
                    Err(error) => {
                        self.fenced = true;
                        return Err(error.into());
                    }
                };
                let pending = PendingCommit {
                    outcome,
                    logical_time_ms,
                    next_due_ms,
                    cuts,
                    prepared: None,
                    durable: false,
                };
                self.pending_bytes = self
                    .pending_bytes
                    .checked_add(pending.retained_bytes())
                    .ok_or(Error::Capacity("pending publication bytes"))?;
                self.pending.push_back(pending);
                Ok(CommandExecution::Pending)
            }
        }
    }
}

fn runtime_metadata(
    transaction: &crab_ltx::rusqlite::Transaction<'_>,
    cell: CellId,
    incarnation: IncarnationId,
    schema: u32,
) -> Result<(i64, i64)> {
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
    Ok((meta.2, meta.3))
}

fn transaction_error(error: TransactionError<Error>) -> Error {
    match error {
        TransactionError::Admission(error) => admission_error(error),
        TransactionError::Operation(error) => error,
        TransactionError::Sqlite(error) => error.into(),
        TransactionError::Capture(error) => error.into(),
    }
}

fn admission_error(error: crab_ltx::CrabError) -> Error {
    // A declared bound refused the work before any side effect, so the caller
    // sees a capacity refusal instead of a fence or an unknown outcome.
    match error.classify() {
        crab_ltx::FailureClass::Capacity => ltx_capacity_error(&error),
        _ => error.into(),
    }
}

fn ltx_capacity_error(error: &crab_ltx::CrabError) -> Error {
    match error {
        crab_ltx::CrabError::Limit(kind) => Error::Capacity(kind.as_str()),
        _ => Error::Capacity("local storage"),
    }
}

fn transaction_error_with_io(db: &Db, error: TransactionError<Error>) -> Error {
    db.take_io_error()
        .map_or_else(|| transaction_error(error), ltx_error)
}

fn ltx_error(error: crab_ltx::CrabError) -> Error {
    match error.classify() {
        crab_ltx::FailureClass::Capacity => ltx_capacity_error(&error),
        crab_ltx::FailureClass::Fenced => Error::Fenced,
        _ => match error {
            crab_ltx::CrabError::Deadline => Error::Deadline,
            error => Error::Ltx(error),
        },
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
mod tests;
