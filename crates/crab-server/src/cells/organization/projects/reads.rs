use super::*;
use planning::grouping::{self, GroupBy};

pub(crate) struct HasIssueItem;
impl Read for HasIssueItem {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 36;
    type Input = (Uuid, u64);
    type Output = bool;

    fn execute(context: &mut Transaction<'_>, (repository, number): Self::Input) -> Result<bool> {
        if repository.is_nil() || number == 0 {
            return Err(Error::Invalid("issue project reference is invalid"));
        }
        if context.document::<bool>(READY)? != Some(true) {
            return Ok(false);
        }
        let found = context.query(
            "SELECT 1 FROM organization_project_repositories r
             JOIN organization_project_items i ON i.project=r.project AND i.repository=r.repository
             WHERE r.repository=? AND i.kind=0 AND i.number=? LIMIT 1",
            &[blob(repository), integer(number)?],
        )?;
        Ok(!found.rows.is_empty())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListQuery {
    pub actor: Uuid,
    pub archived: bool,
    pub search: String,
    pub after: Option<Uuid>,
    pub revision: Option<u64>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Listing {
    pub items: Vec<Project>,
    pub next: Option<Uuid>,
    pub revision: u64,
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
        let rows=context.query("SELECT data FROM organization_projects WHERE archived=? AND id>? AND instr(name_key,?)>0 ORDER BY id LIMIT 31",&[SqlValue::Integer(i64::from(input.archived)),blob(input.after.unwrap_or(Uuid::nil())),SqlValue::Text(input.search.to_lowercase())])?;
        let mut items = rows
            .rows
            .iter()
            .map(|row| serde_json::from_str::<Project>(&text(&row[0])?).map_err(Error::from))
            .collect::<Result<Vec<_>>>()?;
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
        })
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectKey {
    pub actor: Uuid,
    pub project: Uuid,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct View {
    pub project: Project,
    pub repositories: Vec<Uuid>,
    pub can_manage: bool,
}
pub(crate) struct Get;
impl Read for Get {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 32;
    type Input = ProjectKey;
    type Output = Option<View>;
    fn execute(context: &mut Transaction<'_>, input: ProjectKey) -> Result<Option<View>> {
        load(context, input.project)?
            .map(|project| {
                Ok(View {
                    can_manage: can_manage(context, input.actor, &project)?,
                    repositories: repositories(context, input.project)?,
                    project,
                })
            })
            .transpose()
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ItemsQuery {
    pub actor: Uuid,
    pub project: Uuid,
    pub after: Option<Uuid>,
    pub version: Option<u64>,
    pub status: Option<String>,
    pub repository: Option<Uuid>,
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
pub(crate) struct TypeSourcesQuery {
    pub actor: Uuid,
    pub project: Uuid,
    pub repository: Option<Uuid>,
    pub item: Option<Uuid>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TypeSource {
    pub repository: Uuid,
    pub item: Uuid,
    pub number: u64,
    pub status: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TypeSourcePage {
    pub project_version: u64,
    pub items: Vec<TypeSource>,
}

pub(crate) struct TypeSources;
impl Read for TypeSources {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 35;
    type Input = TypeSourcesQuery;
    type Output = Option<TypeSourcePage>;

    fn execute(context: &mut Transaction<'_>, input: TypeSourcesQuery) -> Result<Self::Output> {
        let Some(project) = load(context, input.project)? else {
            return Ok(None);
        };
        // The work cap bounds the cross-Cell join. Read UUIDs and issue
        // numbers alone so attachment, title and draft data stay on their cells.
        let repository = input.repository.map(blob).unwrap_or(SqlValue::Null);
        let item = input.item.map(blob).unwrap_or(SqlValue::Null);
        let rows = context.query(
            "SELECT repository,id,number,status FROM organization_project_items WHERE project=? AND kind=0 AND (? IS NULL OR repository=?) AND (? IS NULL OR id=?) ORDER BY repository,number LIMIT ?",
            &[blob(input.project), repository.clone(), repository, item.clone(), item, integer(MAX_ITEMS + 1)?],
        )?;
        if rows.rows.len() as u64 > MAX_ITEMS {
            return Err(Error::Invalid("Project issue type source limit exceeded"));
        }
        let items = rows
            .rows
            .iter()
            .map(|row| {
                Ok(TypeSource {
                    repository: uuid(&row[0])?,
                    item: uuid(&row[1])?,
                    number: uint(&row[2])?,
                    status: text(&row[3])?,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Some(TypeSourcePage {
            project_version: project.version,
            items,
        }))
    }
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
        let repository = input.repository.map(blob).unwrap_or(SqlValue::Null);
        let status = input
            .status
            .clone()
            .map(SqlValue::Text)
            .unwrap_or(SqlValue::Null);
        let selected = input.item.map(blob).unwrap_or(SqlValue::Null);
        let mut predicate = "FROM organization_project_items p WHERE project=? AND (? IS NULL OR repository=?) AND (? IS NULL OR id=?)".to_owned();
        let mut bindings = vec![
            blob(input.project),
            repository.clone(),
            repository,
            selected.clone(),
            selected,
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
