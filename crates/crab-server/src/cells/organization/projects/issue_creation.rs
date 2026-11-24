use super::*;
use crate::cells::repository::{
    collaboration::{Subject, SubjectKind},
    projects::organization_issue::Outcome as SourceOutcome,
};

// Four validated planning items fit in a bounded Cell document. Source
// titles and bodies are only present in the original browser submission.
const MAX_PENDING: usize = 4;

#[derive(Deserialize, Serialize)]
pub(super) struct Reservation {
    request_id: Uuid,
    pub(super) repository: Uuid,
    item: planning::Item,
}

pub(super) type Reservations = BTreeMap<Uuid, Reservation>;

fn key(project: Uuid) -> String {
    format!("organization/projects/v1/pending-issues/{project}")
}

pub(super) fn reservations(context: &Transaction<'_>, project: Uuid) -> Result<Reservations> {
    Ok(context.document(&key(project))?.unwrap_or_default())
}

pub(super) fn can_edit(
    pending: &Reservations,
    archived: bool,
    definition: &planning::Definition,
) -> bool {
    pending.is_empty() || (!archived && pending.values().all(|entry| entry.item.valid(definition)))
}

pub(super) fn has_room(
    context: &Transaction<'_>,
    project: Uuid,
    pending: &Reservations,
) -> Result<bool> {
    let count = context.query(
        "SELECT COUNT(*) FROM organization_project_items WHERE project=?",
        &[blob(project)],
    )?;
    Ok(uint(&count.rows[0][0])?.saturating_add(pending.len() as u64) < MAX_ITEMS)
}

fn rejected(reason: &str) -> MutationResult<Outcome> {
    MutationResult::Rejected(Outcome::Rejected {
        reason: reason.into(),
    })
}

pub(super) fn reserve(
    context: &Transaction<'_>,
    request: &Request,
    project: &Project,
    mut pending: Reservations,
    repository: Uuid,
    item: &planning::Item,
    type_id: Option<Uuid>,
) -> Result<MutationResult<Outcome>> {
    let Some(ownership_epoch) = request.source_epoch.filter(|_| request.source_allowed) else {
        return Ok(rejected(
            "Choose a linked organization repository where you have write access",
        ));
    };
    if !repositories(context, project.id)?.contains(&repository) {
        return Ok(rejected("Choose a linked organization repository"));
    }
    if item.subject.is_some()
        || !item.valid(&project.definition)
        || type_id.is_some_and(|id| id.is_nil())
    {
        return Ok(rejected("Check the new issue and its Project fields"));
    }
    if pending.contains_key(&item.id) {
        return Ok(rejected("This item has a pending issue submission"));
    }
    let version = ledger::item_version(context, project.id, item.id)?;
    if version != 0 {
        return Ok(MutationResult::Rejected(Outcome::ItemConflict {
            project: project.id,
            item: item.id,
            version,
        }));
    }
    if pending.len() >= MAX_PENDING || !has_room(context, project.id, &pending)? {
        return Ok(rejected(
            "This Project has no available issue creation slots",
        ));
    }
    let mut item = item.clone();
    item.title.clear();
    item.body.clear();
    // A placeholder validates fields as linked work without publishing a row.
    item.subject = Some(Subject {
        kind: SubjectKind::Issue,
        number: 1,
    });
    pending.insert(
        item.id,
        Reservation {
            request_id: request.request_id,
            repository,
            item,
        },
    );
    context.put_document(&key(project.id), &pending)?;
    Ok(MutationResult::Success(Outcome::PendingIssue {
        ownership_epoch,
    }))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResolveInput {
    pub request: Request,
    pub receipt: SourceOutcome,
}

pub(crate) struct ResolveIssue;
impl Mutation for ResolveIssue {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 32;
    type Input = ResolveInput;
    type Output = Outcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: ResolveInput,
    ) -> Result<MutationResult<Outcome>> {
        let request = input.request;
        let Some(outcome) = commands::recover(context, &request)? else {
            return Err(Error::AccessDenied);
        };
        if !matches!(outcome, Outcome::PendingIssue { .. }) {
            return Ok(MutationResult::Success(outcome));
        }
        let Command::CreateIssue {
            project,
            repository,
            item,
            ..
        } = &request.command
        else {
            return Err(Error::Invalid(
                "Project issue resolution has no reservation",
            ));
        };
        let mut pending = reservations(context, *project)?;
        let reserved = pending
            .remove(&item.id)
            .filter(|entry| {
                entry.request_id == request.request_id && entry.repository == *repository
            })
            .ok_or(Error::Invalid("Project issue reservation is missing"))?;
        if pending.is_empty() {
            context.delete_document(&key(*project))?;
        } else {
            context.put_document(&key(*project), &pending)?;
        }
        let outcome = match input.receipt {
            SourceOutcome::Cancelled => Outcome::Cancelled,
            SourceOutcome::Rejected { reason } => Outcome::Rejected { reason },
            SourceOutcome::RequestConflict => {
                return Err(Error::Invalid("Project issue receipt conflicts"));
            }
            SourceOutcome::Created { number } => {
                let existing = context.query("SELECT 1 FROM organization_project_items WHERE project=? AND repository=? AND kind=0 AND number=?", &[blob(*project), blob(*repository), integer(number)?])?;
                if !existing.rows.is_empty() {
                    let version = load(context, *project)?
                        .ok_or(Error::Invalid("Project issue target is missing"))?
                        .version;
                    return commands::save_outcome(
                        context,
                        &request,
                        Outcome::IssueAlreadyLinked {
                            project: *project,
                            version,
                        },
                    );
                }
                let mut linked = reserved.item;
                linked.subject = Some(Subject {
                    kind: SubjectKind::Issue,
                    number,
                });
                let applied = commands::apply(
                    context,
                    &Request {
                        command: Command::Item {
                            project: *project,
                            version: 0,
                            repository: Some(*repository),
                            item: linked,
                        },
                        source_allowed: true,
                        ..request.clone()
                    },
                )?;
                let MutationResult::Success(outcome) = applied else {
                    return Err(Error::Invalid("Reserved Project item could not be linked"));
                };
                outcome
            }
        };
        commands::save_outcome(context, &request, outcome)
    }
}
