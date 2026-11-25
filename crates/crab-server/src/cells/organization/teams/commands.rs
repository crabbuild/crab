use super::*;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub actor: Uuid,
    pub request_id: Uuid,
    pub command: Command,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "TeamCommand"))]
pub(crate) enum Command {
    Create {
        name: String,
        description: String,
        visibility: Visibility,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(test, ts(optional))]
        parent: Option<Uuid>,
    },
    Edit {
        team: Uuid,
        version: u64,
        name: String,
        description: String,
        visibility: Visibility,
    },
    Delete {
        team: Uuid,
        version: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(test, ts(optional))]
        hierarchy_version: Option<u64>,
    },
    Reparent {
        team: Uuid,
        version: u64,
        parent: Option<Uuid>,
    },
    ProposeHierarchy {
        team: Uuid,
        parent: Uuid,
        version: u64,
        side: hierarchy_requests::Side,
    },
    DecideHierarchy {
        team: Uuid,
        request: Uuid,
        decision: hierarchy_requests::Decision,
    },
    Member {
        team: Uuid,
        member: Uuid,
        version: u64,
        role: Option<Role>,
    },
    ReviewRouting {
        team: Uuid,
        version: u64,
        algorithm: review_routing::Algorithm,
        reviewers: u8,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(test, ts(optional))]
        excluded: Option<Vec<Uuid>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(test, ts(optional))]
        options: Option<review_routing::Options>,
    },
    Policy {
        version: u64,
        member_creation: bool,
    },
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Outcome {
    Applied { id: Uuid },
    Cancelled { id: Uuid },
    Conflict,
    InvalidName,
    InvalidDescription,
    InvalidParent,
    NestedVisibility,
    InvalidRouting,
    InvalidRoutingExclusions,
    NameTaken,
    NotMember,
    Missing,
    SubmissionConflict,
}
fn invalid_fields(name: &str, description: &str) -> Option<Outcome> {
    if name.is_empty()
        || name.trim() != name
        || name.chars().count() > 100
        || name.chars().any(char::is_control)
    {
        return Some(Outcome::InvalidName);
    }
    if description.chars().count() > 1000 || description.contains('\0') {
        return Some(Outcome::InvalidDescription);
    }
    None
}
fn digest(input: &Request) -> Result<blake3::Hash> {
    Ok(blake3::hash(&serde_json::to_vec(input)?))
}
pub(super) fn recover(context: &Transaction<'_>, input: &Request) -> Result<Option<Outcome>> {
    let rows = context.query(
        "SELECT actor,digest,outcome FROM organization_team_submissions WHERE request_id=?",
        &[blob(input.request_id)],
    )?;
    let Some([owner, SqlValue::Blob(old), SqlValue::Blob(outcome)]) =
        rows.rows.first().map(Vec::as_slice)
    else {
        if !rows.rows.is_empty() {
            return Err(Error::Invalid("team submission is invalid"));
        }
        return Ok(None);
    };
    if uuid(owner)? != input.actor || old.as_slice() != digest(input)?.as_bytes() {
        return Ok(Some(Outcome::SubmissionConflict));
    }
    let outcome = serde_json::from_slice(outcome)?;
    if matches!(outcome, Outcome::SubmissionConflict) {
        return Err(Error::Invalid("team submission outcome is invalid"));
    }
    Ok(Some(outcome))
}
fn remember(
    context: &Transaction<'_>,
    input: &Request,
    outcome: Outcome,
) -> Result<MutationResult<Outcome>> {
    // Domain guards reject before any write. A rejection is still a terminal
    // decision: committing it fences other transports when names or candidate
    // eligibility change, without modifying the team or its audit history.
    context.query("INSERT INTO organization_team_submissions(request_id,actor,digest,outcome) VALUES (?,?,?,?)", &[blob(input.request_id),blob(input.actor),SqlValue::Blob(digest(input)?.as_bytes().to_vec()),SqlValue::Blob(serde_json::to_vec(&outcome)?)])?;
    Ok(MutationResult::Success(outcome))
}

