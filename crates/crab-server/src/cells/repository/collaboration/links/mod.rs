use serde::{Deserialize, Serialize};

use super::super::{
    Mutation, MutationResult, Read, RepositoryAuthor, RepositoryTransaction, advance_revision,
    settings, validate_author, validate_number,
};
use super::{Page, Subject, SubjectKind, SubjectSummary, Target, activity, next_version};

const ROOT: &str = "collaboration/v1/links";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pair {
    pub pull: u64,
    pub issue: u64,
}

impl Pair {
    fn validate(self) -> crate::cells::Result<()> {
        validate_number(self.pull)?;
        validate_number(self.issue)
    }

    fn key(self) -> String {
        format!("{ROOT}/pairs/{:016x}/{:016x}", self.pull, self.issue)
    }

    fn subjects(self) -> [Subject; 2] {
        [
            Subject {
                kind: SubjectKind::Pull,
                number: self.pull,
            },
            Subject {
                kind: SubjectKind::Issue,
                number: self.issue,
            },
        ]
    }

    fn exists(self, context: &RepositoryTransaction<'_>) -> crate::cells::Result<bool> {
        for subject in self.subjects() {
            if subject.closed(context)?.is_none() {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Link {
    pub pair: Pair,
    pub active: bool,
    pub version: u64,
}

fn load(context: &RepositoryTransaction<'_>, pair: Pair) -> crate::cells::Result<Link> {
    pair.validate()?;
    match context.document::<Link>(&pair.key())? {
        None => Ok(Link {
            pair,
            active: false,
            version: 0,
        }),
        Some(link) => {
            validate_number(link.version)?;
            if link.pair != pair {
                return Err(crate::cells::Error::Invalid(
                    "linked work does not match its key",
                ));
            }
            Ok(link)
        }
    }
}

fn prefix(subject: Subject) -> String {
    format!("{ROOT}/subjects/{}/", subject.key())
}

fn index(subject: Subject, other: u64) -> String {
    format!("{}{other:016x}", prefix(subject))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection {
    pub pair: Pair,
    pub active: bool,
    pub version: u64,
    pub actor: RepositoryAuthor,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum Outcome {
    Applied(Link),
    Archived,
    NotFound,
    Conflict,
}

pub(crate) struct SetLink;
impl Mutation for SetLink {
    const ID: u32 = 230;
    type Input = Selection;
    type Output = Outcome;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        input: Selection,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(Outcome::Archived));
        }
        validate_author(&input.actor)?;
        if input.version > super::super::MAX_NUMBER {
            return Err(crate::cells::Error::Invalid(
                "linked work version is invalid",
            ));
        }
        input.pair.validate()?;
        if !input.pair.exists(context)? {
            return Ok(MutationResult::Rejected(Outcome::NotFound));
        }
        let mut link = load(context, input.pair)?;
        if link.active == input.active {
            return Ok(MutationResult::Success(Outcome::Applied(link)));
        }
        if link.version != input.version {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        link.active = input.active;
        link.version = next_version(link.version)?;
        // Keep the inactive record's version. Deleting it would let a delayed
        // add overwrite a later unlink, or a delayed unlink erase a new link.
        context.put_document(&link.pair.key(), &link)?;
        let [pull, issue] = link.pair.subjects();
        for (subject, other) in [(pull, issue.number), (issue, pull.number)] {
            let key = index(subject, other);
            if link.active {
                context.put_document(&key, &true)?;
            } else {
                context.delete_document(&key)?;
            }
        }
        let summary = issue
            .summary(context)?
            .ok_or(crate::cells::Error::Invalid("linked issue is missing"))?;
        let action = if link.active {
            activity::Action::IssueLinked {
                number: issue.number,
                title: summary.title,
            }
        } else {
            activity::Action::IssueUnlinked {
                number: issue.number,
                title: summary.title,
            }
        };
        activity::append(
            context,
            Target::Pull {
                number: pull.number,
            },
            &input.actor,
            action,
        )?;
        advance_revision(context)?;
        Ok(MutationResult::Success(Outcome::Applied(link)))
    }
}

pub(crate) struct GetLink;
impl Read for GetLink {
    const ID: u32 = 230;
    type Input = Pair;
    type Output = Option<Link>;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        pair: Pair,
    ) -> crate::cells::Result<Self::Output> {
        pair.validate()?;
        if !pair.exists(context)? {
            return Ok(None);
        }
        load(context, pair).map(Some)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Query {
    pub subject: Subject,
    pub before: Option<u64>,
    pub limit: u8,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LinkedWork {
    pub work: SubjectSummary,
    pub version: u64,
}

pub(crate) struct ListLinks;
impl Read for ListLinks {
    const ID: u32 = 231;
    type Input = Query;
    type Output = Option<Page<LinkedWork>>;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        input: Query,
    ) -> crate::cells::Result<Self::Output> {
        if !(1..=50).contains(&input.limit) {
            return Err(crate::cells::Error::Invalid(
                "linked work pages require 1–50 items",
            ));
        }
        if let Some(before) = input.before {
            validate_number(before)?;
        }
        if input.subject.closed(context)?.is_none() {
            return Ok(None);
        }
        let prefix = prefix(input.subject);
        let before = input.before.map(|number| index(input.subject, number));
        let limit = usize::from(input.limit);
        let mut items = context
            .documents::<bool>(&prefix, before.as_deref(), limit + 1)?
            .into_iter()
            .map(|(key, marker)| {
                let suffix = key
                    .strip_prefix(&prefix)
                    .filter(|suffix| suffix.len() == 16)
                    .ok_or(crate::cells::Error::Invalid(
                        "linked work index key is invalid",
                    ))?;
                let number = u64::from_str_radix(suffix, 16).map_err(|_| {
                    crate::cells::Error::Invalid("linked work index key is invalid")
                })?;
                validate_number(number)?;
                let (pair, other) = match input.subject.kind {
                    SubjectKind::Pull => (
                        Pair {
                            pull: input.subject.number,
                            issue: number,
                        },
                        Subject {
                            kind: SubjectKind::Issue,
                            number,
                        },
                    ),
                    SubjectKind::Issue => (
                        Pair {
                            issue: input.subject.number,
                            pull: number,
                        },
                        Subject {
                            kind: SubjectKind::Pull,
                            number,
                        },
                    ),
                };
                let link = load(context, pair)?;
                if !marker || !link.active {
                    return Err(crate::cells::Error::Invalid("linked work index is invalid"));
                }
                let work = other
                    .summary(context)?
                    .ok_or(crate::cells::Error::Invalid("linked work is missing"))?;
                Ok(LinkedWork {
                    work,
                    version: link.version,
                })
            })
            .collect::<crate::cells::Result<Vec<_>>>()?;
        let has_more = items.len() > limit;
        items.truncate(limit);
        let next = if has_more {
            items.last().map(|item| item.work.subject.number)
        } else {
            None
        };
        Ok(Some(Page { items, next }))
    }
}

#[cfg(test)]
mod tests;
