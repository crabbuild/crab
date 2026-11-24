use serde::{Deserialize, Serialize};

use super::{PREFIX, Subject, SubjectKind, Target, activity};
use crate::cells::{
    Result,
    application::{Mutation, MutationResult, Transaction},
    repository::{
        CLASS, MAX_NUMBER, RepositoryAuthor, advance_revision, settings, validate_author,
        validate_number,
    },
};

const MAX_CHAIN: usize = 50;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Mark {
    event: u64,
    canonical: u64,
    active: bool,
}

fn key(subject: Subject) -> String {
    format!("{PREFIX}/duplicates/{}", subject.key())
}

fn target(subject: Subject) -> Target {
    match subject.kind {
        SubjectKind::Issue => Target::Issue {
            number: subject.number,
        },
        SubjectKind::Pull => Target::Pull {
            number: subject.number,
        },
    }
}

pub(super) fn current(context: &Transaction<'_>, subject: Subject) -> Result<Option<u64>> {
    Ok(context
        .document::<Mark>(&key(subject))?
        .filter(|mark| mark.active)
        .map(|mark| mark.event))
}

fn reference(body: &str) -> Option<u64> {
    if !body.contains('#') {
        return None;
    }
    let mut found = None;
    for range in crate::markdown::prose_ranges(body) {
        let before = body[..range.start].rsplit('\n').next().unwrap_or_default();
        let after = body[range.end..].split('\n').next().unwrap_or_default();
        if !before.trim().is_empty() || !after.trim().is_empty() {
            continue;
        }
        let mut fields = body[range].split_ascii_whitespace();
        let (Some(keyword), Some(of), Some(token), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if !keyword.eq_ignore_ascii_case("duplicate") || !of.eq_ignore_ascii_case("of") {
            continue;
        }
        let Some(number) = token
            .strip_suffix('.')
            .unwrap_or(token)
            .strip_prefix('#')
            .filter(|value| value.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|number| (1..=MAX_NUMBER).contains(number))
        else {
            continue;
        };
        if found.is_some_and(|earlier| earlier != number) {
            return None;
        }
        found = Some(number);
    }
    found
}

fn chain_is_valid(
    context: &Transaction<'_>,
    subject: Subject,
    mut canonical: Subject,
) -> Result<bool> {
    for _ in 0..MAX_CHAIN {
        if subject == canonical {
            return Ok(false);
        }
        let Some(mark) = context
            .document::<Mark>(&key(canonical))?
            .filter(|mark| mark.active)
        else {
            return Ok(true);
        };
        canonical.number = mark.canonical;
    }
    Ok(false)
}

pub(in crate::cells::repository) fn from_comment(
    context: &Transaction<'_>,
    source: &activity::Event,
    body: &str,
) -> Result<()> {
    let Some(number) = reference(body) else {
        return Ok(());
    };
    let subject = source.target.subject();
    // Crab uses separate issue and PR numbers, so references retain the
    // discussion kind of the comment's parent.
    let canonical = Subject {
        kind: subject.kind,
        number,
    };
    if context
        .document::<Mark>(&key(subject))?
        .is_some_and(|mark| mark.active && mark.canonical == number)
        || !chain_is_valid(context, subject, canonical)?
    {
        return Ok(());
    }
    let Some(summary) = canonical.summary(context)? else {
        return Ok(());
    };
    let event = activity::append(
        context,
        target(subject),
        &source.actor,
        activity::Action::DuplicateMarked {
            number,
            subject_kind: subject.kind,
            title: summary.title,
        },
    )?;
    context.put_document(
        &key(subject),
        &Mark {
            event: event.number,
            canonical: number,
            active: true,
        },
    )
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Removal {
    pub subject: Subject,
    pub event: u64,
    pub actor: RepositoryAuthor,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Outcome {
    Removed,
    Archived,
    NotFound,
    Conflict,
}

pub(crate) struct Undo;
impl Mutation for Undo {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 317;
    type Input = Removal;
    type Output = Outcome;

    fn execute(context: &mut Transaction<'_>, input: Removal) -> Result<MutationResult<Outcome>> {
        validate_author(&input.actor)?;
        validate_number(input.subject.number)?;
        validate_number(input.event)?;
        let key = key(input.subject);
        let Some(mut mark) = context.document::<Mark>(&key)? else {
            return Ok(MutationResult::Rejected(Outcome::NotFound));
        };
        if mark.event != input.event {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        if !mark.active {
            return Ok(MutationResult::Success(Outcome::Removed));
        }
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(Outcome::Archived));
        }
        let canonical = Subject {
            kind: input.subject.kind,
            number: mark.canonical,
        };
        let Some(summary) = canonical.summary(context)? else {
            return Ok(MutationResult::Rejected(Outcome::NotFound));
        };
        mark.active = false;
        context.put_document(&key, &mark)?;
        activity::append(
            context,
            target(input.subject),
            &input.actor,
            activity::Action::DuplicateUnmarked {
                number: mark.canonical,
                subject_kind: input.subject.kind,
                title: summary.title,
            },
        )?;
        advance_revision(context)?;
        Ok(MutationResult::Success(Outcome::Removed))
    }
}

#[cfg(test)]
mod tests {
    use super::reference;

    #[test]
    fn duplicate_commands_are_unambiguous_standalone_prose() {
        assert_eq!(
            reference("Resolved elsewhere.\n\nDuplicate OF #71."),
            Some(71)
        );
        for body in [
            "Not a duplicate of #1",
            "> Duplicate of #1",
            "`Duplicate of #1`",
            "```md\nDuplicate of #1\n```",
            "[Duplicate of #1](https://example.test)",
            "Duplicate of #0",
            "Duplicate of #1oops",
            "Duplicate of #1\n\nDuplicate of #2",
            "Duplicate of repo/name#1",
        ] {
            assert_eq!(reference(body), None, "{body}");
        }
    }
}
