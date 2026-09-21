use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::effects::EffectBatch;
use crate::{
    BoundedEncoder, CellTarget, EffectCommandIntent, Error, NamespaceId, Result, WireValue,
};

mod api;

pub use api::{CronCommand, CronModule, CronNamespace, CronQueryCommand, register_cron};

const CRON_SCHEMA: &str = include_str!("migrations/cron.sql");
const MIN_INTERVAL_MS: u64 = 1_000;
const MAX_INTERVAL_MS: u64 = 365 * 24 * 60 * 60 * 1_000;
const MAX_PAYLOAD_BYTES: usize = 256 * 1_024;
const MAX_FUTURE_MS: i64 = 5 * 365 * 24 * 60 * 60 * 1_000;
const EFFECT_LIFETIME_MS: i64 = 7 * 24 * 60 * 60 * 1_000;

/// Compile-time destination contract for Cron invocations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CronTarget {
    module: &'static str,
    namespace: NamespaceId,
    command_id: u32,
    codec_version: u32,
    input_limit: u32,
}

impl CronTarget {
    #[must_use]
    pub const fn new(
        module: &'static str,
        namespace: NamespaceId,
        command_id: u32,
        codec_version: u32,
        input_limit: u32,
    ) -> Self {
        Self {
            module,
            namespace,
            command_id,
            codec_version,
            input_limit,
        }
    }

    pub(crate) const fn module(self) -> &'static str {
        self.module
    }
    pub(crate) const fn namespace(self) -> NamespaceId {
        self.namespace
    }
    pub(crate) const fn command_id(self) -> u32 {
        self.command_id
    }
    pub(crate) const fn codec_version(self) -> u32 {
        self.codec_version
    }
    pub(crate) const fn input_limit(self) -> u32 {
        self.input_limit
    }
}

/// Payload delivered exactly once to a destination inbox for one Cron occurrence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronInvocation {
    pub schedule_id: [u8; 16],
    pub generation: u64,
    pub occurrence: u64,
    pub scheduled_at_ms: i64,
    pub payload: Vec<u8>,
}

/// Durable Cron schedule mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CronMutation {
    Upsert {
        schedule_id: [u8; 16],
        target_index: u32,
        target_partition: Vec<u8>,
        payload: Vec<u8>,
        interval_ms: u64,
        next_due_ms: i64,
    },
    Pause {
        schedule_id: [u8; 16],
    },
    Resume {
        schedule_id: [u8; 16],
        next_due_ms: i64,
    },
    Delete {
        schedule_id: [u8; 16],
    },
}

/// Result of one Cron schedule mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CronMutationOutcome {
    Applied { generation: u64 },
    Deleted,
    NotFound,
}

/// Materialized Cron schedule state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronSchedule {
    pub schedule_id: [u8; 16],
    pub target_index: u32,
    pub target_partition: Vec<u8>,
    pub payload: Vec<u8>,
    pub interval_ms: u64,
    pub next_due_ms: i64,
    pub occurrence: u64,
    pub enabled: bool,
    pub generation: u64,
}

/// Bounded Cron schedule query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CronQuery {
    Get { schedule_id: [u8; 16] },
    List { after: Option<[u8; 16]>, limit: u32 },
}

/// Result of a Cron schedule query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CronQueryResult {
    Get(Option<CronSchedule>),
    List {
        schedules: Vec<CronSchedule>,
        next: Option<[u8; 16]>,
    },
}

/// Installs the exact version-one Cron schema.
pub fn install_cron_schema(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(CRON_SCHEMA)?;
    Ok(())
}

