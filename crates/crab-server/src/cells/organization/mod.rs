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
const MAX_VERSION: u64 = 9_007_199_254_740_991;

mod commands;
mod invitations;
mod reads;
pub(crate) mod teams;
pub(crate) use commands::{
    Bootstrap, BootstrapInput, ChangeMember, ChangeOutcome, Edit, EditInput, MemberChange,
};
pub(crate) use invitations::{
    CancelInvitation, DecideInvitation, Decision, GetInvitation, Invitation, InvitationKey,
    InvitationState, Invite, InviteInput, ListInvitations,
};
pub(crate) use reads::{
    Caller, Collection, GetMember, GetOrganization, ListActivity, ListMembers, MemberKey, Page,
    PageCursor, PageQuery,
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
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
    TeamCreated {
        team: Uuid,
    },
    TeamEdited {
        team: Uuid,
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
    Edited {
        name: bool,
        description: bool,
    },
    Membership {
        member: Uuid,
        role: Option<Role>,
        version: u64,
    },
    Invitation {
        recipient: Uuid,
        role: Role,
        version: u64,
        state: InvitationState,
    },
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Event {
    pub revision: u64,
    pub actor: Uuid,
    pub action: Action,
    pub created_at_ms: u64,
}
const EVENTS: &str = "organization/v1/events/";
fn advance(context: &Transaction<'_>, actor: Uuid, action: Action) -> Result<()> {
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
            "INSERT INTO organization_identity(singleton, id, format) VALUES (1, ?, 1)",
            &[blob(id)],
        )?;
    }
    let rows = context.query(
        "SELECT id, format FROM organization_identity WHERE singleton = 1",
        &[],
    )?;
    let Some([stored, format]) = rows.rows.first().map(Vec::as_slice) else {
        return Err(Error::Invalid("organization identity is missing"));
    };
    if uuid(stored)? != id || uint(format)? != 1 {
        return Err(Error::Invalid(
            "organization identity does not match its native target",
        ));
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
    if teams::handles(operation, mutation) {
        return teams::authorize(context, operation, mutation, input);
    }
    let owner = match (operation, mutation) {
        (GetOrganization::ID | ListMembers::ID, false) => false,
        (GetMember::ID | ListActivity::ID | ListInvitations::ID, false)
        | (Bootstrap::ID | Edit::ID | ChangeMember::ID | Invite::ID | CancelInvitation::ID, true) => {
            true
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
        (teams::tests::WithoutTeams::ID | teams::tests::FailedStep::ID, true) => true,
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
        teams::GetPreparation::ID => invoke_read::<teams::GetPreparation>(context, input),
        teams::List::ID => invoke_read::<teams::List>(context, input),
        teams::Get::ID => invoke_read::<teams::Get>(context, input),
        teams::Members::ID => invoke_read::<teams::Members>(context, input),
        teams::GetMembership::ID => invoke_read::<teams::GetMembership>(context, input),
        teams::Candidates::ID => invoke_read::<teams::Candidates>(context, input),
        teams::GetPolicy::ID => invoke_read::<teams::GetPolicy>(context, input),
        GetOrganization::ID => invoke_read::<GetOrganization>(context, input),
        ListMembers::ID => invoke_read::<ListMembers>(context, input),
        GetMember::ID => invoke_read::<GetMember>(context, input),
        ListActivity::ID => invoke_read::<ListActivity>(context, input),
        GetInvitation::ID => invoke_read::<GetInvitation>(context, input),
        ListInvitations::ID => invoke_read::<ListInvitations>(context, input),
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
        teams::tests::WithoutTeams::ID => {
            invoke_mutation::<teams::tests::WithoutTeams>(context, input)
        }
        #[cfg(test)]
        teams::tests::FailedStep::ID => invoke_mutation::<teams::tests::FailedStep>(context, input),
        teams::Prepare::ID => invoke_mutation::<teams::Prepare>(context, input),
        teams::Change::ID => invoke_mutation::<teams::Change>(context, input),
        teams::Cancel::ID => invoke_mutation::<teams::Cancel>(context, input),
        Bootstrap::ID => invoke_mutation::<Bootstrap>(context, input),
        Edit::ID => invoke_mutation::<Edit>(context, input),
        ChangeMember::ID => invoke_mutation::<ChangeMember>(context, input),
        Invite::ID => invoke_mutation::<Invite>(context, input),
        CancelInvitation::ID => invoke_mutation::<CancelInvitation>(context, input),
        DecideInvitation::ID => invoke_mutation::<DecideInvitation>(context, input),
        _ => Err(Error::Invalid("unknown organization command")),
    }
}

#[cfg(test)]
mod tests;
