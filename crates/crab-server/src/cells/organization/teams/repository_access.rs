use super::*;
use crate::RepositoryAccess;
use crate::catalog::{MAX_TEAM_GRANTS, RepositoryBaseline, visibility::Visibility};
use std::collections::BTreeMap;

pub(crate) const MAX_TEAM_ACCESS_ROWS: usize = 4096;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryActor {
    pub actor: Uuid,
    pub include_teams: bool,
}

#[derive(Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActorTeams {
    pub owner: bool,
    pub member: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub creation_visibilities: Vec<Visibility>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub member_transfer: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub member_deletion: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub private_forks: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub external_forks: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub member_visibility: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<RepositoryAccess>,
    pub teams: Vec<Uuid>,
    #[serde(default)]
    pub bypass_teams: Vec<Uuid>,
}

pub(crate) struct RepositoryTeamsForActor;
impl Read for RepositoryTeamsForActor {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 42;
    type Input = RepositoryActor;
    type Output = ActorTeams;

    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        actor(input.actor)?;
        let policy = super::super::repository_policy::load(context)?;
        let Some(organization) = member(context, input.actor)?.filter(|member| member.active)
        else {
            return Ok(ActorTeams {
                private_forks: policy.private_forks,
                external_forks: policy.external_forks,
                member_visibility: policy.member_visibility,
                baseline: policy.baseline,
                ..ActorTeams::default()
            });
        };
        let owner = organization.role == OrganizationRole::Owner;
        let mut grant = ActorTeams {
            owner,
            member: true,
            creation_visibilities: if policy.can_create(organization.role) {
                policy.creation_visibilities(organization.role).to_vec()
            } else {
                Vec::new()
            },
            member_transfer: policy.member_transfer,
            member_deletion: policy.member_deletion,
            private_forks: policy.private_forks,
            external_forks: policy.external_forks,
            member_visibility: policy.member_visibility,
            baseline: policy.baseline,
            ..Default::default()
        };
        if !input.include_teams {
            return Ok(grant);
        }
        for row in active_team_rows(context, input.actor, false)? {
            let [team, visible, _] = row.as_slice() else {
                return Err(Error::Invalid("invalid repository team membership"));
            };
            let team = uuid(team)?;
            grant.teams.push(team);
            if uint(visible)? != 0 {
                grant.bypass_teams.push(team);
            }
        }
        Ok(grant)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewTeamQuery {
    pub actor: Uuid,
    pub names: Vec<String>,
    pub include_mine: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub users: Vec<Uuid>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewTeams {
    pub named: Vec<Option<Uuid>>,
    pub mine: Vec<Uuid>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub users: BTreeMap<Uuid, Vec<Uuid>>,
}

pub(crate) struct ResolveReviewTeams;
impl Read for ResolveReviewTeams {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 49;
    type Input = ReviewTeamQuery;
    type Output = ReviewTeams;

    fn execute(context: &mut Transaction<'_>, input: ReviewTeamQuery) -> Result<ReviewTeams> {
        if input.names.len() > 64
            || input.names.iter().any(|name| {
                name.is_empty() || name.len() > 400 || name.chars().any(char::is_control)
            })
        {
            return Err(Error::Invalid("Invalid review team names"));
        }
        let mut named = Vec::with_capacity(input.names.len());
        for name in input.names {
            let rows = context.query(
                "SELECT id FROM organization_teams WHERE name_key=? AND deleted_at_ms IS NULL",
                &[SqlValue::Text(name)],
            )?;
            let id = match rows.rows.first().map(Vec::as_slice) {
                Some([id]) => Some(uuid(id)?),
                None => None,
                _ => return Err(Error::Invalid("Invalid review team result")),
            };
            let visible = id
                .map(|id| view(context, input.actor, id))
                .transpose()?
                .flatten();
            named.push(visible.map(|team| team.team.id));
        }
        let mut mine = Vec::new();
        if input.include_mine {
            for row in active_team_rows(context, input.actor, false)? {
                let [team, _, _] = row.as_slice() else {
                    return Err(Error::Invalid("Invalid review team membership"));
                };
                mine.push(uuid(team)?);
            }
        }
        let users = if input.users.is_empty() {
            BTreeMap::new()
        } else {
            visible_memberships(context, input.actor, input.users)?
        };
        Ok(ReviewTeams { named, mine, users })
    }
}

fn visible_memberships(
    context: &Transaction<'_>,
    viewer: Uuid,
    users: Vec<Uuid>,
) -> Result<BTreeMap<Uuid, Vec<Uuid>>> {
    if users.len() > 64
        || users.windows(2).any(|pair| pair[0] >= pair[1])
        || users.iter().any(Uuid::is_nil)
    {
        return Err(Error::Invalid("Invalid review team accounts"));
    }
    let organization = member(context, viewer)?
        .filter(|member| member.active)
        .ok_or(Error::AccessDenied)?;
    let owner = organization.role == OrganizationRole::Owner;
    let ids = vec!["?"; users.len()].join(",");
    let mut params = vec![blob(viewer), SqlValue::Integer(i64::from(owner))];
    params.extend(users.into_iter().map(blob));
    params.push(SqlValue::Integer((MAX_TEAM_ACCESS_ROWS + 1) as i64));
    // A viewer can learn another member's team assignment only through the
    // same visibility and current membership epoch as the team roster.
    let rows = context.query(
        &format!(
            "SELECT m.actor,m.team FROM organization_team_members m
             JOIN organization_teams t ON t.id=m.team
             JOIN organization_members o ON o.actor=m.actor
             JOIN organization_membership_epochs e ON e.actor=o.actor
             LEFT JOIN organization_team_members v ON v.team=t.id AND v.actor=?1
             LEFT JOIN organization_membership_epochs ve ON ve.actor=v.actor
             WHERE t.deleted_at_ms IS NULL AND {ACTIVE_MEMBERSHIP}
               AND (?2 OR t.visibility='closed' OR (v.role IS NOT NULL AND v.membership_epoch=ve.epoch))
               AND m.actor IN ({ids})
             ORDER BY m.actor,m.team LIMIT ?"
        ),
        &params,
    )?;
    enforce_row_budget(rows.rows.len())?;
    let mut visible = BTreeMap::<_, Vec<_>>::new();
    for row in &rows.rows {
        let [person, team] = row.as_slice() else {
            return Err(Error::Invalid("Invalid visible review team membership"));
        };
        visible.entry(uuid(person)?).or_default().push(uuid(team)?);
    }
    Ok(visible)
}

pub(super) fn active_team_rows(
    context: &Transaction<'_>,
    actor: Uuid,
    include_names: bool,
) -> Result<Vec<Vec<SqlValue>>> {
    ensure_ready(context)?;
    let teams = hierarchy::effective_teams_sql();
    let rows = context.query(
        &format!(
            "WITH RECURSIVE {teams}
             SELECT t.id, t.visibility = 'closed' AND direct.team IS NOT NULL,
               CASE WHEN ?2 THEN t.name END
             FROM effective JOIN organization_teams t ON t.id=effective.team
             LEFT JOIN direct ON direct.team=t.id
             ORDER BY t.id LIMIT ?3"
        ),
        &[
            blob(actor),
            SqlValue::Integer(i64::from(include_names)),
            SqlValue::Integer((MAX_TEAM_ACCESS_ROWS + 1) as i64),
        ],
    )?;
    enforce_row_budget(rows.rows.len())?;
    Ok(rows.rows)
}

#[derive(Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemberRepositoryRoles {
    pub(crate) owner: bool,
    pub(crate) baseline: Option<RepositoryAccess>,
    pub(crate) teams: BTreeMap<Uuid, String>,
}

pub(crate) struct InspectMemberRepositoryRoles;

impl Read for InspectMemberRepositoryRoles {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 48;
    type Input = super::super::MemberKey;
    type Output = Option<MemberRepositoryRoles>;

    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        actor(input.member)?;
        let Some(target) = member(context, input.member)?.filter(|member| member.active) else {
            return Ok(None);
        };
        let teams = active_team_rows(context, input.member, true)?
            .iter()
            .map(|row| match row.as_slice() {
                [team, _, name] => Ok((uuid(team)?, text(name)?)),
                _ => Err(Error::Invalid("invalid repository team membership")),
            })
            .collect::<Result<_>>()?;
        Ok(Some(MemberRepositoryRoles {
            owner: target.role == OrganizationRole::Owner,
            baseline: super::super::repository_policy::load(context)?.baseline,
            teams,
        }))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryRoster {
    pub owners: Vec<Uuid>,
    pub members: Vec<(Uuid, Uuid)>,
    pub baseline: Option<(RepositoryAccess, Vec<Uuid>)>,
}

pub(crate) struct RepositoryTeamRoster;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryRosterQuery {
    pub(crate) teams: Vec<Uuid>,
    pub(crate) baseline: RepositoryBaseline,
}

impl Read for RepositoryTeamRoster {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 43;
    type Input = RepositoryRosterQuery;
    type Output = RepositoryRoster;

