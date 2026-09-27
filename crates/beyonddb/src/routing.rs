//! Durable initial placement of a table's independently owned data ranges.

use std::collections::HashSet;

use crab_cell_runtime::registry::{Command, CommandContext, CommandResult, Query, QueryContext};
use serde::{Deserialize, Serialize};

use crate::table::{TableRecord, decode_table, statement};
use crate::{Json, MODULE, PartitionSpec, Result, SqlValue};

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
}

impl SplitPlan {
    pub(crate) fn next_epoch(&self) -> Option<u64> {
        self.source.epoch.checked_add(1)
    }

    pub(crate) fn valid_for(&self, table: &TableRecord) -> bool {
        let [left, right] = &self.children;
        let Some(next_epoch) = self.next_epoch() else {
            return false;
        };
        let Some(boundary) = left.upper else {
            return false;
        };
        self.source.table.id == table.id
            && self.source.table.key_schema == table.key_schema
            && self.source.table.attribute_definitions == table.attribute_definitions
            && self.source.epoch > 0
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
    /// Global index ranges must be published before base writes are admitted.
    IndexesNotReady,
    /// The ranges are invalid or do not cover the table's key space.
    InvalidRoute,
}

/// Publish an initial route after its data Cells have been provisioned.
pub struct ActivateTableRoute;

/// Installed initial base directory and its durable publication receipt.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TableRoutePublication {
    pub route: TableRoute,
    pub receipt: crate::DirectoryCopyReceipt,
}

impl Command for ActivateTableRoute {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 10;
    const CODEC_VERSION: u32 = 2;
    type Input = Json<TableRoutePublication>;
    type Output = Json<ActivateTableRouteOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let route = input.route;
        let table_rows = context.sql(&statement(
            "SELECT record FROM ddb_live_tables WHERE table_id = ?1",
            vec![SqlValue::Text(route.table_id.clone())],
        ))?;
        let Some(table) = decode_table(&table_rows[0])? else {
            return Ok(CommandResult::Rejected(Json(
                ActivateTableRouteOutcome::TableNotFound,
            )));
        };
        if route.epoch != 1
            || route.partitions.iter().any(|part| part.epoch != 1)
            || !route.valid_for(&table)
        {
            return Ok(CommandResult::Rejected(Json(
                ActivateTableRouteOutcome::InvalidRoute,
            )));
        }
        for index in &table.global_secondary_indexes {
            if context.sql(&statement(
                "SELECT 1 FROM ddb_directory_roots WHERE table_id = ?1 AND base_table_id = ?2 AND initial_fingerprint IS NOT NULL",
                vec![
                    SqlValue::Text(index.id.clone()),
                    SqlValue::Text(table.id.clone()),
                ],
            ))?[0]
                .rows
                .is_empty()
            {
                return Ok(CommandResult::Rejected(Json(
                    ActivateTableRouteOutcome::IndexesNotReady,
                )));
            }
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
        let spec = crate::DirectorySpec::root(table.id.clone());
        let ranges = route
            .partitions
            .iter()
            .map(|part| RoutePagePartition {
                partition_id: part.partition_id,
                lower: part.lower.unwrap_or([0; 16]),
                upper: part.upper,
                epoch: part.epoch,
            })
            .collect::<Vec<_>>();
        let target = crate::directory::target_for_tenant(context.target().tenant(), &spec)?;
        let fingerprint = crate::directory::fingerprint(&spec, &ranges)?;
        if input.receipt.cell_id != *target.cell_id().as_bytes()
            || input.receipt.sequence == 0
            || input.receipt.sequence > i64::MAX as u64
            || input.receipt.fingerprint != fingerprint
        {
            return Ok(CommandResult::Rejected(Json(
                ActivateTableRouteOutcome::InvalidRoute,
            )));
        }
        let existing = context.sql(&statement(
            "SELECT initial_fingerprint FROM ddb_directory_roots WHERE table_id = ?1 AND base_table_id = ?1",
            vec![SqlValue::Text(table.id.clone())],
        ))?;
        match existing[0].rows.first().map(Vec::as_slice) {
            Some([SqlValue::Null]) => {}
            Some([SqlValue::Blob(previous)]) if previous.as_slice() == fingerprint => {
                return Ok(CommandResult::Success(Json(
                    ActivateTableRouteOutcome::Activated,
                )));
            }
            _ => {
                return Ok(CommandResult::Rejected(Json(
                    ActivateTableRouteOutcome::AlreadyActive,
                )));
            }
        }
        // A fixed-size anchor admits base writes only after the independently
        // installed directory is durable. Replay cannot rewrite its later splits.
        context.sql(&statement(
            "UPDATE ddb_directory_roots SET initial_fingerprint = ?2 WHERE table_id = ?1",
            vec![
                SqlValue::Text(table.id),
                SqlValue::Blob(fingerprint.to_vec()),
            ],
        ))?;
        Ok(CommandResult::Success(Json(
            ActivateTableRouteOutcome::Activated,
        )))
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
    /// Membership version of the leaf containing the previous logical bound.
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
        /// Membership version of the leaf that returned this page.
        epoch: u64,
        /// At most 64 owner ranges.
        partitions: Vec<RoutePagePartition>,
        /// Another owner range follows this page.
        has_more: bool,
    },
}

/// Position of one transfer relative to the published directory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SplitRouteState {
    Unrouted,
    Before,
    After,
    Changed,
}

