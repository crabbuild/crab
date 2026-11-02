use super::super::collaboration::lifecycle::{
    LifecycleStatus, PrepareLifecycleInput, PrepareLifecycleOutcome,
};
use super::*;
use crate::cells::{Error, Result};

pub(super) const KEY: &str = "git_projection/v2/preparation";
const FRAME_KEY: &str = "git_projection/v2/preparation/frame";
const FRAME: usize = 256;
const TABLES: &[(&str, &[&str])] = &[
    ("git_projection_state", &["singleton"]),
    ("git_projection_refs", &["epoch_id", "name"]),
    ("git_attribution_roots", &["epoch_id", "commit_ordinal"]),
    ("git_attribution_edges", &["parent_hash", "component"]),
    ("git_attribution_nodes", &["node_hash"]),
    ("git_epoch_commits", &["epoch_id", "ordinal"]),
    ("git_commits", &["oid"]),
    ("git_projection_epochs", &["epoch_id"]),
];

pub(in crate::cells::repository) fn initialize(context: &Transaction<'_>) -> Result<()> {
    context.install_schema(include_str!("schema.sql"))?;
    context.put_document(KEY, &(TABLES.len() as u64 + 1))
}

pub(super) fn ensure_ready(context: &Transaction<'_>) -> Result<()> {
    if GetState::execute_read(context)? == LifecycleStatus::Ready {
        return Ok(());
    }
    Err(Error::SchemaUnavailable)
}

pub(crate) struct GetState;
impl GetState {
    fn execute_read(context: &Transaction<'_>) -> Result<LifecycleStatus> {
        Ok(match context.document::<u64>(KEY)? {
            None => LifecycleStatus::Outdated,
            Some(version) if version == TABLES.len() as u64 + 1 => LifecycleStatus::Ready,
            Some(version) if version > 0 && version <= TABLES.len() as u64 => {
                LifecycleStatus::Migrating {
                    version: context.document(FRAME_KEY)?.unwrap_or(version),
                    copied: version,
                }
            }
            _ => {
                return Err(Error::Invalid(
                    "unsupported projection preparation progress",
                ));
            }
        })
    }
}
impl Read for GetState {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 399;
    type Input = ();
    type Output = LifecycleStatus;
    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<LifecycleStatus> {
        Self::execute_read(context)
    }
}

pub(crate) struct Prepare;
impl Mutation for Prepare {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 399;
    type Input = PrepareLifecycleInput;
    type Output = PrepareLifecycleOutcome;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        let state = GetState::execute_read(context)?;
        if state == LifecycleStatus::Ready {
            return Ok(MutationResult::Success(PrepareLifecycleOutcome::Applied(
                state,
            )));
        }
        let version = match state {
            LifecycleStatus::Migrating { version, .. } => version,
            _ => 0,
        };
        if version != input.version {
            return Ok(MutationResult::Rejected(PrepareLifecycleOutcome::Conflict(
                state,
            )));
        }
        let phase = context.document::<u64>(KEY)?.unwrap_or(0);
        // DROP performs implicit DELETE with foreign keys enabled. Clear bounded
        // full-key batches first, so a large old graph cannot monopolize the host.
        if let Some((table, keys)) = TABLES.get(phase as usize) {
            let columns = keys.join(",");
            let rows = context
                .query(
                    &format!(
                        "SELECT {columns} FROM {table} ORDER BY {columns} LIMIT {}",
                        FRAME + 1
                    ),
                    &[],
                )?
                .rows;
            let predicate = keys
                .iter()
                .map(|key| format!("{key}=?"))
                .collect::<Vec<_>>()
                .join(" AND ");
            let delete = format!("DELETE FROM {table} WHERE {predicate}");
            for row in rows.iter().take(FRAME) {
                context.query(&delete, row)?;
            }
            let complete = rows.len() <= FRAME;
            if complete {
                context.query(&format!("DROP TABLE {table}"), &[])?;
            }
            // Existing table checkpoints seed the first bounded frame's CAS.
            // Keeping its counter separate lets a table span any number of frames.
            let next = version
                .checked_add(1)
                .filter(|value| *value <= super::super::MAX_NUMBER)
                .ok_or(Error::Invalid(
                    "projection preparation version limit reached",
                ))?;
            context.put_document(KEY, &(phase + u64::from(complete)))?;
            context.put_document(FRAME_KEY, &next)?;
        } else {
            initialize(context)?;
            context.delete_document(FRAME_KEY)?;
        }
        super::super::advance_revision(context)?;
        Ok(MutationResult::Success(PrepareLifecycleOutcome::Applied(
            GetState::execute_read(context)?,
        )))
    }
}
