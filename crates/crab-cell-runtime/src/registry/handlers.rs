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
    #[must_use]
    pub fn cell_id(&self) -> CellId {
        self.target.cell_id()
    }

    /// Returns the runtime-validated target for deterministic cross-Cell routing.
    #[must_use]
    pub const fn target(&self) -> &CellTarget {
        &self.target
    }

    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

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
    #[must_use]
    pub const fn cell_id(&self) -> CellId {
        self.cell
    }

    #[must_use]
    pub const fn commit_sequence(&self) -> u64 {
        self.commit_sequence
    }

    #[must_use]
    pub const fn now_ms(&self) -> i64 {
        self.now_ms
    }

    /// Executes bounded read-only application SQL under the runtime authorizer.
    pub fn sql(&self, batch: &SqlBatch) -> Result<Vec<SqlResultSet>> {
        sql_query_batch(self.connection, batch)
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
    Success(T),
    Rejected(T),
}

/// Statically dispatched typed command implemented by compiled Crab code.
pub trait Command: Send + Sync + 'static {
    const MODULE: &'static str;
    const ID: u32;
    const CODEC_VERSION: u32;
    type Input: WireValue;
    type Output: WireValue;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> Result<CommandResult<Self::Output>>;
}

/// Statically dispatched typed query implemented by compiled Crab code.
pub trait Query: Send + Sync + 'static {
    const MODULE: &'static str;
    const ID: u32;
    const CODEC_VERSION: u32;
    type Input: WireValue;
    type Output: WireValue;

    fn execute(context: &mut QueryContext<'_>, input: Self::Input) -> Result<Self::Output>;
}

/// Validated command selection and bounded input supplied by runtime routing.
pub struct CommandInvocation<'a> {
    pub module: &'a str,
    pub operation_id: u32,
    pub codec_version: u32,
    pub schema: u32,
    pub target: CellTarget,
    pub sequence: u64,
    pub now_ms: i64,
    pub input: &'a [u8],
}

/// Validated query selection and bounded input supplied by runtime routing.
pub struct QueryInvocation<'a> {
    pub module: &'a str,
    pub operation_id: u32,
    pub codec_version: u32,
    pub schema: u32,
    pub cell: CellId,
    pub commit_sequence: u64,
    pub now_ms: i64,
    pub input: &'a [u8],
}

/// Source-level module registration contract for statically linked Crab code.
pub trait CellModule: Send + Sync + 'static {
    const NAME: &'static str;

    fn descriptor(&self) -> &'static ModuleDescriptor;
    fn register(self, registry: &mut RegistryBuilder) -> std::result::Result<(), RegistryError>;
}
