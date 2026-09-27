//! Account generation anchors and traversal of independently owned index directories.

use crab_cell_runtime::registry::{Command, CommandContext, CommandResult, Query, QueryContext};
use serde::{Deserialize, Serialize};

use super::GlobalIndexRecord;
use crate::table::{decode_table, statement};
use crate::{
    Json, Result, RoutePageInput, RoutePageOutcome, RoutePagePartition, SqlValue, TableRecord,
};

/// Initial index directory published after all of its ranges are installed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalIndexRoute {
    pub table: TableRecord,
    pub index: GlobalIndexRecord,
    pub partitions: Vec<RoutePagePartition>,
    pub receipt: crate::DirectoryCopyReceipt,
}

/// Publish the first contiguous directory of one global-index generation.
pub struct ActivateGlobalIndexRoute;

impl Command for ActivateGlobalIndexRoute {
    const MODULE: &'static str = crate::MODULE;
    const ID: u32 = 25;
    const CODEC_VERSION: u32 = 2;
    type Input = Json<GlobalIndexRoute>;
    type Output = Json<bool>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let rows = context.sql(&statement(
            "SELECT record FROM ddb_live_tables WHERE table_id = ?1",
            vec![SqlValue::Text(input.table.id.clone())],
        ))?;
        let Some(table) = decode_table(&rows[0])? else {
            return Ok(CommandResult::Rejected(Json(false)));
        };
        if table != input.table
            || !table.global_secondary_indexes.contains(&input.index)
            || input.partitions.is_empty()
        {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        let mut lower = Some([0; 16]);
        let mut ids = std::collections::HashSet::new();
        for range in &input.partitions {
            if range.epoch != 1
                || lower != Some(range.lower)
                || range.upper.is_some_and(|upper| upper <= range.lower)
                || !ids.insert(range.partition_id)
            {
                return Ok(CommandResult::Rejected(Json(false)));
            }
            lower = range.upper;
        }
        if lower.is_some() {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        let spec = crate::DirectorySpec::root(input.index.id.clone());
        let target = crate::directory::target_for_tenant(context.target().tenant(), &spec)?;
        let fingerprint = crate::directory::fingerprint(&spec, &input.partitions)?;
        if input.receipt.cell_id != *target.cell_id().as_bytes()
            || input.receipt.sequence == 0
            || input.receipt.sequence > i64::MAX as u64
            || input.receipt.fingerprint != fingerprint
        {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        let existing = context.sql(&statement(
            "SELECT initial_fingerprint FROM ddb_global_index_routes WHERE table_id = ?1",
            vec![SqlValue::Text(input.index.id.clone())],
        ))?;
        match existing[0].rows.first().map(Vec::as_slice) {
            Some([SqlValue::Null]) => {}
            Some([SqlValue::Blob(previous)]) if previous.as_slice() == fingerprint => {
                return Ok(CommandResult::Success(Json(true)));
            }
            _ => return Ok(CommandResult::Rejected(Json(false))),
        }
        context.sql(&statement("UPDATE ddb_global_index_routes SET initial_fingerprint = ?3 WHERE table_id = ?1 AND base_table_id = ?2", vec![SqlValue::Text(input.index.id), SqlValue::Text(table.id), SqlValue::Blob(fingerprint.to_vec())]))?;
        Ok(CommandResult::Success(Json(true)))
    }
}

/// Read the published root of a live index generation.
pub struct ReadGlobalIndexDirectory;
impl Query for ReadGlobalIndexDirectory {
    const MODULE: &'static str = crate::MODULE;
    const ID: u32 = 29;
    const CODEC_VERSION: u32 = 2;
    type Input = Json<String>;
    type Output = Json<Option<crate::DirectorySpec>>;
    fn execute(
        context: &mut QueryContext<'_>,
        Json(index_id): Self::Input,
    ) -> Result<Self::Output> {
        let rows = context.sql(&statement("SELECT 1 FROM ddb_global_index_routes r JOIN ddb_live_tables t ON t.table_id = r.base_table_id WHERE r.table_id = ?1 AND r.initial_fingerprint IS NOT NULL", vec![SqlValue::Text(index_id.clone())]))?;
        Ok(Json(
            (!rows[0].rows.is_empty()).then(|| crate::DirectorySpec::root(index_id)),
        ))
    }
}

/// Page a published index through bounded, independently versioned metadata leaves.
pub async fn read_global_index_route_page(
    client: &crab_cell_runtime::client::CellClient,
    account_id: &str,
    input: RoutePageInput,
) -> std::result::Result<RoutePageOutcome, extenddb_storage::error::StorageError> {
    use crate::backend::cell_error;
    use extenddb_storage::error::StorageError;
    let client = client
        .clone()
        .with_read_policy(crab_cell_runtime::client::ReadPolicy::CurrentOwner);
    if input.start_hash.is_some() && input.after_lower.is_some() {
        return Err(StorageError::Validation(
            "invalid index directory cursor".into(),
        ));
    }
    let account = crate::account_target(account_id)
        .map_err(|error| StorageError::Internal(error.to_string()))?;
    if client
        .query::<ReadGlobalIndexDirectory>(&account, None, Json(input.table_id.clone()))
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

/// Resolve the leaf owning an index's logical position after checking its live anchor.
pub async fn global_index_directory_target(
    client: &crab_cell_runtime::client::CellClient,
    account: &crab_cell_runtime::identity::CellTarget,
    index_id: &str,
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
        .query::<ReadGlobalIndexDirectory>(account, None, Json(index_id.into()))
        .await
        .map_err(crate::backend::cell_error)?
        .output
        .0
        .is_none()
    {
        return Err(StorageError::Transient(
            "index directory is not published".into(),
        ));
    }
    let page = crate::read_directory_leaf(&client, account.tenant(), index_id, lower).await?;
    crate::directory::target_for_tenant(account.tenant(), &page.spec)
        .map_err(|error| StorageError::Internal(error.to_string()))
}

pub(crate) async fn global_index_split_route(
    client: &crab_cell_runtime::client::CellClient,
    account_id: &str,
    plan: &super::GlobalIndexSplitPlan,
) -> std::result::Result<crate::SplitRouteState, extenddb_storage::error::StorageError> {
    use crate::SplitRouteState;
    let change = plan.directory_change();
    let left = read_global_index_route_page(
        client,
        account_id,
        RoutePageInput {
            table_id: plan.source.index.id.clone(),
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
    let right = read_global_index_route_page(
        client,
        account_id,
        RoutePageInput {
            table_id: plan.source.index.id.clone(),
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
