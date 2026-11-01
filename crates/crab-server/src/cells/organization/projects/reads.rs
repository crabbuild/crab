use super::*;

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
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ItemPage {
    pub view: View,
    pub items: Vec<Item>,
    pub next: Option<Uuid>,
    pub counts: BTreeMap<String, u64>,
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
        let counts=context.query("SELECT status,COUNT(*) FROM organization_project_items WHERE project=? AND (? IS NULL OR repository=?) GROUP BY status",&[blob(input.project),repository.clone(),repository.clone()])?.rows.iter().map(|row|Ok((text(&row[0])?,uint(&row[1])?))).collect::<Result<BTreeMap<_,_>>>()?;
        let total = match &input.status {
            Some(status) => *counts.get(status).unwrap_or(&0),
            None => counts.values().sum(),
        };
        let rows=context.query("SELECT data FROM organization_project_items WHERE project=? AND id>? AND (? IS NULL OR status=?) AND (? IS NULL OR repository=?) ORDER BY id LIMIT 51",&[blob(input.project),blob(input.after.unwrap_or(Uuid::nil())),status.clone(),status,repository.clone(),repository])?;
        let mut items = rows
            .rows
            .iter()
            .map(|row| serde_json::from_str::<Item>(&text(&row[0])?).map_err(Error::from))
            .collect::<Result<Vec<_>>>()?;
        let more = items.len() > 50;
        items.truncate(50);
        Ok(Some(ItemPage {
            view,
            next: if more {
                items.last().map(|item| item.item.id)
            } else {
                None
            },
            items,
            counts,
            total,
        }))
    }
}
