//! Durable initial placement of a table's independently owned data ranges.

use std::collections::HashSet;

use crab_cell_runtime::registry::{Command, CommandContext, CommandResult, Query, QueryContext};
use serde::{Deserialize, Serialize};

use crate::table::{TableRecord, decode_table, statement};
use crate::{Json, MODULE, PartitionSpec, Result, SqlBatch, SqlResultSet, SqlValue};

mod split_state;

use split_state::split_route_state;
pub use split_state::{
    PublishedPartitionInput, PublishedPartitionOutcome, ReadPublishedPartition, ReadSplitRoute,
    SplitRouteState,
};

/// One published set of contiguous data Cell ranges for a table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TableRoute {
    /// Immutable table identity.
    pub table_id: String,
    /// Directory version; each data Cell retains its own partition epoch.
    pub epoch: u64,
    /// Ranges ordered by lower hash bound, covering the full hash space.
    pub partitions: Vec<PartitionSpec>,
}

impl TableRoute {
    pub(crate) fn valid_for(&self, table: &TableRecord) -> bool {
        if self.table_id != table.id || self.epoch == 0 || self.partitions.is_empty() {
            return false;
        }
        let mut next_lower = None;
        let mut ids = HashSet::new();
        let route_table = &self.partitions[0].table;
        for (index, partition) in self.partitions.iter().enumerate() {
            if partition.table.id != table.id
                || partition.table != *route_table
                || partition.table.key_schema != table.key_schema
                || partition.table.attribute_definitions != table.attribute_definitions
                || partition.epoch == 0
                || partition.epoch > self.epoch
                || partition.lower != next_lower
                || partition
                    .lower
                    .zip(partition.upper)
                    .is_some_and(|(low, high)| low >= high)
                || !ids.insert(partition.partition_id)
                || (index + 1 < self.partitions.len() && partition.upper.is_none())
            {
                return false;
            }
            next_lower = partition.upper;
        }
        next_lower.is_none()
    }
}

/// A durable, validated intent to replace one source range with two children.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SplitPlan {
    /// Existing data Cell to seal and copy.
    pub source: PartitionSpec,
    /// New adjacent ranges that replace the source.
    pub children: [PartitionSpec; 2],
    /// Published directory epoch that this plan is allowed to replace.
    pub expected_epoch: u64,
}

impl SplitPlan {
    pub(crate) fn next_epoch(&self) -> Option<u64> {
        self.expected_epoch.checked_add(1)
    }

    pub(crate) fn valid_for(&self, table: &TableRecord) -> bool {
        let [left, right] = &self.children;
        let Some(next_epoch) = self.next_epoch() else {
            return false;
        };
        let Some(boundary) = left.upper else {
            return false;
        };
        self.expected_epoch > 0
            && self.source.table.id == table.id
            && self.source.table.key_schema == table.key_schema
            && self.source.table.attribute_definitions == table.attribute_definitions
            && self.source.epoch > 0
            && self.source.epoch <= self.expected_epoch
            && self
                .source
                .lower
                .zip(self.source.upper)
                .is_none_or(|(low, high)| low < high)
            && left.table == self.source.table
            && right.table == self.source.table
            && left.partition_id != right.partition_id
            && left.partition_id != self.source.partition_id
            && right.partition_id != self.source.partition_id
            && left.epoch == next_epoch
            && right.epoch == next_epoch
            && left.lower == self.source.lower
            && right.lower == Some(boundary)
            && right.upper == self.source.upper
            && boundary > self.source.lower.unwrap_or([0; 16])
            && self.source.upper.is_none_or(|upper| boundary < upper)
    }
}

/// Outcome of recording one split plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum BeginSplitOutcome {
    /// This exact plan is durable.
    Planned,
    /// The table does not exist.
    TableNotFound,
    /// The table has no active route.
    RouteNotFound,
    /// The proposed route is not a valid one-range split.
    InvalidPlan,
    /// Another split plan is already durable for this table.
    Conflict,
}

/// Record a single-range split before provisioning or copying its children.
pub struct BeginSplit;

