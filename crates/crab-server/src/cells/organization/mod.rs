//! Native organization authority, independent of repository routing and grants.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{
    Error, Result,
    application::{
        Mutation, MutationResult, Read, SqlValue, Transaction, invoke_mutation, invoke_read,
    },
};

pub(in crate::cells) const CLASS: &str = "Organization";
pub(in crate::cells) const NAMESPACE: &str = "ORGANIZATIONS";
const SCHEMA_FORMAT: u64 = 3;
pub(crate) const MAX_VERSION: u64 = 9_007_199_254_740_991;

pub(crate) mod automation;
mod commands;
pub(crate) mod development;
pub(crate) mod discovery;
mod invitations;
pub(crate) mod issue_types;
pub(crate) mod projects;
pub(crate) mod properties;
mod reads;
pub(crate) mod repositories;
pub(crate) mod repository_policy;
pub(crate) mod rulesets;
use teams::repository_access::{
    CodeOwnerTeams, InspectMemberRepositoryRoles, RepositoryTeamNames, RepositoryTeamRoster,
    RepositoryTeamsForActor,
};
pub(crate) mod teams;
pub(crate) use commands::{
    Bootstrap, BootstrapInput, ChangeMember, ChangeOutcome, CheckDeparture, Departure,
    DepartureMember, DepartureMode, Edit, EditInput, FinalizeDeparture, MAX_DEPARTURE_MEMBERS,
    MemberChange,
};
#[cfg(test)]
pub(crate) use invitations::Reinstatement;
pub(crate) use invitations::{
    CancelInvitation, DecideInvitation, Decision, FindInvitations, GetInvitation, Invitation,
    InvitationKey, InvitationState, InvitationView, Invite, InviteInput, ListInvitations,
};
pub(crate) use reads::{
    ActiveMembers, ActivityQuery, Caller, Collection, FindMembers, GetMember, GetOrganization,
    ListActivity, ListMembers, MemberBatch, MemberKey, Page, PageCursor, PageQuery, PeopleQuery,
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "OrganizationRole"))]
pub(crate) enum Role {
    Member,
    Owner,
}
impl Role {
    fn name(self) -> &'static str {
        match self {
            Self::Member => "member",
            Self::Owner => "owner",
        }
    }
    fn parse(value: &str) -> Result<Self> {
        match value {
            "member" => Ok(Self::Member),
            "owner" => Ok(Self::Owner),
            _ => Err(Error::Invalid("organization role is invalid")),
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct Organization {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Member {
    pub actor: Uuid,
    pub role: Role,
    pub active: bool,
    pub version: u64,
}
fn actor(id: Uuid) -> Result<()> {
    if id.is_nil() {
        return Err(Error::Invalid("organization actor ID is invalid"));
    }
    Ok(())
}
fn integer(value: u64) -> Result<SqlValue> {
    if value > MAX_VERSION {
        return Err(Error::Invalid("organization version is invalid"));
    }
    Ok(SqlValue::Integer(value as i64))
}
fn blob(id: Uuid) -> SqlValue {
    SqlValue::Blob(id.as_bytes().to_vec())
}
fn uint(value: &SqlValue) -> Result<u64> {
    match value {
        SqlValue::Integer(value) => {
            u64::try_from(*value).map_err(|_| Error::Invalid("organization integer is negative"))
        }
        _ => Err(Error::Invalid("organization integer is invalid")),
    }
}
fn uuid(value: &SqlValue) -> Result<Uuid> {
    match value {
        SqlValue::Blob(value) => Uuid::from_slice(value).map_err(Error::sdk),
        _ => Err(Error::Invalid("organization ID is invalid")),
    }
}
fn text(value: &SqlValue) -> Result<String> {
    match value {
        SqlValue::Text(value) => Ok(value.clone()),
        _ => Err(Error::Invalid("organization text is invalid")),
    }
}
fn member(context: &Transaction<'_>, id: Uuid) -> Result<Option<Member>> {
    actor(id)?;
    context
        .query(
            "SELECT actor, role, active, version FROM organization_members WHERE actor = ?",
            &[blob(id)],
        )?
        .rows
        .first()
        .map(|row| member_row(row))
        .transpose()
}
fn member_row(row: &[SqlValue]) -> Result<Member> {
    let [id, role, active, version] = row else {
        return Err(Error::Invalid("organization member record is invalid"));
    };
    Ok(Member {
        actor: uuid(id)?,
        role: Role::parse(&text(role)?)?,
        active: uint(active)? == 1,
        version: uint(version)?,
    })
}
fn record(context: &Transaction<'_>) -> Result<Option<Organization>> {
    let rows = context.query("SELECT i.id, d.name, d.description, i.app_revision, d.created_at_ms, d.updated_at_ms FROM organization_identity i JOIN organization_details d ON d.singleton = i.singleton WHERE i.singleton = 1", &[])?;
    rows.rows
        .first()
        .map(|row| {
            let [id, name, description, version, created, updated] = row.as_slice() else {
                return Err(Error::Invalid("organization record is invalid"));
            };
            Ok(Organization {
                id: uuid(id)?,
                name: text(name)?,
                description: text(description)?,
                version: uint(version)?,
                created_at_ms: uint(created)?,
                updated_at_ms: uint(updated)?,
            })
        })
        .transpose()
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Action {
    Created,
    MemberConverted {
        member: Uuid,
        version: u64,
        cancelled_invitations: u64,
    },
    Automation {
        identity: Uuid,
        action: automation::Audit,
    },
    RulesetsChanged {
        version: u64,
    },
    RepositoryPropertiesChanged {
        repository: Option<Uuid>,
        version: u64,
    },
    IssueTypesChanged {
        version: u64,
    },
    ProjectChanged {
        project: Uuid,
    },
    TeamCreated {
        team: Uuid,
    },
    TeamEdited {
        team: Uuid,
    },
    TeamGroupSync {
        team: Uuid,
        group: Uuid,
        group_revision: u64,
        change: teams::sync::AuditChange,
        updated: u64,
        removed: u64,
    },
    TeamDeleted {
        team: Uuid,
    },
    TeamMembership {
        team: Uuid,
        member: Uuid,
        role: Option<teams::Role>,
    },
    TeamCreationPolicy {
        member_creation: bool,
    },
    RepositoryPolicyChanged,
    ProjectPolicyChanged {
        before: projects::AccessRole,
        after: projects::AccessRole,
        affected_projects: usize,
    },
    Edited {
        name: bool,
        description: bool,
    },
    Membership {
        member: Uuid,
        role: Option<Role>,
        version: u64,
        cancelled_invitations: u64,
    },
    Invitation {
        recipient: Uuid,
        role: Role,
        version: u64,
        state: InvitationState,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        reinstates: bool,
    },
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Event {
    pub revision: u64,
    pub actor: Option<Uuid>,
    pub action: Action,
    pub created_at_ms: u64,
}
const EVENTS: &str = "organization/v1/events/";
fn advance(context: &Transaction<'_>, actor: Uuid, action: Action) -> Result<()> {
    append_event(context, Some(actor), action)
}
fn append_event(context: &Transaction<'_>, actor: Option<Uuid>, action: Action) -> Result<()> {
    let result = context.query("UPDATE organization_identity SET app_revision = app_revision + 1 WHERE singleton = 1 AND app_revision < 9007199254740991 RETURNING app_revision", &[])?;
    let Some([value]) = result.rows.first().map(Vec::as_slice) else {
        return Err(Error::Invalid("organization revision limit reached"));
    };
    let revision = uint(value)?;
    let created_at_ms = u64::try_from(context.now_ms())
        .map_err(|_| Error::Invalid("organization clock is negative"))?;
    context.put_document(
        &format!("{EVENTS}{revision:016x}"),
        &Event {
            revision,
            actor,
            action,
            created_at_ms,
        },
    )
}
pub(crate) fn details(name: &str, description: &str) -> Result<()> {
    if name.is_empty()
        || name.trim() != name
        || name.chars().count() > 100
        || name.chars().any(char::is_control)
        || description.len() > 4000
        || description.contains('\0')
    {
        return Err(Error::Invalid(
            "organization name or description is invalid",
        ));
    }
    Ok(())
}
pub(in crate::cells) fn initialize(
    context: &Transaction<'_>,
    id: Uuid,
    create: bool,
) -> Result<()> {
    actor(id)?;
    let table = context.query(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'organization_identity'",
        &[],
    )?;
    if table.rows.is_empty() {
        if !create {
            return Err(Error::NotInitialized);
        }
        context.install_schema(include_str!("schema.sql"))?;
        context.query(
            "INSERT INTO organization_identity(singleton, id, format) VALUES (1, ?, ?)",
            &[blob(id), integer(SCHEMA_FORMAT)?],
        )?;
    }
    let rows = context.query(
        "SELECT id, format FROM organization_identity WHERE singleton = 1",
        &[],
    )?;
    let Some([stored, format]) = rows.rows.first().map(Vec::as_slice) else {
        return Err(Error::Invalid("organization identity is missing"));
    };
    if uuid(stored)? != id {
        return Err(Error::Invalid(
            "organization identity does not match its native target",
        ));
    }
    if uint(format)? != SCHEMA_FORMAT {
        return if create {
            Err(Error::Invalid(
                "bootstrap with a fresh organization UUID for this schema",
            ))
        } else {
            // Directory candidates can outlive development-only organizations.
            Err(Error::NotInitialized)
        };
    }
    Ok(())
}

// Current membership protects organization content and authority before receipt
// replay. Teams separately permit actor-bound minimal acknowledgements after
// revocation; their exception cannot replay privileged content or grants.
pub(in crate::cells) fn authorize(
    context: &Transaction<'_>,
    operation: u32,
    mutation: bool,
    input: &Value,
) -> Result<()> {
    // Repository authorization and private binding use a typed, bounded read.
    // The private result contains only matching team IDs and the current owner bit.
    if !mutation && operation == teams::GetRepositoryTeamAccess::ID {
        return Ok(());
    }
    if projects::handles(operation, mutation) {
        return projects::authorize(context, operation, mutation, input);
    }
    if mutation && operation == teams::sync::SyncGroup::ID {
        // Only the durable SCIM reconciler submits this native operation.
        // Owner link edits check active ownership inside the same transaction.
        return Ok(());
    }
    if teams::handles(operation, mutation) {
        return teams::authorize(context, operation, mutation, input);
    }
    if mutation && operation == properties::SetValues::ID {
        return properties::authorize_save(context, input);
    }
    if mutation
        && matches!(
            operation,
            repositories::AuthorizeImportPublication::ID | properties::InitializeImportValues::ID
        )
    {
        #[derive(Deserialize)]
        struct ImportCaller {
            actor: Uuid,
            visibility: crate::catalog::visibility::Visibility,
        }
        let caller: ImportCaller = serde_json::from_value(input.clone())?;
        actor(caller.actor)?;
        return repositories::require_import(context, caller.actor, caller.visibility);
    }
    let owner = match (operation, mutation) {
        // The private repository reads use catalog-bound team IDs. The public
        // repository boundary authorizes their callers before it returns data.
        (
            repositories::Owners::ID
            | RepositoryTeamsForActor::ID
            | teams::review_routing::ReviewRoster::ID
            | RepositoryTeamRoster::ID
            | RepositoryTeamNames::ID
            | CodeOwnerTeams::ID
            | rulesets::Applicable::ID
            | rulesets::NameProtection::ID
            | properties::GetValues::ID
            | properties::search::MatchRepositories::ID
            | properties::search::Fields::ID
            | issue_types::Active::ID
            | automation::Verify::ID,
            false,
        ) => return Ok(()),
        // Linked work is governed by the two repository grants. The trusted
        // HTTP caller checks both before using this organization-owned index.
        (
            development::List::ID
            | development::Get::ID
            | development::descriptions::Pulls::ID
            | development::branches::Candidates::ID,
            false,
        )
        | (
            development::Set::ID
            | development::descriptions::Replace::ID
            | development::branches::Track::ID
            | development::branches::Promote::ID,
            true,
        ) => {
            return Ok(());
        }
        (discovery::GetLanding::ID, false) => return Ok(()),
        // Only trusted catalog recovery finalizes a prepared departure.
        // Self-removal may already have ended the original owner's access.
        (CheckDeparture::ID, false) | (FinalizeDeparture::ID, true) => return Ok(()),
        (GetOrganization::ID | ListMembers::ID | FindMembers::ID, false) => false,
        (
            GetMember::ID
            | ListActivity::ID
            | ListInvitations::ID
            | FindInvitations::ID
            | rulesets::Get::ID
            | properties::GetSchema::ID
            | properties::GetSelection::ID
            | issue_types::Get::ID
            | InspectMemberRepositoryRoles::ID
            | ActiveMembers::ID
            | automation::List::ID
            | repository_policy::Get::ID,
            false,
        )
        | (
            Bootstrap::ID
            | Edit::ID
            | ChangeMember::ID
            | Invite::ID
            | CancelInvitation::ID
            | repository_policy::Set::ID
            | rulesets::Replace::ID
            | properties::SetSchema::ID
            | properties::SetSelection::ID
            | properties::PrepareTransferValues::ID
            | issue_types::Replace::ID
            | automation::Change::ID,
            true,
        ) => true,
        (
            repositories::AuthorizeCreation::ID | repositories::InitializeCreationProperties::ID,
            true,
        )
        | (repositories::GetCreationOptions::ID, false) => {
            let caller: Caller = serde_json::from_value(input.clone())?;
            actor(caller.actor)?;
            let policy = repository_policy::load(context)?;
            return if member(context, caller.actor)?
                .is_some_and(|member| member.active && policy.can_create(member.role))
            {
                Ok(())
            } else {
                Err(Error::AccessDenied)
            };
        }
        // Invitations belong to the organization. Their recipient can inspect
        // and decide an offer without already holding organization membership.
        (GetInvitation::ID, false) => {
            let key: MemberKey = serde_json::from_value(input.clone())?;
            actor(key.actor)?;
            actor(key.member)?;
            if key.actor == key.member {
                return Ok(());
            }
            true
        }
        (DecideInvitation::ID, true) => {
            let decision: Decision = serde_json::from_value(input.clone())?;
            return actor(decision.actor);
        }
        #[cfg(test)]
        (
            teams::tests::WithoutTeams::ID
            | teams::tests::FailedStep::ID
            | projects::tests::WithoutProjects::ID,
            true,
        ) => true,
        _ => return Err(Error::Invalid("unknown organization operation")),
    };
    #[derive(Deserialize)]
    struct Caller {
        actor: Uuid,
    }
    let caller: Caller = serde_json::from_value(input.clone())?;
    actor(caller.actor)?;
    if operation == Bootstrap::ID && mutation && record(context)?.is_none() {
        return Ok(());
    }
    let allowed = member(context, caller.actor)?
        .is_some_and(|member| member.active && (!owner || member.role == Role::Owner));
    if !allowed {
        return Err(Error::AccessDenied);
    }
    Ok(())
}
pub(in crate::cells) fn dispatch_read(
    context: &mut Transaction<'_>,
    operation: u32,
    input: Value,
) -> Result<Value> {
    match operation {
        teams::GetRepositoryTeamAccess::ID => {
            invoke_read::<teams::GetRepositoryTeamAccess>(context, input)
        }
        projects::GetPreparation::ID => invoke_read::<projects::GetPreparation>(context, input),
        projects::GetHistory::ID => invoke_read::<projects::GetHistory>(context, input),
        projects::GetAccess::ID => invoke_read::<projects::GetAccess>(context, input),
        projects::GetDefaultAccess::ID => invoke_read::<projects::GetDefaultAccess>(context, input),
        projects::GetWorkspace::ID => invoke_read::<projects::GetWorkspace>(context, input),
        projects::InspectCollaborators::ID => {
            invoke_read::<projects::InspectCollaborators>(context, input)
        }
        projects::List::ID => invoke_read::<projects::List>(context, input),
        development::List::ID => invoke_read::<development::List>(context, input),
        development::Get::ID => invoke_read::<development::Get>(context, input),
        development::descriptions::Pulls::ID => {
            invoke_read::<development::descriptions::Pulls>(context, input)
        }
        development::branches::Candidates::ID => {
            invoke_read::<development::branches::Candidates>(context, input)
        }
        projects::Get::ID => invoke_read::<projects::Get>(context, input),
        projects::Items::ID => invoke_read::<projects::Items>(context, input),
        projects::BulkSources::ID => invoke_read::<projects::BulkSources>(context, input),
        projects::Sources::ID => invoke_read::<projects::Sources>(context, input),
        projects::GetAttachmentSources::ID => {
            invoke_read::<projects::GetAttachmentSources>(context, input)
        }
        teams::GetPreparation::ID => invoke_read::<teams::GetPreparation>(context, input),
        teams::List::ID => invoke_read::<teams::List>(context, input),
        teams::hierarchy_requests::List::ID => {
            invoke_read::<teams::hierarchy_requests::List>(context, input)
        }
        teams::ResolveReviewTeams::ID => invoke_read::<teams::ResolveReviewTeams>(context, input),
        teams::Get::ID => invoke_read::<teams::Get>(context, input),
        teams::sync::GetSync::ID => invoke_read::<teams::sync::GetSync>(context, input),
        teams::Members::ID => invoke_read::<teams::Members>(context, input),
        teams::GetMembership::ID => invoke_read::<teams::GetMembership>(context, input),
        teams::Candidates::ID => invoke_read::<teams::Candidates>(context, input),
        teams::GetPolicy::ID => invoke_read::<teams::GetPolicy>(context, input),
        RepositoryTeamsForActor::ID => invoke_read::<RepositoryTeamsForActor>(context, input),
        teams::review_routing::ReviewRoster::ID => {
            invoke_read::<teams::review_routing::ReviewRoster>(context, input)
        }
        teams::review_routing::GetSettings::ID => {
            invoke_read::<teams::review_routing::GetSettings>(context, input)
        }
        RepositoryTeamRoster::ID => invoke_read::<RepositoryTeamRoster>(context, input),
        RepositoryTeamNames::ID => invoke_read::<RepositoryTeamNames>(context, input),
        CodeOwnerTeams::ID => invoke_read::<CodeOwnerTeams>(context, input),
        InspectMemberRepositoryRoles::ID => {
            invoke_read::<InspectMemberRepositoryRoles>(context, input)
        }
        rulesets::Get::ID => invoke_read::<rulesets::Get>(context, input),
        rulesets::Applicable::ID => invoke_read::<rulesets::Applicable>(context, input),
        rulesets::NameProtection::ID => invoke_read::<rulesets::NameProtection>(context, input),
        properties::GetSchema::ID => invoke_read::<properties::GetSchema>(context, input),
        issue_types::Get::ID => invoke_read::<issue_types::Get>(context, input),
        issue_types::Active::ID => invoke_read::<issue_types::Active>(context, input),
        properties::GetValues::ID => invoke_read::<properties::GetValues>(context, input),
        properties::search::MatchRepositories::ID => {
            invoke_read::<properties::search::MatchRepositories>(context, input)
        }
        properties::search::Fields::ID => invoke_read::<properties::search::Fields>(context, input),
        properties::GetSelection::ID => invoke_read::<properties::GetSelection>(context, input),
        repositories::GetCreationOptions::ID => {
            invoke_read::<repositories::GetCreationOptions>(context, input)
        }
        repositories::Owners::ID => invoke_read::<repositories::Owners>(context, input),
        repository_policy::Get::ID => invoke_read::<repository_policy::Get>(context, input),
        teams::MentionMembers::ID => invoke_read::<teams::MentionMembers>(context, input),
        automation::List::ID => invoke_read::<automation::List>(context, input),
        automation::Verify::ID => invoke_read::<automation::Verify>(context, input),
        GetOrganization::ID => invoke_read::<GetOrganization>(context, input),
        discovery::GetLanding::ID => invoke_read::<discovery::GetLanding>(context, input),
        ListMembers::ID => invoke_read::<ListMembers>(context, input),
        FindMembers::ID => invoke_read::<FindMembers>(context, input),
        ActiveMembers::ID => invoke_read::<ActiveMembers>(context, input),
        CheckDeparture::ID => invoke_read::<CheckDeparture>(context, input),
        GetMember::ID => invoke_read::<GetMember>(context, input),
        ListActivity::ID => invoke_read::<ListActivity>(context, input),
        GetInvitation::ID => invoke_read::<GetInvitation>(context, input),
        ListInvitations::ID => invoke_read::<ListInvitations>(context, input),
        FindInvitations::ID => invoke_read::<FindInvitations>(context, input),
        _ => Err(Error::Invalid("unknown organization query")),
    }
}
pub(in crate::cells) fn dispatch_mutation(
    context: &mut Transaction<'_>,
    operation: u32,
    input: Value,
) -> Result<MutationResult<Value>> {
    match operation {
        #[cfg(test)]
        projects::tests::WithoutProjects::ID => {
            invoke_mutation::<projects::tests::WithoutProjects>(context, input)
        }
        projects::Prepare::ID => invoke_mutation::<projects::Prepare>(context, input),
        projects::Change::ID => invoke_mutation::<projects::Change>(context, input),
        development::Set::ID => invoke_mutation::<development::Set>(context, input),
        development::descriptions::Replace::ID => {
            invoke_mutation::<development::descriptions::Replace>(context, input)
        }
        development::branches::Track::ID => {
            invoke_mutation::<development::branches::Track>(context, input)
        }
        development::branches::Promote::ID => {
            invoke_mutation::<development::branches::Promote>(context, input)
        }
        projects::ResolveIssue::ID => invoke_mutation::<projects::ResolveIssue>(context, input),
        #[cfg(test)]
        teams::tests::WithoutTeams::ID => {
            invoke_mutation::<teams::tests::WithoutTeams>(context, input)
        }
        #[cfg(test)]
        teams::tests::FailedStep::ID => invoke_mutation::<teams::tests::FailedStep>(context, input),
        teams::Prepare::ID => invoke_mutation::<teams::Prepare>(context, input),
        teams::Change::ID => invoke_mutation::<teams::Change>(context, input),
        teams::sync::SyncGroup::ID => invoke_mutation::<teams::sync::SyncGroup>(context, input),
        teams::Cancel::ID => invoke_mutation::<teams::Cancel>(context, input),
        Bootstrap::ID => invoke_mutation::<Bootstrap>(context, input),
        repository_policy::Set::ID => invoke_mutation::<repository_policy::Set>(context, input),
        repositories::AuthorizeCreation::ID => {
            invoke_mutation::<repositories::AuthorizeCreation>(context, input)
        }
        repositories::InitializeCreationProperties::ID => {
            invoke_mutation::<repositories::InitializeCreationProperties>(context, input)
        }
        repositories::AuthorizeImportPublication::ID => {
            invoke_mutation::<repositories::AuthorizeImportPublication>(context, input)
        }
        rulesets::Replace::ID => invoke_mutation::<rulesets::Replace>(context, input),
        properties::SetSchema::ID => invoke_mutation::<properties::SetSchema>(context, input),
        issue_types::Replace::ID => invoke_mutation::<issue_types::Replace>(context, input),
        properties::SetValues::ID => invoke_mutation::<properties::SetValues>(context, input),
        properties::InitializeImportValues::ID => {
            invoke_mutation::<properties::InitializeImportValues>(context, input)
        }
        properties::PrepareTransferValues::ID => {
            invoke_mutation::<properties::PrepareTransferValues>(context, input)
        }
        properties::SetSelection::ID => invoke_mutation::<properties::SetSelection>(context, input),
        automation::Change::ID => invoke_mutation::<automation::Change>(context, input),
        Edit::ID => invoke_mutation::<Edit>(context, input),
        ChangeMember::ID => invoke_mutation::<ChangeMember>(context, input),
        FinalizeDeparture::ID => invoke_mutation::<FinalizeDeparture>(context, input),
        Invite::ID => invoke_mutation::<Invite>(context, input),
        CancelInvitation::ID => invoke_mutation::<CancelInvitation>(context, input),
        DecideInvitation::ID => invoke_mutation::<DecideInvitation>(context, input),
        _ => Err(Error::Invalid("unknown organization command")),
    }
}

#[cfg(test)]
mod tests;