pub(crate) struct Change;
impl Mutation for Change {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 11;
    type Input = Request;
    type Output = Outcome;
    fn execute(context: &mut Transaction<'_>, input: Request) -> Result<MutationResult<Outcome>> {
        ensure_ready(context)?;
        actor(input.actor)?;
        actor(input.request_id)?;
        if let Some(old) = recover(context, &input)? {
            return Ok(if !matches!(old, Outcome::SubmissionConflict) {
                MutationResult::Success(old)
            } else {
                MutationResult::Rejected(old)
            });
        }
        if let Command::Create {
            name, description, ..
        }
        | Command::Edit {
            name, description, ..
        } = &input.command
            && let Some(outcome) = invalid_fields(name, description)
        {
            return remember(context, &input, outcome);
        }
        let outcome = match &input.command {
            Command::Create {
                name,
                description,
                visibility,
                parent,
            } => create(context, &input, name, description, *visibility, *parent)?,
            Command::Edit {
                team,
                version,
                name,
                description,
                visibility,
            } => edit(
                context,
                input.actor,
                *team,
                *version,
                name,
                description,
                *visibility,
            )?,
            Command::Delete {
                team,
                version,
                hierarchy_version,
            } => delete(context, input.actor, *team, *version, *hierarchy_version)?,
            Command::Reparent {
                team,
                version,
                parent,
            } => reparent(context, input.actor, *team, *version, *parent)?,
            Command::ProposeHierarchy {
                team,
                parent,
                version,
                side,
            } => hierarchy_requests::propose(context, &input, *team, *parent, *version, *side)?,
            Command::DecideHierarchy {
                team,
                request,
                decision,
            } => hierarchy_requests::decide(context, input.actor, *team, *request, *decision)?,
            Command::Member {
                team,
                member,
                version,
                role,
            } => change_member(context, input.actor, *team, *member, *version, *role)?,
            Command::ReviewRouting {
                team,
                version,
                algorithm,
                reviewers,
                excluded,
                options,
            } => review_routing::set(
                context,
                input.actor,
                *team,
                review_routing::Update {
                    version: *version,
                    algorithm: *algorithm,
                    reviewers: *reviewers,
                    excluded: excluded.as_deref(),
                    options: *options,
                },
            )?,
            Command::Policy {
                version,
                member_creation,
            } => set_policy(context, input.actor, *version, *member_creation)?,
        };
        if matches!(outcome, Outcome::SubmissionConflict) {
            return Ok(MutationResult::Rejected(outcome));
        }
        remember(context, &input, outcome)
    }
}
fn taken(context: &Transaction<'_>, name: &str, except: Uuid) -> Result<bool> {
    Ok(!context.query("SELECT 1 FROM organization_teams WHERE name_key=? AND deleted_at_ms IS NULL AND id<>? LIMIT 1",&[SqlValue::Text(name.to_lowercase()),blob(except)])?.rows.is_empty())
}
fn create(
    context: &Transaction<'_>,
    input: &Request,
    name: &str,
    description: &str,
    visibility: Visibility,
    parent: Option<Uuid>,
) -> Result<Outcome> {
    if !context
        .query(
            "SELECT 1 FROM organization_teams WHERE id=?",
            &[blob(input.request_id)],
        )?
        .rows
        .is_empty()
    {
        return Ok(Outcome::SubmissionConflict);
    }
    if taken(context, name, input.request_id)? {
        return Ok(Outcome::NameTaken);
    }
    if !hierarchy::valid_parent(context, input.request_id, visibility, parent)? {
        return Ok(Outcome::InvalidParent);
    }
    let epoch = membership_epoch(context, input.actor)?;
    context.query("INSERT INTO organization_teams(id,name,name_key,description,visibility,parent,version,created_at_ms,updated_at_ms) VALUES (?,?,?,?,?,?,1,?,?)",&[blob(input.request_id),SqlValue::Text(name.into()),SqlValue::Text(name.to_lowercase()),SqlValue::Text(description.into()),SqlValue::Text(visibility.name().into()),parent.map_or(SqlValue::Null,blob),SqlValue::Integer(context.now_ms()),SqlValue::Integer(context.now_ms())])?;
    context.query("INSERT INTO organization_team_members(team,actor,membership_epoch,role,version) VALUES (?,?,?,'maintainer',1)",&[blob(input.request_id),blob(input.actor),integer(epoch)?])?;
    advance(
        context,
        input.actor,
        Action::TeamCreated {
            team: input.request_id,
        },
    )?;
    Ok(Outcome::Applied {
        id: input.request_id,
    })
}
fn edit(
    context: &Transaction<'_>,
    who: Uuid,
    id: Uuid,
    version: u64,
    name: &str,
    description: &str,
    visibility: Visibility,
) -> Result<Outcome> {
    let Some(old) = team(context, id)? else {
        return Ok(Outcome::Missing);
    };
    if old.version != version {
        return Ok(Outcome::Conflict);
    }
    if taken(context, name, id)? {
        return Ok(Outcome::NameTaken);
    }
    if visibility == Visibility::Secret
        && (old.parent.is_some() || hierarchy::has_children(context, id)?)
    {
        return Ok(Outcome::NestedVisibility);
    }
    if old.name == name && old.description == description && old.visibility == visibility {
        return Ok(Outcome::Applied { id });
    }
    context.query("UPDATE organization_teams SET name=?,name_key=?,description=?,visibility=?,version=?,updated_at_ms=MAX(updated_at_ms,?) WHERE id=?",&[SqlValue::Text(name.into()),SqlValue::Text(name.to_lowercase()),SqlValue::Text(description.into()),SqlValue::Text(visibility.name().into()),integer(next(version)?)?,SqlValue::Integer(context.now_ms()),blob(id)])?;
    if visibility == Visibility::Secret {
        hierarchy_requests::discard_for_team(context, id)?;
    }
    advance(context, who, Action::TeamEdited { team: id })?;
    Ok(Outcome::Applied { id })
}
pub(super) fn reparent(
    context: &Transaction<'_>,
    who: Uuid,
    id: Uuid,
    version: u64,
    parent: Option<Uuid>,
) -> Result<Outcome> {
    let Some(old) = team(context, id)? else {
        return Ok(Outcome::Missing);
    };
    if old.version != version {
        return Ok(Outcome::Conflict);
    }
    if old.parent != parent {
        if !hierarchy::valid_parent(context, id, old.visibility, parent)? {
            return Ok(Outcome::InvalidParent);
        }
        context.query(
            "UPDATE organization_teams SET parent=?,version=?,updated_at_ms=MAX(updated_at_ms,?) WHERE id=?",
            &[parent.map_or(SqlValue::Null, blob), integer(next(version)?)?, SqlValue::Integer(context.now_ms()), blob(id)],
        )?;
        context.query(
            "DELETE FROM organization_team_hierarchy_requests WHERE child=?",
            &[blob(id)],
        )?;
        advance(context, who, Action::TeamEdited { team: id })?;
    }
    Ok(Outcome::Applied { id })
}