impl Command for BeginSplit {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 11;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<SplitPlan>;
    type Output = Json<BeginSplitOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(plan): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let table_id = &plan.source.table.id;
        let table_rows = context.sql(&statement(
            "SELECT record FROM ddb_tables WHERE table_id = ?1",
            vec![SqlValue::Text(table_id.clone())],
        ))?;
        let Some(table) = decode_table(&table_rows[0])? else {
            return Ok(CommandResult::Rejected(Json(
                BeginSplitOutcome::TableNotFound,
            )));
        };
        let state = split_route_state(&plan, |batch| context.sql(batch))?;
        if state == SplitRouteState::Unrouted {
            return Ok(CommandResult::Rejected(Json(
                BeginSplitOutcome::RouteNotFound,
            )));
        }
        if !plan.valid_for(&table) || state != SplitRouteState::Before {
            return Ok(CommandResult::Rejected(Json(
                BeginSplitOutcome::InvalidPlan,
            )));
        }
        let existing_rows = context.sql(&statement(
            "SELECT plan FROM ddb_split_plans WHERE table_id = ?1",
            vec![SqlValue::Text(table_id.clone())],
        ))?;
        if let Some(existing) = decode_plan(&existing_rows[0])? {
            return Ok(if existing == plan {
                CommandResult::Success(Json(BeginSplitOutcome::Planned))
            } else {
                CommandResult::Rejected(Json(BeginSplitOutcome::Conflict))
            });
        }
        context.sql(&statement(
            "INSERT INTO ddb_split_plans (table_id, plan) VALUES (?1, ?2)",
            vec![
                SqlValue::Text(table_id.clone()),
                SqlValue::Blob(serde_json::to_vec(&plan)?),
            ],
        ))?;
        Ok(CommandResult::Success(Json(BeginSplitOutcome::Planned)))
    }
}

/// Outcome of atomically replacing a published route with its planned split.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CommitSplitOutcome {
    /// The planned route is published and the plan has been consumed.
    Committed,
    /// The table or its published route no longer exists.
    RouteNotFound,
    /// No matching split plan is durable for this table.
    PlanNotFound,
    /// The durable plan differs from the submitted plan.
    PlanMismatch,
    /// The published route changed since the split was planned.
    RouteChanged,
}

/// Publish a split after a trusted coordinator verifies both activated children.
///
/// This account-local compare-and-swap does not inspect other Cells. The caller
/// must verify the sealed source and both child activations before invoking it.
pub struct CommitSplit;

impl Command for CommitSplit {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 12;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<SplitPlan>;
    type Output = Json<CommitSplitOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(plan): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let table_id = &plan.source.table.id;
        let table_rows = context.sql(&statement(
            "SELECT record FROM ddb_tables WHERE table_id = ?1",
            vec![SqlValue::Text(table_id.clone())],
        ))?;
        if decode_table(&table_rows[0])?.is_none() {
            return Ok(CommandResult::Rejected(Json(
                CommitSplitOutcome::RouteNotFound,
            )));
        }
        let state = split_route_state(&plan, |batch| context.sql(batch))?;
        if state == SplitRouteState::Unrouted {
            return Ok(CommandResult::Rejected(Json(
                CommitSplitOutcome::RouteNotFound,
            )));
        }
        let plan_rows = context.sql(&statement(
            "SELECT plan FROM ddb_split_plans WHERE table_id = ?1",
            vec![SqlValue::Text(table_id.clone())],
        ))?;
        let Some(durable) = decode_plan(&plan_rows[0])? else {
            return Ok(if state == SplitRouteState::After {
                CommandResult::Success(Json(CommitSplitOutcome::Committed))
            } else {
                CommandResult::Rejected(Json(CommitSplitOutcome::PlanNotFound))
            });
        };
        if durable != plan {
            return Ok(CommandResult::Rejected(Json(
                CommitSplitOutcome::PlanMismatch,
            )));
        }
        if state != SplitRouteState::Before {
            return Ok(CommandResult::Rejected(Json(
                CommitSplitOutcome::RouteChanged,
            )));
        }
        // The epoch and indexed rows must switch in one Cell transaction.
        context.sql(&statement(
            "UPDATE ddb_routes SET route_epoch = ?2 WHERE table_id = ?1",
            vec![
                SqlValue::Text(table_id.clone()),
                SqlValue::Text(
                    plan.next_epoch()
                        .ok_or(crate::Error::Command("split epoch exhausted"))?
                        .to_string(),
                ),
            ],
        ))?;
        let removed = context.sql(&statement(
            "DELETE FROM ddb_route_partitions WHERE table_id = ?1 AND partition_id = ?2",
            vec![
                SqlValue::Text(table_id.clone()),
                SqlValue::Blob(plan.source.partition_id.to_vec()),
            ],
        ))?;
        if removed[0].rows_affected != 1 {
            return Err(crate::Error::Command("split source route row is missing"));
        }
        for partition in &plan.children {
            insert_route_partition(context, partition)?;
        }
        context.sql(&statement(
            "DELETE FROM ddb_split_plans WHERE table_id = ?1",
            vec![SqlValue::Text(table_id.clone())],
        ))?;
        Ok(CommandResult::Success(Json(CommitSplitOutcome::Committed)))
    }
}

