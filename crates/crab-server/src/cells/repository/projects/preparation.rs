use super::super::collaboration::lifecycle::{
    LifecycleStatus, PrepareLifecycleInput, PrepareLifecycleOutcome,
};
use super::*;
use crate::cells::planning;
const KEY: &str = "projects/v1/prepared";
pub(in crate::cells::repository) fn initialize(context: &Transaction<'_>) -> Result<()> {
    context.install_schema(include_str!("schema.sql"))?;
    context.put_document(KEY, &1u8)?;
    planning::initialize(context)
}
pub(super) fn ensure_ready(context: &Transaction<'_>) -> Result<()> {
    match context.document::<u8>(KEY)? {
        Some(1) => planning::ensure_ready(context),
        None => Err(Error::SchemaUnavailable),
        _ => Err(Error::Invalid("unsupported project format")),
    }
}
pub(crate) struct GetState;
impl Read for GetState {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 390;
    type Input = ();
    type Output = LifecycleStatus;
    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<Self::Output> {
        match context.document::<u8>(KEY)? {
            Some(1) => planning::status(context),
            None => Ok(LifecycleStatus::Outdated),
            _ => Err(Error::Invalid("unsupported project format")),
        }
    }
}
pub(crate) struct Prepare;
impl Mutation for Prepare {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 390;
    type Input = PrepareLifecycleInput;
    type Output = PrepareLifecycleOutcome;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        let state = GetState::execute(context, ())?;
        if state == LifecycleStatus::Ready {
            return Ok(MutationResult::Success(PrepareLifecycleOutcome::Applied(
                state,
            )));
        }
        let version = match state {
            LifecycleStatus::Migrating { version, .. } => version,
            _ => 0,
        };
        if input.version != version {
            return Ok(MutationResult::Rejected(PrepareLifecycleOutcome::Conflict(
                state,
            )));
        }
        if context.document::<u8>(KEY)?.is_none() {
            initialize(context)?;
        } else {
            planning::prepare(context, planning::Owner::Repository, input.version)?;
        }
        advance_revision(context)?;
        Ok(MutationResult::Success(PrepareLifecycleOutcome::Applied(
            GetState::execute(context, ())?,
        )))
    }
}
