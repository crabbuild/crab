use super::*;
use crate::cells::planning::{self as ledger, bulk};
use crate::cells::repository::issues::issue_type;
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
    ColumnLimit {
        project: Uuid,
        version: u64,
        column: grouping::ColumnLimit,
    },
    CreateIssue {
        project: Uuid,
        version: u64,
        item: Item,
        parent: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        type_id: Option<Uuid>,
    },
    Item {
        project: Uuid,
        version: u64,
        item: Item,
    },
    BulkFields {
        project: Uuid,
        items: Vec<bulk::Edit>,
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
    ReorderSubIssue {
        parent: u64,
        child: u64,
        sibling: u64,
        position: hierarchy::Position,
        order_token: String,
    },
}
impl Command {
    pub(crate) fn is_hierarchy(&self) -> bool {
        matches!(
            self,
            Self::SubIssue { .. } | Self::Parent { .. } | Self::ReorderSubIssue { .. }
        )
    }

    pub(crate) fn permitted(&self, can_write: bool, can_triage: bool) -> bool {
        can_write || (can_triage && self.is_hierarchy())
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub request_id: Uuid,
    pub actor: RepositoryAuthor,
    pub can_write: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub can_triage: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_type: Option<issue_type::Selection>,
    pub command: Command,
    pub cancel: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Outcome {
    Applied {
        project: Option<Uuid>,
        version: u64,
    },
    Cancelled,
    Rejected {
        reason: String,
    },
    ItemConflict {
        project: Uuid,
        item: Uuid,
        version: u64,
    },
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
        let outcome = if !input.command.permitted(input.can_write, input.can_triage) {
            Outcome::Rejected {
                reason: if input.command.is_hierarchy() {
                    "Triage access is required to manage sub-issues"
                } else {
                    "Write access is required to manage projects"
                }
                .into(),
            }
        } else if input.cancel {
            Outcome::Cancelled
        } else {
            match context.apply(|context| apply(context, &input, &input.command))? {
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
    actor: &RepositoryAuthor,
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
    let project = Project {
        id,
        definition,
        version: 1,
        archived: false,
        created_at: now,
        updated_at: now,
    };
    save(context, &project)?;
    history::append(
        context,
        actor,
        &project,
        ledger::Target::Project,
        None,
        Some(history::project_snapshot(&project)),
    )?;
    advance_revision(context)?;
    Ok(MutationResult::Success(Outcome::Applied {
        project: Some(id),
        version: 1,
    }))
}
fn apply(
    context: &mut Transaction<'_>,
    input: &Request,
    command: &Command,
) -> Result<MutationResult<Outcome>> {
    if super::super::settings::archived(context)? {
        return Ok(rejected("Archived repositories are read-only"));
    }
    match command {
        Command::BulkFields { project, items } => {
            return apply_bulk(context, input, *project, items);
        }
        Command::Create { id, name, template } => {
            let Some(template) = templates().into_iter().find(|t| t.id == *template) else {
                return Ok(rejected("Unknown project template"));
            };
            return new_project(context, *id, template.definition, name, &input.actor);
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
            return new_project(context, *id, project.definition, name, &input.actor);
        }
        Command::SubIssue {
            parent,
            title,
            body,
        } => {
            if validate_title(title).is_err() || body.len() > 16 * 1024 || body.contains('\0') {
                return Ok(rejected("Invalid sub-issue title or description"));
            }
            let created = super::super::CreateIssue::for_project(
                context,
                super::super::CreateIssueInput {
                    submission_id: *input.request_id.as_bytes(),
                    author: input.actor.clone(),
                    title: title.clone(),
                    body: body.clone(),
                    metadata: Default::default(),
                },
                None,
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
        Command::ReorderSubIssue {
            parent,
            child,
            sibling,
            position,
            order_token,
        } => return hierarchy::reorder(context, *parent, *child, *sibling, *position, order_token),
        _ => {}
    }
    let (project_id, version) = match command {
        Command::Edit {
            project, version, ..
        }
        | Command::ColumnLimit {
            project, version, ..
        }
        | Command::Item {
            project, version, ..
        }
        | Command::CreateIssue {
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
    if matches!(command, Command::Edit { .. } | Command::ColumnLimit { .. })
        && project.version != version
    {
        return Ok(rejected(
            "This project changed; refresh and review before saving",
        ));
    }
    let mut event_target = ledger::Target::Project;
    let mut before = Some(history::project_snapshot(&project));
    let mut after = None;
    match command {
        Command::Edit {
            definition,
            archived,
            ..
        } => {
            let mut definition = definition.clone();
            definition.prune_board_limits();
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
            project.definition = definition;
            project.archived = *archived;
        }
        Command::ColumnLimit { column, .. } => {
            if project.archived {
                return Ok(rejected("Archived projects are read-only"));
            }
            if let Err(reason) = column.apply(&mut project.definition) {
                return Ok(rejected(reason));
            }
        }
        Command::Item { item: entry, .. } | Command::CreateIssue { item: entry, .. } => {
            if project.archived {
                return Ok(rejected("Reopen the project before changing items"));
            }
            if !entry.valid(&project.definition) {
                return Ok(rejected("Invalid project item fields or dates"));
            }
            let previous = item(context, project_id, entry.id)?;
            let current = ledger::item_version(context, project_id, entry.id)?;
            if previous.is_some() && current == 0 {
                return Err(Error::Invalid("stored project item has no revision"));
            }
            if version != current || previous.is_none() && version != 0 {
                return Ok(MutationResult::Rejected(Outcome::ItemConflict {
                    project: project_id,
                    item: entry.id,
                    version: current,
                }));
            }
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
            // Rows already in the destination still need the same source and
            // revision checks. Only changed rows should get a revision and Undo.
            if matches!(input.command, Command::BulkFields { .. })
                && previous.as_ref() == Some(entry)
            {
                return Ok(MutationResult::Success(Outcome::Applied {
                    project: Some(project_id),
                    version: project.version,
                }));
            }
            let mut entry = entry.clone();
            if let Command::CreateIssue {
                parent, type_id, ..
            } = command
            {
                if entry.subject.is_some() {
                    return Ok(rejected("This item is already an issue or pull request"));
                }
                let selection = if let Some(type_id) = type_id {
                    let Some(selection) = input
                        .issue_type
                        .filter(|selection| selection.id == *type_id)
                    else {
                        return Ok(rejected("Choose an enabled issue type"));
                    };
                    Some(selection)
                } else {
                    None
                };
                // Conversion shares the canonical upsert and its item precondition.
                // A rejected parent or membership rolls back the new issue too.
                let created = super::super::CreateIssue::for_project(
                    context,
                    super::super::CreateIssueInput {
                        submission_id: *input.request_id.as_bytes(),
                        author: input.actor.clone(),
                        title: entry.title.clone(),
                        body: entry.body.clone(),
                        metadata: Default::default(),
                    },
                    selection,
                )?;
                let MutationResult::Success(super::super::CreateIssueOutcome::Created(issue)) =
                    created
                else {
                    return Err(Error::Invalid("project issue creation failed"));
                };
                if let Some(parent) = parent
                    && let MutationResult::Rejected(outcome) =
                        hierarchy::change(context, issue.number, Some(*parent), 0, &input.actor)?
                {
                    return Ok(MutationResult::Rejected(outcome));
                }
                entry.subject = Some(Subject {
                    kind: SubjectKind::Issue,
                    number: issue.number,
                });
                entry.title.clear();
                entry.body.clear();
            }
            event_target = history::item_target(context, &entry)?;
            before = previous.as_ref().map(ledger::item_snapshot).transpose()?;
            after = Some(ledger::item_snapshot(&entry)?);
            context.query("INSERT INTO repository_project_items(project,id,kind,number,status,data) VALUES (?,?,?,?,?,?) ON CONFLICT(project,id) DO UPDATE SET kind=excluded.kind,number=excluded.number,status=excluded.status,data=excluded.data",&[id(project_id),id(entry.id),entry.subject.map(|s|SqlValue::Integer(kind(s))).unwrap_or(SqlValue::Null),entry.subject.map(|s|integer(s.number)).transpose()?.unwrap_or(SqlValue::Null),SqlValue::Text(entry.status.clone()),SqlValue::Text(serde_json::to_string(&entry)?)])?;
            ledger::advance_item(context, project_id, entry.id)?;
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
            let previous = item(context, project_id, *item_id)?;
            let current = ledger::item_version(context, project_id, *item_id)?;
            if previous.is_none() || version != current {
                return Ok(MutationResult::Rejected(Outcome::ItemConflict {
                    project: project_id,
                    item: *item_id,
                    version: current,
                }));
            }
            let entry = previous.ok_or(Error::Invalid("project item disappeared"))?;
            event_target = history::item_target(context, &entry)?;
            before = Some(ledger::item_snapshot(&entry)?);
            context.query(
                "DELETE FROM repository_project_items WHERE project=? AND id=?",
                &[id(project_id), id(*item_id)],
            )?;
            ledger::advance_item(context, project_id, *item_id)?;
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
    if matches!(command, Command::Edit { .. } | Command::ColumnLimit { .. }) {
        after = Some(history::project_snapshot(&project));
    }
    history::append(context, &input.actor, &project, event_target, before, after)?;
    advance_revision(context)?;
    Ok(MutationResult::Success(Outcome::Applied {
        project: Some(project.id),
        version: project.version,
    }))
}

fn apply_bulk(
    context: &mut Transaction<'_>,
    input: &Request,
    project: Uuid,
    edits: &[bulk::Edit],
) -> Result<MutationResult<Outcome>> {
    if !bulk::valid(edits) {
        return Ok(rejected("Select up to 50 distinct Project items"));
    }
    let Some(saved) = load(context, project)? else {
        return Ok(rejected("Project not found"));
    };
    let mut version = saved.version;
    for edit in edits {
        let Some(mut item) = item(context, project, edit.item)? else {
            return Ok(MutationResult::Rejected(Outcome::ItemConflict {
                project,
                item: edit.item,
                version: ledger::item_version(context, project, edit.item)?,
            }));
        };
        if !edit.change.update(&mut item, &saved.definition) {
            return Ok(rejected("The selected Project field is unavailable"));
        }
        let command = Command::Item {
            project,
            version: edit.version,
            item,
        };
        match apply(context, input, &command)? {
            MutationResult::Success(Outcome::Applied { version: next, .. }) => version = next,
            outcome => return Ok(outcome),
        }
    }
    Ok(MutationResult::Success(Outcome::Applied {
        project: Some(project),
        version,
    }))
}