/// Read a durable split plan for recovery after owner loss.
pub struct ReadSplitPlan;

impl Query for ReadSplitPlan {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 12;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<String>;
    type Output = Json<Option<SplitPlan>>;

    fn execute(
        context: &mut QueryContext<'_>,
        Json(table_id): Self::Input,
    ) -> Result<Self::Output> {
        Ok(Json(decode_plan(
            &context.sql(&statement(
                "SELECT plan FROM ddb_split_plans WHERE table_id = ?1",
                vec![SqlValue::Text(table_id)],
            ))?[0],
        )?))
    }
}

fn decode_plan(rows: &SqlResultSet) -> Result<Option<SplitPlan>> {
    let Some(row) = rows.rows.first() else {
        return Ok(None);
    };
    let [SqlValue::Blob(bytes)] = row.as_slice() else {
        return Err(crate::Error::Command("invalid split plan row"));
    };
    Ok(Some(serde_json::from_slice(bytes)?))
}

/// Outcome of publishing initial table placement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ActivateTableRouteOutcome {
    /// Prepared account transactions still own keys in the table.
    TransactionConflict,
    /// This exact route is now durable.
    Activated,
    /// The table no longer exists in the account Cell.
    TableNotFound,
    /// A different route already owns the table.
    AlreadyActive,
    /// Account-local items must be migrated before partition activation.
    TableNotEmpty,
    /// The ranges are invalid or do not cover the table's key space.
    InvalidRoute,
}

/// Publish an initial route after its data Cells have been provisioned.
pub struct ActivateTableRoute;

impl Command for ActivateTableRoute {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 10;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<TableRoute>;
    type Output = Json<ActivateTableRouteOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(route): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let table_rows = context.sql(&statement(
            "SELECT record FROM ddb_tables WHERE table_id = ?1",
            vec![SqlValue::Text(route.table_id.clone())],
        ))?;
        let Some(table) = decode_table(&table_rows[0])? else {
            return Ok(CommandResult::Rejected(Json(
                ActivateTableRouteOutcome::TableNotFound,
            )));
        };
        if !route.valid_for(&table) {
            return Ok(CommandResult::Rejected(Json(
                ActivateTableRouteOutcome::InvalidRoute,
            )));
        }
        // A prepared create has no live row yet, but its destination is fixed.
        if crate::items::transaction::table_locked(context, &table.id)? {
            return Ok(CommandResult::Rejected(Json(
                ActivateTableRouteOutcome::TransactionConflict,
            )));
        }
        let account_items = context.sql(&statement(
            "SELECT 1 FROM ddb_items WHERE table_id = ?1 LIMIT 1",
            vec![SqlValue::Text(route.table_id.clone())],
        ))?;
        if !account_items[0].rows.is_empty() {
            return Ok(CommandResult::Rejected(Json(
                ActivateTableRouteOutcome::TableNotEmpty,
            )));
        }
        if let Some(current) = load_route(&route.table_id, |batch| context.sql(batch))? {
            let outcome = if current == route {
                ActivateTableRouteOutcome::Activated
            } else {
                ActivateTableRouteOutcome::AlreadyActive
            };
            return Ok(if outcome == ActivateTableRouteOutcome::Activated {
                CommandResult::Success(Json(outcome))
            } else {
                CommandResult::Rejected(Json(outcome))
            });
        }
        // The table snapshot and indexed ranges must become visible in one Cell commit.
        context.sql(&statement(
            "INSERT INTO ddb_routes (table_id, route_epoch, route_table) VALUES (?1, ?2, ?3)",
            vec![
                SqlValue::Text(route.table_id.clone()),
                SqlValue::Text(route.epoch.to_string()),
                SqlValue::Blob(serde_json::to_vec(&route.partitions[0].table)?),
            ],
        ))?;
        for partition in &route.partitions {
            insert_route_partition(context, partition)?;
        }
        Ok(CommandResult::Success(Json(
            ActivateTableRouteOutcome::Activated,
        )))
    }
}

