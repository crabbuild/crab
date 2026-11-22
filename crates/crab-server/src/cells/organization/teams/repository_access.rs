use super::*;
use std::collections::BTreeMap;

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
        // Team deletion and an old organization-membership incarnation must not
        // leave a catalog grant usable after the membership has been removed.
        let rows = context.query(
            &format!(
                "SELECT t.id, CASE WHEN ?2 THEN t.name END FROM organization_teams t
                 LEFT JOIN organization_team_members m ON m.team=t.id AND m.actor=?1
                 LEFT JOIN organization_membership_epochs e ON e.actor=m.actor
                 WHERE t.deleted_at_ms IS NULL AND t.id IN ({placeholders})
                   AND (?1 IS NULL OR (m.role IS NOT NULL AND m.membership_epoch=e.epoch))"
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
