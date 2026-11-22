use serde::{Deserialize, Serialize};

use super::{
    CLASS, Error, IdentityKey, Key, Mode, Read, Result, Subject, SubjectKind, SubscriptionOrigin,
    SubscriptionReason, Transaction, discussions::DiscussionSubscription, identity_key, key_text,
    subscription, thread_subscription,
};

pub(crate) const TIMED_PAGE: usize = 20;

const REASONS: [SubscriptionReason; 4] = [
    SubscriptionReason::Manual,
    SubscriptionReason::Participation,
    SubscriptionReason::Mention,
    SubscriptionReason::Assignment,
];

fn prefix(actor: &str, kind: SubjectKind, reason: SubscriptionReason) -> String {
    let kind = match kind {
        SubjectKind::Issue => "issue",
        SubjectKind::Pull => "pull",
    };
    let reason = match reason {
        SubscriptionReason::Manual => "manual",
        SubscriptionReason::Participation => "participation",
        SubscriptionReason::Mention => "mention",
        SubscriptionReason::Assignment => "assignment",
        SubscriptionReason::Unknown => "unknown",
    };
    format!("collaboration/v2/watches/order/{actor}/{kind}/{reason}/")
}

fn stamp(at: u64, subject: Subject) -> String {
    format!("{at:016x}/{}", subject.key())
}

pub(super) fn update(
    context: &Transaction<'_>,
    actor: Key,
    subject: Subject,
    previous: Option<SubscriptionOrigin>,
    current: Option<SubscriptionOrigin>,
) -> Result<()> {
    let actor = key_text(actor);
    let key = |origin: SubscriptionOrigin| {
        prefix(&actor, subject.kind, origin.reason) + &stamp(origin.subscribed_at_ms, subject)
    };
    if let Some(previous) = previous {
        context.delete_document(&key(previous))?;
    }
    if let Some(current) = current {
        context.put_document(&key(current), &subject)?;
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum TimeBound {
    Start(u64),
    End(u64),
    Subject(u64, Subject),
}

impl TimeBound {
    fn key(&self) -> String {
        match self {
            Self::Start(at) => format!("{at:016x}/"),
            Self::End(at) => format!("{at:016x}/~"),
            Self::Subject(at, subject) => stamp(*at, *subject),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TimedQuery {
    pub actor: IdentityKey,
    pub kind: Option<SubjectKind>,
    pub reason: Option<SubscriptionReason>,
    pub oldest: bool,
    pub bound: Option<TimeBound>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TimedPage {
    pub items: Vec<DiscussionSubscription>,
    pub muted: bool,
}

pub(crate) struct ListTimedDiscussions;

impl Read for ListTimedDiscussions {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 428;
    type Input = TimedQuery;
    type Output = TimedPage;

    fn execute(context: &mut Transaction<'_>, input: TimedQuery) -> Result<TimedPage> {
        let actor = identity_key(&input.actor)?;
        let digest = key_text(actor);
        let bound = input.bound.map(|bound| bound.key());
        let mut candidates = Vec::new();
        // One durable row per subscription. Merge the small tails of only
        // the relevant partitions rather than scanning unrelated reasons.
        for kind in [SubjectKind::Issue, SubjectKind::Pull] {
            if input.kind.is_some_and(|value| value != kind) {
                continue;
            }
            for reason in REASONS {
                if input.reason.is_some_and(|value| value != reason) {
                    continue;
                }
                let prefix = prefix(&digest, kind, reason);
                let cursor = bound.as_ref().map(|value| format!("{prefix}{value}"));
                let rows = if input.oldest {
                    context.documents_after::<Subject>(
                        &prefix,
                        cursor.as_deref(),
                        TIMED_PAGE + 1,
                    )?
                } else {
                    context.documents::<Subject>(&prefix, cursor.as_deref(), TIMED_PAGE + 1)?
                };
                for (key, subject) in rows {
                    let position = key
                        .strip_prefix(&prefix)
                        .ok_or(Error::Invalid("Subscription index key is invalid"))?;
                    candidates.push((position.to_owned(), subject, reason));
                }
            }
        }
        candidates.sort_unstable_by(|a, b| {
            if input.oldest {
                a.0.cmp(&b.0)
            } else {
                b.0.cmp(&a.0)
            }
        });
        let mut items = Vec::new();
        for (position, subject, reason) in candidates.into_iter().take(TIMED_PAGE + 1) {
            let current = thread_subscription(context, subject, actor)?;
            let origin = current
                .origin
                .filter(|origin| {
                    origin.reason == reason && stamp(origin.subscribed_at_ms, subject) == position
                })
                .filter(|_| current.preference.mode.follows())
                .ok_or(Error::Invalid("Subscription index entry is inconsistent"))?;
            if let Some(discussion) = subject.summary(context)? {
                items.push(DiscussionSubscription {
                    discussion,
                    preference: current.preference,
                    origin: Some(origin),
                });
            }
        }
        Ok(TimedPage {
            items,
            muted: subscription(context, actor)?.preference.mode == Mode::Muted,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests;
