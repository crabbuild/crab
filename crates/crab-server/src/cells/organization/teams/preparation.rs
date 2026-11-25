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
pub(in crate::cells::organization) fn membership_epoch(
    context: &Transaction<'_>,
    person: Uuid,
) -> Result<u64> {
    let rows = context.query(
        "SELECT epoch FROM organization_membership_epochs WHERE actor=?",
        &[blob(person)],
    )?;
    let Some([epoch]) = rows.rows.first().map(Vec::as_slice) else {
        return Err(Error::Invalid("organization membership epoch is missing"));
    };
    uint(epoch)
}

pub(in crate::cells::organization) fn restore_memberships(
    context: &Transaction<'_>,
    person: Uuid,
    former_epoch: u64,
    next_epoch: u64,
) -> Result<()> {
    let args = [blob(person), integer(former_epoch)?];
    let rows = context.query(
        "SELECT m.team FROM organization_team_members m
         JOIN organization_teams t ON t.id=m.team
         WHERE m.actor=? AND m.membership_epoch=? AND m.role IS NOT NULL
           AND t.deleted_at_ms IS NULL",
        &args,
    )?;
    let mut restored = Vec::new();
    for row in rows.rows {
        let [id] = row.as_slice() else {
            return Err(Error::Invalid("former team membership is invalid"));
        };
        let id = uuid(id)?;
        if sync::binding(context, id)?.is_none() {
            restored.push(id.simple().to_string());
        }
    }
    if restored.is_empty() {
        return Ok(());
    }
    let selected = SqlValue::Text(serde_json::to_string(&restored)?);
    // Rebind only the surviving manual edges to the new incarnation and
    // invalidate versions captured before the restored roster was visible.
    context.query(
        "UPDATE organization_team_members SET membership_epoch=?1,version=version+1
         WHERE actor=?2 AND membership_epoch=?3
         AND lower(hex(team)) IN (SELECT value FROM json_each(?4))",
        &[
            integer(next_epoch)?,
            args[0].clone(),
            args[1].clone(),
            selected.clone(),
        ],
    )?;
    context.query(
        "UPDATE organization_teams SET version=version+1,updated_at_ms=MAX(updated_at_ms,?1)
         WHERE lower(hex(id)) IN (SELECT value FROM json_each(?2))",
        &[SqlValue::Integer(context.now_ms()), selected],
    )?;
    Ok(())
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
    // so only edges explicitly restored by a reinstatement can grant again.
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