/// Applies one durable Cron schedule mutation.
pub fn cron_mutate(
    transaction: &Transaction<'_>,
    now_ms: i64,
    issued_at_ms: i64,
    targets: &[CronTarget],
    mutation: &CronMutation,
) -> Result<CronMutationOutcome> {
    validate_now(now_ms)?;
    validate_now(issued_at_ms)?;
    match mutation {
        CronMutation::Upsert {
            schedule_id,
            target_index,
            target_partition,
            payload,
            interval_ms,
            next_due_ms,
        } => {
            let target = target_at(targets, *target_index)?;
            validate_schedule(
                issued_at_ms,
                target,
                target_partition,
                payload,
                *interval_ms,
                *next_due_ms,
            )?;
            transaction.execute(
                "INSERT INTO cron_schedules(schedule_id, target_index, target_partition, payload, interval_ms, next_due_ms, occurrence, enabled, generation, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 1, 1, ?7) ON CONFLICT(schedule_id) DO UPDATE SET target_index = excluded.target_index, target_partition = excluded.target_partition, payload = excluded.payload, interval_ms = excluded.interval_ms, next_due_ms = excluded.next_due_ms, occurrence = 0, enabled = 1, generation = cron_schedules.generation + 1, updated_at_ms = excluded.updated_at_ms",
                (schedule_id.as_slice(), i64::from(*target_index), target_partition, payload, i64::try_from(*interval_ms).map_err(|_| Error::Command("cron interval overflow"))?, *next_due_ms, now_ms),
            )?;
            Ok(CronMutationOutcome::Applied {
                generation: schedule_generation(transaction, *schedule_id)?,
            })
        }
        CronMutation::Pause { schedule_id } => {
            set_enabled(transaction, now_ms, *schedule_id, false, None)
        }
        CronMutation::Resume {
            schedule_id,
            next_due_ms,
        } => {
            validate_due(issued_at_ms, *next_due_ms)?;
            set_enabled(transaction, now_ms, *schedule_id, true, Some(*next_due_ms))
        }
        CronMutation::Delete { schedule_id } => {
            let changed = transaction.execute(
                "DELETE FROM cron_schedules WHERE schedule_id = ?1",
                [schedule_id.as_slice()],
            )?;
            Ok(if changed == 1 {
                CronMutationOutcome::Deleted
            } else {
                CronMutationOutcome::NotFound
            })
        }
    }
}

/// Reads one or one bounded page of Cron schedules.
pub fn cron_query(connection: &Connection, query: &CronQuery) -> Result<CronQueryResult> {
    match query {
        CronQuery::Get { schedule_id } => Ok(CronQueryResult::Get(load_schedule(
            connection,
            *schedule_id,
        )?)),
        CronQuery::List { after, limit } => {
            if !(1..=128).contains(limit) {
                return Err(Error::Command("cron list limit must be in 1..=128"));
            }
            let after = after.unwrap_or([0; 16]);
            let mut statement = connection.prepare(
                "SELECT schedule_id, target_index, target_partition, payload, interval_ms, next_due_ms, occurrence, enabled, generation FROM cron_schedules WHERE schedule_id > ?1 ORDER BY schedule_id LIMIT ?2",
            )?;
            let rows =
                statement.query_map((after.as_slice(), i64::from(*limit) + 1), decode_schedule)?;
            let mut schedules = Vec::with_capacity(*limit as usize);
            let mut bytes = 0_usize;
            let mut truncated = false;
            for row in rows {
                let schedule = row?;
                let schedule_bytes = schedule
                    .target_partition
                    .len()
                    .checked_add(schedule.payload.len())
                    .and_then(|value| value.checked_add(96))
                    .ok_or(Error::Command("cron list byte count overflow"))?;
                if schedules.len() == *limit as usize
                    || bytes.saturating_add(schedule_bytes) > 512 * 1024
                {
                    truncated = true;
                    break;
                }
                bytes += schedule_bytes;
                schedules.push(schedule);
            }
            let next = truncated
                .then(|| schedules.last().map(|value| value.schedule_id))
                .flatten();
            Ok(CronQueryResult::List { schedules, next })
        }
    }
}

