use super::*;
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectQuery {
    pub after: Option<Uuid>,
    pub archived: bool,
    pub templates: bool,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectPage {
    pub items: Vec<Project>,
    pub next: Option<Uuid>,
}
pub(crate) struct ListProjects;
impl Read for ListProjects {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 391;
    type Input = ProjectQuery;
    type Output = ProjectPage;
    fn execute(context: &mut Transaction<'_>, input: ProjectQuery) -> Result<Self::Output> {
        preparation::ensure_ready(context)?;
        let rows=context.query("SELECT data FROM repository_projects WHERE archived=? AND (?=0 OR is_template=1) AND id>? ORDER BY id LIMIT 21",&[SqlValue::Integer(i64::from(input.archived)),SqlValue::Integer(i64::from(input.templates)),id(input.after.unwrap_or(Uuid::nil()))])?;
        let mut items = rows
            .rows
            .iter()
            .map(|row| Ok(serde_json::from_str::<Project>(&result_text(row, 0)?)?))
            .collect::<Result<Vec<_>>>()?;
        let more = items.len() > 20;
        items.truncate(20);
        Ok(ProjectPage {
            next: if more {
                items.last().map(|p| p.id)
            } else {
                None
            },
            items,
        })
    }
}
pub(crate) struct GetProject;
impl Read for GetProject {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 392;
    type Input = Uuid;
    type Output = Option<Project>;
    fn execute(context: &mut Transaction<'_>, input: Uuid) -> Result<Self::Output> {
        preparation::ensure_ready(context)?;
        load(context, input)
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ItemQuery {
    pub project: Uuid,
    pub after: Option<Uuid>,
    pub revision: Option<u64>,
    pub query: String,
    pub status: Option<String>,
    pub hide_closed: bool,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Row {
    pub item: Item,
    pub work: Option<SubjectSummary>,
    pub assignees: Vec<super::super::assignments::Assignee>,
    pub progress: Option<hierarchy::Progress>,
    pub parent: Option<SubjectSummary>,
    pub links: Vec<SubjectSummary>,
    pub more_links: bool,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ItemPage {
    pub project: Project,
    pub revision: u64,
    pub items: Vec<Row>,
    pub next: Option<Uuid>,
    pub counts: BTreeMap<String, u64>,
    pub total: u64,
}
pub(crate) struct Items;
impl Read for Items {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 393;
    type Input = ItemQuery;
    type Output = Option<ItemPage>;
    fn execute(context: &mut Transaction<'_>, input: ItemQuery) -> Result<Self::Output> {
        preparation::ensure_ready(context)?;
        if !text_valid(&input.query, 256, false) {
            return Err(Error::Invalid("invalid project filter"));
        }
        let Some(project) = load(context, input.project)? else {
            return Ok(None);
        };
        if input
            .status
            .as_ref()
            .is_some_and(|s| !project.definition.statuses.contains(s))
        {
            return Err(Error::Invalid("unknown project workflow status"));
        }
        let revision = context.query(
            "SELECT app_revision FROM repository_identity WHERE singleton=1",
            &[],
        )?;
        let revision = result_u64_from_row(&revision.rows[0], 0)?;
        if input.after.is_some() && input.revision != Some(revision) {
            return Err(Error::StaleSearch);
        }
        let predicate = "FROM repository_project_items p LEFT JOIN repository_issues i ON p.kind=0 AND p.number=i.number LEFT JOIN repository_pulls r ON p.kind=1 AND p.number=r.number WHERE p.project=? AND (? IS NULL OR p.status=?) AND (?=0 OR COALESCE(i.state,r.state,0)=0) AND instr(lower(COALESCE(i.title,r.title,json_extract(p.data,'$.title'))),lower(?))>0";
        let mut args = vec![
            id(project.id),
            input
                .status
                .clone()
                .map(SqlValue::Text)
                .unwrap_or(SqlValue::Null),
            input
                .status
                .clone()
                .map(SqlValue::Text)
                .unwrap_or(SqlValue::Null),
            SqlValue::Integer(i64::from(input.hide_closed)),
            SqlValue::Text(input.query),
        ];
        let count = context.query(
            &format!("SELECT p.status,COUNT(*) {predicate} GROUP BY p.status"),
            &args,
        )?;
        let counts = count
            .rows
            .iter()
            .map(|r| Ok((result_text(r, 0)?, result_u64_from_row(r, 1)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        args.push(id(input.after.unwrap_or(Uuid::nil())));
        let rows = context.query(
            &format!("SELECT p.data,COALESCE(i.assignees,r.assignees) {predicate} AND p.id>? ORDER BY p.id LIMIT 51"),
            &args,
        )?;
        let mut items = Vec::new();
        for row in rows.rows.iter().take(50) {
            let entry: Item = serde_json::from_str(&result_text(row, 0)?)?;
            if !entry.valid(&project.definition) {
                return Err(Error::Invalid("stored project item is invalid"));
            }
            let mut links = Vec::new();
            let mut more_links = false;
            let mut parent = None;
            let mut progress = None;
            let work = entry
                .subject
                .map(|s| s.summary(context))
                .transpose()?
                .flatten();
            if let Some(subject) = entry.subject {
                if work.is_none() {
                    return Err(Error::Invalid("project source is missing"));
                }
                if subject.kind == SubjectKind::Issue {
                    progress = Some(hierarchy::progress(context, subject.number)?);
                    parent = hierarchy::parent(context, subject.number)?
                        .0
                        .map(|n| {
                            Subject {
                                kind: SubjectKind::Issue,
                                number: n,
                            }
                            .summary(context)
                        })
                        .transpose()?
                        .flatten();
                }
                let page = super::super::collaboration::links::ListLinks::execute(
                    context,
                    super::super::collaboration::links::Query {
                        subject,
                        before: None,
                        limit: 3,
                    },
                )?
                .ok_or(Error::Invalid("project links source is missing"))?;
                links = page.items.into_iter().map(|item| item.work).collect();
                more_links = page.next.is_some();
            }
            let assignees = match row.get(1) {
                Some(SqlValue::Blob(bytes)) => super::super::assignments::decode(bytes)?,
                Some(SqlValue::Null) => vec![],
                _ => return Err(Error::Invalid("project source assignments are invalid")),
            };
            items.push(Row {
                assignees,
                item: entry,
                work,
                progress,
                parent,
                links,
                more_links,
            });
        }
        let next = if rows.rows.len() > 50 {
            items.last().map(|r| r.item.id)
        } else {
            None
        };
        let total = counts.values().sum();
        Ok(Some(ItemPage {
            project,
            revision,
            items,
            next,
            counts,
            total,
        }))
    }
}
pub(crate) struct Memberships;
impl Read for Memberships {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 395;
    type Input = Subject;
    type Output = Option<Vec<(Project, Item)>>;
    fn execute(context: &mut Transaction<'_>, subject: Subject) -> Result<Self::Output> {
        preparation::ensure_ready(context)?;
        if subject.summary(context)?.is_none() {
            return Ok(None);
        }
        let rows=context.query("SELECT x.data,p.data FROM repository_project_items x JOIN repository_projects p ON p.id=x.project WHERE x.kind=? AND x.number=? ORDER BY x.project LIMIT 21",&[SqlValue::Integer(kind(subject)),integer(subject.number)?])?;
        if rows.rows.len() > 20 {
            return Err(Error::Invalid("too many project memberships"));
        }
        rows.rows
            .iter()
            .map(|r| {
                Ok((
                    serde_json::from_str(&result_text(r, 1)?)?,
                    serde_json::from_str(&result_text(r, 0)?)?,
                ))
            })
            .collect::<Result<Vec<_>>>()
            .map(Some)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CandidateQuery {
    pub q: String,
    pub kind: Option<SubjectKind>,
}
pub(crate) struct Candidates;
impl Read for Candidates {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 396;
    type Input = CandidateQuery;
    type Output = Vec<SubjectSummary>;
    fn execute(context: &mut Transaction<'_>, input: CandidateQuery) -> Result<Self::Output> {
        preparation::ensure_ready(context)?;
        if !text_valid(&input.q, 256, false) {
            return Err(Error::Invalid("invalid work search"));
        }
        if let Some(number) = input
            .q
            .strip_prefix('#')
            .and_then(|n| n.parse::<u64>().ok())
        {
            validate_number(number)?;
            let mut items = Vec::new();
            for kind in [SubjectKind::Issue, SubjectKind::Pull] {
                if input.kind.is_none_or(|selected| selected == kind)
                    && let Some(work) = (Subject { kind, number }).summary(context)?
                {
                    items.push(work);
                }
            }
            return Ok(items);
        }
        let literal = if input.q.is_empty() {
            None
        } else {
            Some(format!("in:title {}", serde_json::to_string(&input.q)?))
        };
        let suffix = match input.kind {
            Some(SubjectKind::Issue) => "is:issue",
            Some(SubjectKind::Pull) => "is:pr",
            None => "",
        };
        let query = Some(
            format!("{} {suffix}", literal.unwrap_or_default())
                .trim()
                .to_owned(),
        );
        let page = super::super::ListDiscussions::execute(
            context,
            super::super::SearchInput {
                query,
                limit: 20,
                ..Default::default()
            },
        )?;
        page.items
            .into_iter()
            .map(|item| {
                let subject = match item {
                    super::super::DiscussionSummary::Issue(issue) => Subject {
                        kind: SubjectKind::Issue,
                        number: issue.number,
                    },
                    super::super::DiscussionSummary::Pull(pull) => Subject {
                        kind: SubjectKind::Pull,
                        number: pull.number,
                    },
                };
                subject
                    .summary(context)?
                    .ok_or(Error::Invalid("work search source is missing"))
            })
            .collect()
    }
}
