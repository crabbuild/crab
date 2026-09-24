use std::collections::{HashMap, HashSet, VecDeque};

use crab_ltx::rusqlite::Transaction;

use crate::cell::catalog::{CatalogProof, CatalogShardScan, CellCatalog};
use crate::control::authority::{CellAuthority, VersionedControl};
use crate::identity::{CellTarget, SessionId};
use crate::node::NodeAdvertisement;
use crate::primitives::blob::blob_cleanup_expired;
use crate::primitives::cron::CronTarget;
use crate::primitives::cron::cron_fire_due_bounded;
use crate::primitives::effects::EffectBatch;
use crate::primitives::effects::{
    effect_cleanup_terminal_bounded, effect_expire_ready_bounded, effect_reclaim_expired_bounded,
    inbox_cleanup_expired_bounded,
};
use crate::primitives::kv::kv_cleanup_expired_bounded;
use crate::primitives::queue::QueueDeadLetterTarget;
use crate::primitives::queue::{
    MAX_ATTEMPTS, QueueDeadLetterWriter, queue_cleanup_expired_bounded, queue_expire_ready_bounded,
    queue_expire_ready_bounded_with_dead_letter, queue_reclaim_expired_bounded,
    queue_reclaim_expired_bounded_with_dead_letter,
};
use crate::primitives::workflow::WorkflowDefinition;
use crate::primitives::workflow::{
    workflow_cleanup_terminal_bounded, workflow_fail_one_expired_activity,
    workflow_fire_one_due_timer, workflow_reclaim_expired_bounded,
};
use crate::primitives::{blob, cron, kv, queue, workflow};
use crate::{Error, Result};

const WORKFLOW_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const MAX_TICK_ITEMS: usize = 128;
const CONTROL_BATCH: usize = 32;
/// Unconditional classes: expired requests, inbox, terminal effects, due
/// effects, and expired effect leases.
const BASE_MAINTENANCE_CLASSES: usize = 5;

#[derive(Clone, Copy)]
struct ProgressObservation {
    progress: u64,
    changed_at_ms: i64,
}

/// Tracks advertised scanner progress and excludes sessions that stop advancing.
#[derive(Default)]
pub struct SchedulerFleet {
    observations: HashMap<SessionId, ProgressObservation>,
}

