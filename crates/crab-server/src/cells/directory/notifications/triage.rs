use super::{
    Access, Entry, Filed, Inbox, Position, State, Thread, active_account, conversations, filing,
};
use crate::cells::directory::{
    CLASS, Error, IdentityKey, Mutation, MutationResult, Read, Result, Transaction, advance,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;

const MAX_SELECTION: usize = 50;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Action {
    Read(bool),
    Save(bool),
    Done(bool),
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selected {
    pub id: Uuid,
    pub repository_id: Uuid,
    pub expected: Position,
}

impl Selected {
    fn matches(self, entry: &Entry) -> bool {
        entry.notice.repository == self.repository_id && entry.notice.thread_id() == self.id
    }
}

pub(crate) fn validate_selection(items: &[Selected]) -> std::result::Result<(), &'static str> {
    if items.is_empty() || items.len() > MAX_SELECTION {
        return Err("Notification selection requires 1–50 rows");
    }
    let mut ids = BTreeSet::new();
    for item in items {
        item.expected
            .validate()
            .map_err(|_| "Notification position is invalid")?;
        if item.id.is_nil() || item.repository_id.is_nil() || !ids.insert(item.id) {
            return Err("Notification selection has invalid or duplicate rows");
        }
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Selection {
    pub identity: IdentityKey,
    pub items: Vec<Selected>,
}

pub(crate) struct CheckSelection;
impl Read for CheckSelection {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 24;
    type Input = Selection;
    type Output = Vec<bool>;

    fn execute(context: &mut Transaction<'_>, input: Selection) -> Result<Self::Output> {
        validate_selection(&input.items).map_err(Error::Invalid)?;
        let Some(actor) = active_account(context, input.identity)? else {
            return Ok(vec![false; input.items.len()]);
        };
        let inboxes = Access::CHANNELS
            .into_iter()
            .map(|access| {
                context
                    .document::<Inbox>(&access.inbox(actor))
                    .map(Option::unwrap_or_default)
            })
            .collect::<Result<Vec<_>>>()?;
        input
            .items
            .into_iter()
            .map(|item| {
                let archived = filing::load(context, actor, item.id)?.filter(|record| {
                    item.matches(&record.entry) && record.retained(context.now_ms())
                });
                let latest = inboxes
                    .iter()
                    .flat_map(|inbox| &inbox.entries)
                    .filter(|entry| item.matches(entry))
                    .map(|entry| entry.notice.position())
                    .chain(
                        archived
                            .as_ref()
                            .map(|record| record.entry.notice.position()),
                    )
                    .max();
                Ok(latest == Some(item.expected))
            })
            .collect()
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct SetStateInput {
    pub identity: IdentityKey,
    pub items: Vec<Selected>,
    pub action: Action,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Changed {
    pub id: Uuid,
    #[serde(flatten)]
    pub state: State,
}

pub(crate) struct SetState;
impl Mutation for SetState {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 20;
    type Input = SetStateInput;
    type Output = Option<Vec<Changed>>;

    fn execute(
        context: &mut Transaction<'_>,
        input: SetStateInput,
    ) -> Result<MutationResult<Self::Output>> {
        validate_selection(&input.items).map_err(Error::Invalid)?;
        let Some(actor) = active_account(context, input.identity)? else {
            return Ok(MutationResult::Rejected(None));
        };
        let mut inboxes: Vec<_> = Access::CHANNELS
            .into_iter()
            .map(|access| {
                let key = access.inbox(actor);
                let inbox = context.document::<Inbox>(&key)?.unwrap_or_default();
                Ok((key, inbox, false))
            })
            .collect::<Result<_>>()?;
        let mut changed = false;
        let mut applied = Vec::with_capacity(input.items.len());
        let now = context.now_ms();
        for item in input.items {
            let previous =
                filing::load(context, actor, item.id)?.filter(|record| item.matches(&record.entry));
            let mut current = previous.as_ref().map(|record| record.entry.clone());
            let mut in_recent = false;
            for entry in inboxes.iter().flat_map(|(_, inbox, _)| &inbox.entries) {
                if item.matches(entry) {
                    in_recent |= entry.notice.position() == item.expected;
                    if let Some(current) = &mut current {
                        current.merge(entry.clone());
                    } else {
                        current = Some(entry.clone());
                    }
                }
            }
            let Some(entry) = current.filter(|entry| entry.notice.position() == item.expected)
            else {
                // The Directory savepoint rolls back earlier rows when any
                // selection is missing or newer than the version the user saw.
                return Ok(MutationResult::Rejected(None));
            };
            let mut filed = previous
                .clone()
                .unwrap_or_else(|| Filed::new(entry.clone()));
            if !filed.retained(now) && (!in_recent || filed.done_at.is_some()) {
                return Ok(MutationResult::Rejected(None));
            }
            filed.entry = entry;
            match input.action {
                Action::Read(read) => filed.entry.read = read,
                Action::Save(save) => filed.saved = save,
                Action::Done(true) => {
                    filed.entry.read = true;
                    filed.done_at.get_or_insert(now);
                    filed.restored_at = None;
                }
                Action::Done(false) => {
                    if filed.done_at.take().is_some() && !in_recent {
                        filed.restored_at = Some(now);
                    }
                }
            }
            changed |= filing::store(context, actor, previous.as_ref(), &filed)?;
            for (_, inbox, updates) in &mut inboxes {
                for entry in inbox.entries.iter_mut().filter(|entry| item.matches(entry)) {
                    let reason = if entry.notice.position() == item.expected {
                        entry.reason.max(filed.entry.reason)
                    } else {
                        entry.reason
                    };
                    *updates |= entry.read != filed.entry.read || entry.reason != reason;
                    entry.read = filed.entry.read;
                    entry.reason = reason;
                }
            }
            applied.push(Changed {
                id: item.id,
                state: Thread::from(filed).state,
            });
        }
        for (key, mut inbox, updates) in inboxes {
            if updates {
                inbox.entries = conversations(inbox.entries);
                context.put_document(&key, &inbox)?;
                changed = true;
            }
        }
        if changed {
            advance(context)?;
        }
        Ok(MutationResult::Success(Some(applied)))
    }
}
