use serde::{Deserialize, Serialize};

use super::super::{
    Mutation, MutationResult, Read, RepositoryAuthor, Transaction, advance_revision, settings,
    validate_author, validate_number,
};
use super::{Page, Subject, SubjectKind, SubjectSummary, Target, activity, next_version};

const ROOT: &str = "collaboration/v1/links";
const MAX_CLOSING_ISSUES: usize = 10;

pub(crate) mod branches;
mod closure;
pub(crate) mod commits;
pub(crate) mod keywords;
pub(in crate::cells::repository) use closure::{
    capture_closure, clear_closure, complete_closure, projected_keywords,
};

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

    fn exists(self, context: &Transaction<'_>) -> crate::cells::Result<bool> {
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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub closes_on_merge: bool,
}

fn load(context: &Transaction<'_>, pair: Pair) -> crate::cells::Result<Link> {
    pair.validate()?;
    match context.document::<Link>(&pair.key())? {
        None => Ok(Link {
            pair,
            active: false,
            version: 0,
            closes_on_merge: false,
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

pub(in crate::cells::repository) fn has_explicit_links(
    context: &Transaction<'_>,
    subject: Subject,
) -> crate::cells::Result<bool> {
    Ok(!context
        .documents::<bool>(&prefix(subject), None, 1)?
        .is_empty())
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closes_on_merge: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum Outcome {
    Applied(Link),
    Archived,
    NotFound,
    Conflict,
    LimitReached,
}

pub(crate) struct SetLink;
impl Mutation for SetLink {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 230;
    type Input = Selection;
    type Output = Outcome;

    fn execute(
        context: &mut Transaction<'_>,
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
        let closes = input.active && input.closes_on_merge.unwrap_or(link.closes_on_merge);
        let changed_link = link.active != input.active;
        if !changed_link && link.closes_on_merge == closes {
            return Ok(MutationResult::Success(Outcome::Applied(link)));
        }
        if link.version != input.version {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        if link.closes_on_merge != closes {
            let prefix = closure::closing_prefix(link.pair.pull);
            let key = format!("{prefix}{:016x}", link.pair.issue);
            if closes {
                if context
                    .documents::<u64>(&prefix, None, MAX_CLOSING_ISSUES)?
                    .len()
                    == MAX_CLOSING_ISSUES
                {
                    return Ok(MutationResult::Rejected(Outcome::LimitReached));
                }
                context.put_document(&key, &link.pair.issue)?;
            } else {
                context.delete_document(&key)?;
            }
            link.closes_on_merge = closes;
        }
        link.active = input.active;
        link.version = next_version(link.version)?;
        // Keep the inactive record's version. Deleting it would let a delayed
        // add overwrite a later unlink, or a delayed unlink erase a new link.
        context.put_document(&link.pair.key(), &link)?;
        advance_revision(context)?;
        if !changed_link {
            return Ok(MutationResult::Success(Outcome::Applied(link)));
        }
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
        Ok(MutationResult::Success(Outcome::Applied(link)))
    }
}

pub(crate) struct GetLink;
impl Read for GetLink {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 230;
    type Input = Pair;
    type Output = Option<Link>;

    fn execute(context: &mut Transaction<'_>, pair: Pair) -> crate::cells::Result<Self::Output> {
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
    pub default_ref: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LinkedWork {
    pub work: SubjectSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub closes_on_merge: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_description: bool,
}

pub(crate) struct ListLinks;
impl Read for ListLinks {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 231;
    type Input = Query;
    type Output = Option<Page<LinkedWork>>;

    fn execute(context: &mut Transaction<'_>, input: Query) -> crate::cells::Result<Self::Output> {
        if !(1..=50).contains(&input.limit) {
            return Err(crate::cells::Error::Invalid(
                "linked work pages require 1–50 items",
            ));
        }
        if let Some(before) = input.before {
            validate_number(before)?;
        }
        crate::cells::repository::search::index::ensure_ready(context)?;
        if input.subject.closed(context)?.is_none() {
            return Ok(None);
        }
        let prefix = prefix(input.subject);
        let before = input.before.map(|number| index(input.subject, number));
        let limit = usize::from(input.limit);
        let mut numbers = context
            .documents::<bool>(&prefix, before.as_deref(), limit + 1)?
            .into_iter()
            .map(|(key, marker)| {
                if !marker {
                    return Err(crate::cells::Error::Invalid("linked work index is invalid"));
                }
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
                Ok((number, false))
            })
            .collect::<crate::cells::Result<std::collections::BTreeMap<_, _>>>()?;
        numbers.extend(
            keywords::page(context, &input)?
                .into_iter()
                .map(|number| (number, true)),
        );
        let mut items = numbers
            .into_iter()
            .rev()
            .take(limit + 1)
            .map(|(number, from_description)| {
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
                if !from_description && !link.active {
                    return Err(crate::cells::Error::Invalid("linked work index is invalid"));
                }
                let work = other
                    .summary(context)?
                    .ok_or(crate::cells::Error::Invalid("linked work is missing"))?;
                Ok(LinkedWork {
                    work,
                    version: link.active.then_some(link.version),
                    closes_on_merge: from_description || link.closes_on_merge,
                    from_description,
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
