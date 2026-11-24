//! Organization-owned planning; repository grants and source work stay independent.

use super::*;
use crate::cells::repository::projects as planning;
use std::collections::BTreeMap;

mod access;
mod commands;
mod history;
mod issue_creation;
mod policy;
mod reads;
use crate::cells::planning as ledger;
pub(crate) use access::{
    AccessActor, AccessRole, AccessTarget, CollaboratorInspection, GetAccess, GetWorkspace,
    InspectCollaborators, ProjectCaller, WorkspaceAccess,
};
pub(crate) use commands::{AttachmentCheck, Change, Command, Outcome, Request};
pub(crate) use history::{GetHistory, HistoryQuery};
pub(in crate::cells) use issue_creation::due;
pub(crate) use issue_creation::{ResolveInput, ResolveIssue};
pub(crate) use policy::{GetDefaultAccess, Preview as DefaultAccessPreview};
pub(crate) use reads::{
    BulkSourceQuery, BulkSources, Get, GetAttachmentSources, Items, ItemsQuery, List, ListQuery,
    MatchedItems, ProjectKey, Sources, SourcesQuery, View, WorkSource,
};

const READY: &str = "organization/projects/v1/ready";
const MAX_ITEMS: u64 = 5000;
const MAX_REPOSITORIES: usize = 20;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Planned,
    InProgress,
    Paused,
    Completed,
    Cancelled,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Health {
    OnTrack,
    AtRisk,
    OffTrack,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Properties {
    pub status: Status,
    pub health: Health,
    pub lead: Option<Uuid>,
    pub start: Option<String>,
    pub target: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Project {
    pub id: Uuid,
    pub definition: planning::Definition,
    pub properties: Properties,
    pub version: u64,
    pub archived: bool,
    pub created_at: u64,
    pub updated_at: u64,
    pub created_by: Uuid,
    pub creator_epoch: u64,
    #[serde(default, skip_serializing_if = "access::Policy::is_default")]
    access: access::Policy,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Item {
    pub repository: Option<Uuid>,
    pub item: planning::Item,
}
fn ensure_ready(context: &Transaction<'_>) -> Result<()> {
    if context.document::<bool>(READY)? != Some(true) {
        return Err(Error::SchemaUnavailable);
    }
    ledger::ensure_ready(context)
}
pub(in crate::cells::organization) fn initialize(context: &Transaction<'_>) -> Result<()> {
    context.install_schema(include_str!("schema.sql"))?;
    context.put_document(READY, &true)?;
    ledger::initialize(context)
}
fn epoch(context: &Transaction<'_>, person: Uuid) -> Result<u64> {
    let rows = context.query(
        "SELECT epoch FROM organization_membership_epochs WHERE actor=?",
        &[blob(person)],
    )?;
    rows.rows
        .first()
        .and_then(|row| row.first())
        .map(uint)
        .transpose()?
        .ok_or(Error::AccessDenied)
}
fn can_manage(context: &Transaction<'_>, person: AccessActor, project: &Project) -> Result<bool> {
    Ok(access::effective_role(context, person, project)? >= AccessRole::Write)
}
fn load(context: &Transaction<'_>, project: Uuid) -> Result<Option<Project>> {
    context
        .query(
            "SELECT data FROM organization_projects WHERE id=?",
            &[blob(project)],
        )?
        .rows
        .first()
        .map(|row| {
            let record: Project = serde_json::from_str(&text(&row[0])?)?;
            if record.id != project
                || record.version == 0
                || record.version > MAX_VERSION
                || !record.definition.valid()
                || !record.access.valid()
            {
                return Err(Error::Invalid("organization project record is invalid"));
            }
            Ok(record)
        })
        .transpose()
}
fn save(context: &Transaction<'_>, project: &Project) -> Result<()> {
    context.query("INSERT INTO organization_projects(id,archived,name_key,data) VALUES(?,?,?,?) ON CONFLICT(id) DO UPDATE SET archived=excluded.archived,name_key=excluded.name_key,data=excluded.data", &[blob(project.id),SqlValue::Integer(i64::from(project.archived)),SqlValue::Text(project.definition.name.to_lowercase()),SqlValue::Text(serde_json::to_string(project)?)])?;
    Ok(())
}
fn repositories(context: &Transaction<'_>, project: Uuid) -> Result<Vec<Uuid>> {
    context.query("SELECT repository FROM organization_project_repositories WHERE project=? ORDER BY repository", &[blob(project)])?.rows.iter().map(|row|uuid(&row[0])).collect()
}
fn item(context: &Transaction<'_>, project: Uuid, id: Uuid) -> Result<Option<Item>> {
    context
        .query(
            "SELECT data FROM organization_project_items WHERE project=? AND id=?",
            &[blob(project), blob(id)],
        )?
        .rows
        .first()
        .map(|row| serde_json::from_str(&text(&row[0])?).map_err(Error::from))
        .transpose()
}
fn properties_valid(context: &Transaction<'_>, properties: &Properties) -> Result<bool> {
    Ok(properties.start.as_deref().is_none_or(planning::date_valid)
        && properties
            .target
            .as_deref()
            .is_none_or(planning::date_valid)
        && match (&properties.start, &properties.target) {
            (Some(a), Some(b)) => a <= b,
            _ => true,
        }
        && match properties.lead {
            Some(person) => member(context, person)?.is_some_and(|member| member.active),
            None => true,
        })
}
pub(crate) struct Prepare;
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrepareInput {
    pub actor: Uuid,
    pub version: u64,
}
pub(crate) struct GetPreparation;
impl Read for GetPreparation {
    const CLASS: &'static str = super::CLASS;
    const ID: u32 = 30;
    type Input = Caller;
    type Output = crate::cells::repository::collaboration::lifecycle::LifecycleStatus;
    fn execute(context: &mut Transaction<'_>, _: Caller) -> Result<Self::Output> {
        ledger::status(context)
    }
}
impl Mutation for Prepare {
    const CLASS: &'static str = super::CLASS;
    const ID: u32 = 30;
    type Input = PrepareInput;
    type Output = crate::cells::repository::collaboration::lifecycle::LifecycleStatus;
    fn execute(
        context: &mut Transaction<'_>,
        input: PrepareInput,
    ) -> Result<MutationResult<Self::Output>> {
        teams::ensure_ready(context)?;
        if context.document::<bool>(READY)? != Some(true) {
            initialize(context)?;
        }
        Ok(MutationResult::Success(ledger::prepare(
            context,
            ledger::Owner::Organization,
            input.version,
        )?))
    }
}
pub(in crate::cells::organization) fn handles(operation: u32, mutation: bool) -> bool {
    if mutation {
        matches!(operation, Prepare::ID | Change::ID | ResolveIssue::ID)
    } else {
        matches!(
            operation,
            GetPreparation::ID
                | List::ID
                | Get::ID
                | Items::ID
                | GetHistory::ID
                | Sources::ID
                | GetAttachmentSources::ID
                | BulkSources::ID
                | GetAccess::ID
                | GetDefaultAccess::ID
                | GetWorkspace::ID
                | InspectCollaborators::ID
        )
    }
}
pub(in crate::cells::organization) fn authorize(
    context: &Transaction<'_>,
    operation: u32,
    mutation: bool,
    input: &Value,
) -> Result<()> {
    if mutation && operation == ResolveIssue::ID {
        // The private resolver only settles an accepted reservation with the
        // bound repository result. It cannot create a new Project write.
        return ensure_ready(context);
    }
    #[derive(Deserialize)]
    struct Person {
        actor: AccessActor,
        project: Option<Uuid>,
    }
    let who: Person = serde_json::from_value(input.clone())?;
    actor(who.actor.id())?;
    let membership = member(context, who.actor.id())?;
    if operation == Prepare::ID {
        return if membership.is_some_and(|member| member.active && member.role == Role::Owner) {
            Ok(())
        } else {
            Err(Error::AccessDenied)
        };
    }
    ensure_ready(context)?;
    if mutation {
        let request: Request = serde_json::from_value(input.clone())?;
        // Minimal, actor-bound terminal outcomes remain recoverable after access
        // removal. No protected project or source content passes this exception.
        if commands::recover(context, &request)?
            .is_some_and(|receipt| membership.is_some() || receipt != Outcome::RequestConflict)
        {
            return Ok(());
        }
        if request.cancel && (membership.is_some() || who.actor.guest()) {
            return Ok(());
        }
        let active = membership.as_ref().filter(|member| member.active);
        if let Some(id) = request.command.project() {
            let Some(project) = load(context, id)? else {
                return Err(Error::AccessDenied);
            };
            let required = match request.command {
                Command::SetAccess {
                    target: AccessTarget::Team { .. },
                    ..
                } if active.is_none() => return Err(Error::AccessDenied),
                Command::SetAccess { .. } => AccessRole::Admin,
                Command::Edit { archived, .. } if archived != project.archived => AccessRole::Admin,
                _ => AccessRole::Write,
            };
            if access::effective_role(context, who.actor, &project)? < required {
                return Err(Error::AccessDenied);
            }
        } else if active.is_none()
            || (matches!(request.command, Command::SetDefaultAccess { .. })
                && !active.is_some_and(|member| member.role == Role::Owner))
        {
            return Err(Error::AccessDenied);
        }
        return Ok(());
    }
    let active = membership.is_some_and(|member| {
        member.active && (operation != GetDefaultAccess::ID || member.role == Role::Owner)
    });
    let guest = who.actor.guest() && !matches!(operation, GetDefaultAccess::ID);
    if !(active || guest) {
        return Err(Error::AccessDenied);
    }
    if !matches!(operation, Get::ID | Items::ID | GetAccess::ID)
        && let Some(id) = who.project
        && let Some(project) = load(context, id)?
        && access::effective_role(context, who.actor, &project)? == AccessRole::None
    {
        return Err(Error::AccessDenied);
    }
    Ok(())
}

#[cfg(test)]
pub(in crate::cells::organization) mod tests;
