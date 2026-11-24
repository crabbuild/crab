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
    Outside { id: Uuid },
}

// The private HTTP bridge vouches for a current direct repository grant.
// A bare account retains the existing wire shape and has no guest authority.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub(crate) enum AccessActor {
    Account(Uuid),
    Guest { id: Uuid },
}

impl AccessActor {
    pub(crate) fn id(self) -> Uuid {
        match self {
            Self::Account(id) | Self::Guest { id } => id,
        }
    }

    pub(crate) fn guest(self) -> bool {
        matches!(self, Self::Guest { .. })
    }
}

impl From<Uuid> for AccessActor {
    fn from(id: Uuid) -> Self {
        Self::Account(id)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct MemberGrant {
    role: AccessRole,
    epoch: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct OutsideGrant {
    role: AccessRole,
    member_version: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Policy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base: Option<AccessRole>,
    members: BTreeMap<Uuid, MemberGrant>,
    teams: BTreeMap<Uuid, AccessRole>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    outside: BTreeMap<Uuid, OutsideGrant>,
}

impl Policy {
    pub(super) fn has_team(&self, team: Uuid) -> bool {
        self.teams.contains_key(&team)
    }

    pub(super) fn is_default(&self) -> bool {
        self.base.is_none()
            && self.members.is_empty()
            && self.teams.is_empty()
            && self.outside.is_empty()
    }

    pub(super) fn valid(&self) -> bool {
        self.members.len() + self.teams.len() + self.outside.len() <= MAX_GRANTS
            && self.members.iter().all(|(id, grant)| {
                !id.is_nil() && grant.epoch > 0 && grant.role != AccessRole::None
            })
            && self
                .teams
                .iter()
                .all(|(id, role)| !id.is_nil() && *role != AccessRole::None)
            && self.outside.iter().all(|(id, grant)| {
                !id.is_nil()
                    && grant.role != AccessRole::None
                    && grant.member_version.is_none_or(|version| version > 0)
            })
    }

    pub(super) fn change(
        &mut self,
        context: &Transaction<'_>,
        administrator: Uuid,
        target: AccessTarget,
        role: Option<AccessRole>,
        default_version: Option<u64>,
        eligible_outside: Option<Uuid>,
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
            (AccessTarget::Outside { id }, Some(AccessRole::None)) => {
                self.outside.remove(&id);
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
            (AccessTarget::Outside { id }, Some(role)) => {
                actor(id)?;
                let membership = member(context, id)?;
                if eligible_outside != Some(id)
                    || membership.as_ref().is_some_and(|member| member.active)
                {
                    return Ok(Some("Choose a current outside repository collaborator"));
                }
                self.outside.insert(
                    id,
                    OutsideGrant {
                        role,
                        member_version: membership.map(|member| member.version),
                    },
                );
            }
        }
        Ok((!self.valid()).then_some("A Project can have up to 100 explicit grants"))
    }
}

pub(super) struct Viewer {
    person: Uuid,
    epoch: Option<u64>,
    member_version: Option<u64>,
    owner: bool,
    teams: Vec<Uuid>,
    default_role: AccessRole,
}

impl Viewer {
    pub(super) fn load(
        context: &Transaction<'_>,
        caller: AccessActor,
        include_teams: bool,
    ) -> Result<Self> {
        let person = caller.id();
        let membership = member(context, person)?;
        let active = membership.as_ref().filter(|member| member.active);
        if active.is_none() && !caller.guest() {
            return Err(Error::AccessDenied);
        }
        let owner = active.is_some_and(|member| member.role == Role::Owner);
        Ok(Self {
            person,
            epoch: active.map(|_| epoch(context, person)).transpose()?,
            member_version: membership.as_ref().map(|member| member.version),
            owner,
            default_role: policy::load(context)?.role,
            teams: if include_teams && !owner && active.is_some() {
                teams::active_ids(context, person)?
            } else {
                Vec::new()
            },
        })
    }

    pub(super) fn member(&self) -> bool {
        self.epoch.is_some()
    }

    pub(super) fn role(&self, project: &Project) -> AccessRole {
        let policy = &project.access;
        let Some(epoch) = self.epoch else {
            return policy
                .outside
                .get(&self.person)
                .filter(|grant| grant.member_version == self.member_version)
                .map_or(AccessRole::None, |grant| grant.role);
        };
        if self.owner || (self.person == project.created_by && epoch == project.creator_epoch) {
            return AccessRole::Admin;
        }
        let direct = policy
            .members
            .get(&self.person)
            .filter(|grant| grant.epoch == epoch)
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
    pub can_manage_teams: bool,
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
        let viewer = Viewer::load(context, input.actor, !project.access.teams.is_empty())?;
        if viewer.role(&project) != AccessRole::Admin {
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
            if !viewer.member() {
                break;
            }
            let active = teams::team(context, id)?.is_some();
            let label = teams::view(context, input.actor.id(), id)?.map(|view| view.team.name);
            grants.push(GrantView {
                target: AccessTarget::Team { id },
                role,
                active,
                label,
            });
        }
        for (id, grant) in policy.outside {
            let membership = member(context, id)?;
            let active = !membership.as_ref().is_some_and(|member| member.active)
                && membership.map(|member| member.version) == grant.member_version;
            grants.push(GrantView {
                target: AccessTarget::Outside { id },
                role: grant.role,
                active,
                label: None,
            });
        }
        Ok(Some(AccessView {
            version: project.version,
            base: policy.base.unwrap_or(default.role),
            base_override: policy.base,
            default_role: default.role,
            default_version: default.version,
            grants,
            can_manage_teams: viewer.member(),
        }))
    }
}

pub(super) fn effective_role(
    context: &Transaction<'_>,
    person: AccessActor,
    project: &Project,
) -> Result<AccessRole> {
    Ok(Viewer::load(context, person, !project.access.teams.is_empty())?.role(project))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectCaller {
    pub actor: AccessActor,
}

#[derive(Deserialize, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "OrganizationProjectWorkspace"))]
pub(crate) struct WorkspaceAccess {
    pub name: String,
    pub can_create: bool,
}

pub(crate) struct GetWorkspace;
impl Read for GetWorkspace {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 94;
    type Input = ProjectCaller;
    type Output = WorkspaceAccess;

    fn execute(context: &mut Transaction<'_>, input: ProjectCaller) -> Result<WorkspaceAccess> {
        let organization = record(context)?.ok_or(Error::NotInitialized)?;
        Ok(WorkspaceAccess {
            name: organization.name,
            can_create: member(context, input.actor.id())?.is_some_and(|member| member.active),
        })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CollaboratorInspection {
    pub actor: AccessActor,
    pub project: Uuid,
    pub candidates: Vec<Uuid>,
}

pub(crate) struct InspectCollaborators;
impl Read for InspectCollaborators {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 95;
    type Input = CollaboratorInspection;
    type Output = Vec<Uuid>;

    fn execute(
        context: &mut Transaction<'_>,
        input: CollaboratorInspection,
    ) -> Result<Self::Output> {
        let project = load(context, input.project)?.ok_or(Error::AccessDenied)?;
        if effective_role(context, input.actor, &project)? != AccessRole::Admin {
            return Err(Error::AccessDenied);
        }
        if input.candidates.len() > 20 {
            return Err(Error::Invalid("Choose at most 20 collaborator candidates"));
        }
        let mut members = Vec::new();
        for id in input.candidates {
            if member(context, id)?.is_some_and(|member| member.active) {
                members.push(id);
            }
        }
        Ok(members)
    }
}