pub(crate) fn cron_fire_due_bounded(
    transaction: &Transaction<'_>,
    effects: &mut EffectBatch,
    source: &CellTarget,
    now_ms: i64,
    targets: &[CronTarget],
    limit: usize,
) -> Result<usize> {
    let mut processed = 0;
    while processed < limit {
        let row = transaction
            .query_row(
                "SELECT schedule_id, target_index, target_partition, payload, interval_ms, next_due_ms, occurrence, generation FROM cron_schedules INDEXED BY cron_due WHERE enabled = 1 AND next_due_ms <= ?1 ORDER BY next_due_ms, schedule_id LIMIT 1",
                [now_ms],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?, row.get::<_, Vec<u8>>(2)?, row.get::<_, Vec<u8>>(3)?, row.get::<_, i64>(4)?, row.get::<_, i64>(5)?, row.get::<_, i64>(6)?, row.get::<_, i64>(7)?)),
            )
            .optional()?;
        let Some((
            schedule_id,
            target_index,
            partition,
            payload,
            interval_ms,
            scheduled_at_ms,
            occurrence,
            generation,
        )) = row
        else {
            break;
        };
        let schedule_id: [u8; 16] = schedule_id
            .try_into()
            .map_err(|_| Error::Command("invalid stored cron schedule ID"))?;
        let target_index = u32::try_from(target_index)
            .map_err(|_| Error::Command("invalid stored cron target index"))?;
        let target = target_at(targets, target_index)?;
        let occurrence = u64::try_from(occurrence)
            .map_err(|_| Error::Command("invalid stored cron occurrence"))?;
        let generation = u64::try_from(generation)
            .map_err(|_| Error::Command("invalid stored cron generation"))?;
        let next_occurrence = occurrence
            .checked_add(1)
            .ok_or(Error::Command("cron occurrence overflow"))?;
        let invocation = CronInvocation {
            schedule_id,
            generation,
            occurrence: next_occurrence,
            scheduled_at_ms,
            payload,
        };
        let mut encoder = BoundedEncoder::new(target.input_limit())?;
        invocation.encode(&mut encoder)?;
        let destination = CellTarget::new(
            source.tenant(),
            source.application(),
            target.namespace(),
            &partition,
        )?;
        effects.insert_command(
            transaction,
            &EffectCommandIntent {
                target: destination,
                command_id: target.command_id(),
                codec_version: target.codec_version(),
                input: encoder.finish(),
                expires_at_ms: now_ms
                    .checked_add(EFFECT_LIFETIME_MS)
                    .ok_or(Error::Command("cron effect expiry overflow"))?,
            },
        )?;
        let next_due_ms = scheduled_at_ms
            .checked_add(interval_ms)
            .ok_or(Error::Command("cron due time overflow"))?;
        if transaction.execute(
            "UPDATE cron_schedules SET next_due_ms = ?1, occurrence = ?2 WHERE schedule_id = ?3 AND enabled = 1 AND next_due_ms = ?4 AND occurrence = ?5 AND generation = ?6",
            (next_due_ms, i64::try_from(next_occurrence).map_err(|_| Error::Command("cron occurrence overflow"))?, schedule_id.as_slice(), scheduled_at_ms, i64::try_from(occurrence).map_err(|_| Error::Command("cron occurrence overflow"))?, i64::try_from(generation).map_err(|_| Error::Command("cron generation overflow"))?),
        )? != 1 { return Err(Error::Command("cron schedule changed during serialized fire")); }
        processed += 1;
    }
    Ok(processed)
}

fn set_enabled(
    transaction: &Transaction<'_>,
    now_ms: i64,
    schedule_id: [u8; 16],
    enabled: bool,
    next_due_ms: Option<i64>,
) -> Result<CronMutationOutcome> {
    let enabled_value = if enabled { 1_i64 } else { 0_i64 };
    let changed = transaction.execute(
        "UPDATE cron_schedules SET enabled = ?1, next_due_ms = coalesce(?2, next_due_ms), generation = generation + CASE WHEN enabled = ?1 AND (?2 IS NULL OR next_due_ms = ?2) THEN 0 ELSE 1 END, updated_at_ms = ?3 WHERE schedule_id = ?4",
        (enabled_value, next_due_ms, now_ms, schedule_id.as_slice()),
    )?;
    if changed == 0 {
        return Ok(CronMutationOutcome::NotFound);
    }
    Ok(CronMutationOutcome::Applied {
        generation: schedule_generation(transaction, schedule_id)?,
    })
}

fn schedule_generation(transaction: &Transaction<'_>, schedule_id: [u8; 16]) -> Result<u64> {
    let value = transaction.query_row(
        "SELECT generation FROM cron_schedules WHERE schedule_id = ?1",
        [schedule_id.as_slice()],
        |row| row.get::<_, i64>(0),
    )?;
    u64::try_from(value).map_err(|_| Error::Command("invalid stored cron generation"))
}

fn load_schedule(connection: &Connection, schedule_id: [u8; 16]) -> Result<Option<CronSchedule>> {
    connection.query_row(
        "SELECT schedule_id, target_index, target_partition, payload, interval_ms, next_due_ms, occurrence, enabled, generation FROM cron_schedules WHERE schedule_id = ?1",
        [schedule_id.as_slice()], decode_schedule,
    ).optional().map_err(Into::into)
}

fn decode_schedule(row: &rusqlite::Row<'_>) -> rusqlite::Result<CronSchedule> {
    let schedule_id = row
        .get::<_, Vec<u8>>(0)?
        .try_into()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    Ok(CronSchedule {
        schedule_id,
        target_index: u32::try_from(row.get::<_, i64>(1)?)
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        target_partition: row.get(2)?,
        payload: row.get(3)?,
        interval_ms: u64::try_from(row.get::<_, i64>(4)?)
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        next_due_ms: row.get(5)?,
        occurrence: u64::try_from(row.get::<_, i64>(6)?)
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        enabled: row.get::<_, i64>(7)? != 0,
        generation: u64::try_from(row.get::<_, i64>(8)?)
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
    })
}

