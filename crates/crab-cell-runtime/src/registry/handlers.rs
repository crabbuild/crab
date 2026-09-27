//! Command, query, and activity handler contracts.

use std::future::Future;
use std::pin::Pin;

use rusqlite::{Connection, Transaction};

use super::*;

/// Synchronous application command context with no raw transaction accessor.
pub struct CommandContext<'borrow, 'connection> {
    pub(super) transaction: &'borrow Transaction<'connection>,
    pub(super) target: CellTarget,
    pub(super) effect_targets: &'static [NamespaceId],
    pub(super) sequence: u64,
    pub(super) now_ms: i64,
    pub(super) issued_at_ms: i64,
    pub(super) input_limit: u32,
    pub(super) output_limit: u32,
    pub(super) effects: Option<EffectBatch>,
}

impl CommandContext<'_, '_> {
    /// Returns the Cell this command targets.
    #[must_use]
    pub fn cell_id(&self) -> CellId {
        self.target.cell_id()
    }

    /// Returns the runtime-validated target for deterministic cross-Cell routing.
    #[must_use]
    pub const fn target(&self) -> &CellTarget {
        &self.target
    }

    /// Returns the actor-ordered sequence the command was admitted at.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the logical runtime time for the command.
    #[must_use]
    pub const fn now_ms(&self) -> i64 {
        self.now_ms
    }

    /// Returns the timestamp at which the caller created this mutation.
    ///
    /// This remains crate-private because application commands should use the
    /// logical runtime timestamp for domain decisions. Native primitives use
    /// it only when validating an absolute expiry that is part of a request.
    pub(crate) const fn issued_at_ms(&self) -> i64 {
        self.issued_at_ms
    }

    /// Emits one durable cross-Cell command with this command's allocator.
    pub fn emit_effect(&mut self, intent: &EffectCommandIntent) -> Result<[u8; 32]> {
        if intent.target.tenant() != self.target.tenant()
            || intent.target.application() != self.target.application()
        {
            return Err(Error::Identity(
                "effect target is outside the source application scope",
            ));
        }
        if !self.effect_targets.contains(&intent.target.namespace()) {
            return Err(Error::Command("effect target is not declared"));
        }
        self.ensure_effects()?;
        let transaction = self.transaction;
        self.effects
            .as_mut()
            .ok_or(Error::Command("effect ledger was not initialized"))?
            .insert_command(transaction, intent)
    }

    pub(crate) fn primitive_effects(&mut self) -> Result<(&Transaction<'_>, &mut EffectBatch)> {
        self.ensure_effects()?;
        let transaction = self.transaction;
        let effects = self
            .effects
            .as_mut()
            .ok_or(Error::Command("effect ledger was not initialized"))?;
        Ok((transaction, effects))
    }

    pub(super) fn ensure_effects(&mut self) -> Result<&mut EffectBatch> {
        if self.effects.is_none() {
            self.effects = Some(EffectBatch::new(
                self.transaction,
                &self.target,
                self.sequence,
                self.now_ms,
            )?);
        }
        self.effects
            .as_mut()
            .ok_or(Error::Command("effect ledger was not initialized"))
    }

    /// Executes bounded application SQL under the runtime authorizer.
    pub fn sql(&self, batch: &SqlBatch) -> Result<Vec<SqlResultSet>> {
        sql_batch(self.transaction, batch)
    }

    /// Reserves database bytes under a stable key for later commands in this Cell.
    ///
    /// Install the capacity primitive schema first. Reusing a key requires the
    /// same rounded page count. Reservations persist until explicitly released;
    /// every runtime commit protects them, including its own receipt writes.
    pub fn reserve_database_capacity(&self, key: &[u8], bytes: u64) -> Result<()> {
        crate::primitives::capacity::reserve(self.transaction, key, bytes)
    }

    /// Releases a reservation in this command, returning whether it existed.
    ///
    /// Release before applying deferred work. Failure of this command restores
    /// the reservation with the rest of the transaction.
    pub fn release_database_capacity(&self, key: &[u8]) -> Result<bool> {
        crate::primitives::capacity::release(self.transaction, key)
    }

