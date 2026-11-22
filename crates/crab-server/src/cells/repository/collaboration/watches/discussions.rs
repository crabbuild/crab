use serde::{Deserialize, Serialize};

use super::{
    CLASS, IdentityKey, Mode, Read, Result, Subject, SubjectKind, ThreadPreference, Transaction,
    identity_key, subscription, thread_subscription,
};
use crate::cells::{
    Error,
    repository::{SqlValue, collaboration::SubjectSummary, integer},
};

const SCAN: usize = 32;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiscussionQuery {
    pub actor: IdentityKey,
    pub kind: Option<SubjectKind>,
    pub after: Option<Subject>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiscussionSubscription {
    pub discussion: SubjectSummary,
    pub preference: ThreadPreference,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiscussionPage {
    pub items: Vec<DiscussionSubscription>,
    pub next: Option<Subject>,
    pub muted: bool,
}

pub(crate) struct ListDiscussions;

impl Read for ListDiscussions {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 427;
    type Input = DiscussionQuery;
    type Output = DiscussionPage;

    fn execute(context: &mut Transaction<'_>, input: DiscussionQuery) -> Result<DiscussionPage> {
        if let Some(after) = input.after {
            crate::cells::repository::validate_number(after.number)?;
            if input.kind.is_some_and(|kind| kind != after.kind) {
                return Err(Error::Invalid("Subscription cursor kind does not match"));
            }
        }
        let actor = identity_key(&input.actor)?;
        let mut scanned = Vec::new();
        // Walk the canonical discussion primary keys and probe this actor's
        // preferences. Follows created before this query already use those keys.
        for kind in [SubjectKind::Issue, SubjectKind::Pull] {
            if input.kind.is_some_and(|selection| selection != kind)
                || (kind == SubjectKind::Issue
                    && input
                        .after
                        .is_some_and(|after| after.kind == SubjectKind::Pull))
            {
                continue;
            }
            let offset = input
                .after
                .filter(|after| after.kind == kind)
                .map_or(0, |after| after.number);
            let limit = SCAN + 1 - scanned.len();
            let sql = match kind {
                SubjectKind::Issue => {
                    "SELECT number FROM repository_issues WHERE number > ? ORDER BY number LIMIT ?"
                }
                SubjectKind::Pull => {
                    "SELECT number FROM repository_pulls WHERE number > ? ORDER BY number LIMIT ?"
                }
            };
            for row in context
                .query(sql, &[integer(offset)?, integer(limit as u64)?])?
                .rows
            {
                let [SqlValue::Integer(number)] = row.as_slice() else {
                    return Err(Error::Invalid("Subscription discussion row is invalid"));
                };
                scanned.push(Subject {
                    kind,
                    number: u64::try_from(*number)
                        .map_err(|_| Error::Invalid("Subscription discussion number is invalid"))?,
                });
            }
            if scanned.len() > SCAN {
                break;
            }
        }
        let next = (scanned.len() > SCAN).then(|| scanned[SCAN - 1]);
        let mut items = Vec::new();
        for subject in scanned.into_iter().take(SCAN) {
            let preference = thread_subscription(context, subject, actor)?.preference;
            if preference.mode.follows()
                && let Some(discussion) = subject.summary(context)?
            {
                items.push(DiscussionSubscription {
                    discussion,
                    preference,
                });
            }
        }
        Ok(DiscussionPage {
            items,
            next,
            muted: subscription(context, actor)?.preference.mode == Mode::Muted,
        })
    }
}
