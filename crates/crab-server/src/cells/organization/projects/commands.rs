use super::*;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Command {
    Create {
        id: Uuid,
        name: String,
        template: String,
    },
    Edit {
        project: Uuid,
        version: u64,
        definition: planning::Definition,
        archived: bool,
    },
    Planning {
        project: Uuid,
        version: u64,
        properties: Properties,
    },
    Repository {
        project: Uuid,
        version: u64,
        repository: Uuid,
        linked: bool,
    },
    Item {
        project: Uuid,
        version: u64,
        repository: Option<Uuid>,
        item: planning::Item,
    },
    Remove {
        project: Uuid,
        version: u64,
        item: Uuid,
    },
}
impl Command {
    pub(super) fn project(&self) -> Option<Uuid> {
        match self {
            Self::Create { .. } => None,
            Self::Edit { project, .. }
            | Self::Planning { project, .. }
            | Self::Repository { project, .. }
            | Self::Item { project, .. }
            | Self::Remove { project, .. } => Some(*project),
        }
    }
    fn version(&self) -> u64 {
        match self {
            Self::Create { .. } => 0,
            Self::Edit { version, .. }
            | Self::Planning { version, .. }
            | Self::Repository { version, .. }
            | Self::Item { version, .. }
            | Self::Remove { version, .. } => *version,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub actor: Uuid,
    pub request_id: Uuid,
    pub command: Command,
    pub cancel: bool,
    // Repository access is checked by HTTP, independently of project ownership.
    // It is excluded from the digest so a lost reply can recover after revocation.
    pub source_allowed: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Outcome {
    Applied { project: Option<Uuid>, version: u64 },
    Cancelled,
    Rejected { reason: String },
    RequestConflict,
}
#[derive(Deserialize, Serialize)]
struct Saved {
    digest: String,
    outcome: Outcome,
}
fn digest(input: &Request) -> Result<String> {
    Ok(
        blake3::hash(&serde_json::to_vec(&(input.actor, &input.command))?)
            .to_hex()
            .to_string(),
    )
}
fn key(input: &Request) -> String {
    format!("organization/projects/v1/submissions/{}", input.request_id)
}
pub(super) fn recover(context: &Transaction<'_>, input: &Request) -> Result<Option<Outcome>> {
    actor(input.request_id)?;
    let expected = digest(input)?;
    Ok(context.document::<Saved>(&key(input))?.map(|saved| {
        if expected == saved.digest {
            saved.outcome
        } else {
            Outcome::RequestConflict
        }
    }))
}
fn rejected(reason: &str) -> MutationResult<Outcome> {
    MutationResult::Rejected(Outcome::Rejected {
        reason: reason.into(),
    })
}
pub(crate) struct Change;
impl Mutation for Change {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 31;
    type Input = Request;
    type Output = Outcome;
    fn execute(context: &mut Transaction<'_>, input: Request) -> Result<MutationResult<Outcome>> {
        ensure_ready(context)?;
        actor(input.actor)?;
        actor(input.request_id)?;
        if let Some(outcome) = recover(context, &input)? {
            return Ok(MutationResult::Success(outcome));
        }
        let outcome = if input.cancel {
            Outcome::Cancelled
        } else {
            match context.apply(|context| apply(context, &input))? {
                MutationResult::Success(outcome) | MutationResult::Rejected(outcome) => outcome,
            }
        };
        context.put_document(
            &key(&input),
            &Saved {
                digest: digest(&input)?,
                outcome: outcome.clone(),
            },
        )?;
        Ok(MutationResult::Success(outcome))
    }
}
fn apply(context: &mut Transaction<'_>, input: &Request) -> Result<MutationResult<Outcome>> {
    if let Command::Create { id, name, template } = &input.command {
        let Some(template) = planning::templates()
            .into_iter()
            .find(|candidate| candidate.id == *template)
        else {
            return Ok(rejected("Choose a project template"));
        };
        let mut definition = template.definition;
        definition.name = name.clone();
        definition.is_template = false;
        if id.is_nil() || !definition.valid() {
            return Ok(rejected("Enter a project name of 1–100 characters"));
        }
        if load(context, *id)?.is_some() {
            return Ok(rejected("This project ID is already in use"));
        }
        let count = context.query("SELECT COUNT(*) FROM organization_projects", &[])?;
        if uint(&count.rows[0][0])? >= 200 {
            return Ok(rejected(
                "This organization has reached its 200-project limit",
            ));
        }
        let now = u64::try_from(context.now_ms())
            .map_err(|_| Error::Invalid("project clock is negative"))?;
        let project = Project {
            id: *id,
            definition,
            properties: Properties {
                status: Status::Planned,
                health: Health::OnTrack,
                lead: Some(input.actor),
                start: None,
                target: None,
            },
            version: 1,
            archived: false,
            created_at: now,
            updated_at: now,
            created_by: input.actor,
            creator_epoch: epoch(context, input.actor)?,
        };
        save(context, &project)?;
        advance(
            context,
            input.actor,
            Action::ProjectChanged {
                project: project.id,
            },
        )?;
        return Ok(MutationResult::Success(Outcome::Applied {
            project: Some(project.id),
            version: 1,
        }));
    }
    let Some(project_id) = input.command.project() else {
        return Err(Error::Invalid("project target is missing"));
    };
    let Some(mut project) = load(context, project_id)? else {
        return Ok(rejected("Project not found"));
    };
    if project.version != input.command.version() {
        return Ok(rejected(
            "The project changed. Review its current version before saving your draft.",
        ));
    }
    if project.version >= MAX_VERSION {
        return Ok(rejected("Project version limit reached"));
    }
    if project.archived && !matches!(input.command, Command::Edit { .. }) {
        return Ok(rejected("Archived projects are read-only"));
    }
    match &input.command {
        Command::Edit {
            definition,
            archived,
            ..
        } => {
            if !definition.valid() {
                return Ok(rejected(
                    "Check the project name, workflow, fields and views",
                ));
            }
            // Source title/state predicates require repository-local indexes.
            // Shared views promise only filters owned by this project's Cell.
            if definition.is_template
                || definition
                    .views
                    .iter()
                    .any(|view| !view.query.is_empty() || view.hide_closed)
            {
                return Ok(rejected(
                    "Shared project views support layouts and planning-status filters",
                ));
            }
            let rows = context.query(
                "SELECT data FROM organization_project_items WHERE project=?",
                &[blob(project_id)],
            )?;
            for row in rows.rows {
                let item: Item = serde_json::from_str(&text(&row[0])?)?;
                if !item.item.valid(definition) {
                    return Ok(rejected(
                        "Existing work uses a removed status or field. Update those items first.",
                    ));
                }
            }
            project.definition = definition.clone();
            project.archived = *archived;
        }
        Command::Planning { properties, .. } => {
            if !properties_valid(context, properties)? {
                return Ok(rejected(
                    "Choose a current organization member and valid start/target dates",
                ));
            }
            project.properties = properties.clone();
        }
        Command::Repository {
            repository, linked, ..
        } => {
            if repository.is_nil() || !input.source_allowed {
                return Ok(rejected(
                    "Repository access is required to change this link",
                ));
            }
            let linked_repositories = repositories(context, project_id)?;
            if *linked {
                if !linked_repositories.contains(repository)
                    && linked_repositories.len() >= MAX_REPOSITORIES
                {
                    return Ok(rejected("A project can link up to 20 repositories"));
                }
                context.query("INSERT INTO organization_project_repositories(project,repository) VALUES(?,?) ON CONFLICT DO NOTHING",&[blob(project_id),blob(*repository)])?;
            } else {
                if !context.query("SELECT 1 FROM organization_project_items WHERE project=? AND repository=? LIMIT 1",&[blob(project_id),blob(*repository)])?.rows.is_empty() {return Ok(rejected("Remove this repository's project items before unlinking it"));}
                context.query("DELETE FROM organization_project_repositories WHERE project=? AND repository=?",&[blob(project_id),blob(*repository)])?;
            }
        }
        Command::Item {
            repository,
            item: next,
            ..
        } => {
            if !next.valid(&project.definition) || repository.is_some() != next.subject.is_some() {
                return Ok(rejected("Check the work item and its repository"));
            }
            if let Some(repository) = repository
                && (!input.source_allowed
                    || !repositories(context, project_id)?.contains(repository))
            {
                return Ok(rejected("Choose accessible work from a linked repository"));
            }
            let old = item(context, project_id, next.id)?;
            if old.as_ref().is_some_and(|old| {
                old.repository != *repository || old.item.subject != next.subject
            }) {
                return Ok(rejected(
                    "An existing project item's source cannot be replaced",
                ));
            }
            if old.is_none() {
                let count = context.query(
                    "SELECT COUNT(*) FROM organization_project_items WHERE project=?",
                    &[blob(project_id)],
                )?;
                if uint(&count.rows[0][0])? >= MAX_ITEMS {
                    return Ok(rejected("This project has reached its 5,000-item limit"));
                }
            }
            let (kind, number) = match next.subject {
                Some(subject) => (
                    SqlValue::Integer(
                        if subject.kind
                            == crate::cells::repository::collaboration::SubjectKind::Issue
                        {
                            0
                        } else {
                            1
                        },
                    ),
                    integer(subject.number)?,
                ),
                None => (SqlValue::Null, SqlValue::Null),
            };
            let repository_value = repository.map(blob).unwrap_or(SqlValue::Null);
            if next.subject.is_some() && !context.query("SELECT 1 FROM organization_project_items WHERE project=? AND repository=? AND kind=? AND number=? AND id!=?",&[blob(project_id),repository_value.clone(),kind.clone(),number.clone(),blob(next.id)])?.rows.is_empty() {return Ok(rejected("This work is already in the project"));}
            let data = Item {
                repository: *repository,
                item: next.clone(),
            };
            context.query("INSERT INTO organization_project_items(project,id,repository,kind,number,status,data) VALUES(?,?,?,?,?,?,?) ON CONFLICT(project,id) DO UPDATE SET status=excluded.status,data=excluded.data",&[blob(project_id),blob(next.id),repository_value,kind,number,SqlValue::Text(next.status.clone()),SqlValue::Text(serde_json::to_string(&data)?)])?;
        }
        Command::Remove { item, .. } => {
            context.query(
                "DELETE FROM organization_project_items WHERE project=? AND id=?",
                &[blob(project_id), blob(*item)],
            )?;
        }
        Command::Create { .. } => {
            return Err(Error::Invalid("project create reached item dispatch"));
        }
    }
    project.version += 1;
    project.updated_at = project.updated_at.max(
        u64::try_from(context.now_ms()).map_err(|_| Error::Invalid("project clock is negative"))?,
    );
    save(context, &project)?;
    advance(
        context,
        input.actor,
        Action::ProjectChanged {
            project: project_id,
        },
    )?;
    Ok(MutationResult::Success(Outcome::Applied {
        project: Some(project_id),
        version: project.version,
    }))
}