    /// Returns the SQLite page size for application allocation accounting.
    pub fn database_page_size(&self) -> Result<u32> {
        Ok(self
            .transaction
            .query_row("PRAGMA page_size", [], |row| row.get(0))?)
    }

    /// Writes a bounded slice into an existing application BLOB in this command.
    ///
    /// Allocate its fixed size with SQL `zeroblob` first. Only main-database
    /// rowid tables and unindexed, non-key columns are supported. SQLite does
    /// not run triggers or CHECK constraints for incremental writes: callers
    /// must maintain application invariants in the same command. Protected
    /// tables, writes beyond the BLOB, and operations over 1 MiB are rejected.
    /// Propagate failures to roll back the command's application savepoint.
    pub fn write_sql_blob(
        &self,
        table: &str,
        column: &str,
        row_id: i64,
        offset: usize,
        bytes: &[u8],
    ) -> Result<()> {
        crate::primitives::sql::write_blob(self.transaction, table, column, row_id, offset, bytes)
    }

    pub(crate) const fn primitive_transaction(&self) -> &Transaction<'_> {
        self.transaction
    }
}

/// Read-only application query context with no raw connection accessor.
pub struct QueryContext<'borrow> {
    pub(super) connection: &'borrow Connection,
    pub(super) cell: CellId,
    pub(super) commit_sequence: u64,
    pub(super) now_ms: i64,
    pub(super) input_limit: u32,
    pub(super) output_limit: u32,
}

impl QueryContext<'_> {
    /// Returns the Cell this query reads.
    #[must_use]
    pub const fn cell_id(&self) -> CellId {
        self.cell
    }

    /// Returns the highest committed sequence the query may observe.
    #[must_use]
    pub const fn commit_sequence(&self) -> u64 {
        self.commit_sequence
    }

    /// Returns the logical runtime time for the query.
    #[must_use]
    pub const fn now_ms(&self) -> i64 {
        self.now_ms
    }

    /// Executes bounded read-only application SQL under the runtime authorizer.
    pub fn sql(&self, batch: &SqlBatch) -> Result<Vec<SqlResultSet>> {
        sql_query_batch(self.connection, batch)
    }

    /// Returns occupied SQLite page bytes, including runtime and indexes but excluding the freelist.
    pub fn database_used_bytes(&self) -> Result<u64> {
        let pages: i64 = self
            .connection
            .query_row("PRAGMA page_count", [], |row| row.get(0))?;
        let free_pages: i64 = self
            .connection
            .query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
        let page_size: i64 = self
            .connection
            .query_row("PRAGMA page_size", [], |row| row.get(0))?;
        let pages = pages
            .checked_sub(free_pages)
            .ok_or(Error::Command("invalid database freelist count"))?;
        let pages =
            u64::try_from(pages).map_err(|_| Error::Command("invalid database page count"))?;
        let page_size =
            u64::try_from(page_size).map_err(|_| Error::Command("invalid database page size"))?;
        pages
            .checked_mul(page_size)
            .ok_or(Error::Command("database size overflow"))
    }

    pub(crate) const fn primitive_connection(&self) -> &Connection {
        self.connection
    }
}

pub(super) type CommandHandler = for<'borrow, 'connection> fn(
    &mut CommandContext<'borrow, 'connection>,
    &[u8],
) -> Result<HandlerOutcome>;

pub(super) type QueryHandler =
    for<'borrow> fn(&mut QueryContext<'borrow>, &[u8]) -> Result<Vec<u8>>;
pub(super) type ActivityFuture =
    Pin<Box<dyn Future<Output = Result<ActivityExecution>> + Send + 'static>>;
pub(super) type AsyncActivityFunction = fn(ActivityContext, Vec<u8>) -> ActivityFuture;
pub(super) type BlockingActivityFunction =
    fn(ActivityContext, Vec<u8>) -> Result<ActivityExecution>;

#[derive(Clone, Copy)]
pub(super) enum ActivityFunction {
    Async(AsyncActivityFunction),
    Blocking(BlockingActivityFunction),
}
pub(super) type MaintenanceFuture = Pin<
    Box<
        dyn Future<
                Output = std::result::Result<
                    Committed<MaintenanceTickOutcome>,
                    InvocationError<MaintenanceTickOutcome>,
                >,
            > + Send
            + 'static,
    >,