/// Read the published root of a live base or index generation.
pub struct ReadRouteDirectory;
impl Query for ReadRouteDirectory {
    const MODULE: &'static str = crate::MODULE;
    const ID: u32 = 29;
    const CODEC_VERSION: u32 = 3;
    type Input = Json<String>;
    type Output = Json<Option<crate::DirectorySpec>>;
    fn execute(
        context: &mut QueryContext<'_>,
        Json(directory_id): Self::Input,
    ) -> Result<Self::Output> {
        let rows = context.sql(&statement("SELECT 1 FROM ddb_directory_roots r JOIN ddb_live_tables t ON t.table_id = r.base_table_id WHERE r.table_id = ?1 AND r.initial_fingerprint IS NOT NULL", vec![SqlValue::Text(directory_id.clone())]))?;
        Ok(Json(
            (!rows[0].rows.is_empty()).then(|| crate::DirectorySpec::root(directory_id)),
        ))
    }
}

/// Page a published base or index directory through bounded, independently versioned metadata leaves.
pub async fn read_route_page(
    client: &crab_cell_runtime::client::CellClient,
    account: &crab_cell_runtime::identity::CellTarget,
    input: RoutePageInput,
) -> std::result::Result<RoutePageOutcome, extenddb_storage::error::StorageError> {
    use crate::backend::cell_error;
    use extenddb_storage::error::StorageError;
    let client = client
        .clone()
        .with_read_policy(crab_cell_runtime::client::ReadPolicy::CurrentOwner);
    if input.start_hash.is_some() && input.after_lower.is_some() {
        return Err(StorageError::Validation("invalid directory cursor".into()));
    }
    if client
        .query::<ReadRouteDirectory>(account, None, Json(input.table_id.clone()))
        .await
        .map_err(cell_error)?
        .output
        .0
        .is_none()
    {
        return Ok(RoutePageOutcome::Unrouted);
    }
    let hash = input.after_lower.or(input.start_hash).unwrap_or([0; 16]);
    let mut page =
        crate::read_directory_leaf(&client, account.tenant(), &input.table_id, hash).await?;
    if input
        .expected_epoch
        .is_some_and(|expected| expected != page.version)
    {
        return Ok(RoutePageOutcome::Changed);
    }
    if let Some(after) = input.after_lower {
        page.ranges.retain(|range| range.lower > after);
        if page.ranges.is_empty()
            && let Some(next) = page.spec.upper
        {
            // The previous leaf was checked at its logical continuation.
            // Neighbour versions are independent; crossing its immutable
            // upper bound starts a new membership observation.
            page = crate::read_directory_leaf(&client, account.tenant(), &input.table_id, next)
                .await?;
        }
    }
    Ok(RoutePageOutcome::Page {
        epoch: page.version,
        has_more: page.ranges.last().is_some_and(|last| last.upper.is_some()),
        partitions: page.ranges,
    })
}

/// Resolve the leaf owning a logical position after checking its live anchor.
pub async fn route_directory_target(
    client: &crab_cell_runtime::client::CellClient,
    account: &crab_cell_runtime::identity::CellTarget,
    directory_id: &str,
    lower: [u8; 16],
) -> std::result::Result<
    crab_cell_runtime::identity::CellTarget,
    extenddb_storage::error::StorageError,
> {
    use extenddb_storage::error::StorageError;
    let client = client
        .clone()
        .with_read_policy(crab_cell_runtime::client::ReadPolicy::CurrentOwner);
    if client
        .query::<ReadRouteDirectory>(account, None, Json(directory_id.into()))
        .await
        .map_err(crate::backend::cell_error)?
        .output
        .0
        .is_none()
    {
        return Err(StorageError::Transient(
            "route directory is not published".into(),
        ));
    }
    let page = crate::read_directory_leaf(&client, account.tenant(), directory_id, lower).await?;
    crate::directory::target_for_tenant(account.tenant(), &page.spec)
        .map_err(|error| StorageError::Internal(error.to_string()))
}

pub(crate) async fn split_route_state(
    client: &crab_cell_runtime::client::CellClient,
    account_id: &str,
    plan: &crate::DirectoryTransfer,
) -> std::result::Result<crate::SplitRouteState, extenddb_storage::error::StorageError> {
    let account = crate::account_target(account_id)
        .map_err(|error| extenddb_storage::error::StorageError::Internal(error.to_string()))?;
    let change = plan.directory_change();
    let left = read_route_page(
        client,
        &account,
        RoutePageInput {
            table_id: plan.table_id().to_owned(),
            start_hash: Some(change.source.lower),
            after_lower: None,
            expected_epoch: None,
        },
    )
    .await?;
    let RoutePageOutcome::Page { partitions, .. } = left else {
        return Ok(SplitRouteState::Unrouted);
    };
    if partitions.first() == Some(&change.source) {
        return Ok(SplitRouteState::Before);
    }
    if partitions.first() != Some(&change.children[0]) {
        return Ok(SplitRouteState::Changed);
    }
    let right = read_route_page(
        client,
        &account,
        RoutePageInput {
            table_id: plan.table_id().to_owned(),
            start_hash: Some(change.children[1].lower),
            after_lower: None,
            expected_epoch: None,
        },
    )
    .await?;
    Ok(match right {
        RoutePageOutcome::Page { partitions, .. }
            if partitions.first() == Some(&change.children[1]) =>
        {
            SplitRouteState::After
        }
        _ => SplitRouteState::Changed,
    })
}
