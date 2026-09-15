use std::collections::{HashSet, VecDeque};

use crab_ltx::rusqlite::Transaction;

use crate::{
    CatalogProof, CatalogShardScan, CellAuthority, CellCatalog, EffectBatch, Error, Result,
    SessionId, VersionedControl, WorkflowDefinition,
    effects::{
        effect_cleanup_terminal_bounded, effect_expire_ready_bounded,
        effect_reclaim_expired_bounded, inbox_cleanup_expired_bounded,
    },
    kv::kv_cleanup_expired_bounded,
    queue::{
        queue_cleanup_expired_bounded, queue_expire_ready_bounded, queue_reclaim_expired_bounded,
    },
    workflow::{
        workflow_cleanup_terminal_bounded, workflow_fail_one_expired_activity,
        workflow_fire_one_due_timer, workflow_reclaim_expired_bounded,
    },
};

const WORKFLOW_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const MAX_TICK_ITEMS: usize = 128;
const CONTROL_BATCH: usize = 32;

/// One due catalog entry and its exact observed control token.
pub struct DueCell {
    catalog: CatalogProof,
    control: VersionedControl,
}

impl DueCell {
    #[must_use]
    pub const fn catalog(&self) -> &CatalogProof {
        &self.catalog
    }

    #[must_use]
    pub const fn control(&self) -> &VersionedControl {
        &self.control
    }
}

/// Revision-pinned catalog scan with at most 32 control reads per step.
pub struct DueCellScan {
    catalog: CatalogShardScan,
    authority: CellAuthority,
    pending: VecDeque<CatalogProof>,
}

impl DueCellScan {
    /// Pins one catalog shard head before any control records are inspected.
    pub async fn new(catalog: &CellCatalog, authority: CellAuthority, shard: u8) -> Result<Self> {
        Ok(Self {
            catalog: catalog.scan_shard(shard).await?,
            authority,
            pending: VecDeque::new(),
        })
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.catalog.revision()
    }

    /// Advances by at most 32 entries; an empty page still represents progress.
    pub async fn next_batch(&mut self, now_ms: i64) -> Result<Option<Vec<DueCell>>> {
        if now_ms < 0 {
            return Err(Error::Command("negative scheduler scan time"));
        }
        if self.pending.is_empty() {
            let Some(page) = self.catalog.next_page().await? else {
                return Ok(None);
            };
            self.pending.extend(page.entries().iter().cloned());
        }
        let count = self.pending.len().min(CONTROL_BATCH);
        let mut due = Vec::new();
        for _ in 0..count {
            let proof = self
                .pending
                .pop_front()
                .ok_or(Error::Catalog("scheduler scan queue underflow"))?;
            let Some(control) = self.authority.load(proof.entry().cell()).await? else {
                continue;
            };
            if control.value().cell != proof.entry().cell() {
                return Err(Error::Control("catalog control changed Cell"));
            }
            if control.value().root.is_some()
                && control
                    .value()
                    .next_due_ms
                    .is_some_and(|deadline| deadline <= now_ms)
                && control.value().state != crate::ControlState::Tombstoned
            {
                due.push(DueCell {
                    catalog: proof,
                    control,
                });
            }
        }
        Ok(Some(due))
    }
}

/// Selects one preferred live scanner for a catalog shard by rendezvous score.
pub fn preferred_scanner(shard: u8, nodes: &[SessionId]) -> Result<Option<SessionId>> {
    let mut unique = HashSet::with_capacity(nodes.len());
    let mut winner = None;
    for node in nodes {
        if !unique.insert(*node.as_bytes()) {
            return Err(Error::Control("duplicate scheduler node session"));
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"crab.scheduler-rendezvous.v1\0");
        hasher.update(&[shard]);
        hasher.update(node.as_bytes());
        let score = *hasher.finalize().as_bytes();
        if winner
            .as_ref()
            .is_none_or(|(best, _): &([u8; 32], SessionId)| score > *best)
        {
            winner = Some((score, *node));
        }
    }
    Ok(winner.map(|(_, node)| node))
}

/// Bounded durable work performed by one serialized scheduler Tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedulerTickOutcome {
    pub processed: u32,
}

