use super::*;
use crate::cells::repository::{
    collaboration::{Subject, SubjectKind},
    issues::transfer::publication::Destination,
};
use planning::grouping::{self, GroupBy};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BulkSourceQuery {
    pub actor: AccessActor,
    pub project: Uuid,
    pub items: Vec<Uuid>,
}

pub(crate) struct BulkSources;
impl Read for BulkSources {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 90;
    type Input = BulkSourceQuery;
    type Output = Option<Vec<(Uuid, Subject)>>;

    fn execute(context: &mut Transaction<'_>, input: BulkSourceQuery) -> Result<Self::Output> {
        let Some(project) = load(context, input.project)? else {
            return Ok(None);
        };
        if input.items.len() > ledger::bulk::LIMIT || !can_manage(context, input.actor, &project)? {
            return Ok(None);
        }
        let mut sources = Vec::new();
        for id in input.items {
            let Some(entry) = item(context, input.project, id)? else {
                return Ok(None);
            };
            if let Some((repo, subject)) = entry.repository.zip(entry.item.subject) {
                sources.push((repo, subject));
            }
        }
        Ok(Some(sources))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttachmentSources {
    pub project_version: u64,
    pub issues: Vec<Destination>,
    pub pending: Vec<(Uuid, Uuid)>,
}

pub(crate) struct GetAttachmentSources;
impl Read for GetAttachmentSources {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 36;
    type Input = ProjectKey;
    type Output = Option<AttachmentSources>;

    fn execute(context: &mut Transaction<'_>, input: ProjectKey) -> Result<Self::Output> {
        let Some(project) = load(context, input.project)? else {
            return Ok(None);
        };
        if !can_manage(context, input.actor, &project)? {
            return Ok(None);
        }
        let rows = context.query(
            "SELECT repository,number FROM organization_project_items WHERE project=? AND kind=0 LIMIT ?",
            &[blob(project.id), integer(MAX_ITEMS + 1)?],
        )?;
        if rows.rows.len() as u64 > MAX_ITEMS {
            return Err(Error::Invalid(
                "Project source lookup exceeds the item limit",
            ));
        }
        let issues = rows
            .rows
            .iter()
            .map(|row| {
                Ok(Destination {
                    repository: uuid(&row[0])?,
                    number: uint(&row[1])?,
                })
            })
            .collect::<Result<_>>()?;
        let pending = issue_creation::reservations(context, project.id)?
            .into_values()
            .map(|reservation| (reservation.repository, reservation.request_id))
            .collect();
        Ok(Some(AttachmentSources {
            project_version: project.version,
            issues,
            pending,
        }))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListQuery {
    pub actor: AccessActor,
    pub archived: bool,
    pub search: String,
    pub after: Option<Uuid>,
    pub revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<Uuid>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub linkable: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Listing {
    pub items: Vec<Project>,
    pub next: Option<Uuid>,
    pub revision: u64,
    pub can_create: bool,
}
pub(crate) struct List;
impl Read for List {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 31;
    type Input = ListQuery;
    type Output = Listing;
    fn execute(context: &mut Transaction<'_>, input: ListQuery) -> Result<Listing> {
        let organization = record(context)?.ok_or(Error::NotInitialized)?;
        if input.search.chars().count() > 100 || input.search.chars().any(char::is_control) {
            return Err(Error::Invalid("project search is invalid"));
        }
        if input.after.is_some() && input.revision != Some(organization.version) {
            return Err(Error::Invalid(
                "project page changed; reload the first page",
            ));
        }
        if input.team.is_some() && input.repository.is_some() {
            return Err(Error::Invalid("Choose a team or a repository"));
        }
        if let Some(team) = input.team {
            let team = teams::view(context, input.actor.id(), team)?.ok_or(Error::AccessDenied)?;
            if input.linkable && !team.can_manage {
                return Err(Error::AccessDenied);
            }
        } else if input.linkable && input.repository.is_none() {
            return Err(Error::Invalid(
                "Choose a team or repository before searching for linkable Projects",
            ));
        }
        let viewer = access::Viewer::load(context, input.actor, true)?;
        let search = SqlValue::Text(input.search.to_lowercase());
        let repository = input.repository.map(blob).unwrap_or(SqlValue::Null);
        let linked = SqlValue::Integer(i64::from(!input.linkable));
        let mut after = input.after.unwrap_or(Uuid::nil());
        let mut items = Vec::new();
        // Scan bounded batches so inaccessible projects do not consume visible page slots.
        while items.len() <= 30 {
            let rows = context.query(
                "SELECT p.id,p.data FROM organization_projects p
                 WHERE p.archived=? AND p.id>? AND instr(p.name_key,?)>0
                   AND (? IS NULL OR EXISTS (
                     SELECT 1 FROM organization_project_repositories
                     WHERE project=p.id AND repository=?
                   ) = ?)
                 ORDER BY p.id LIMIT 31",
                &[
                    SqlValue::Integer(i64::from(input.archived)),
                    blob(after),
                    search.clone(),
                    repository.clone(),
                    repository.clone(),
                    linked.clone(),
                ],
            )?;
            for row in &rows.rows {
                after = uuid(&row[0])?;
                let project: Project = serde_json::from_str(&text(&row[1])?)?;
                if project.id != after {
                    return Err(Error::Invalid("organization project record is invalid"));
                }
                let role = viewer.role(&project);
                let matches = match input.team {
                    Some(team) if input.linkable => {
                        role == AccessRole::Admin
                            && !project.access.has_team(team)
                            && !project.archived
                    }
                    Some(team) => role != AccessRole::None && project.access.has_team(team),
                    None if input.linkable => role >= AccessRole::Write && !project.archived,
                    None => role != AccessRole::None,
                };
                if matches {
                    items.push(project);
                    if items.len() > 30 {
                        break;
                    }
                }
            }
            if rows.rows.len() < 31 {
                break;
            }
        }
        let more = items.len() > 30;
        items.truncate(30);
        Ok(Listing {
            next: if more {
                items.last().map(|project| project.id)
            } else {
                None
            },
            items,
            revision: organization.version,
            can_create: input.team.is_none() && input.repository.is_none() && viewer.member(),
        })
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectKey {
    pub actor: AccessActor,
    pub project: Uuid,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct View {
    pub project: Project,
    pub repositories: Vec<Uuid>,
    pub can_manage: bool,
    #[serde(default)]
    pub can_admin: bool,
}
pub(crate) struct Get;
impl Read for Get {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 32;
    type Input = ProjectKey;
    type Output = Option<View>;
    fn execute(context: &mut Transaction<'_>, input: ProjectKey) -> Result<Option<View>> {
        let Some(project) = load(context, input.project)? else {
            return Ok(None);
        };
        let role = access::effective_role(context, input.actor, &project)?;
        if role == AccessRole::None {
            return Ok(None);
        }
        Ok(Some(View {
            can_manage: role >= AccessRole::Write,
            can_admin: role == AccessRole::Admin,
            repositories: repositories(context, input.project)?,
            project,
        }))
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ItemsQuery {
    pub actor: AccessActor,
    pub project: Uuid,
    pub after: Option<Uuid>,
    pub version: Option<u64>,
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<u8>,
    pub item: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched: Option<MatchedItems>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by: Option<GroupBy>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MatchedItems {
    pub project_version: u64,
    pub items: Option<Vec<Uuid>>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourcesQuery {
    pub actor: AccessActor,
    pub project: Uuid,
    pub item: Option<Uuid>,
    pub include_pulls: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<u8>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkSource {
    pub repository: Uuid,
    pub item: Uuid,
    pub subject: Subject,
    pub status: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourcePage {
    pub project_version: u64,
    pub items: Vec<WorkSource>,
}

pub(crate) struct Sources;
impl Read for Sources {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 35;
    type Input = SourcesQuery;
    type Output = Option<SourcePage>;

    fn execute(context: &mut Transaction<'_>, input: SourcesQuery) -> Result<Self::Output> {
        let Some(project) = load(context, input.project)? else {
            return Ok(None);
        };
        // The work cap bounds the cross-Cell join. Read UUIDs and discussion
        // numbers alone so attachment, title and draft data stay on their cells.
        let item = input.item.map(blob).unwrap_or(SqlValue::Null);
        let priority = priority_filter(input.priority)?;
        let rows = context.query(
            "SELECT repository,id,kind,number,status FROM organization_project_items
             WHERE project=? AND kind IN (0,?) AND (? IS NULL OR id=?)
             AND (? IS NULL OR json_extract(data,'$.item.priority')=?)
             ORDER BY repository,kind,number LIMIT ?",
            &[
                blob(input.project),
                SqlValue::Integer(i64::from(input.include_pulls)),
                item.clone(),
                item,
                priority.clone(),
                priority,
                integer(MAX_ITEMS + 1)?,
            ],
        )?;
        if rows.rows.len() as u64 > MAX_ITEMS {
            return Err(Error::Invalid(
                "Project source lookup exceeds the item limit",
            ));
        }
        let items = rows
            .rows
            .iter()
            .map(|row| {
                Ok(WorkSource {
                    repository: uuid(&row[0])?,
                    item: uuid(&row[1])?,
                    subject: Subject {
                        kind: match uint(&row[2])? {
                            0 => SubjectKind::Issue,
                            1 => SubjectKind::Pull,
                            _ => return Err(Error::Invalid("Project work kind is invalid")),
                        },
                        number: uint(&row[3])?,
                    },
                    status: text(&row[4])?,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Some(SourcePage {
            project_version: project.version,
            items,
        }))
    }
}

fn priority_filter(priority: Option<u8>) -> Result<SqlValue> {
    if priority.is_some_and(|priority| priority > planning::MAX_PRIORITY) {
        return Err(Error::Invalid("invalid project priority filter"));
    }
    Ok(priority.map_or(SqlValue::Null, |value| SqlValue::Integer(i64::from(value))))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Entry {
    pub record: Item,
    pub version: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ItemPage {
    pub view: View,
    pub items: Vec<Entry>,
    pub next: Option<Uuid>,
    pub counts: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_counts: Option<BTreeMap<String, u64>>,
    pub total: u64,
}
pub(crate) struct Items;
impl Read for Items {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 33;
    type Input = ItemsQuery;
    type Output = Option<ItemPage>;
    fn execute(context: &mut Transaction<'_>, input: ItemsQuery) -> Result<Option<ItemPage>> {
        let Some(view) = Get::execute(
            context,
            ProjectKey {
                actor: input.actor,
                project: input.project,
            },
        )?
        else {
            return Ok(None);
        };
        if input.after.is_some() && input.version != Some(view.project.version) {
            return Err(Error::Invalid(
                "project items changed; reload the first page",
            ));
        }
        if input
            .matched
            .as_ref()
            .is_some_and(|matched| matched.project_version != view.project.version)
        {
            return Err(Error::StaleSearch);
        }
        if input
            .matched
            .as_ref()
            .and_then(|matched| matched.items.as_ref())
            .is_some_and(|items| items.len() as u64 > MAX_ITEMS)
        {
            return Err(Error::Invalid("Project issue type result limit exceeded"));
        }
        if input
            .status
            .as_ref()
            .is_some_and(|status| !view.project.definition.statuses.contains(status))
        {
            return Err(Error::Invalid("project status filter is invalid"));
        }
        let status = input
            .status
            .clone()
            .map(SqlValue::Text)
            .unwrap_or(SqlValue::Null);
        let selected = input.item.map(blob).unwrap_or(SqlValue::Null);
        let priority = priority_filter(input.priority)?;
        let mut predicate =
            "FROM organization_project_items p WHERE project=? AND (? IS NULL OR id=?)
             AND (? IS NULL OR json_extract(p.data,'$.item.priority')=?)"
                .to_owned();
        let mut bindings = vec![
            blob(input.project),
            selected.clone(),
            selected,
            priority.clone(),
            priority,
        ];
        if let Some(items) = input.matched.and_then(|matched| matched.items) {
            let ids = items
                .iter()
                .map(|id| id.simple().to_string())
                .collect::<Vec<_>>();
            predicate.push_str(" AND lower(hex(id)) IN (SELECT value FROM json_each(?))");
            bindings.push(SqlValue::Text(serde_json::to_string(&ids)?));
        }
        let counts = context
            .query(
                &format!("SELECT status,COUNT(*) {predicate} GROUP BY status"),
                &bindings,
            )?
            .rows
            .iter()
            .map(|row| Ok((text(&row[0])?, uint(&row[1])?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let total = match &input.status {
            Some(status) => *counts.get(status).unwrap_or(&0),
            None => counts.values().sum(),
        };
        predicate.push_str(" AND (? IS NULL OR status=?)");
        bindings.extend([status.clone(), status]);
        bindings.push(blob(input.after.unwrap_or(Uuid::nil())));
        let rows = context.query(
            &format!("SELECT data {predicate} AND id>? ORDER BY id LIMIT 51"),
            &bindings,
        )?;
        let mut items = rows
            .rows
            .iter()
            .map(|row| {
                let record = serde_json::from_str::<Item>(&text(&row[0])?)?;
                let version = ledger::item_version(context, input.project, record.item.id)?;
                if version == 0 {
                    return Err(Error::Invalid("project item revision is missing"));
                }
                Ok(Entry { record, version })
            })
            .collect::<Result<Vec<_>>>()?;
        let more = items.len() > 50;
        items.truncate(50);
        let group_counts = match input.group_by {
            Some(GroupBy::Status) => Some(match &input.status {
                Some(status) => BTreeMap::from([(status.clone(), total)]),
                None => counts.clone(),
            }),
            Some(GroupBy::Field(field)) => Some(grouping::field_counts(
                context,
                &view.project.definition,
                field,
                "$.item.values",
                &predicate,
                &bindings[..bindings.len() - 1],
                items.iter().map(|row| &row.record.item),
            )?),
            _ => None,
        };
        Ok(Some(ItemPage {
            view,
            next: if more {
                items.last().map(|item| item.record.item.id)
            } else {
                None
            },
            items,
            counts,
            group_counts,
            total,
        }))
    }
}