/// Read the published route for a table, if activated.
pub struct ReadTableRoute;

impl Query for ReadTableRoute {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 11;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<String>;
    type Output = Json<Option<TableRoute>>;

    fn execute(
        context: &mut QueryContext<'_>,
        Json(table_id): Self::Input,
    ) -> Result<Self::Output> {
        Ok(Json(load_route(&table_id, |batch| context.sql(batch))?))
    }
}

fn load_route(
    table_id: &str,
    sql: impl Fn(&SqlBatch) -> Result<Vec<SqlResultSet>>,
) -> Result<Option<TableRoute>> {
    let route_rows = sql(&statement(
        "SELECT route_epoch, route_table FROM ddb_routes WHERE table_id = ?1",
        vec![SqlValue::Text(table_id.to_owned())],
    ))?;
    let route_rows = &route_rows[0];
    let Some(row) = route_rows.rows.first() else {
        return Ok(None);
    };
    let [SqlValue::Text(epoch), SqlValue::Blob(table)] = row.as_slice() else {
        return Err(crate::Error::Command("invalid table route row"));
    };
    let table: TableRecord = serde_json::from_slice(table)?;
    if table.id != table_id {
        return Err(crate::Error::Command("invalid table route state"));
    }
    let mut partitions = Vec::new();
    let mut after_lower: Option<[u8; 16]> = None;
    loop {
        let page = match after_lower {
            Some(lower) => statement(
                "SELECT partition_id, lower_bound, upper_bound, epoch \
                 FROM ddb_route_partitions WHERE table_id = ?1 AND lower_bound > ?2 \
                 ORDER BY lower_bound LIMIT 256",
                vec![
                    SqlValue::Text(table_id.to_owned()),
                    SqlValue::Blob(lower.to_vec()),
                ],
            ),
            None => statement(
                "SELECT partition_id, lower_bound, upper_bound, epoch \
                 FROM ddb_route_partitions WHERE table_id = ?1 \
                 ORDER BY lower_bound LIMIT 256",
                vec![SqlValue::Text(table_id.to_owned())],
            ),
        };
        let rows = sql(&page)?;
        for row in &rows[0].rows {
            let range = decode_page_partition(row)?;
            partitions.push(PartitionSpec {
                table: table.clone(),
                partition_id: range.partition_id,
                lower: (range.lower != [0; 16]).then_some(range.lower),
                upper: range.upper,
                epoch: range.epoch,
            });
            after_lower = Some(range.lower);
        }
        if rows[0].rows.len() < 256 {
            break;
        }
    }
    let route = TableRoute {
        table_id: table_id.to_owned(),
        epoch: parse_epoch(epoch)?,
        partitions,
    };
    if !route.valid_for(&table) {
        return Err(crate::Error::Command("invalid table route state"));
    }
    Ok(Some(route))
}

fn insert_route_partition(
    context: &mut CommandContext<'_, '_>,
    partition: &PartitionSpec,
) -> Result<()> {
    context.sql(&statement(
        "INSERT INTO ddb_route_partitions \
         (table_id, partition_id, lower_bound, upper_bound, epoch) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        vec![
            SqlValue::Text(partition.table.id.clone()),
            SqlValue::Blob(partition.partition_id.to_vec()),
            SqlValue::Blob(partition.lower.unwrap_or([0; 16]).to_vec()),
            // The 17-byte sentinel sorts after every 16-byte key hash.
            SqlValue::Blob(
                partition
                    .upper
                    .map_or_else(|| vec![0xff; 17], |bound| bound.to_vec()),
            ),
            SqlValue::Text(partition.epoch.to_string()),
        ],
    ))?;
    Ok(())
}

