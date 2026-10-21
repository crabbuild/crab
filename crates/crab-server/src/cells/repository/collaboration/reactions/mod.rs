use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::super::{
    MAX_NUMBER, Mutation, MutationResult, Read, RepositoryAuthor, RepositoryTransaction,
    advance_revision, settings, validate_author,
};
use super::{PREFIX, Target};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    ThumbsUp,
    ThumbsDown,
    Laugh,
    Hooray,
    Confused,
    Heart,
    Rocket,
    Eyes,
}

impl Kind {
    const ALL: [Self; 8] = [
        Self::ThumbsUp,
        Self::ThumbsDown,
        Self::Laugh,
        Self::Hooray,
        Self::Confused,
        Self::Heart,
        Self::Rocket,
        Self::Eyes,
    ];

    fn key(self) -> &'static str {
        match self {
            Self::ThumbsUp => "thumbs_up",
            Self::ThumbsDown => "thumbs_down",
            Self::Laugh => "laugh",
            Self::Hooray => "hooray",
            Self::Confused => "confused",
            Self::Heart => "heart",
            Self::Rocket => "rocket",
            Self::Eyes => "eyes",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Group {
    pub count: u64,
    pub reacted: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Summary {
    pub groups: BTreeMap<Kind, Group>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection {
    pub target: Target,
    pub kind: Kind,
    pub active: bool,
    pub actor: RepositoryAuthor,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Query {
    pub targets: Vec<Target>,
    pub actor: RepositoryAuthor,
}

type Counts = BTreeMap<Kind, u64>;

fn counts_key(target: Target) -> String {
    format!("{PREFIX}/reactions/{}/counts", target.key())
}

fn member_key(
    target: Target,
    kind: Kind,
    actor: &RepositoryAuthor,
) -> crate::cells::Result<String> {
    // Hash stable identity claims; display-name changes cannot add a second
    // membership, and the reaction ledger need not store private identity claims.
    let digest = blake3::hash(&serde_json::to_vec(&(&actor.issuer, &actor.subject))?);
    Ok(format!(
        "{PREFIX}/reactions/{}/actors/{}/{}",
        target.key(),
        digest.to_hex(),
        kind.key()
    ))
}

fn counts(context: &RepositoryTransaction<'_>, target: Target) -> crate::cells::Result<Counts> {
    let counts: Counts = context.document(&counts_key(target))?.unwrap_or_default();
    if counts
        .values()
        .any(|value| *value == 0 || *value > MAX_NUMBER)
    {
        return Err(crate::cells::Error::Invalid(
            "stored reaction counts are invalid",
        ));
    }
    Ok(counts)
}

fn membership(context: &RepositoryTransaction<'_>, key: &str) -> crate::cells::Result<bool> {
    match context.document::<bool>(key)? {
        None => Ok(false),
        Some(true) => Ok(true),
        Some(false) => Err(crate::cells::Error::Invalid(
            "stored reaction membership is invalid",
        )),
    }
}

fn summary(
    context: &RepositoryTransaction<'_>,
    target: Target,
    actor: &RepositoryAuthor,
) -> crate::cells::Result<Summary> {
    let counts = counts(context, target)?;
    let groups = Kind::ALL
        .into_iter()
        .map(|kind| {
            let reacted = membership(context, &member_key(target, kind, actor)?)?;
            let count = counts.get(&kind).copied().unwrap_or(0);
            if reacted && count == 0 {
                return Err(crate::cells::Error::Invalid(
                    "reaction membership has no aggregate count",
                ));
            }
            Ok((kind, Group { count, reacted }))
        })
        .collect::<crate::cells::Result<BTreeMap<_, _>>>()?;
    Ok(Summary { groups })
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum Outcome {
    Applied(Summary),
    Archived,
    NotFound,
}

pub(crate) struct SetReaction;
impl Mutation for SetReaction {
    const ID: u32 = 210;
    type Input = Selection;
    type Output = Outcome;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(Outcome::Archived));
        }
        validate_author(&input.actor)?;
        if !input.target.exists(context)? {
            return Ok(MutationResult::Rejected(Outcome::NotFound));
        }
        let key = member_key(input.target, input.kind, &input.actor)?;
        let active = membership(context, &key)?;
        if active == input.active {
            return Ok(MutationResult::Success(Outcome::Applied(summary(
                context,
                input.target,
                &input.actor,
            )?)));
        }
        let mut counts = counts(context, input.target)?;
        let current = counts.get(&input.kind).copied().unwrap_or(0);
        let count = if input.active {
            current.checked_add(1)
        } else {
            current.checked_sub(1)
        }
        .filter(|value| *value <= MAX_NUMBER)
        .ok_or(crate::cells::Error::Invalid(
            "reaction count is exhausted or inconsistent",
        ))?;
        if count == 0 {
            counts.remove(&input.kind);
        } else {
            counts.insert(input.kind, count);
        }
        if input.active {
            context.put_document(&key, &true)?;
        } else {
            context.delete_document(&key)?;
        }
        let key = counts_key(input.target);
        if counts.is_empty() {
            context.delete_document(&key)?;
        } else {
            context.put_document(&key, &counts)?;
        }
        advance_revision(context)?;
        Ok(MutationResult::Success(Outcome::Applied(summary(
            context,
            input.target,
            &input.actor,
        )?)))
    }
}

pub(crate) struct GetReactions;
impl Read for GetReactions {
    const ID: u32 = 210;
    type Input = Query;
    type Output = Vec<Option<Summary>>;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        validate_author(&input.actor)?;
        if input.targets.is_empty() || input.targets.len() > 50 {
            return Err(crate::cells::Error::Invalid(
                "reaction queries require 1–50 targets",
            ));
        }
        // A discussion page reads summaries in one owner transaction instead
        // of starting a browser request and native ingress call per comment.
        input
            .targets
            .into_iter()
            .map(|target| {
                if !target.exists(context)? {
                    return Ok(None);
                }
                summary(context, target, &input.actor).map(Some)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