    fn execute(context: &mut Transaction<'_>, query: Self::Input) -> Result<Self::Output> {
        let owners = super::super::repositories::Owners::execute(context, ())?;
        let members = team_members(context, query.teams)?;
        let policy = super::super::repository_policy::load(context)?;
        let baseline = if let Some(access) = query.baseline.resolve(policy.baseline) {
            let rows = context.query(
                "SELECT actor FROM organization_members WHERE active=1 AND role='member' ORDER BY actor LIMIT ?",
                &[SqlValue::Integer((MAX_TEAM_ACCESS_ROWS + 1) as i64)],
            )?;
            enforce_row_budget(rows.rows.len())?;
            let members = rows
                .rows
                .iter()
                .map(|row| match row.as_slice() {
                    [actor] => uuid(actor),
                    _ => Err(Error::Invalid("invalid organization baseline roster")),
                })
                .collect::<Result<_>>()?;
            Some((access, members))
        } else {
            None
        };
        Ok(RepositoryRoster {
            owners,
            members,
            baseline,
        })
    }
}

pub(super) fn team_members(
    context: &Transaction<'_>,
    teams: Vec<Uuid>,
) -> Result<Vec<(Uuid, Uuid)>> {
    validate_ids(&teams)?;
    if teams.is_empty() {
        return Ok(Vec::new());
    }
    ensure_ready(context)?;
    let ids = vec!["?"; teams.len()].join(",");
    let mut params = teams.into_iter().map(blob).collect::<Vec<_>>();
    params.push(SqlValue::Integer((MAX_TEAM_ACCESS_ROWS + 1) as i64));
    let descendants = hierarchy::DESCENDANT_STEP;
    let rows = context.query(
        &format!(
            "WITH RECURSIVE team_tree(root,team) AS (
                 SELECT id,id FROM organization_teams WHERE id IN ({ids}) AND deleted_at_ms IS NULL
                 UNION {descendants}
             ) SELECT DISTINCT tree.root,m.actor FROM team_tree tree
                 JOIN organization_team_members m ON m.team=tree.team
                 JOIN organization_members o ON o.actor=m.actor
                 JOIN organization_membership_epochs e ON e.actor=o.actor
                 WHERE {ACTIVE_MEMBERSHIP}
                 ORDER BY tree.root,m.actor LIMIT ?"
        ),
        &params,
    )?;
    enforce_row_budget(rows.rows.len())?;
    rows.rows
        .iter()
        .map(|row| match row.as_slice() {
            [team, actor] => Ok((uuid(team)?, uuid(actor)?)),
            _ => Err(Error::Invalid("invalid repository team roster")),
        })
        .collect()
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryTeamQuery {
    pub actor: Uuid,
    pub teams: Vec<Uuid>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VisibleRepositoryTeams {
    pub owner: bool,
    pub names: Vec<(Uuid, String)>,
}

pub(crate) struct RepositoryTeamNames;
impl Read for RepositoryTeamNames {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 44;
    type Input = RepositoryTeamQuery;
    type Output = VisibleRepositoryTeams;

    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        actor(input.actor)?;
        validate_ids(&input.teams)?;
        let Some(organization) = member(context, input.actor)?.filter(|member| member.active)
        else {
            return Ok(VisibleRepositoryTeams::default());
        };
        ensure_ready(context)?;
        let mut result = VisibleRepositoryTeams {
            owner: organization.role == OrganizationRole::Owner,
            names: Vec::new(),
        };
        for id in input.teams {
            if let Some(visible) = view(context, input.actor, id)? {
                result.names.push((id, visible.team.name));
            }
        }
        Ok(result)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodeOwnerTeamQuery {
    pub teams: Vec<Uuid>,
    pub approvers: Vec<Uuid>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodeOwnerTeam {
    pub id: Uuid,
    pub name: String,
    pub approved: bool,
}

pub(crate) struct CodeOwnerTeams;
impl Read for CodeOwnerTeams {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 45;
    type Input = CodeOwnerTeamQuery;
    type Output = Vec<CodeOwnerTeam>;

    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        validate_ids(&input.teams)?;
        if input.approvers.len() > 100 || input.approvers.iter().any(Uuid::is_nil) {
            return Err(Error::Invalid("invalid code owner approval accounts"));
        }
        if input.teams.is_empty() {
            return Ok(Vec::new());
        }
        ensure_ready(context)?;
        let teams = vec!["?"; input.teams.len()].join(",");
        let descendants = hierarchy::DESCENDANT_STEP;
        let approval = if input.approvers.is_empty() {
            "0".to_owned()
        } else {
            let ids = vec!["?"; input.approvers.len()].join(",");
            format!(
                "EXISTS (SELECT 1 FROM team_tree tree
                     JOIN organization_team_members m ON m.team=tree.team
                     JOIN organization_members o ON o.actor=m.actor
                     JOIN organization_membership_epochs e ON e.actor=o.actor
                     WHERE tree.root=t.id AND m.actor IN ({ids}) AND {ACTIVE_MEMBERSHIP})"
            )
        };
        let params = input
            .teams
            .into_iter()
            .chain(input.approvers)
            .map(blob)
            .collect::<Vec<_>>();
        let rows = context.query(
            &format!("WITH RECURSIVE requested(id) AS (
                          SELECT id FROM organization_teams WHERE id IN ({teams})
                          AND visibility='closed' AND deleted_at_ms IS NULL
                      ), team_tree(root,team) AS (SELECT id,id FROM requested UNION {descendants})
                      SELECT t.id,t.name,{approval} FROM requested JOIN organization_teams t ON t.id=requested.id"),
            &params,
        )?;
        rows.rows
            .iter()
            .map(|row| match row.as_slice() {
                [id, name, approved] => Ok(CodeOwnerTeam {
                    id: uuid(id)?,
                    name: text(name)?,
                    approved: uint(approved)? != 0,
                }),
                _ => Err(Error::Invalid("invalid code owner team row")),
            })
            .collect()
    }
}

fn validate_ids(teams: &[Uuid]) -> Result<()> {
    if teams.len() > MAX_TEAM_GRANTS || teams.iter().any(Uuid::is_nil) {
        return Err(Error::Invalid("invalid repository team IDs"));
    }
    Ok(())
}

fn enforce_row_budget(rows: usize) -> Result<()> {
    if rows > MAX_TEAM_ACCESS_ROWS {
        return Err(Error::Invalid(
            "repository team membership exceeds the read limit",
        ));
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryTeams {
    pub actor: Option<Uuid>,
    pub teams: Vec<Uuid>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_names: bool,
}

#[derive(Default, Deserialize, Serialize)]
pub(crate) struct RepositoryTeamAccess {
    pub owner: bool,
    pub teams: Vec<Uuid>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub team_names: BTreeMap<Uuid, String>,
}

pub(crate) struct GetRepositoryTeamAccess;
impl Read for GetRepositoryTeamAccess {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 17;
    type Input = RepositoryTeams;
    type Output = RepositoryTeamAccess;

    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        if input.teams.len() > crate::catalog::MAX_TEAM_GRANTS
            || input.teams.iter().any(Uuid::is_nil)
        {
            return Err(Error::Invalid("invalid repository team selection"));
        }
        if record(context)?.is_none() {
            return Err(Error::NotInitialized);
        }
        ensure_ready(context)?;
        let current = match input.actor {
            Some(actor) => match member(context, actor)?.filter(|member| member.active) {
                Some(member) => Some(member),
                None => return Ok(RepositoryTeamAccess::default()),
            },
            None => None,
        };
        let owner = current.is_some_and(|member| member.role == OrganizationRole::Owner);
        if input.teams.is_empty() {
            return Ok(RepositoryTeamAccess {
                owner,
                ..RepositoryTeamAccess::default()
            });
        }
        let placeholders = vec!["?"; input.teams.len()].join(",");
        let include_names = owner && input.include_names;
        let mut params = vec![
            input.actor.filter(|_| !owner).map_or(SqlValue::Null, blob),
            SqlValue::Integer(i64::from(include_names)),
        ];
        params.extend(input.teams.into_iter().map(blob));
        let effective = hierarchy::effective_teams_sql();
        // Team deletion and an old organization-membership incarnation must not
        // leave a catalog grant usable after the membership has been removed.
        let rows = context.query(
            &format!(
                "WITH RECURSIVE {effective}
                 SELECT t.id, CASE WHEN ?2 THEN t.name END FROM organization_teams t
                 WHERE t.deleted_at_ms IS NULL AND t.id IN ({placeholders})
                   AND (?1 IS NULL OR t.id IN (SELECT team FROM effective))"
            ),
            &params,
        )?;
        let mut access = RepositoryTeamAccess {
            owner,
            ..RepositoryTeamAccess::default()
        };
        for row in &rows.rows {
            let [id, name] = row.as_slice() else {
                return Err(Error::Invalid("invalid team identity row"));
            };
            let id = uuid(id)?;
            access.teams.push(id);
            if include_names {
                access.team_names.insert(id, text(name)?);
            }
        }
        Ok(access)
    }
}