/// One hash key's published owner without materializing the full table route.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartitionLookupInput {
    /// Immutable table identity.
    pub table_id: String,
    /// Hash of the canonical partition key.
    pub hash: [u8; 16],
}

/// Result of a point lookup in the durable route directory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PartitionLookupOutcome {
    /// No initial route has been published.
    Unrouted,
    /// The published route maps this hash to one partition.
    Routed { partition_id: [u8; 16], epoch: u64 },
}

/// Resolve one partition key through the indexed account directory.
pub struct ReadPartitionRoute;

impl Query for ReadPartitionRoute {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 14;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PartitionLookupInput>;
    type Output = Json<PartitionLookupOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        let rows = context.sql(&statement(
            "SELECT p.partition_id, p.epoch FROM ddb_routes r \
             LEFT JOIN ddb_route_partitions p ON p.table_id = r.table_id \
             AND p.lower_bound <= ?2 AND ?2 < p.upper_bound \
             WHERE r.table_id = ?1 ORDER BY p.lower_bound DESC LIMIT 2",
            vec![
                SqlValue::Text(input.table_id),
                SqlValue::Blob(input.hash.to_vec()),
            ],
        ))?;
        let rows = &rows[0].rows;
        match rows.as_slice() {
            [] => Ok(Json(PartitionLookupOutcome::Unrouted)),
            [row] => match row.as_slice() {
                [SqlValue::Blob(id), SqlValue::Text(epoch)] => {
                    let partition_id: [u8; 16] = id
                        .as_slice()
                        .try_into()
                        .map_err(|_| crate::Error::Command("invalid route partition ID"))?;
                    Ok(Json(PartitionLookupOutcome::Routed {
                        partition_id,
                        epoch: parse_epoch(epoch)?,
                    }))
                }
                _ => Err(crate::Error::Command(
                    "published route has no owner for hash",
                )),
            },
            _ => Err(crate::Error::Command(
                "published route has overlapping owners",
            )),
        }
    }
}

/// Bounded directory request used by a routed Scan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoutePageInput {
    /// Immutable table identity.
    pub table_id: String,
    /// First page starts at the partition containing this hash, if present.
    pub start_hash: Option<[u8; 16]>,
    /// Later pages start after this range lower bound.
    pub after_lower: Option<[u8; 16]>,
    /// Route epoch pinned by the first page of this request.
    pub expected_epoch: Option<u64>,
}

/// One owner range required to scan a table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoutePagePartition {
    /// Installed data Cell identity.
    pub partition_id: [u8; 16],
    /// Inclusive lower hash boundary; zero represents the first range.
    pub lower: [u8; 16],
    /// Exclusive upper hash boundary, or the end of the hash space.
    pub upper: Option<[u8; 16]>,
    /// Installed data Cell epoch.
    pub epoch: u64,
}

/// Bounded scan of a published route.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RoutePageOutcome {
    /// The table has no published route.
    Unrouted,
    /// A split changed the route during this Scan request.
    Changed,
    /// One ordered route page and whether another directory page remains.
    Page {
        /// Route epoch pinned within this request.
        epoch: u64,
        /// At most 64 owner ranges.
        partitions: Vec<RoutePagePartition>,
        /// Another owner range follows this page.
        has_more: bool,
    },
}

/// Return up to 64 indexed owner ranges without transferring the full route.
pub struct ReadRoutePage;

