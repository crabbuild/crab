use super::{
    CLASS, Error, IdentityKey, Mutation, MutationResult, Read, Result, Subject, ThreadMode,
    Transaction, activity, identity_key, save_thread, thread_subscription,
};
use crate::cells::directory::notifications::Position;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

const MAX_BATCH: usize = 50;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Dismissal {
    through: Position,
    resumed: bool,
}

impl Dismissal {
    pub(super) fn resume(&mut self) {
        self.resumed = true;
    }

    fn shows(&self, item: InboxActivity) -> bool {
        item.updated > self.through
            && (self.resumed || item.resumed.is_some_and(|source| source > self.through))
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InboxActivity {
    pub subject: Subject,
    pub updated: Position,
    pub resumed: Option<Position>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InboxVisibilityInput {
    pub actor: IdentityKey,
    pub items: Vec<InboxActivity>,
}

pub(crate) struct InboxVisibility;
impl Read for InboxVisibility {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 424;
    type Input = InboxVisibilityInput;
    type Output = Vec<bool>;

    fn execute(context: &mut Transaction<'_>, input: InboxVisibilityInput) -> Result<Self::Output> {
        if input.items.len() > MAX_BATCH {
            return Err(Error::Invalid("Notification visibility batch is too large"));
        }
        let key = identity_key(&input.actor)?;
        input
            .items
            .into_iter()
            .map(|item| {
                let subscription = thread_subscription(context, item.subject, key)?;
                Ok(subscription
                    .dismissed
                    .is_none_or(|dismissed| dismissed.shows(item)))
            })
            .collect()
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DismissItem {
    pub subject: Subject,
    pub expected: Position,
    pub selected: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DismissSelection {
    pub actor: IdentityKey,
    pub items: Vec<DismissItem>,
}

pub(crate) struct Dismiss;
impl Mutation for Dismiss {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 424;
    type Input = DismissSelection;
    type Output = Vec<bool>;

    fn execute(
        context: &mut Transaction<'_>,
        input: DismissSelection,
    ) -> Result<MutationResult<Self::Output>> {
        if input.items.is_empty() || input.items.len() > MAX_BATCH {
            return Err(Error::Invalid("Inbox unsubscription batch is invalid"));
        }
        let mut subjects = BTreeSet::new();
        for item in &input.items {
            item.expected.validate()?;
            if !subjects.insert(item.subject.key()) {
                return Err(Error::Invalid(
                    "Inbox unsubscription contains duplicate discussions",
                ));
            }
        }
        let key = identity_key(&input.actor)?;
        let choice_after = activity::latest(context)?;
        let mut results = Vec::with_capacity(input.items.len());
        for item in input.items {
            let mut current = thread_subscription(context, item.subject, key)?;
            // A replay must not undo a follow made after this dismissal.
            if current
                .dismissed
                .as_ref()
                .is_some_and(|dismissed| dismissed.through >= item.expected)
            {
                results.push(true);
                continue;
            }
            if !item.selected || item.subject.closed(context)?.is_none() {
                results.push(false);
                continue;
            }
            current.choice_after = choice_after;
            current.after = choice_after;
            current.dismissed = Some(Dismissal {
                through: item.expected,
                resumed: false,
            });
            save_thread(
                context,
                item.subject,
                &input.actor,
                key,
                &mut current,
                ThreadMode::Unsubscribed,
                None,
            )?;
            results.push(true);
        }
        Ok(MutationResult::Success(results))
    }
}
