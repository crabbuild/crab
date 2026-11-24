use super::*;

const MAX_GRANTS: usize = 100;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "ProjectAccessRole"))]
pub(crate) enum AccessRole {
    None,
    #[default]
    Read,
    Write,
    Admin,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum AccessTarget {
    Base,
    Member { id: Uuid },
    Team { id: Uuid },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct MemberGrant {
    role: AccessRole,
    epoch: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Policy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base: Option<AccessRole>,
    members: BTreeMap<Uuid, MemberGrant>,
    teams: BTreeMap<Uuid, AccessRole>,
}

impl Policy {
    pub(super) fn is_default(&self) -> bool {
        self.base.is_none() && self.members.is_empty() && self.teams.is_empty()
    }

    pub(super) fn valid(&self) -> bool {
        self.members.len() + self.teams.len() <= MAX_GRANTS
            && self.members.iter().all(|(id, grant)| {
                !id.is_nil() && grant.epoch > 0 && grant.role != AccessRole::None
            })
            && self
                .teams
                .iter()
                .all(|(id, role)| !id.is_nil() && *role != AccessRole::None)
    }

    pub(super) fn change(
        &mut self,
        context: &Transaction<'_>,
        administrator: Uuid,
        target: AccessTarget,
        role: Option<AccessRole>,
        default_version: Option<u64>,
    ) -> Result<Option<&'static str>> {
        match (target, role) {
            (AccessTarget::Base, role) => {
                if role.is_none() && default_version != Some(policy::load(context)?.version) {
                    return Ok(Some(
                        "The organization default changed. Refresh before using it.",
                    ));
                }
                self.base = role;
            }
            (_, None) => return Ok(Some("Choose a role for this collaborator")),
            (AccessTarget::Member { id }, Some(AccessRole::None)) => {
                self.members.remove(&id);
            }
            (AccessTarget::Team { id }, Some(AccessRole::None)) => {
                self.teams.remove(&id);
            }
            (AccessTarget::Member { id }, Some(role)) => {
                actor(id)?;
                if !member(context, id)?.is_some_and(|member| member.active) {
                    return Ok(Some("Choose an active organization member"));
                }
                self.members.insert(
                    id,
                    MemberGrant {
                        role,
                        epoch: epoch(context, id)?,
                    },
                );
            }
            (AccessTarget::Team { id }, Some(role)) => {
                if teams::view(context, administrator, id)?.is_none() {
                    return Ok(Some("Choose an available organization team"));
                }
                self.teams.insert(id, role);
            }
        }
        Ok((!self.valid()).then_some("A Project can have up to 100 member and team grants"))
    }
}

pub(super) struct Viewer {
    person: Uuid,
    epoch: u64,
    owner: bool,
    teams: Vec<Uuid>,
    default_role: AccessRole,
}

impl Viewer {
    pub(super) fn load(
        context: &Transaction<'_>,
        person: Uuid,
        include_teams: bool,
    ) -> Result<Self> {
        let member = member(context, person)?
            .filter(|member| member.active)
            .ok_or(Error::AccessDenied)?;
        let owner = member.role == Role::Owner;
        Ok(Self {
            person,
            epoch: epoch(context, person)?,
            owner,
            default_role: policy::load(context)?.role,
            teams: if include_teams && !owner {
                teams::active_ids(context, person)?
            } else {
                Vec::new()
            },
        })
    }

    pub(super) fn role(&self, project: &Project) -> AccessRole {
        if self.owner || (self.person == project.created_by && self.epoch == project.creator_epoch)
        {
            return AccessRole::Admin;
        }
        let policy = &project.access;
        let direct = policy
            .members
            .get(&self.person)
            .filter(|grant| grant.epoch == self.epoch)
            .map_or(policy.base.unwrap_or(self.default_role), |grant| grant.role);
        self.teams
            .iter()
            .filter_map(|id| policy.teams.get(id))
            .copied()
            .fold(direct, AccessRole::max)
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct GrantView {
    pub target: AccessTarget,
    pub role: AccessRole,
    pub active: bool,
    pub label: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct AccessView {
    pub version: u64,
    pub base: AccessRole,
    pub base_override: Option<AccessRole>,
    pub default_role: AccessRole,
    pub default_version: u64,
    pub grants: Vec<GrantView>,
}

pub(crate) struct GetAccess;
impl Read for GetAccess {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 91;
    type Input = ProjectKey;
    type Output = Option<AccessView>;

    fn execute(context: &mut Transaction<'_>, input: ProjectKey) -> Result<Self::Output> {
        let Some(project) = load(context, input.project)? else {
            return Ok(None);
        };
        if effective_role(context, input.actor, &project)? != AccessRole::Admin {
            return Err(Error::AccessDenied);
        }
        let policy = project.access;
        let default = super::policy::load(context)?;
        let mut grants = Vec::new();
        for (id, grant) in policy.members {
            let active = member(context, id)?.is_some_and(|member| member.active)
                && epoch(context, id)? == grant.epoch;
            grants.push(GrantView {
                target: AccessTarget::Member { id },
                role: grant.role,
                active,
                label: None,
            });
        }
        for (id, role) in policy.teams {
            let active = teams::team(context, id)?.is_some();
            let label = teams::view(context, input.actor, id)?.map(|view| view.team.name);
            grants.push(GrantView {
                target: AccessTarget::Team { id },
                role,
                active,
                label,
            });
        }
        Ok(Some(AccessView {
            version: project.version,
            base: policy.base.unwrap_or(default.role),
            base_override: policy.base,
            default_role: default.role,
            default_version: default.version,
            grants,
        }))
    }
}

pub(super) fn effective_role(
    context: &Transaction<'_>,
    person: Uuid,
    project: &Project,
) -> Result<AccessRole> {
    Ok(Viewer::load(context, person, !project.access.teams.is_empty())?.role(project))
}