impl SchedulerFleet {
    /// Returns sessions eligible for rendezvous assignment at this observation.
    pub fn eligible_sessions(
        &mut self,
        nodes: &[NodeAdvertisement],
        now_ms: i64,
        stale_after_ms: i64,
    ) -> Result<Vec<SessionId>> {
        if now_ms < 0 || stale_after_ms <= 0 {
            return Err(Error::Control("scheduler liveness interval is invalid"));
        }
        let live = nodes
            .iter()
            .map(NodeAdvertisement::session)
            .collect::<HashSet<_>>();
        self.observations
            .retain(|session, _| live.contains(session));

        let mut eligible = Vec::with_capacity(nodes.len());
        for node in nodes {
            let observation =
                self.observations
                    .entry(node.session())
                    .or_insert(ProgressObservation {
                        progress: node.progress(),
                        changed_at_ms: now_ms,
                    });
            if node.progress() < observation.progress {
                return Err(Error::Control("scheduler progress regressed"));
            }
            if node.progress() > observation.progress {
                *observation = ProgressObservation {
                    progress: node.progress(),
                    changed_at_ms: now_ms,
                };
            }
            let capacity = node.capacity();
            if now_ms.saturating_sub(observation.changed_at_ms) < stale_after_ms
                && capacity.free_memory_bytes != 0
                && capacity.free_disk_bytes != 0
                && capacity.job_credits != 0
            {
                eligible.push(node.session());
            }
        }
        Ok(eligible)
    }
}

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
        self.next_batch_bounded(now_ms, CONTROL_BATCH).await
    }

    /// Advances by at most `limit` entries without discarding unvisited proofs.
    pub async fn next_batch_bounded(
        &mut self,
        now_ms: i64,
        limit: usize,
    ) -> Result<Option<Vec<DueCell>>> {
        if now_ms < 0 {
            return Err(Error::Command("negative scheduler scan time"));
        }
        if limit == 0 {
            return Err(Error::Command("scheduler scan limit is zero"));
        }
        if self.pending.is_empty() {
            let Some(page) = self.catalog.next_page().await? else {
                return Ok(None);
            };
            self.pending.extend(page.entries().iter().cloned());
        }
        let count = self.pending.len().min(CONTROL_BATCH).min(limit);
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
                && control.value().state != crate::control::ControlState::Tombstoned
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
///
/// This runs the same maintenance classes as the module Tick command, including
/// cron firing and queue dead-lettering: an embedding caller that omits the two
/// targets changes what the Tick does, so they stay explicit here instead of
/// defaulting to "no cron, no dead letter".
pub fn scheduler_tick(
    transaction: &Transaction<'_>,
    source: &CellTarget,
    logical_time_ms: i64,
    workflow_definitions: &[&'static dyn WorkflowDefinition],
    queue_dead_letter: Option<QueueDeadLetterTarget>,
    cron_targets: &[CronTarget],
) -> Result<SchedulerTickOutcome> {
    let command_sequence = transaction.query_row(
        "SELECT commit_sequence + 1 FROM sys_meta WHERE singleton = 1 AND commit_sequence < 9223372036854775807",
        [],
        |row| row.get::<_, u64>(0),
    )?;
    scheduler_tick_at(
        transaction,
        source,
        command_sequence,
        logical_time_ms,
        workflow_definitions,
        queue_dead_letter,
        cron_targets,
    )
}

pub(crate) fn scheduler_tick_at(
    transaction: &Transaction<'_>,
    source: &CellTarget,
    command_sequence: u64,
    logical_time_ms: i64,
    workflow_definitions: &[&'static dyn WorkflowDefinition],
    queue_dead_letter: Option<QueueDeadLetterTarget>,
    cron_targets: &[CronTarget],
) -> Result<SchedulerTickOutcome> {
    if logical_time_ms < 0 {
        return Err(Error::Command("negative scheduler logical time"));
    }
    let tables = installed_tables(transaction)?;
    let mut effects = EffectBatch::new(transaction, source, command_sequence, logical_time_ms)?;
    let mut budget = MaintenanceBudget::new(reserved_classes(&tables))?;

    budget.run(|limit| {
        transaction
            .execute(
                "DELETE FROM sys_requests WHERE request_id IN (SELECT request_id FROM sys_requests INDEXED BY sys_requests_expiry WHERE retain_until_ms <= ?1 ORDER BY retain_until_ms, request_id LIMIT ?2)",
                (logical_time_ms, limit as i64),
            )
            .map_err(Into::into)
    })?;
    budget.run(|limit| inbox_cleanup_expired_bounded(transaction, logical_time_ms, limit))?;
    budget.run(|limit| effect_cleanup_terminal_bounded(transaction, logical_time_ms, limit))?;
    budget.run(|limit| effect_expire_ready_bounded(transaction, logical_time_ms, limit))?;
    budget.run(|limit| effect_reclaim_expired_bounded(transaction, logical_time_ms, limit))?;

    if tables.contains(kv::KV_TABLE) {
        budget.run(|limit| kv_cleanup_expired_bounded(transaction, logical_time_ms, limit))?;
    }
    if tables.contains(blob::BLOB_TABLE) {
        budget.run(|limit| blob_cleanup_expired(transaction, logical_time_ms, limit))?;
    }
    if tables.contains(cron::CRON_TABLE) {
        budget.run(|limit| {
            cron_fire_due_bounded(
                transaction,
                &mut effects,
                source,
                logical_time_ms,
                cron_targets,
                limit,
            )
        })?;
    }
    if tables.contains(queue::QUEUE_TABLE) {
        budget.run(|limit| queue_cleanup_expired_bounded(transaction, logical_time_ms, limit))?;
        budget.run(|limit| {
            if let Some(target) = queue_dead_letter {
                let mut dead_letter = QueueDeadLetterWriter::new(target, &mut effects);
                queue_expire_ready_bounded_with_dead_letter(
                    transaction,
                    logical_time_ms,
                    limit,
                    Some(&mut dead_letter),
                )
            } else {
                queue_expire_ready_bounded(transaction, logical_time_ms, limit)
            }
        })?;
        budget.run(|limit| {
            if let Some(target) = queue_dead_letter {
                let mut dead_letter = QueueDeadLetterWriter::new(target, &mut effects);
                queue_reclaim_expired_bounded_with_dead_letter(
                    transaction,
                    logical_time_ms,
                    limit,
                    Some(&mut dead_letter),
                )
            } else {
                queue_reclaim_expired_bounded(transaction, logical_time_ms, limit)
            }
        })?;
    }
    if tables.contains(workflow::WORKFLOW_TABLE) {
        budget
            .run(|limit| workflow_cleanup_terminal_bounded(transaction, logical_time_ms, limit))?;
        budget.run(|limit| {
            repeat(limit, || {
                workflow_fail_one_expired_activity(
                    transaction,
                    &mut effects,
                    source,
                    logical_time_ms,
                    workflow_definitions,
                )
            })
        })?;
        budget
            .run(|limit| workflow_reclaim_expired_bounded(transaction, logical_time_ms, limit))?;
        budget.run(|limit| {
            repeat(limit, || {
                workflow_fire_one_due_timer(
                    transaction,
                    &mut effects,
                    source,
                    logical_time_ms,
                    workflow_definitions,
                )
            })
        })?;
    }

    // Cleanup classes run only once so rows terminalized above remain observable
    // until the next Tick. Other classes can safely consume unused capacity.
    budget.fill(|limit| {
        transaction
            .execute(
                "DELETE FROM sys_requests WHERE request_id IN (SELECT request_id FROM sys_requests INDEXED BY sys_requests_expiry WHERE retain_until_ms <= ?1 ORDER BY retain_until_ms, request_id LIMIT ?2)",
                (logical_time_ms, limit as i64),
            )
            .map_err(Into::into)
    })?;
    budget.fill(|limit| inbox_cleanup_expired_bounded(transaction, logical_time_ms, limit))?;
    budget.fill(|limit| effect_expire_ready_bounded(transaction, logical_time_ms, limit))?;
    budget.fill(|limit| effect_reclaim_expired_bounded(transaction, logical_time_ms, limit))?;
    if tables.contains(kv::KV_TABLE) {
        budget.fill(|limit| kv_cleanup_expired_bounded(transaction, logical_time_ms, limit))?;
    }
    if tables.contains(blob::BLOB_TABLE) {
        budget.fill(|limit| blob_cleanup_expired(transaction, logical_time_ms, limit))?;
    }
    if tables.contains(cron::CRON_TABLE) {
        budget.fill(|limit| {
            cron_fire_due_bounded(
                transaction,
                &mut effects,
                source,
                logical_time_ms,
                cron_targets,
                limit,
            )
        })?;
    }
    if tables.contains(queue::QUEUE_TABLE) {
        budget.fill(|limit| {
            if let Some(target) = queue_dead_letter {
                let mut dead_letter = QueueDeadLetterWriter::new(target, &mut effects);
                queue_expire_ready_bounded_with_dead_letter(
                    transaction,
                    logical_time_ms,
                    limit,
                    Some(&mut dead_letter),
                )
            } else {
                queue_expire_ready_bounded(transaction, logical_time_ms, limit)
            }
        })?;
        budget.fill(|limit| {
            if let Some(target) = queue_dead_letter {
                let mut dead_letter = QueueDeadLetterWriter::new(target, &mut effects);
                queue_reclaim_expired_bounded_with_dead_letter(
                    transaction,
                    logical_time_ms,
                    limit,
                    Some(&mut dead_letter),
                )
            } else {
                queue_reclaim_expired_bounded(transaction, logical_time_ms, limit)
            }
        })?;
    }
    if tables.contains(workflow::WORKFLOW_TABLE) {
        budget.fill(|limit| {
            repeat(limit, || {
                workflow_fail_one_expired_activity(
                    transaction,
                    &mut effects,
                    source,
                    logical_time_ms,
                    workflow_definitions,
                )
            })
        })?;
        budget
            .fill(|limit| workflow_reclaim_expired_bounded(transaction, logical_time_ms, limit))?;
        budget.fill(|limit| {
            repeat(limit, || {
                workflow_fire_one_due_timer(
                    transaction,
                    &mut effects,
                    source,
                    logical_time_ms,
                    workflow_definitions,
                )
            })
        })?;
    }
    budget.finish()?;
    Ok(SchedulerTickOutcome {
        processed: u32::try_from(budget.processed())
            .map_err(|_| Error::Command("scheduler Tick count overflow"))?,
    })
}

struct MaintenanceBudget {
    remaining: usize,
    remaining_classes: usize,
    fair_share: usize,
}

impl MaintenanceBudget {
    fn new(classes: usize) -> Result<Self> {
        if classes == 0 {
            return Err(Error::Command("scheduler has no maintenance classes"));
        }
        Ok(Self {
            remaining: MAX_TICK_ITEMS,
            remaining_classes: classes,
            fair_share: MAX_TICK_ITEMS / classes,
        })
    }

    fn run(&mut self, operation: impl FnOnce(usize) -> Result<usize>) -> Result<()> {
        self.remaining_classes = self
            .remaining_classes
            .checked_sub(1)
            .ok_or(Error::Command("scheduler maintenance class mismatch"))?;
        // Unused work flows forward, but every later class retains one share.
        let reserved = self.fair_share * self.remaining_classes;
        let limit = self.remaining.saturating_sub(reserved);
        let processed = operation(limit)?;
        if processed > limit {
            return Err(Error::Command(
                "scheduler maintenance class exceeded its budget",
            ));
        }
        self.remaining = self
            .remaining
            .checked_sub(processed)
            .ok_or(Error::Command("scheduler Tick exceeded 128 items"))?;
        Ok(())
    }

    fn fill(&mut self, operation: impl FnOnce(usize) -> Result<usize>) -> Result<()> {
        if self.remaining == 0 {
            return Ok(());
        }
        let limit = self.remaining;
        let processed = operation(limit)?;
        if processed > limit {
            return Err(Error::Command(
                "scheduler maintenance class exceeded its budget",
            ));
        }
        self.remaining = self
            .remaining
            .checked_sub(processed)
            .ok_or(Error::Command("scheduler Tick exceeded 128 items"))?;
        Ok(())
    }

    fn processed(&self) -> usize {
        MAX_TICK_ITEMS - self.remaining
    }

    /// Confirms every reserved class ran.
    ///
    /// A section that stops running would otherwise keep its reserved share and
    /// silently lower the Tick's usable work, hiding the missing maintenance.
    fn finish(&self) -> Result<()> {
        if self.remaining_classes != 0 {
            return Err(Error::Command("scheduler maintenance class mismatch"));
        }
        Ok(())
    }
}

fn repeat(limit: usize, mut operation: impl FnMut() -> Result<bool>) -> Result<usize> {
    let mut processed = 0;
    while processed < limit && operation()? {
        processed += 1;
    }
    Ok(processed)
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

    if tables.contains(kv::KV_TABLE) {
        include_minimum(
            transaction,
            "SELECT min(expires_at_ms) FROM kv_entries INDEXED BY kv_expiry WHERE expires_at_ms IS NOT NULL",
            logical_time_ms,
            &mut next,
        )?;
    }
    if tables.contains(queue::QUEUE_TABLE) {
        let exhausted_ready: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM queue_messages INDEXED BY queue_attempts WHERE state = 0 AND attempt >= ?1)",
            [i64::from(MAX_ATTEMPTS)],
            |row| row.get(0),
        )?;
        if exhausted_ready {
            merge_due(logical_time_ms, logical_time_ms, &mut next)?;
        }
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
    if tables.contains(workflow::WORKFLOW_TABLE) {
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
            "SELECT min(completed_at_ms) FROM workflow_runs INDEXED BY workflow_retention WHERE status BETWEEN 1 AND 3 AND completed_at_ms IS NOT NULL",
        )?;
        if let Some(completed_at_ms) = completed {
            let retention = completed_at_ms
                .checked_add(WORKFLOW_RETENTION_MS)
                .ok_or(Error::Command("workflow retention deadline overflow"))?;
            merge_due(retention, logical_time_ms, &mut next)?;
        }
    }
    if tables.contains(blob::BLOB_TABLE) {
        include_minimum(
            transaction,
            "SELECT min(expires_at_ms) FROM blob_uploads INDEXED BY blob_upload_expiry WHERE NOT EXISTS (SELECT 1 FROM blob_objects WHERE blob_objects.upload_id = blob_uploads.upload_id)",
            logical_time_ms,
            &mut next,
        )?;
    }
    if tables.contains(cron::CRON_TABLE) {
        include_minimum(
            transaction,
            "SELECT min(next_due_ms) FROM cron_schedules INDEXED BY cron_due WHERE enabled = 1",
            logical_time_ms,
            &mut next,
        )?;
    }
    Ok(next)
}

/// Optional primitive tables the scheduler maintains, with the Tick classes
/// each one consumes.
///
/// The capability probe, the class budget, and the maintenance guards all read
/// this list, so a renamed table cannot silently drop a primitive's
/// maintenance. A class count must equal the number of `MaintenanceBudget::run`
/// calls its section performs, which `MaintenanceBudget::finish` enforces.
const PRIMITIVE_TABLES: [(&str, usize); 5] = [
    (kv::KV_TABLE, 1),
    (queue::QUEUE_TABLE, 3),
    (workflow::WORKFLOW_TABLE, 4),
    (blob::BLOB_TABLE, 1),
    (cron::CRON_TABLE, 1),
];

/// Probes the optional primitive tables the Cell schema currently installs.
fn installed_tables(transaction: &Transaction<'_>) -> Result<HashSet<&'static str>> {
    let mut statement =
        transaction.prepare("SELECT name FROM sqlite_schema WHERE type = 'table'")?;
    let mut rows = statement.query([])?;
    let mut installed = HashSet::new();
    while let Some(row) = rows.next()? {
        let name: String = row.get(0)?;
        if let Some((table, _)) = PRIMITIVE_TABLES.iter().find(|(table, _)| *table == name) {
            installed.insert(*table);
        }
    }
    Ok(installed)
}

/// Classes one Tick reserves for the installed primitives.
fn reserved_classes(tables: &HashSet<&'static str>) -> usize {
    BASE_MAINTENANCE_CLASSES
        + PRIMITIVE_TABLES
            .iter()
            .filter(|(table, _)| tables.contains(table))
            .map(|(_, classes)| classes)
            .sum::<usize>()
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

#[cfg(test)]
mod tests {
    use crab_ltx::rusqlite::Connection;

    use super::*;

    fn connection() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(include_str!("../migrations/runtime.sql"))
            .unwrap();
        connection
    }

    const PRIMITIVE_SCHEMAS: [&str; 5] = [
        include_str!("../migrations/kv.sql"),
        include_str!("../migrations/queue.sql"),
        include_str!("../migrations/workflow.sql"),
        include_str!("../migrations/blob.sql"),
        include_str!("../migrations/cron.sql"),
    ];

    #[test]
    fn declared_primitive_tables_are_installed_by_their_migrations() {
        let mut seen = HashSet::new();
        for (table, _) in PRIMITIVE_TABLES {
            let declaration = format!("CREATE TABLE {table}");
            assert!(
                seen.insert(table),
                "primitive table {table} is declared twice"
            );
            assert!(
                PRIMITIVE_SCHEMAS
                    .iter()
                    .any(|migration| migration.contains(&declaration)),
                "no migration declares {declaration}"
            );
        }
    }

    #[test]
    fn probe_tracks_every_installed_primitive_schema() {
        let mut partial_connection = connection();
        partial_connection
            .execute_batch(include_str!("../migrations/kv.sql"))
            .unwrap();
        let transaction = partial_connection.transaction().unwrap();
        let partial = installed_tables(&transaction).unwrap();
        drop(transaction);
        assert!(partial.contains(kv::KV_TABLE));
        assert!(!partial.contains(queue::QUEUE_TABLE));
        assert_eq!(reserved_classes(&partial), BASE_MAINTENANCE_CLASSES + 1);

        let mut complete_connection = connection();
        for schema in PRIMITIVE_SCHEMAS {
            complete_connection
                .execute_batch(schema)
                .expect("primitive schema installs");
        }
        let transaction = complete_connection.transaction().unwrap();
        let complete = installed_tables(&transaction).unwrap();
        assert_eq!(complete.len(), PRIMITIVE_TABLES.len());
        for (table, _) in PRIMITIVE_TABLES {
            assert!(complete.contains(table), "{table} is not reported");
        }
        assert_eq!(reserved_classes(&complete), BASE_MAINTENANCE_CLASSES + 10);
    }

    #[test]
    fn budget_finish_rejects_a_reserved_class_that_never_ran() {
        let mut unused = MaintenanceBudget::new(2).unwrap();
        unused.run(|_| Ok(0)).unwrap();
        assert!(unused.finish().is_err());

        let mut exhausted = MaintenanceBudget::new(1).unwrap();
        exhausted.run(|_| Ok(0)).unwrap();
        exhausted.finish().unwrap();
    }
}
