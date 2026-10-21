use super::index::{self, Labelled, Page, SearchInput};
use crate::cells::application::{Read, RepositoryTransaction};
use crate::cells::repository::{IssueSummary, PullSummary, collaboration::SubjectKind};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum DiscussionSummary {
    Issue(IssueSummary),
    Pull(PullSummary),
}

impl Labelled for DiscussionSummary {
    fn label_ids(&self) -> &[u64] {
        match self {
            Self::Issue(issue) => &issue.label_ids,
            Self::Pull(pull) => &pull.label_ids,
        }
    }
}

pub(crate) struct ListDiscussions;
impl Read for ListDiscussions {
    const ID: u32 = 3;
    type Input = SearchInput;
    type Output = Page<DiscussionSummary>;
    fn execute(
        context: &mut RepositoryTransaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        index::page(
            context,
            input,
            None,
            |context, subject, metrics| match subject.kind {
                SubjectKind::Issue => IssueSummary::load(context, subject.number, metrics)
                    .map(DiscussionSummary::Issue),
                SubjectKind::Pull => {
                    PullSummary::load(context, subject.number, metrics).map(DiscussionSummary::Pull)
                }
            },
        )
    }
}

#[cfg(test)]
mod tests;
