use super::*;
use crate::cells::repository::{
    CreateIssue, CreateIssueInput, CreateIssueOutcome, issues::issue_type,
};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub organization: Uuid,
    pub project: Uuid,
    pub request_id: Uuid,
    pub actor: RepositoryAuthor,
    pub title: String,
    pub body: String,
    pub type_id: Option<Uuid>,
    pub ownership_epoch: u32,
    pub issue_type: Option<issue_type::Selection>,
    pub can_write: bool,
    pub cancel: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum Outcome {
    Created { number: u64 },
    Cancelled,
    Rejected { reason: String },
    RequestConflict,
}

#[derive(Deserialize, Serialize)]
struct Saved {
    digest: String,
    outcome: Outcome,
}

pub(crate) struct Publish;
impl Mutation for Publish {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 392;
    type Input = Request;
    type Output = Outcome;

    fn execute(context: &mut Transaction<'_>, input: Request) -> Result<MutationResult<Outcome>> {
        validate_author(&input.actor)?;
        if input.organization.is_nil() || input.project.is_nil() || input.request_id.is_nil() {
            return Err(Error::Invalid(
                "Organization Project issue submission ID is invalid",
            ));
        }
        let key = format!(
            "projects/v1/organization-issues/{}/{}",
            input.organization, input.request_id
        );
        let digest = blake3::hash(&serde_json::to_vec(&(
            input.project,
            &input.actor.issuer,
            &input.actor.subject,
            &input.title,
            &input.body,
            input.type_id,
            input.ownership_epoch,
        ))?)
        .to_hex()
        .to_string();
        // A retry or cancellation must recover the repository decision even
        // when the source became inaccessible or its issue type was disabled.
        if let Some(saved) = context.document::<Saved>(&key)? {
            return Ok(MutationResult::Success(if saved.digest == digest {
                saved.outcome
            } else {
                Outcome::RequestConflict
            }));
        }
        let outcome = if input.cancel {
            Outcome::Cancelled
        } else if !input.can_write {
            rejected("Write access to a linked repository is required to create an issue")
        } else if input.type_id.is_some()
            && !input.issue_type.is_some_and(|selection| {
                Some(selection.id) == input.type_id
                    && selection.organization == input.organization
                    && selection.ownership_epoch == input.ownership_epoch
            })
        {
            rejected("Choose an enabled issue type")
        } else {
            let selection = input
                .issue_type
                .filter(|selection| Some(selection.id) == input.type_id);
            let created = context.apply(|context| {
                CreateIssue::for_project(
                    context,
                    CreateIssueInput {
                        submission_id: *input.request_id.as_bytes(),
                        author: input.actor,
                        title: input.title,
                        body: input.body,
                        metadata: Default::default(),
                    },
                    selection,
                )
            });
            match created {
                Ok(MutationResult::Success(CreateIssueOutcome::Created(issue))) => {
                    Outcome::Created {
                        number: issue.number,
                    }
                }
                Ok(MutationResult::Rejected(CreateIssueOutcome::Archived)) => {
                    rejected("The repository is archived")
                }
                Err(Error::AttachmentUnavailable) => {
                    // Issue construction rolled back, including its allocated
                    // number. Settle the reservation before the UI drops its intent.
                    rejected("Remove or reupload the unavailable attachment")
                }
                Err(error) => return Err(error),
                Ok(_) => return Err(Error::Invalid("Organization Project issue creation failed")),
            }
        };
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

fn rejected(reason: &str) -> Outcome {
    Outcome::Rejected {
        reason: reason.into(),
    }
}
