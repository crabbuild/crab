use super::*;

const STATE: &str = "organization/teams/v1/state";
const BATCH: usize = 8;

/// Explicit preparation state for native organization team authority.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum OrganizationTeamState {
    Missing,
    Building {
        after: Option<Uuid>,
        processed: u64,
        version: u64,
    },
    Ready,
}
fn state(context: &Transaction<'_>) -> Result<OrganizationTeamState> {
    Ok(context
        .document(STATE)?
        .unwrap_or(OrganizationTeamState::Missing))
}
pub(in crate::cells::organization) fn ensure_ready(context: &Transaction<'_>) -> Result<()> {
    if state(context)? != OrganizationTeamState::Ready {
        return Err(Error::SchemaUnavailable);
    }
    Ok(())
}
pub(in crate::cells::organization) fn initialize(context: &Transaction<'_>) -> Result<()> {
    context.install_schema(include_str!("schema.sql"))?;
    context.put_document(STATE, &OrganizationTeamState::Ready)
}
pub(in crate::cells::organization) fn membership_changed(
    context: &Transaction<'_>,
    member: &super::super::Member,
    was_active: Option<bool>,
) -> Result<()> {
    ensure_ready(context)?;
    if was_active == Some(member.active) {
        return Ok(());
    }
    // Role edits retain membership. Leaving/rejoining advances its incarnation,
    // so old team edges cannot become grants again when an invitation is accepted.
    context.query("INSERT INTO organization_membership_epochs(actor,epoch) VALUES (?,?) ON CONFLICT(actor) DO UPDATE SET epoch=excluded.epoch",&[blob(member.actor),integer(member.version)?])?;
    Ok(())
}
pub(crate) struct GetPreparation;
impl Read for GetPreparation {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 10;
    type Input = Caller;
    type Output = OrganizationTeamState;
    fn execute(context: &mut Transaction<'_>, _: Caller) -> Result<Self::Output> {
        state(context)
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Step {
    Begin,
    Step { version: u64 },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrepareInput {
    pub actor: Uuid,
    pub step: Step,
}
pub(crate) struct Prepare;
impl Mutation for Prepare {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 10;
    type Input = PrepareInput;
    type Output = OrganizationTeamState;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        let old = state(context)?;
        let (after, processed, version) = match (&old, input.step) {
            (OrganizationTeamState::Missing, Step::Begin) => {
                context.install_schema(include_str!("schema.sql"))?;
                let building = OrganizationTeamState::Building {
                    after: None,
                    processed: 0,
                    version: 1,
                };
                context.put_document(STATE, &building)?;
                return Ok(MutationResult::Success(building));
            }
            (
                OrganizationTeamState::Building {
                    after,
                    processed,
                    version,
                },
                Step::Step { version: expected },
            ) if *version == expected => (*after, *processed, *version),
            _ => return Ok(MutationResult::Success(old)),
        };
        let rows = context.query(
            "SELECT actor,version FROM organization_members WHERE actor>? ORDER BY actor LIMIT 8",
            &[blob(after.unwrap_or(Uuid::nil()))],
        )?;
        for row in &rows.rows {
            let [id, epoch] = row.as_slice() else {
                return Err(Error::Invalid(
                    "organization membership epoch source is invalid",
                ));
            };
            context.query(
                "INSERT INTO organization_membership_epochs(actor,epoch) VALUES (?,?)",
                &[blob(uuid(id)?), integer(uint(epoch)?)?],
            )?;
        }
        let new = if rows.rows.len() < BATCH {
            OrganizationTeamState::Ready
        } else {
            OrganizationTeamState::Building {
                after: rows.rows.last().map(|row| uuid(&row[0])).transpose()?,
                processed: processed
                    .checked_add(BATCH as u64)
                    .ok_or(Error::Invalid("team preparation count overflowed"))?,
                version: next(version)?,
            }
        };
        context.put_document(STATE, &new)?;
        Ok(MutationResult::Success(new))
    }
}