fn delete(
    context: &Transaction<'_>,
    who: Uuid,
    id: Uuid,
    version: u64,
    hierarchy_version: Option<u64>,
) -> Result<Outcome> {
    let Some(old) = team(context, id)? else {
        return Ok(Outcome::Missing);
    };
    if old.version != version {
        return Ok(Outcome::Conflict);
    }
    if (hierarchy_version.is_some() || hierarchy::has_children(context, id)?)
        && record(context)?.map(|org| org.version) != hierarchy_version
    {
        return Ok(Outcome::Conflict);
    }
    // Tombstones preserve references and original submissions. A new team with
    // the same name has a different UUID and cannot inherit these memberships.
    context.query(
        "WITH RECURSIVE tree(id) AS (
            SELECT ?1 UNION SELECT t.id FROM organization_teams t
            JOIN tree ON t.parent=tree.id WHERE t.deleted_at_ms IS NULL
         ) UPDATE organization_teams SET version=version+1,updated_at_ms=MAX(updated_at_ms,?2),
           deleted_at_ms=MAX(updated_at_ms,?2) WHERE id IN (SELECT id FROM tree)",
        &[blob(id), SqlValue::Integer(context.now_ms())],
    )?;
    context.query(
        "DELETE FROM organization_team_hierarchy_requests
         WHERE child IN (SELECT id FROM organization_teams WHERE deleted_at_ms IS NOT NULL)
            OR parent IN (SELECT id FROM organization_teams WHERE deleted_at_ms IS NOT NULL)",
        &[],
    )?;
    advance(context, who, Action::TeamDeleted { team: id })?;
    Ok(Outcome::Applied { id })
}
fn change_member(
    context: &Transaction<'_>,
    who: Uuid,
    id: Uuid,
    person: Uuid,
    version: u64,
    role: Option<Role>,
) -> Result<Outcome> {
    actor(person)?;
    let Some(old_team) = team(context, id)? else {
        return Ok(Outcome::Missing);
    };
    if sync::binding(context, id)?.is_some() {
        return Ok(Outcome::Conflict);
    }
    let old = membership(context, id, person)?;
    if old.version != version {
        return Ok(Outcome::Conflict);
    }
    if role.is_some() && member(context, person)?.is_none_or(|member| !member.active) {
        return Ok(Outcome::NotMember);
    }
    if old.role == role {
        return Ok(Outcome::Applied { id });
    }
    let epoch = membership_epoch(context, person)?;
    context.query("INSERT INTO organization_team_members(team,actor,membership_epoch,role,version) VALUES (?,?,?,?,?) ON CONFLICT(team,actor) DO UPDATE SET membership_epoch=excluded.membership_epoch,role=excluded.role,version=excluded.version",&[blob(id),blob(person),integer(epoch)?,role.map_or(SqlValue::Null,|role|SqlValue::Text(role.name().into())),integer(next(version)?)?])?;
    context.query(
        "UPDATE organization_teams SET version=?,updated_at_ms=MAX(updated_at_ms,?) WHERE id=?",
        &[
            integer(next(old_team.version)?)?,
            SqlValue::Integer(context.now_ms()),
            blob(id),
        ],
    )?;
    advance(
        context,
        who,
        Action::TeamMembership {
            team: id,
            member: person,
            role,
        },
    )?;
    Ok(Outcome::Applied { id })
}
fn set_policy(
    context: &Transaction<'_>,
    who: Uuid,
    version: u64,
    member_creation: bool,
) -> Result<Outcome> {
    let old = policy(context)?;
    if old.version != version {
        return Ok(Outcome::Conflict);
    }
    if old.member_creation != member_creation {
        context.query(
            "UPDATE organization_team_settings SET member_creation=?,version=? WHERE singleton=1",
            &[
                SqlValue::Integer(i64::from(member_creation)),
                integer(next(version)?)?,
            ],
        )?;
        advance(context, who, Action::TeamCreationPolicy { member_creation })?;
    }
    let id = record(context)?
        .ok_or(Error::Invalid("organization is missing"))?
        .id;
    Ok(Outcome::Applied { id })
}

pub(crate) struct Cancel;
impl Mutation for Cancel {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 12;
    type Input = Request;
    type Output = Outcome;
    fn execute(context: &mut Transaction<'_>, input: Request) -> Result<MutationResult<Outcome>> {
        if let Some(old) = recover(context, &input)? {
            return Ok(if matches!(old, Outcome::SubmissionConflict) {
                MutationResult::Rejected(old)
            } else {
                MutationResult::Success(old)
            });
        }
        let id = match input.command {
            Command::Create { .. } => input.request_id,
            Command::Policy { .. } => {
                record(context)?
                    .ok_or(Error::Invalid("organization is missing"))?
                    .id
            }
            Command::Edit { team, .. }
            | Command::Delete { team, .. }
            | Command::Reparent { team, .. }
            | Command::ProposeHierarchy { team, .. }
            | Command::DecideHierarchy { team, .. }
            | Command::Member { team, .. }
            | Command::ReviewRouting { team, .. } => team,
        };
        // Cancellation and mutation consult the same ledger in this Cell's
        // transaction. Whichever commits first determines the permanent result;
        // a late original request cannot run after an acknowledged cancellation.
        remember(context, &input, Outcome::Cancelled { id })
    }
}
