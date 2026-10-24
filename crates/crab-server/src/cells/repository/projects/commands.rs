use super::*;
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Command {
    SubIssue {
        parent: u64,
        title: String,
        body: String,
    },
    Create {
        id: Uuid,
        name: String,
        template: String,
    },
    Copy {
        id: Uuid,
        source: Uuid,
        version: u64,
        name: String,
    },
    Edit {
        project: Uuid,
        version: u64,
        definition: Definition,
        archived: bool,
    },
    CreateIssue {
        project: Uuid,
        version: u64,
        item: Item,
        parent: Option<u64>,
    },
    Item {
        project: Uuid,
        version: u64,
        item: Item,
    },
    Remove {
        project: Uuid,
        version: u64,
        item: Uuid,
    },
    Parent {
        child: u64,
        parent: Option<u64>,
        version: u64,
    },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub request_id: Uuid,
    pub actor: RepositoryAuthor,
    pub can_write: bool,
    pub command: Command,
    pub cancel: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Outcome {
    Applied { project: Option<Uuid>, version: u64 },
    Cancelled,
    Rejected { reason: String },
    RequestConflict,
}
fn rejected(reason: &str) -> MutationResult<Outcome> {
    MutationResult::Rejected(Outcome::Rejected {
        reason: reason.into(),
    })
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    digest: String,
    outcome: Outcome,
}
pub(crate) struct Change;
impl Mutation for Change {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 391;
    type Input = Request;
    type Output = Outcome;
    fn execute(context: &mut Transaction<'_>, input: Request) -> Result<MutationResult<Outcome>> {
        preparation::ensure_ready(context)?;
        validate_author(&input.actor)?;
        if input.request_id.is_nil() {
            return Err(Error::Invalid("project submission ID is invalid"));
        }
        let digest = blake3::hash(&serde_json::to_vec(&(
            &input.command,
            &input.actor.issuer,
            &input.actor.subject,
        ))?)
        .to_hex()
        .to_string();
        let key = format!("projects/v1/submissions/{}", input.request_id);
        if let Some(saved) = context.document::<Saved>(&key)? {
            return Ok(if saved.digest == digest {
                MutationResult::Success(saved.outcome)
            } else {
                MutationResult::Rejected(Outcome::RequestConflict)
            });
        }
        let outcome = if !input.can_write {
            Outcome::Rejected {
                reason: "Write access is required to manage projects and sub-issues".into(),
            }
        } else if input.cancel {
            Outcome::Cancelled
        } else {
            match context.apply(|context| apply(context, &input))? {
                MutationResult::Success(value) | MutationResult::Rejected(value) => value,
            }
        };
        // A permanent payload-bound decision makes retries safe after response loss
        // and prevents a cancelled or rejected intent executing on a delayed transport.
        context.put_document(
            &key,
            &Saved {
                digest,
                outcome: outcome.clone(),
            },
        )?;
        Ok(MutationResult::Success(outcome))
    }
}
fn new_project(
    context: &Transaction<'_>,
    id: Uuid,
    mut definition: Definition,
    name: &str,
) -> Result<MutationResult<Outcome>> {
    definition.name = name.into();
    definition.is_template = false;
    if id.is_nil() || !definition.valid() {
        return Ok(rejected("Invalid project name or definition"));
    }
    if load(context, id)?.is_some() {
        return Ok(rejected("Project ID already exists"));
    }
    let count = context.query("SELECT COUNT(*) FROM repository_projects", &[])?;
    if result_u64_from_row(&count.rows[0], 0)? >= 200 {
        return Ok(rejected(
            "This repository has reached its 200-project limit",
        ));
    }
    let now = super::super::timestamp(context.now_ms())?;
    save(
        context,
        &Project {
            id,
            definition,
            version: 1,
            archived: false,
            created_at: now,
            updated_at: now,
        },
    )?;
    advance_revision(context)?;
    Ok(MutationResult::Success(Outcome::Applied {
        project: Some(id),
        version: 1,
    }))
}
fn apply(context: &mut Transaction<'_>, input: &Request) -> Result<MutationResult<Outcome>> {
    if super::super::settings::archived(context)? {
        return Ok(rejected("Archived repositories are read-only"));
    }
    match &input.command {
        Command::Create { id, name, template } => {
            let Some(template) = templates().into_iter().find(|t| t.id == *template) else {
                return Ok(rejected("Unknown project template"));
            };
            return new_project(context, *id, template.definition, name);
        }
        Command::Copy {
            id,
            source,
            version,
            name,
        } => {
            let Some(project) = load(context, *source)? else {
                return Ok(rejected("Source project not found"));
            };
            if project.version != *version {
                return Ok(rejected(
                    "The source project changed; reload before copying",
                ));
            }
            return new_project(context, *id, project.definition, name);
        }
        Command::SubIssue {
            parent,
            title,
            body,
        } => {
            if validate_title(title).is_err() || body.len() > 16 * 1024 || body.contains('\0') {
                return Ok(rejected("Invalid sub-issue title or description"));
            }
            let created = super::super::CreateIssue::execute(
                context,
                super::super::CreateIssueInput {
                    submission_id: *input.request_id.as_bytes(),
                    author: input.actor.clone(),
                    title: title.clone(),
                    body: body.clone(),
                    metadata: Default::default(),
                },
            )?;
            let MutationResult::Success(super::super::CreateIssueOutcome::Created(issue)) = created
            else {
                return Err(Error::Invalid("sub-issue creation failed"));
            };
            return hierarchy::change(context, issue.number, Some(*parent), 0, &input.actor);
        }
        Command::Parent {
            child,
            parent,
            version,
        } => return hierarchy::change(context, *child, *parent, *version, &input.actor),
        _ => {}
    }
    if let Command::CreateIssue {
        project,
        version,
        item: entry,
        parent,
    } = &input.command
    {
        let Some(current) = load(context, *project)? else {
            return Ok(rejected("Project not found"));
        };
        if current.version != *version || current.archived {
            return Ok(rejected(
                "The project changed or is archived; refresh before creating an issue",
            ));
        }
        if entry.subject.is_some() || !entry.valid(&current.definition) {
            return Ok(rejected("Invalid new issue"));
        }
        if item(context, *project, entry.id)?.is_some_and(|old| old.subject.is_some()) {
            return Ok(rejected(
                "This project item is already an issue or pull request",
            ));
        }
        let created = super::super::CreateIssue::execute(
            context,
            super::super::CreateIssueInput {
                submission_id: *input.request_id.as_bytes(),
                author: input.actor.clone(),
                title: entry.title.clone(),
                body: entry.body.clone(),
                metadata: Default::default(),
            },
        )?;
        let MutationResult::Success(super::super::CreateIssueOutcome::Created(issue)) = created
        else {
            return Err(Error::Invalid("project issue creation failed"));
        };
        if let Some(parent) = parent
            && let MutationResult::Rejected(outcome) =
                hierarchy::change(context, issue.number, Some(*parent), 0, &input.actor)?
        {
            return Ok(MutationResult::Rejected(outcome));
        }
        // Converting a draft and creating/attaching its canonical issue share
        // this transaction; a rejected membership cannot leave an orphan issue.
        context.query(
            "DELETE FROM repository_project_items WHERE project=? AND id=?",
            &[id(*project), id(entry.id)],
        )?;
        let mut entry = entry.clone();
        entry.subject = Some(Subject {
            kind: SubjectKind::Issue,
            number: issue.number,
        });
        entry.title.clear();
        entry.body.clear();
        let converted = Request {
            command: Command::Item {
                project: *project,
                version: *version,
                item: entry,
            },
            ..input.clone()
        };
        return apply(context, &converted);
    }
    let (project_id, version) = match &input.command {
        Command::Edit {
            project, version, ..
        }
        | Command::Item {
            project, version, ..
        }
        | Command::Remove {
            project, version, ..
        } => (*project, *version),
        _ => return Err(Error::Invalid("unexpected project command")),
    };
    let Some(mut project) = load(context, project_id)? else {
        return Ok(rejected("Project not found"));
    };
    if project.version != version {
        return Ok(rejected(
            "This project changed; refresh and review before saving",
        ));
    }
    match &input.command {
        Command::Edit {
            definition,
            archived,
            ..
        } => {
            if !definition.valid() {
                return Ok(rejected("Invalid project settings, fields, or views"));
            }
            let used = context.query(
                "SELECT DISTINCT status FROM repository_project_items WHERE project=?",
                &[id(project_id)],
            )?;
            if used.rows.iter().any(|row| {
                result_text(row, 0).is_ok_and(|status| !definition.statuses.contains(&status))
            }) {
                return Ok(rejected("Move items before removing a workflow status"));
            }
            if !used.rows.is_empty()
                && project.definition.fields.iter().any(|old| {
                    !definition.fields.iter().any(|new| {
                        new.id == old.id
                            && match (&new.definition, &old.definition) {
                                (
                                    FieldKind::Select { options: new },
                                    FieldKind::Select { options: old },
                                ) => old.iter().all(|option| new.contains(option)),
                                (a, b) => a == b,
                            }
                    })
                })
            {
                return Ok(rejected(
                    "Existing fields and their types must be retained while the project contains items",
                ));
            }
            project.definition = definition.clone();
            project.archived = *archived;
        }
        Command::Item { item: entry, .. } => {
            if project.archived {
                return Ok(rejected("Reopen the project before changing items"));
            }
            if !entry.valid(&project.definition) {
                return Ok(rejected("Invalid project item fields or dates"));
            }
            let previous = item(context, project_id, entry.id)?;
            if previous
                .as_ref()
                .is_some_and(|old| old.subject != entry.subject)
            {
                return Ok(rejected(
                    "An item’s issue or pull request reference cannot be changed",
                ));
            }
            if let Some(subject) = entry.subject {
                if subject.summary(context)?.is_none() {
                    return Ok(rejected("Issue or pull request not found"));
                }
                let same=context.query("SELECT id FROM repository_project_items WHERE project=? AND kind=? AND number=?",&[id(project_id),SqlValue::Integer(kind(subject)),integer(subject.number)?])?;
                if same
                    .rows
                    .first()
                    .is_some_and(|row| result_text(row, 0).is_ok_and(|s| s != entry.id.to_string()))
                {
                    return Ok(rejected("This work is already in the project"));
                }
                if previous.is_none() {
                    let memberships = context.query(
                        "SELECT COUNT(*) FROM repository_project_items WHERE kind=? AND number=?",
                        &[SqlValue::Integer(kind(subject)), integer(subject.number)?],
                    )?;
                    if result_u64_from_row(&memberships.rows[0], 0)? >= 20 {
                        return Ok(rejected(
                            "An issue or pull request can belong to at most 20 projects",
                        ));
                    }
                }
            }
            if previous.is_none() {
                let count = context.query(
                    "SELECT COUNT(*) FROM repository_project_items WHERE project=?",
                    &[id(project_id)],
                )?;
                if result_u64_from_row(&count.rows[0], 0)? >= 1000 {
                    return Ok(rejected("This project has reached its 1,000-item limit"));
                }
            }
            context.query("INSERT INTO repository_project_items(project,id,kind,number,status,data) VALUES (?,?,?,?,?,?) ON CONFLICT(project,id) DO UPDATE SET status=excluded.status,data=excluded.data",&[id(project_id),id(entry.id),entry.subject.map(|s|SqlValue::Integer(kind(s))).unwrap_or(SqlValue::Null),entry.subject.map(|s|integer(s.number)).transpose()?.unwrap_or(SqlValue::Null),SqlValue::Text(entry.status.clone()),SqlValue::Text(serde_json::to_string(entry)?)])?;
            if let Some(subject) = entry.subject {
                activity::append(
                    context,
                    target(subject),
                    &input.actor,
                    activity::Action::ProjectChanged {
                        project: project.id,
                        title: project.definition.name.clone(),
                    },
                )?;
            }
        }
        Command::Remove { item: item_id, .. } => {
            if project.archived {
                return Ok(rejected("Reopen the project before changing items"));
            }
            let Some(entry) = item(context, project_id, *item_id)? else {
                return Ok(rejected("Project item not found"));
            };
            context.query(
                "DELETE FROM repository_project_items WHERE project=? AND id=?",
                &[id(project_id), id(*item_id)],
            )?;
            if let Some(subject) = entry.subject {
                activity::append(
                    context,
                    target(subject),
                    &input.actor,
                    activity::Action::ProjectRemoved {
                        project: project.id,
                        title: project.definition.name.clone(),
                    },
                )?;
            }
        }
        _ => return Err(Error::Invalid("unexpected project command")),
    }
    project.version = super::super::collaboration::next_version(project.version)?;
    project.updated_at = super::super::timestamp(context.now_ms())?;
    save(context, &project)?;
    advance_revision(context)?;
    Ok(MutationResult::Success(Outcome::Applied {
        project: Some(project.id),
        version: project.version,
    }))
}
