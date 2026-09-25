//! Indexed account reads for split planning and publication recovery.

use crab_cell_runtime::registry::{Query, QueryContext};
use serde::{Deserialize, Serialize};

use super::{SplitPlan, decode_page_partition, parse_epoch};
use crate::table::{TableRecord, statement};
use crate::{Json, MODULE, PartitionSpec, Result, SqlBatch, SqlResultSet, SqlValue};

/// Identify one published range by its data Cell partition ID.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PublishedPartitionInput {
    /// Immutable table identity.
    pub table_id: String,
    /// Data Cell partition identity.
    pub partition_id: [u8; 16],
}

/// Published state of one data Cell partition ID.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PublishedPartitionOutcome {
    /// The table has no active route.
    Unrouted,
    /// The route exists but this partition no longer owns a range.
    Missing,
    /// The partition's immutable range and the current directory epoch.
    Published {
        route_epoch: u64,
        spec: Box<PartitionSpec>,
    },
}

/// Read one indexed owner row and the route epoch.
pub struct ReadPublishedPartition;

impl Query for ReadPublishedPartition {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 16;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PublishedPartitionInput>;
    type Output = Json<PublishedPartitionOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        let Some((epoch, table)) = route_head(&input.table_id, |batch| context.sql(batch))? else {
            return Ok(Json(PublishedPartitionOutcome::Unrouted));
        };
        let rows = context.sql(&statement(
            "SELECT partition_id, lower_bound, upper_bound, epoch \
             FROM ddb_route_partitions WHERE table_id = ?1 AND partition_id = ?2",
            vec![
                SqlValue::Text(input.table_id),
                SqlValue::Blob(input.partition_id.to_vec()),
            ],
        ))?;
        let Some(row) = rows[0].rows.first() else {
            return Ok(Json(PublishedPartitionOutcome::Missing));
        };
        Ok(Json(PublishedPartitionOutcome::Published {
            route_epoch: epoch,
            spec: Box::new(partition_spec(row, &table)?),
        }))
    }
}

/// Position of one durable split relative to the indexed route.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SplitRouteState {
    /// The table has no active route.
    Unrouted,
    /// The source is published and neither child is published.
    Before,
    /// The children are published and the source is absent.
    After,
    /// The directory no longer matches this plan.
    Changed,
}

/// Compare one compact split plan with the published directory atomically.
pub struct ReadSplitRoute;

impl Query for ReadSplitRoute {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 17;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<SplitPlan>;
    type Output = Json<SplitRouteState>;

    fn execute(context: &mut QueryContext<'_>, Json(plan): Self::Input) -> Result<Self::Output> {
        Ok(Json(split_route_state(&plan, |batch| context.sql(batch))?))
    }
}

pub(super) fn split_route_state(
    plan: &SplitPlan,
    sql: impl Fn(&SqlBatch) -> Result<Vec<SqlResultSet>>,
) -> Result<SplitRouteState> {
    let table_id = &plan.source.table.id;
    let Some((epoch, table)) = route_head(table_id, &sql)? else {
        return Ok(SplitRouteState::Unrouted);
    };
    if table != plan.source.table || !plan.valid_for(&table) {
        return Ok(SplitRouteState::Changed);
    }
    let rows = sql(&statement(
        "SELECT partition_id, lower_bound, upper_bound, epoch \
         FROM ddb_route_partitions WHERE table_id = ?1 \
         AND partition_id IN (?2, ?3, ?4)",
        vec![
            SqlValue::Text(table_id.clone()),
            SqlValue::Blob(plan.source.partition_id.to_vec()),
            SqlValue::Blob(plan.children[0].partition_id.to_vec()),
            SqlValue::Blob(plan.children[1].partition_id.to_vec()),
        ],
    ))?;
    let mut source = false;
    let mut children = [false; 2];
    for row in &rows[0].rows {
        let spec = partition_spec(row, &table)?;
        if spec.partition_id == plan.source.partition_id {
            source = spec == plan.source;
        } else if spec.partition_id == plan.children[0].partition_id {
            children[0] = spec == plan.children[0];
        } else if spec.partition_id == plan.children[1].partition_id {
            children[1] = spec == plan.children[1];
        }
    }
    let state = if epoch == plan.expected_epoch && source && rows[0].rows.len() == 1 {
        SplitRouteState::Before
    } else if plan.next_epoch() == Some(epoch)
        && !source
        && children == [true, true]
        && rows[0].rows.len() == 2
    {
        SplitRouteState::After
    } else {
        SplitRouteState::Changed
    };
    Ok(state)
}

fn route_head(
    table_id: &str,
    sql: impl Fn(&SqlBatch) -> Result<Vec<SqlResultSet>>,
) -> Result<Option<(u64, TableRecord)>> {
    let rows = sql(&statement(
        "SELECT route_epoch, route_table FROM ddb_routes WHERE table_id = ?1",
        vec![SqlValue::Text(table_id.to_owned())],
    ))?;
    let Some(row) = rows[0].rows.first() else {
        return Ok(None);
    };
    let [SqlValue::Text(epoch), SqlValue::Blob(table)] = row.as_slice() else {
        return Err(crate::Error::Command("invalid route metadata"));
    };
    let table: TableRecord = serde_json::from_slice(table)?;
    if table.id != table_id {
        return Err(crate::Error::Command("invalid route table identity"));
    }
    Ok(Some((parse_epoch(epoch)?, table)))
}

fn partition_spec(row: &Vec<SqlValue>, table: &TableRecord) -> Result<PartitionSpec> {
    let range = decode_page_partition(row)?;
    Ok(PartitionSpec {
        table: table.clone(),
        partition_id: range.partition_id,
        lower: (range.lower != [0; 16]).then_some(range.lower),
        upper: range.upper,
        epoch: range.epoch,
    })
}
