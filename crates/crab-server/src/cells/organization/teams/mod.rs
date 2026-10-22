//! Organization-owned teams and membership authority.

use super::{
    Action, Caller, Error, Mutation, MutationResult, Read, Result, Role as OrganizationRole,
    SqlValue, Transaction, actor, advance, blob, integer, member, record, text, uint, uuid,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

mod access;
mod commands;
mod preparation;
mod reads;
pub(super) use access::{authorize, handles};
pub(crate) use commands::{Cancel, Change, Command, Outcome, Request};
pub use preparation::OrganizationTeamState;
pub(crate) use preparation::{GetPreparation, Prepare, PrepareInput};
pub(super) use preparation::{ensure_ready, initialize, membership_changed};
pub(crate) use reads::{
    Candidates, Get, GetMembership, GetPolicy, List, ListInput, MemberCursor, MemberInput, Members,
    Page, QueryCursor,
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Visibility {
    Closed,
    Secret,
}
impl Visibility {
    fn name(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Secret => "secret",
        }
    }
    fn parse(value: &str) -> Result<Self> {
        match value {
            "closed" => Ok(Self::Closed),
            "secret" => Ok(Self::Secret),
            _ => Err(Error::Invalid("team visibility is invalid")),
        }
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Role {
    Member,
    Maintainer,
}
impl Role {
    fn name(self) -> &'static str {
        match self {
            Self::Member => "member",
            Self::Maintainer => "maintainer",
        }
    }
    fn parse(value: &str) -> Result<Self> {
        match value {
            "member" => Ok(Self::Member),
            "maintainer" => Ok(Self::Maintainer),
            _ => Err(Error::Invalid("team role is invalid")),
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Team {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub visibility: Visibility,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct View {
    pub team: Team,
    pub role: Option<Role>,
    pub can_manage: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Membership {
    pub actor: Uuid,
    pub role: Option<Role>,
    pub version: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Policy {
    pub member_creation: bool,
    pub version: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Key {
    pub actor: Uuid,
    pub team: Uuid,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemberKey {
    pub actor: Uuid,
    pub team: Uuid,
    pub member: Uuid,
}

const COLUMNS: &str = "id,name,description,visibility,version,created_at_ms,updated_at_ms";
const ACTIVE_MEMBERSHIP: &str = "m.role IS NOT NULL AND o.active=1 AND m.membership_epoch=e.epoch";
fn team(context: &Transaction<'_>, id: Uuid) -> Result<Option<Team>> {
    actor(id)?;
    context
        .query(
            &format!(
                "SELECT {COLUMNS} FROM organization_teams WHERE id=? AND deleted_at_ms IS NULL"
            ),
            &[blob(id)],
        )?
        .rows
        .first()
        .map(|row| team_row(row))
        .transpose()
}
fn team_row(row: &[SqlValue]) -> Result<Team> {
    let [id, name, description, visibility, version, created, updated] = row else {
        return Err(Error::Invalid("team record is invalid"));
    };
    Ok(Team {
        id: uuid(id)?,
        name: text(name)?,
        description: text(description)?,
        visibility: Visibility::parse(&text(visibility)?)?,
        version: uint(version)?,
        created_at_ms: uint(created)?,
        updated_at_ms: uint(updated)?,
    })
}
fn policy(context: &Transaction<'_>) -> Result<Policy> {
    let rows = context.query(
        "SELECT member_creation,version FROM organization_team_settings WHERE singleton=1",
        &[],
    )?;
    let Some([allowed, version]) = rows.rows.first().map(Vec::as_slice) else {
        return Err(Error::Invalid("team policy is missing"));
    };
    Ok(Policy {
        member_creation: uint(allowed)? == 1,
        version: uint(version)?,
    })
}
fn membership(context: &Transaction<'_>, team: Uuid, person: Uuid) -> Result<Membership> {
    actor(person)?;
    let rows=context.query(&format!("SELECT CASE WHEN {ACTIVE_MEMBERSHIP} THEN m.role END,m.version FROM organization_team_members m JOIN organization_members o ON o.actor=m.actor JOIN organization_membership_epochs e ON e.actor=o.actor WHERE m.team=? AND m.actor=?"),&[blob(team),blob(person)])?;
    let Some([role, version]) = rows.rows.first().map(Vec::as_slice) else {
        return Ok(Membership {
            actor: person,
            role: None,
            version: 0,
        });
    };
    let role = match role {
        SqlValue::Null => None,
        other => Some(Role::parse(&text(other)?)?),
    };
    Ok(Membership {
        actor: person,
        role,
        version: uint(version)?,
    })
}
fn view(context: &Transaction<'_>, person: Uuid, id: Uuid) -> Result<Option<View>> {
    let Some(organization) = member(context, person)?.filter(|member| member.active) else {
        return Err(Error::AccessDenied);
    };
    let Some(team) = team(context, id)? else {
        return Ok(None);
    };
    let role = membership(context, id, person)?.role;
    let owner = organization.role == OrganizationRole::Owner;
    if team.visibility == Visibility::Secret && !owner && role.is_none() {
        return Ok(None);
    }
    Ok(Some(View {
        team,
        role,
        can_manage: owner || role == Some(Role::Maintainer),
    }))
}
fn next(value: u64) -> Result<u64> {
    value
        .checked_add(1)
        .filter(|v| *v <= super::MAX_VERSION)
        .ok_or(Error::Invalid("team version limit reached"))
}

#[cfg(test)]
pub(in crate::cells::organization) mod tests;
