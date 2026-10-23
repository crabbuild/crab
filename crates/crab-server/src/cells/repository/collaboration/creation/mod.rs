use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Decision, Subject, SubjectKind, milestones};
use crate::cells::{
    Error, Result,
    application::{Mutation, MutationResult, Read, Transaction},
    repository::{
        RepositoryAuthor,
        assignments::{self, Assignee},
        integer, validate_label_ids, validate_number,
    },
};

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Selection {
    pub label_ids: Vec<u64>,
    pub assignee_ids: Vec<Uuid>,
    pub milestone: Option<u64>,
}
impl Selection {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_label_ids(&self.label_ids)?;
        for number in self.label_ids.iter().copied().chain(self.milestone) {
            validate_number(number)?;
        }
        if self.assignee_ids.len() > 10
            || self.assignee_ids.iter().any(Uuid::is_nil)
            || self.assignee_ids.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(Error::Invalid("Initial assignee selection is invalid"));
        }
        Ok(())
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.label_ids.is_empty() && self.assignee_ids.is_empty() && self.milestone.is_none()
    }
    pub(in crate::cells::repository) fn hash(&self, hasher: &mut blake3::Hasher) {
        // Absence has no digest terms: existing text-only submissions retain
        // their identity without a retired payload reader or receipt rewrite.
        for label in &self.label_ids {
            hasher.update(b"label\0");
            hasher.update(&label.to_be_bytes());
        }
        for actor in &self.assignee_ids {
            hasher.update(b"assignee\0");
            hasher.update(actor.as_bytes());
        }
        if let Some(milestone) = self.milestone {
            hasher.update(b"milestone\0");
            hasher.update(&milestone.to_be_bytes());
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub selection: Selection,
    pub can_triage: bool,
    pub eligible: Vec<Assignee>,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Failure {
    Forbidden,
    Labels,
    Assignees,
    Milestone,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Comparison {
    pub base_oid: String,
    pub head_oid: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Rejected {
    digest: String,
    reason: Failure,
    comparison: Option<Comparison>,
}
pub(in crate::cells::repository) enum Resolution {
    Ready,
    Rejected(Failure),
    Conflict,
}

fn rejection_key(kind: SubjectKind, id: [u8; 16]) -> String {
    format!(
        "collaboration/v1/creation-rejections/{}/{}",
        if kind == SubjectKind::Issue {
            "issue"
        } else {
            "pull"
        },
        Uuid::from_bytes(id)
    )
}
pub(in crate::cells::repository) fn rejected_comparison(
    context: &Transaction<'_>,
    id: [u8; 16],
) -> Result<Option<Comparison>> {
    Ok(context
        .document::<Rejected>(&rejection_key(SubjectKind::Pull, id))?
        .and_then(|saved| saved.comparison))
}

impl Input {
    pub(in crate::cells::repository) fn resolve(
        &self,
        context: &mut Transaction<'_>,
        kind: SubjectKind,
        id: [u8; 16],
        digest: blake3::Hash,
        comparison: Option<Comparison>,
    ) -> Result<Resolution> {
        let key = rejection_key(kind, id);
        let digest = digest.to_hex().to_string();
        if let Some(saved) = context.document::<Rejected>(&key)? {
            return Ok(if saved.digest == digest {
                Resolution::Rejected(saved.reason)
            } else {
                Resolution::Conflict
            });
        }
        if let Some(reason) = self.prepare(context)? {
            context.put_document(
                &key,
                &Rejected {
                    digest,
                    reason,
                    comparison,
                },
            )?;
            return Ok(Resolution::Rejected(reason));
        }
        Ok(Resolution::Ready)
    }

    // Current permission and roster snapshots affect new writes, not the digest
    // or recovery of an already-created discussion after names/grants change.
    fn prepare(&self, context: &mut Transaction<'_>) -> Result<Option<Failure>> {
        if !self.selection.is_empty() && !self.can_triage {
            return Ok(Some(Failure::Forbidden));
        }
        if self.eligible.len() != self.selection.assignee_ids.len()
            || self.eligible.iter().map(|person| person.id).ne(self
                .selection
                .assignee_ids
                .iter()
                .copied())
            || assignments::validate(&self.eligible).is_err()
        {
            return Ok(Some(Failure::Assignees));
        }
        for number in &self.selection.label_ids {
            let rows = context.query(
                "SELECT 1 FROM repository_labels WHERE number = ? AND deleted_version IS NULL",
                &[integer(*number)?],
            )?;
            if rows.rows.is_empty() {
                return Ok(Some(Failure::Labels));
            }
        }
        if let Some(number) = self.selection.milestone
            && milestones::GetMilestone::execute(context, number)?.is_none()
        {
            return Ok(Some(Failure::Milestone));
        }
        Ok(None)
    }
}

pub(in crate::cells::repository) fn assign_milestone(
    context: &mut Transaction<'_>,
    subject: Subject,
    actor: &RepositoryAuthor,
    number: Option<u64>,
) -> Result<()> {
    let Some(number) = number else {
        return Ok(());
    };
    let result = milestones::AssignMilestone::execute(
        context,
        milestones::AssignMilestoneInput {
            subject,
            milestone: Some(number),
            version: 0,
            actor: actor.clone(),
        },
    )?;
    match result {
        MutationResult::Success(Decision::Applied(_)) => Ok(()),
        // Validation and assignment share the transaction. Any other outcome is
        // an invariant failure; returning an error rolls creation and numbering back.
        _ => Err(Error::Invalid(
            "Initial milestone assignment was not applied",
        )),
    }
}

#[cfg(test)]
pub(in crate::cells::repository) mod tests;