>;
pub(super) type MaintenanceRunner =
    fn(CellClient, CellTarget, MutationIdentity, MaintenanceTickRequest) -> MaintenanceFuture;
pub(super) type EffectFuture = Pin<
    Box<
        dyn Future<Output = std::result::Result<EffectRunOutcome, EffectSupervisorError>>
            + Send
            + 'static,
    >,
>;
pub(super) type EffectRunner = fn(CellClient, CellTarget, EffectPeerClient, u32) -> EffectFuture;
pub(super) type ActivityRunFuture = Pin<
    Box<
        dyn Future<Output = std::result::Result<ActivityRunOutcome, ActivitySupervisorError>>
            + Send
            + 'static,
    >,
>;
pub(super) type ActivityRunner = fn(
    CellClient,
    TenantId,
    ApplicationId,
    u32,
    u32,
    Option<BlockingActivityReservation>,
) -> ActivityRunFuture;

/// Stored command decision encoded with the command's declared output codec.
pub enum CommandResult<T> {
    /// The command committed a result the caller consumes as success.
    Success(T),
    /// The command committed a rejection the caller consumes as the result.
    Rejected(T),
}

/// Statically dispatched typed command implemented by compiled Crab code.
pub trait Command: Send + Sync + 'static {
    /// Module the command is registered under.
    const MODULE: &'static str;
    /// Command id within its module.
    const ID: u32;
    /// Input codec version the command accepts.
    const CODEC_VERSION: u32;
    /// Declared input type.
    type Input: WireValue;
    /// Declared output type.
    type Output: WireValue;

    /// Runs the command inside its savepoint and returns its decision.
    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> Result<CommandResult<Self::Output>>;
}

/// Statically dispatched typed query implemented by compiled Crab code.
pub trait Query: Send + Sync + 'static {
    /// Module the query is registered under.
    const MODULE: &'static str;
    /// Query id within its module.
    const ID: u32;
    /// Input codec version the query accepts.
    const CODEC_VERSION: u32;
    /// Declared input type.
    type Input: WireValue;
    /// Declared output type.
    type Output: WireValue;

    /// Runs the read-only query.
    fn execute(context: &mut QueryContext<'_>, input: Self::Input) -> Result<Self::Output>;
}

/// Validated command selection and bounded input supplied by runtime routing.
pub struct CommandInvocation<'a> {
    /// Module the routing selected.
    pub module: &'a str,
    /// Command id the routing selected.
    pub operation_id: u32,
    /// Codec version the caller declared.
    pub codec_version: u32,
    /// Schema version the Cell serves.
    pub schema: u32,
    /// Validated target the command must own.
    pub target: CellTarget,
    /// Actor-ordered sequence of the command.
    pub sequence: u64,
    /// Logical runtime time for the command.
    pub now_ms: i64,
    /// Bounded encoded input.
    pub input: &'a [u8],
}

/// Validated query selection and bounded input supplied by runtime routing.
pub struct QueryInvocation<'a> {
    /// Module the routing selected.
    pub module: &'a str,
    /// Query id the routing selected.
    pub operation_id: u32,
    /// Codec version the caller declared.
    pub codec_version: u32,
    /// Schema version the Cell serves.
    pub schema: u32,
    /// Cell the query reads.
    pub cell: CellId,
    /// Highest committed sequence the query may observe.
    pub commit_sequence: u64,
    /// Logical runtime time for the query.
    pub now_ms: i64,
    /// Bounded encoded input.
    pub input: &'a [u8],
}

/// Source-level module registration contract for statically linked Crab code.
pub trait CellModule: Send + Sync + 'static {
    /// Source-level module name, matched against its descriptor.
    const NAME: &'static str;

    /// Returns the static descriptor this module registers.
    fn descriptor(&self) -> &'static ModuleDescriptor;
    /// Registers this module's typed bindings.
    fn register(self, registry: &mut RegistryBuilder) -> std::result::Result<(), RegistryError>;
}
