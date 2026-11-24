use super::*;

pub(crate) const MAX_GROUP_MEMBERS: usize = 1000;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub group: Uuid,
    pub generation: Uuid,
    pub revision: u64,
    pub name: String,
}

fn binding_key(team: Uuid) -> String {
    format!("organization/v1/team-groups/{team}")
}

pub(super) fn binding(context: &Transaction<'_>, team: Uuid) -> Result<Option<Binding>> {
    context.document(&binding_key(team))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Status {
    pub binding: Option<Binding>,
    pub version: u64,
    pub can_link: bool,
    pub can_manage: bool,
}

pub(crate) struct GetSync;
impl Read for GetSync {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 19;
    type Input = Key;
    type Output = Option<Status>;

    fn execute(context: &mut Transaction<'_>, input: Key) -> Result<Self::Output> {
        let Some(view) = view(context, input.actor, input.team)? else {
            return Ok(None);
        };
        Ok(Some(Status {
            binding: binding(context, view.team.id)?,
            version: view.team.version,
            can_manage: view.can_manage,
            can_link: owner(context, input.actor)?,
        }))
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum SyncAction {
    Link { actor: Uuid, version: u64 },
    Refresh,
    Unlink { actor: Uuid, version: u64 },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuditChange {
    Linked,
    Synchronized,
    Unlinked,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SyncInput {
    pub team: Uuid,
    pub binding: Binding,
    pub members: Vec<Uuid>,
    pub action: SyncAction,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SyncOutcome {
    Applied,
    Stale,
    Missing,
}

pub(crate) struct SyncGroup;
impl Mutation for SyncGroup {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 19;
    type Input = SyncInput;
    type Output = SyncOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: SyncInput,
    ) -> Result<MutationResult<SyncOutcome>> {
        ensure_ready(context)?;
        let Some(team) = team(context, input.team)? else {
            return Ok(MutationResult::Success(SyncOutcome::Missing));
        };
        let current = binding(context, team.id)?;
        let matches = current.as_ref().is_some_and(|row| {
            row.group == input.binding.group && row.generation == input.binding.generation
        });

        let (actor, change) = match input.action {
            SyncAction::Link { actor, version } => {
                if matches {
                    return Ok(MutationResult::Success(SyncOutcome::Applied));
                }
                if current.is_some() || team.version != version || !owner(context, actor)? {
                    return Ok(MutationResult::Success(SyncOutcome::Stale));
                }
                (Some(actor), AuditChange::Linked)
            }
            SyncAction::Unlink { actor, version } => {
                if current.is_none() {
                    return Ok(MutationResult::Success(SyncOutcome::Applied));
                }
                if !matches || team.version != version || !owner(context, actor)? {
                    return Ok(MutationResult::Success(SyncOutcome::Stale));
                }
                (Some(actor), AuditChange::Unlinked)
            }
            SyncAction::Refresh => {
                if !matches {
                    return Ok(MutationResult::Success(SyncOutcome::Missing));
                }
                if current
                    .as_ref()
                    .is_some_and(|row| row.revision >= input.binding.revision)
                {
                    return Ok(MutationResult::Success(SyncOutcome::Applied));
                }
                (None, AuditChange::Synchronized)
            }
        };

        if input.members.len() > MAX_GROUP_MEMBERS || input.members.iter().any(Uuid::is_nil) {
            return Err(Error::Invalid("identity provider group roster is invalid"));
        }
        let (updated, removed) = replace_roster(context, team.id, &input.members)?;
        if change == AuditChange::Unlinked {
            context.delete_document(&binding_key(team.id))?;
        } else {
            context.put_document(&binding_key(team.id), &input.binding)?;
        }
        context.query(
            "UPDATE organization_teams SET version=?,updated_at_ms=MAX(updated_at_ms,?) WHERE id=?",
            &[
                integer(next(team.version)?)?,
                SqlValue::Integer(context.now_ms()),
                blob(team.id),
            ],
        )?;
        // Commit the audit event with the binding and roster. A replay that
        // observes the committed binding returns before changing either.
        super::super::append_event(
            context,
            actor,
            Action::TeamGroupSync {
                team: team.id,
                group: input.binding.group,
                group_revision: input.binding.revision,
                change,
                updated,
                removed,
            },
        )?;
        Ok(MutationResult::Success(SyncOutcome::Applied))
    }
}

fn owner(context: &Transaction<'_>, actor: Uuid) -> Result<bool> {
    Ok(member(context, actor)?
        .is_some_and(|member| member.active && member.role == OrganizationRole::Owner))
}

fn replace_roster(context: &Transaction<'_>, team: Uuid, members: &[Uuid]) -> Result<(u64, u64)> {
    let people = members
        .iter()
        .map(|id| id.simple().to_string())
        .collect::<Vec<_>>();
    let args = [blob(team), SqlValue::Text(serde_json::to_string(&people)?)];
    // Synced accounts are existing organization members. Store their current
    // membership epoch so leaving the organization invalidates all team grants.
    let removed = context.query(
        "UPDATE organization_team_members SET role=NULL,version=version+1
         WHERE team=?1 AND role IS NOT NULL
           AND lower(hex(actor)) NOT IN (SELECT value FROM json_each(?2))",
        &args,
    )?;
    let updated = context.query(
        "INSERT INTO organization_team_members(team,actor,membership_epoch,role,version)
         SELECT ?1,m.actor,e.epoch,'member',1
         FROM organization_members m JOIN organization_membership_epochs e ON e.actor=m.actor
         WHERE m.active=1 AND lower(hex(m.actor)) IN (SELECT value FROM json_each(?2))
         ON CONFLICT(team,actor) DO UPDATE
           SET role='member',membership_epoch=excluded.membership_epoch,version=version+1
         WHERE role IS NOT 'member' OR membership_epoch<>excluded.membership_epoch",
        &args,
    )?;
    Ok((updated.rows_affected, removed.rows_affected))
}