/// Rechecks and advances at most 128 due maintenance items in one transaction.
pub fn scheduler_tick(
    transaction: &Transaction<'_>,
    logical_time_ms: i64,
    workflow_definitions: &[&'static dyn WorkflowDefinition],
) -> Result<SchedulerTickOutcome> {
    let command_sequence = transaction.query_row(
        "SELECT commit_sequence + 1 FROM sys_meta WHERE singleton = 1 AND commit_sequence < 9223372036854775807",
        [],
        |row| row.get::<_, u64>(0),
    )?;
    scheduler_tick_at(
        transaction,
        command_sequence,
        logical_time_ms,
        workflow_definitions,
    )
}

pub(crate) fn scheduler_tick_at(
    transaction: &Transaction<'_>,
    command_sequence: u64,
    logical_time_ms: i64,
    workflow_definitions: &[&'static dyn WorkflowDefinition],
) -> Result<SchedulerTickOutcome> {
    if logical_time_ms < 0 {
        return Err(Error::Command("negative scheduler logical time"));
    }
    let tables = installed_tables(transaction)?;
    let mut effects = EffectBatch::new(transaction, command_sequence, logical_time_ms)?;
    let mut remaining = MAX_TICK_ITEMS;

    consume_with(&mut remaining, |limit| {
        transaction
            .execute(
            "DELETE FROM sys_requests WHERE request_id IN (SELECT request_id FROM sys_requests INDEXED BY sys_requests_expiry WHERE retain_until_ms <= ?1 ORDER BY retain_until_ms, request_id LIMIT ?2)",
                (logical_time_ms, limit as i64),
            )
            .map_err(Into::into)
    })?;
    consume_with(&mut remaining, |limit| {
        inbox_cleanup_expired_bounded(transaction, logical_time_ms, limit)
    })?;
    consume_with(&mut remaining, |limit| {
        effect_cleanup_terminal_bounded(transaction, logical_time_ms, limit)
    })?;
    consume_with(&mut remaining, |limit| {
        effect_expire_ready_bounded(transaction, logical_time_ms, limit)
    })?;
    consume_with(&mut remaining, |limit| {
        effect_reclaim_expired_bounded(transaction, logical_time_ms, limit)
    })?;

    if tables.contains("kv_entries") {
        consume_with(&mut remaining, |limit| {
            kv_cleanup_expired_bounded(transaction, logical_time_ms, limit)
        })?;
    }
    if tables.contains("queue_messages") {
        consume_with(&mut remaining, |limit| {
            queue_cleanup_expired_bounded(transaction, logical_time_ms, limit)
        })?;
        consume_with(&mut remaining, |limit| {
            queue_expire_ready_bounded(transaction, logical_time_ms, limit)
        })?;
        consume_with(&mut remaining, |limit| {
            queue_reclaim_expired_bounded(transaction, logical_time_ms, limit)
        })?;
    }
    if tables.contains("workflow_activities") {
        consume_with(&mut remaining, |limit| {
            workflow_cleanup_terminal_bounded(transaction, logical_time_ms, limit)
        })?;
        while remaining != 0
            && workflow_fail_one_expired_activity(
                transaction,
                &mut effects,
                logical_time_ms,
                workflow_definitions,
            )?
        {
            remaining -= 1;
        }
        consume_with(&mut remaining, |limit| {
            workflow_reclaim_expired_bounded(transaction, logical_time_ms, limit)
        })?;
        while remaining != 0
            && workflow_fire_one_due_timer(
                transaction,
                &mut effects,
                logical_time_ms,
                workflow_definitions,
            )?
        {
            remaining -= 1;
        }
    }

    Ok(SchedulerTickOutcome {
        processed: u32::try_from(MAX_TICK_ITEMS - remaining)
            .map_err(|_| Error::Command("scheduler Tick count overflow"))?,
    })
}

/// Computes the earliest durable work or retention deadline after a command.
///
/// Optional primitive schemas may be absent. Every returned deadline is clamped
/// to the command's logical time so stale work remains immediately discoverable.
pub fn scheduler_next_due_ms(
    transaction: &Transaction<'_>,
    logical_time_ms: i64,
) -> Result<Option<i64>> {
    if logical_time_ms < 0 {
        return Err(Error::Command("negative scheduler logical time"));
    }
    let tables = installed_tables(transaction)?;
    let mut next = None;

    include_minimum(
        transaction,
        "SELECT min(retain_until_ms) FROM sys_requests INDEXED BY sys_requests_expiry",
        logical_time_ms,
        &mut next,
    )?;
    include_minimum(
        transaction,
        "SELECT min(retain_until_ms) FROM sys_inbox INDEXED BY sys_inbox_expiry",
        logical_time_ms,
        &mut next,
    )?;
    include_minimum(
        transaction,
        "SELECT min(due_at_ms) FROM sys_effects INDEXED BY sys_effects_due WHERE state = 0",
        logical_time_ms,
        &mut next,
    )?;
    include_minimum(
        transaction,
        "SELECT min(lease_until_ms) FROM sys_effects INDEXED BY sys_effects_leases WHERE state = 1",
        logical_time_ms,
        &mut next,
    )?;
    include_minimum(
        transaction,
        "SELECT min(expires_at_ms) FROM sys_effects INDEXED BY sys_effects_expiry",
        logical_time_ms,
        &mut next,
    )?;

    if tables.contains("kv_entries") {
        include_minimum(
            transaction,
            "SELECT min(expires_at_ms) FROM kv_entries INDEXED BY kv_expiry WHERE expires_at_ms IS NOT NULL",
            logical_time_ms,
            &mut next,
        )?;
    }
    if tables.contains("queue_messages") {
        include_minimum(
            transaction,
            "SELECT min(due_at_ms) FROM queue_messages INDEXED BY queue_ready WHERE state = 0",
            logical_time_ms,
            &mut next,
        )?;
        include_minimum(
            transaction,
            "SELECT min(lease_until_ms) FROM queue_messages INDEXED BY queue_leases WHERE state = 1",
            logical_time_ms,
            &mut next,
        )?;
        include_minimum(
            transaction,
            "SELECT min(expires_at_ms) FROM queue_messages INDEXED BY queue_retention",
            logical_time_ms,
            &mut next,
        )?;
        include_minimum(
            transaction,
            "SELECT min(retain_until_ms) FROM queue_dedup INDEXED BY queue_dedup_expiry",
            logical_time_ms,
            &mut next,
        )?;
    }
    if tables.contains("workflow_activities") {
        include_minimum(
            transaction,
            "SELECT min(due_at_ms) FROM workflow_activities INDEXED BY activities_due WHERE state = 0",
            logical_time_ms,
            &mut next,
        )?;
        include_minimum(
            transaction,
            "SELECT min(lease_until_ms) FROM workflow_activities INDEXED BY activities_leases WHERE state = 1",
            logical_time_ms,
            &mut next,
        )?;
        include_minimum(
            transaction,
            "SELECT min(expires_at_ms) FROM workflow_activities INDEXED BY activities_expiry WHERE state IN (0, 1)",
            logical_time_ms,
            &mut next,
        )?;
        include_minimum(
            transaction,
            "SELECT min(due_at_ms) FROM workflow_timers INDEXED BY timers_due WHERE state = 0",
            logical_time_ms,
            &mut next,
        )?;
        let completed = minimum(
            transaction,
            "SELECT min(completed_at_ms) FROM workflow_runs INDEXED BY workflow_retention WHERE status != 0 AND completed_at_ms IS NOT NULL",
        )?;
        if let Some(completed_at_ms) = completed {
            let retention = completed_at_ms
                .checked_add(WORKFLOW_RETENTION_MS)
                .ok_or(Error::Command("workflow retention deadline overflow"))?;
            merge_due(retention, logical_time_ms, &mut next)?;
        }
    }
    Ok(next)
}

fn installed_tables(transaction: &Transaction<'_>) -> Result<HashSet<String>> {
    let mut statement = transaction.prepare(
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name IN ('kv_entries', 'queue_messages', 'workflow_activities')",
    )?;
    let tables = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<HashSet<_>, _>>()?;
    Ok(tables)
}

fn include_minimum(
    transaction: &Transaction<'_>,
    query: &'static str,
    logical_time_ms: i64,
    next: &mut Option<i64>,
) -> Result<()> {
    if let Some(value) = minimum(transaction, query)? {
        merge_due(value, logical_time_ms, next)?;
    }
    Ok(())
}

fn minimum(transaction: &Transaction<'_>, query: &'static str) -> Result<Option<i64>> {
    Ok(transaction.query_row(query, [], |row| row.get::<_, Option<i64>>(0))?)
}

fn merge_due(value: i64, logical_time_ms: i64, next: &mut Option<i64>) -> Result<()> {
    if value < 0 {
        return Err(Error::Command("negative stored scheduler deadline"));
    }
    let value = value.max(logical_time_ms);
    *next = Some(next.map_or(value, |current| current.min(value)));
    Ok(())
}

fn consume(remaining: &mut usize, processed: usize) -> Result<()> {
    *remaining = remaining
        .checked_sub(processed)
        .ok_or(Error::Command("scheduler Tick exceeded 128 items"))?;
    Ok(())
}

fn consume_with(
    remaining: &mut usize,
    operation: impl FnOnce(usize) -> Result<usize>,
) -> Result<()> {
    let processed = operation(*remaining)?;
    consume(remaining, processed)
}
