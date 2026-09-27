//! Range replacement reservations stay with one metadata leaf until completion.

use super::*;

/// One data or GSI range replacement, independent of unrelated leaf versions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirectoryChange {
    pub source: RoutePagePartition,
    pub children: [RoutePagePartition; 2],
}

impl DirectoryChange {
    fn valid(&self) -> bool {
        let [left, right] = &self.children;
        let Some(boundary) = left.upper else {
            return false;
        };
        let Some(epoch) = self.source.epoch.checked_add(1) else {
            return false;
        };
        self.source.epoch > 0
            && left.epoch == epoch
            && right.epoch == epoch
            && left.lower == self.source.lower
            && right.lower == boundary
            && right.upper == self.source.upper
            && boundary > left.lower
            && right.upper.is_none_or(|upper| boundary < upper)
            && left.partition_id != right.partition_id
            && left.partition_id != self.source.partition_id
            && right.partition_id != self.source.partition_id
    }
}

fn change(context: &CommandContext<'_, '_>, lower: [u8; 16]) -> Result<Option<DirectoryChange>> {
    let rows = context.sql(&statement(
        "SELECT plan FROM ddb_directory_changes WHERE lower_bound = ?1",
        vec![SqlValue::Blob(lower.to_vec())],
    ))?;
    match rows[0].rows.first().map(Vec::as_slice) {
        None => Ok(None),
        Some([SqlValue::Blob(bytes)]) => Ok(Some(serde_json::from_slice(bytes)?)),
        _ => Err(Error::Command("invalid directory change")),
    }
}

fn current(context: &CommandContext<'_, '_>, range: &RoutePagePartition) -> Result<bool> {
    Ok(ranges(|batch| context.sql(batch), range.lower, 1)?.first() == Some(range))
}

/// Reserve a source and both replacement identities before any data owner seals.
pub struct BeginDirectoryChange;
impl Command for BeginDirectoryChange {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 2;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<DirectoryChange>;
    type Output = Json<bool>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let Some(state) = state(|batch| context.sql(batch))? else {
            return Ok(CommandResult::Rejected(Json(false)));
        };
        if !matches!(state.mode, DirectoryMode::Leaf) || !input.valid() {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        if let Some(existing) = change(context, input.source.lower)? {
            return Ok(if existing == input {
                CommandResult::Success(Json(true))
            } else {
                CommandResult::Rejected(Json(false))
            });
        }
        if !current(context, &input.source)? {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        let rows=context.sql(&statement("SELECT (SELECT COUNT(*) FROM ddb_directory_ranges) + (SELECT COUNT(*) FROM ddb_directory_changes)",vec![]))?;
        let Some([SqlValue::Integer(count)]) = rows[0].rows.first().map(Vec::as_slice) else {
            return Err(Error::Command("invalid directory occupancy"));
        };
        if *count >= MAX_RANGES as i64 {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        let existing = all_ranges(context)?;
        for range in std::iter::once(&input.source).chain(input.children.iter()) {
            if !context.sql(&statement(
                "SELECT 1 FROM ddb_directory_members WHERE partition_id = ?1",
                vec![SqlValue::Blob(range.partition_id.to_vec())],
            ))?[0]
                .rows
                .is_empty()
                || (range != &input.source
                    && existing
                        .iter()
                        .any(|row| row.partition_id == range.partition_id))
            {
                return Ok(CommandResult::Rejected(Json(false)));
            }
        }
        context.sql(&statement(
            "INSERT INTO ddb_directory_changes VALUES (?1, ?2)",
            vec![
                SqlValue::Blob(input.source.lower.to_vec()),
                SqlValue::Blob(serde_json::to_vec(&input)?),
            ],
        ))?;
        // All three identities stay reserved through child opening. A second
        // split or a metadata move must not steal this plan's recovery ownership.
        for range in std::iter::once(&input.source).chain(input.children.iter()) {
            context.sql(&statement(
                "INSERT INTO ddb_directory_members VALUES (?1, ?2)",
                vec![
                    SqlValue::Blob(range.partition_id.to_vec()),
                    SqlValue::Blob(input.source.lower.to_vec()),
                ],
            ))?;
        }
        Ok(CommandResult::Success(Json(true)))
    }
}

/// Replace exactly the reserved source, retaining the plan until children open.
pub struct PublishDirectoryChange;
impl Command for PublishDirectoryChange {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 3;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<DirectoryChange>;
    type Output = Json<bool>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let Some(mut state) = state(|batch| context.sql(batch))? else {
            return Ok(CommandResult::Rejected(Json(false)));
        };
        if !matches!(state.mode, DirectoryMode::Leaf)
            || change(context, input.source.lower)?.as_ref() != Some(&input)
        {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        if current(context, &input.children[0])? && current(context, &input.children[1])? {
            return Ok(CommandResult::Success(Json(true)));
        }
        if !current(context, &input.source)? {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        context.sql(&statement(
            "DELETE FROM ddb_directory_ranges WHERE lower_bound = ?1",
            vec![SqlValue::Blob(input.source.lower.to_vec())],
        ))?;
        for child in &input.children {
            insert_range(context, child)?;
        }
        state.version = state
            .version
            .checked_add(1)
            .ok_or(Error::Command("directory version overflow"))?;
        save(context, &state)?;
        Ok(CommandResult::Success(Json(true)))
    }
}

/// Remove a range-change reservation after both installed children are open.
pub struct FinishDirectoryChange;
impl Command for FinishDirectoryChange {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 4;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<DirectoryChange>;
    type Output = Json<bool>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        if !current(context, &input.children[0])? || !current(context, &input.children[1])? {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        if let Some(existing) = change(context, input.source.lower)? {
            if existing != input {
                return Ok(CommandResult::Rejected(Json(false)));
            }
            context.sql(&statement(
                "DELETE FROM ddb_directory_changes WHERE lower_bound = ?1",
                vec![SqlValue::Blob(input.source.lower.to_vec())],
            ))?;
        }
        Ok(CommandResult::Success(Json(true)))
    }
}

/// Read at most 64 pending data-range plans for leaf-local recovery.
pub struct ReadDirectoryChanges;
impl Query for ReadDirectoryChanges {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 3;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<Option<[u8; 16]>>;
    type Output = Json<Vec<DirectoryChange>>;
    fn execute(context: &mut QueryContext<'_>, Json(after): Self::Input) -> Result<Self::Output> {
        let rows=context.sql(&statement("SELECT plan FROM ddb_directory_changes WHERE (?1 IS NULL OR lower_bound > ?1) ORDER BY lower_bound LIMIT 64",
            vec![after.map_or(SqlValue::Null,|key|SqlValue::Blob(key.to_vec()))]))?;
        Ok(Json(
            rows[0]
                .rows
                .iter()
                .map(|row| match row.as_slice() {
                    [SqlValue::Blob(bytes)] => Ok(serde_json::from_slice(bytes)?),
                    _ => Err(Error::Command("invalid directory plan")),
                })
                .collect::<Result<_>>()?,
        ))
    }
}