fn validate_schedule(
    issued_at_ms: i64,
    target: CronTarget,
    partition: &[u8],
    payload: &[u8],
    interval_ms: u64,
    next_due_ms: i64,
) -> Result<()> {
    if partition.len() > 1_024 || payload.len() > MAX_PAYLOAD_BYTES {
        return Err(Error::Command("cron target or payload exceeds limits"));
    }
    if !(MIN_INTERVAL_MS..=MAX_INTERVAL_MS).contains(&interval_ms) {
        return Err(Error::Command(
            "cron interval must be between one second and one year",
        ));
    }
    validate_due(issued_at_ms, next_due_ms)?;
    let wrapper_bytes = payload
        .len()
        .checked_add(64)
        .ok_or(Error::Command("cron invocation size overflow"))?;
    if wrapper_bytes > target.input_limit() as usize {
        return Err(Error::Command("cron invocation exceeds target input limit"));
    }
    Ok(())
}

fn validate_due(issued_at_ms: i64, due_ms: i64) -> Result<()> {
    if due_ms < issued_at_ms || due_ms > issued_at_ms.saturating_add(MAX_FUTURE_MS) {
        return Err(Error::Command("cron due time is outside five-year window"));
    }
    Ok(())
}

fn target_at(targets: &[CronTarget], index: u32) -> Result<CronTarget> {
    targets
        .get(index as usize)
        .copied()
        .ok_or(Error::Command("cron target index is unavailable"))
}

fn validate_now(now_ms: i64) -> Result<()> {
    if now_ms < 0 {
        return Err(Error::Command("negative cron logical time"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ApplicationId, IncarnationId, TenantId};
    use crab_ltx::rusqlite::Connection;

    const SOURCE_NAMESPACE: NamespaceId = NamespaceId::from_bytes([1; 16]);
    const TARGET_NAMESPACE: NamespaceId = NamespaceId::from_bytes([2; 16]);
    const TARGETS: &[CronTarget] = &[CronTarget::new(
        "cron-destination",
        TARGET_NAMESPACE,
        9,
        1,
        1024,
    )];

    #[test]
    fn due_occurrence_and_advance_are_one_transaction() {
        let mut connection = Connection::open_in_memory().unwrap();
        let transaction = connection.transaction().unwrap();
        let source = CellTarget::new(
            TenantId::from_bytes([3; 16]),
            ApplicationId::from_bytes([4; 16]),
            SOURCE_NAMESPACE,
            b"cron-shard",
        )
        .unwrap();
        crate::schema::install_runtime_schema_in(
            &transaction,
            source.cell_id(),
            IncarnationId::from_bytes([5; 16]),
            1,
        )
        .unwrap();
        install_cron_schema(&transaction).unwrap();
        cron_mutate(
            &transaction,
            200,
            10,
            TARGETS,
            &CronMutation::Upsert {
                schedule_id: [6; 16],
                target_index: 0,
                target_partition: b"destination".to_vec(),
                payload: b"compact".to_vec(),
                interval_ms: 1_000,
                next_due_ms: 100,
            },
        )
        .unwrap();
        let mut effects = EffectBatch::new(&transaction, &source, 1, 200).unwrap();
        assert_eq!(
            cron_fire_due_bounded(&transaction, &mut effects, &source, 200, TARGETS, 8).unwrap(),
            1
        );
        assert_eq!(
            transaction
                .query_row("SELECT count(*) FROM sys_effects", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            1
        );
        let schedule = load_schedule(&transaction, [6; 16]).unwrap().unwrap();
        assert_eq!(schedule.occurrence, 1);
        assert_eq!(schedule.next_due_ms, 1_100);
        let CronQueryResult::List { schedules, next } = cron_query(
            &transaction,
            &CronQuery::List {
                after: None,
                limit: 10,
            },
        )
        .unwrap() else {
            panic!("cron list was not returned");
        };
        assert_eq!(schedules.len(), 1);
        assert_eq!(next, None);
        assert_eq!(
            cron_fire_due_bounded(&transaction, &mut effects, &source, 200, TARGETS, 8).unwrap(),
            0
        );
    }

    #[test]
    fn checked_in_cron_schema_matches_runtime_schema() {
        assert_eq!(CRON_SCHEMA, include_str!("../docs/contracts/cron.sql"));
    }
}