impl Query for ReadRoutePage {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 15;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<RoutePageInput>;
    type Output = Json<RoutePageOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        if input.start_hash.is_some() && input.after_lower.is_some() {
            return Err(crate::Error::Command("invalid route page cursor"));
        }
        let epoch_rows = context.sql(&statement(
            "SELECT route_epoch FROM ddb_routes WHERE table_id = ?1",
            vec![SqlValue::Text(input.table_id.clone())],
        ))?;
        let Some(row) = epoch_rows[0].rows.first() else {
            return Ok(Json(RoutePageOutcome::Unrouted));
        };
        let [SqlValue::Text(epoch)] = row.as_slice() else {
            return Err(crate::Error::Command("invalid route epoch row"));
        };
        let epoch = parse_epoch(epoch)?;
        if input
            .expected_epoch
            .is_some_and(|expected| expected != epoch)
        {
            return Ok(Json(RoutePageOutcome::Changed));
        }
        let page = match (input.start_hash, input.after_lower) {
            (Some(hash), None) => statement(
                "SELECT partition_id, lower_bound, upper_bound, epoch \
                 FROM ddb_route_partitions WHERE table_id = ?1 AND lower_bound >= \
                 (SELECT MAX(lower_bound) FROM ddb_route_partitions \
                  WHERE table_id = ?1 AND lower_bound <= ?2) \
                 ORDER BY lower_bound LIMIT 65",
                vec![
                    SqlValue::Text(input.table_id),
                    SqlValue::Blob(hash.to_vec()),
                ],
            ),
            (None, Some(lower)) => statement(
                "SELECT partition_id, lower_bound, upper_bound, epoch \
                 FROM ddb_route_partitions WHERE table_id = ?1 AND lower_bound > ?2 \
                 ORDER BY lower_bound LIMIT 65",
                vec![
                    SqlValue::Text(input.table_id),
                    SqlValue::Blob(lower.to_vec()),
                ],
            ),
            (None, None) => statement(
                "SELECT partition_id, lower_bound, upper_bound, epoch \
                 FROM ddb_route_partitions WHERE table_id = ?1 \
                 ORDER BY lower_bound LIMIT 65",
                vec![SqlValue::Text(input.table_id)],
            ),
            (Some(_), Some(_)) => return Err(crate::Error::Command("invalid route page cursor")),
        };
        let rows = context.sql(&page)?;
        let has_more = rows[0].rows.len() > 64;
        let partitions = rows[0]
            .rows
            .iter()
            .take(64)
            .map(decode_page_partition)
            .collect::<Result<Vec<_>>>()?;
        if input.after_lower.is_none() && partitions.is_empty() {
            return Err(crate::Error::Command("published route has no partitions"));
        }
        if input.start_hash.is_none()
            && input.after_lower.is_none()
            && partitions
                .first()
                .is_some_and(|first| first.lower != [0; 16])
        {
            return Err(crate::Error::Command("route page misses hash-space start"));
        }
        if !has_more && partitions.last().is_some_and(|last| last.upper.is_some()) {
            return Err(crate::Error::Command("route page misses hash-space end"));
        }
        for pair in partitions.windows(2) {
            if pair[0].upper != Some(pair[1].lower) {
                return Err(crate::Error::Command("route page has a gap or overlap"));
            }
        }
        if input.start_hash.is_some_and(|hash| {
            partitions.first().is_none_or(|first| {
                hash < first.lower || first.upper.is_some_and(|upper| hash >= upper)
            })
        }) {
            return Err(crate::Error::Command("route page misses start hash"));
        }
        Ok(Json(RoutePageOutcome::Page {
            epoch,
            partitions,
            has_more,
        }))
    }
}

fn decode_page_partition(row: &Vec<SqlValue>) -> Result<RoutePagePartition> {
    let [
        SqlValue::Blob(id),
        SqlValue::Blob(lower),
        SqlValue::Blob(upper),
        SqlValue::Text(epoch),
    ] = row.as_slice()
    else {
        return Err(crate::Error::Command("invalid route page row"));
    };
    let partition_id = id
        .as_slice()
        .try_into()
        .map_err(|_| crate::Error::Command("invalid route partition ID"))?;
    let lower = lower
        .as_slice()
        .try_into()
        .map_err(|_| crate::Error::Command("invalid route lower bound"))?;
    let upper = if upper.as_slice() == [0xff; 17] {
        None
    } else {
        Some(
            upper
                .as_slice()
                .try_into()
                .map_err(|_| crate::Error::Command("invalid route upper bound"))?,
        )
    };
    Ok(RoutePagePartition {
        partition_id,
        lower,
        upper,
        epoch: parse_epoch(epoch)?,
    })
}

fn parse_epoch(epoch: &str) -> Result<u64> {
    let parsed = epoch
        .parse::<u64>()
        .map_err(|_| crate::Error::Command("invalid route partition epoch"))?;
    if parsed == 0 || parsed.to_string() != epoch {
        return Err(crate::Error::Command("invalid route partition epoch"));
    }
    Ok(parsed)
}
