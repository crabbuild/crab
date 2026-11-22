use super::*;
use crate::catalog::MAX_REPOSITORY_TEAMS;

const MAX_TEAM_ACCESS_ROWS: usize = 4096;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryActor {
    pub actor: Uuid,
    pub include_teams: bool,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryTeamAccess {
    pub owner: bool,
    pub teams: Vec<Uuid>,
    #[serde(default)]
    pub bypass_teams: Vec<Uuid>,
}

pub(crate) struct RepositoryTeamsForActor;
impl Read for RepositoryTeamsForActor {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 42;
    type Input = RepositoryActor;
    type Output = RepositoryTeamAccess;

    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        actor(input.actor)?;
        let Some(organization) = member(context, input.actor)?.filter(|member| member.active)
        else {
            return Ok(RepositoryTeamAccess::default());
        };
        let owner = organization.role == OrganizationRole::Owner;
        if !input.include_teams {
            return Ok(RepositoryTeamAccess {
                owner,
                teams: Vec::new(),
                bypass_teams: Vec::new(),
            });
        }
        ensure_ready(context)?;
        let rows = context.query(
            &format!(
                "SELECT m.team, t.visibility = 'closed' FROM organization_team_members m
                 JOIN organization_teams t ON t.id=m.team
                 JOIN organization_members o ON o.actor=m.actor
                 JOIN organization_membership_epochs e ON e.actor=o.actor
                 WHERE m.actor=? AND t.deleted_at_ms IS NULL AND {ACTIVE_MEMBERSHIP}
                 ORDER BY m.team LIMIT ?"
            ),
            &[
                blob(input.actor),
                SqlValue::Integer((MAX_TEAM_ACCESS_ROWS + 1) as i64),
            ],
        )?;
        enforce_row_budget(rows.rows.len())?;
        let mut teams = Vec::new();
        let mut bypass_teams = Vec::new();
        for row in &rows.rows {
            let [team, visible] = row.as_slice() else {
                return Err(Error::Invalid("invalid repository team membership"));
            };
            let team = uuid(team)?;
            teams.push(team);
            if uint(visible)? != 0 {
                bypass_teams.push(team);
            }
        }
        Ok(RepositoryTeamAccess {
            owner,
            teams,
            bypass_teams,
        })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryRoster {
    pub owners: Vec<Uuid>,
    pub members: Vec<(Uuid, Uuid)>,
}

pub(crate) struct RepositoryTeamRoster;
impl Read for RepositoryTeamRoster {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 43;
    type Input = Vec<Uuid>;
    type Output = RepositoryRoster;

    fn execute(context: &mut Transaction<'_>, teams: Self::Input) -> Result<Self::Output> {
        validate_ids(&teams)?;
        let owners = super::super::repositories::Owners::execute(context, ())?;
        if teams.is_empty() {
            return Ok(RepositoryRoster {
                owners,
                members: Vec::new(),
            });
        }
        ensure_ready(context)?;
        let ids = vec!["?"; teams.len()].join(",");
        let mut params = teams.into_iter().map(blob).collect::<Vec<_>>();
        params.push(SqlValue::Integer((MAX_TEAM_ACCESS_ROWS + 1) as i64));
        let rows = context.query(
            &format!(
                "SELECT m.team,m.actor FROM organization_team_members m
                 JOIN organization_teams t ON t.id=m.team
                 JOIN organization_members o ON o.actor=m.actor
                 JOIN organization_membership_epochs e ON e.actor=o.actor
                 WHERE m.team IN ({ids}) AND t.deleted_at_ms IS NULL AND {ACTIVE_MEMBERSHIP}
                 ORDER BY m.team,m.actor LIMIT ?"
            ),
            &params,
        )?;
        enforce_row_budget(rows.rows.len())?;
        let members = rows
            .rows
            .iter()
            .map(|row| match row.as_slice() {
                [team, actor] => Ok((uuid(team)?, uuid(actor)?)),
                _ => Err(Error::Invalid("invalid repository team roster")),
            })
            .collect::<Result<_>>()?;
        Ok(RepositoryRoster { owners, members })
    }
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
        let approval = if input.approvers.is_empty() {
            "0".to_owned()
        } else {
            let ids = vec!["?"; input.approvers.len()].join(",");
            format!(
                "EXISTS (SELECT 1 FROM organization_team_members m
                     JOIN organization_members o ON o.actor=m.actor
                     JOIN organization_membership_epochs e ON e.actor=o.actor
                     WHERE m.team=t.id AND m.actor IN ({ids}) AND {ACTIVE_MEMBERSHIP})"
            )
        };
        let teams = vec!["?"; input.teams.len()].join(",");
        let params = input
            .approvers
            .into_iter()
            .chain(input.teams)
            .map(blob)
            .collect::<Vec<_>>();
        let rows = context.query(
            &format!("SELECT t.id, t.name, {approval} FROM organization_teams t
                      WHERE t.id IN ({teams}) AND t.visibility='closed' AND t.deleted_at_ms IS NULL"),
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
    if teams.len() > MAX_REPOSITORY_TEAMS || teams.iter().any(Uuid::is_nil) {
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
