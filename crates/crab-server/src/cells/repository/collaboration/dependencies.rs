use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{PREFIX, Subject, SubjectKind, SubjectSummary, Target, activity, next_version};
use crate::cells::{
    Error, Result,
    application::{Mutation, MutationResult, Read, Transaction},
    repository::{
        CLASS, MAX_NUMBER, RepositoryAuthor, advance_revision, settings, validate_author,
        validate_number,
    },
};

const LIMIT: usize = 50;
const WALK_LIMIT: usize = 2048;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Direction {
    BlockedBy,
    Blocking,
}

impl Direction {
    pub(crate) fn pair(self, issue: u64, other: u64) -> Pair {
        match self {
            Self::BlockedBy => Pair {
                blocked: issue,
                blocker: other,
            },
            Self::Blocking => Pair {
                blocked: other,
                blocker: issue,
            },
        }
    }

    fn prefix(self, issue: u64) -> String {
        let name = match self {
            Self::BlockedBy => "blocked-by",
            Self::Blocking => "blocking",
        };
        format!("{PREFIX}/issue-dependencies/{name}/{issue:016x}/")
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pair {
    blocked: u64,
    blocker: u64,
}

impl Pair {
    fn key(self) -> String {
        format!(
            "{PREFIX}/issue-dependencies/pairs/{:016x}/{:016x}",
            self.blocked, self.blocker
        )
    }

    fn summaries(self, context: &Transaction<'_>) -> Result<Option<[SubjectSummary; 2]>> {
        let Some(blocked) = issue(self.blocked).summary(context)? else {
            return Ok(None);
        };
        let Some(blocker) = issue(self.blocker).summary(context)? else {
            return Ok(None);
        };
        Ok(Some([blocked, blocker]))
    }
}

fn issue(number: u64) -> Subject {
    Subject {
        kind: SubjectKind::Issue,
        number,
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct State {
    pub active: bool,
    pub version: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Edge {
    number: u64,
    version: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Item {
    work: SubjectSummary,
    version: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Page {
    pub items: Vec<Item>,
    pub next: Option<u64>,
    pub archived: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Query {
    pub issue: u64,
    pub direction: Direction,
    pub after: Option<u64>,
}

pub(crate) struct List;
impl Read for List {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 315;
    type Input = Query;
    type Output = Option<Page>;

    fn execute(context: &mut Transaction<'_>, query: Query) -> Result<Self::Output> {
        if issue(query.issue).closed(context)?.is_none() {
            return Ok(None);
        }
        let prefix = query.direction.prefix(query.issue);
        let after = query
            .after
            .map(|number| {
                validate_number(number)?;
                Ok::<_, Error>(format!("{prefix}{number:016x}"))
            })
            .transpose()?;
        let rows = context.documents_after::<Edge>(&prefix, after.as_deref(), 31)?;
        let more = rows.len() > 30;
        let mut items = Vec::with_capacity(rows.len().min(30));
        for (_, edge) in rows.into_iter().take(30) {
            let work = issue(edge.number)
                .summary(context)?
                .ok_or(Error::Invalid("dependent issue is missing"))?;
            items.push(Item {
                work,
                version: edge.version,
            });
        }
        let next = items
            .last()
            .filter(|_| more)
            .map(|row| row.work.subject.number);
        Ok(Some(Page {
            items,
            next,
            archived: settings::archived(context)?,
        }))
    }
}

pub(crate) struct Get;
impl Read for Get {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 316;
    type Input = Pair;
    type Output = Option<State>;

    fn execute(context: &mut Transaction<'_>, pair: Pair) -> Result<Self::Output> {
        if pair.blocked == pair.blocker || pair.summaries(context)?.is_none() {
            return Ok(None);
        }
        Ok(Some(context.document(&pair.key())?.unwrap_or_default()))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Edit {
    pub pair: Pair,
    pub active: bool,
    pub version: u64,
    pub actor: RepositoryAuthor,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Outcome {
    Applied(State),
    NotFound,
    Archived,
    Conflict,
    Cycle,
    DegreeLimit,
    TraversalLimit,
}

fn check_path(context: &Transaction<'_>, pair: Pair) -> Result<Option<Outcome>> {
    let mut visited = BTreeSet::from([pair.blocker]);
    let mut pending = vec![pair.blocker];
    let mut remaining = WALK_LIMIT;
    while let Some(number) = pending.pop() {
        let edges =
            context.documents_after::<Edge>(&Direction::BlockedBy.prefix(number), None, LIMIT)?;
        let Some(left) = remaining.checked_sub(edges.len()) else {
            return Ok(Some(Outcome::TraversalLimit));
        };
        remaining = left;
        for (_, edge) in edges {
            if edge.number == pair.blocked {
                return Ok(Some(Outcome::Cycle));
            }
            if visited.insert(edge.number) {
                pending.push(edge.number);
            }
        }
    }
    Ok(None)
}

pub(crate) struct Set;
impl Mutation for Set {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 315;
    type Input = Edit;
    type Output = Outcome;

    fn execute(context: &mut Transaction<'_>, input: Edit) -> Result<MutationResult<Outcome>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(Outcome::Archived));
        }
        validate_author(&input.actor)?;
        if input.version > MAX_NUMBER {
            return Err(Error::Invalid("issue dependency version is invalid"));
        }
        let pair = input.pair;
        if pair.blocked == pair.blocker {
            return Ok(MutationResult::Rejected(Outcome::Cycle));
        }
        let Some([blocked, blocker]) = pair.summaries(context)? else {
            return Ok(MutationResult::Rejected(Outcome::NotFound));
        };
        let key = pair.key();
        let prior = context.document::<State>(&key)?.unwrap_or_default();
        if prior.active == input.active
            && (input.version == prior.version || input.version + 1 == prior.version)
        {
            return Ok(MutationResult::Success(Outcome::Applied(prior)));
        }
        if prior.version != input.version {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        let endpoints = [
            (Direction::BlockedBy, &blocked, &blocker),
            (Direction::Blocking, &blocker, &blocked),
        ];
        if input.active {
            for (direction, source, _) in endpoints {
                if context
                    .documents_after::<Edge>(&direction.prefix(source.subject.number), None, LIMIT)?
                    .len()
                    >= LIMIT
                {
                    return Ok(MutationResult::Rejected(Outcome::DegreeLimit));
                }
            }
            if let Some(reason) = check_path(context, pair)? {
                return Ok(MutationResult::Rejected(reason));
            }
        }
        let state = State {
            active: input.active,
            version: next_version(prior.version)?,
        };
        // Both directions and the version tombstone share the issue transaction.
        context.put_document(&key, &state)?;
        for (direction, source, other) in endpoints {
            let number = other.subject.number;
            let index = format!("{}{number:016x}", direction.prefix(source.subject.number));
            if state.active {
                context.put_document(
                    &index,
                    &Edge {
                        number,
                        version: state.version,
                    },
                )?;
            } else {
                context.delete_document(&index)?;
            }
        }
        activity::append(
            context,
            Target::Issue {
                number: pair.blocked,
            },
            &input.actor,
            activity::Action::DependencyChanged {
                number: pair.blocker,
                title: blocker.title,
                active: state.active,
            },
        )?;
        advance_revision(context)?;
        Ok(MutationResult::Success(Outcome::Applied(state)))
    }
}
