use super::{Entry, Position, Result, Transaction, View};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(super) const RETENTION_MS: i64 = 90 * 24 * 60 * 60 * 1_000;
const FRAME: usize = 50;

pub(super) fn within_retention(now: i64, at: i64) -> bool {
    now.saturating_sub(at) < RETENTION_MS
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Filed {
    pub(super) entry: Entry,
    pub(super) saved: bool,
    pub(super) done_at: Option<i64>,
    pub(super) restored_at: Option<i64>,
}

impl Filed {
    pub(super) fn new(entry: Entry) -> Self {
        Self {
            entry,
            saved: false,
            done_at: None,
            restored_at: None,
        }
    }

    pub(super) fn deliver(&mut self, entry: Entry, restored_at: Option<i64>) -> Result<()> {
        if entry.notice.id == self.entry.notice.id && entry.notice != self.entry.notice {
            return Err(super::Error::RequestConflict);
        }
        if entry.notice.position() > self.entry.notice.position() {
            self.done_at = None;
            self.restored_at = restored_at;
        }
        self.entry.merge(entry);
        Ok(())
    }

    pub(super) fn retained(&self, now: i64) -> bool {
        self.saved
            || self
                .done_at
                .or(self.restored_at)
                .is_some_and(|at| within_retention(now, at))
    }

    pub(super) fn visible(&self, view: View, now: i64) -> bool {
        match view {
            View::Inbox => self.done_at.is_none() && self.retained(now),
            View::Saved => self.saved,
            View::Done => self.done_at.is_some_and(|at| within_retention(now, at)),
        }
    }

    fn indexed_in(&self, view: View) -> bool {
        match view {
            View::Inbox => self.done_at.is_none() && (self.saved || self.restored_at.is_some()),
            View::Saved => self.saved,
            View::Done => self.done_at.is_some(),
        }
    }

    pub(super) fn id(&self) -> Uuid {
        self.entry.notice.thread_id()
    }

    pub(super) fn position(&self) -> Position {
        self.entry.notice.position()
    }
}

pub(super) const ITEM_PREFIX: &str = "notifications/v1/filed/";

pub(super) fn item_key(actor: Uuid, thread: Uuid) -> String {
    format!("{ITEM_PREFIX}{actor}/{thread}")
}

fn index_prefix(actor: Uuid, view: View) -> String {
    let name = match view {
        View::Inbox => "inbox",
        View::Saved => "saved",
        View::Done => "done",
    };
    format!("notifications/v1/filed-order/{actor}/{name}/")
}

fn index_key(prefix: &str, position: Position) -> String {
    format!("{prefix}{:016}:{}", position.created_at_ms, position.id)
}

pub(super) fn load(context: &Transaction<'_>, actor: Uuid, thread: Uuid) -> Result<Option<Filed>> {
    context.document(&item_key(actor, thread))
}

pub(super) fn store(
    context: &Transaction<'_>,
    actor: Uuid,
    previous: Option<&Filed>,
    current: &Filed,
) -> Result<bool> {
    let keep = current.saved || current.done_at.is_some() || current.restored_at.is_some();
    if previous == keep.then_some(current) {
        return Ok(false);
    }
    // Move the owner and all affected indexes in one transaction. Active
    // reads and badge polling only walk active filed rows.
    for view in [View::Inbox, View::Saved, View::Done] {
        let prefix = index_prefix(actor, view);
        let old = previous
            .filter(|record| record.indexed_in(view))
            .map(|record| index_key(&prefix, record.position()));
        let new = current
            .indexed_in(view)
            .then(|| index_key(&prefix, current.position()));
        if old == new {
            continue;
        }
        if let Some(key) = old {
            context.delete_document(&key)?;
        }
        if let Some(key) = new {
            context.put_document(&key, &current.id())?;
        }
    }
    // While completed, the owner record also suppresses older recent copies.
    let key = item_key(actor, current.id());
    if keep {
        context.put_document(&key, current)?;
    } else {
        context.delete_document(&key)?;
    }
    Ok(true)
}

pub(super) fn remove_done_index(
    context: &Transaction<'_>,
    actor: Uuid,
    record: &Filed,
) -> Result<bool> {
    context.delete_document(&index_key(
        &index_prefix(actor, View::Done),
        record.position(),
    ))
}

pub(super) fn page(
    context: &Transaction<'_>,
    actor: Uuid,
    before: Option<Position>,
    view: View,
) -> Result<(Vec<Filed>, Option<Position>)> {
    let prefix = index_prefix(actor, view);
    let end = before.map(|position| index_key(&prefix, position));
    let keys = context.documents::<Uuid>(&prefix, end.as_deref(), FRAME + 1)?;
    let more = keys.len() > FRAME;
    let mut items = Vec::with_capacity(keys.len().min(FRAME));
    for (key, thread) in keys.into_iter().take(FRAME) {
        let record = load(context, actor, thread)?
            .filter(|record| {
                record.indexed_in(view) && index_key(&prefix, record.position()) == key
            })
            .ok_or(super::Error::Invalid(
                "Notification index entry is inconsistent",
            ))?;
        items.push(record);
    }
    let next = items.last().filter(|_| more).map(Filed::position);
    Ok((items, next))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cells::{
        directory::notifications::{Notice, Reason},
        repository::collaboration::Target,
    };

    #[test]
    fn completion_expires_from_done_without_expiring_its_bookmark() {
        let start = 100;
        let mut filed = Filed::new(Entry {
            notice: Notice {
                id: Uuid::now_v7(),
                repository: Uuid::now_v7(),
                target: Target::Issue { number: 1 }.into(),
                created_at_ms: 1,
                sender: "Alice".into(),
            },
            reason: Reason::Mention,
            read: true,
            resumed: None,
        });
        filed.saved = true;
        filed.done_at = Some(start);

        assert!(filed.visible(View::Done, start + RETENTION_MS - 1));
        assert!(!filed.visible(View::Done, start + RETENTION_MS));
        assert!(filed.visible(View::Saved, start + RETENTION_MS));
        assert!(!filed.visible(View::Inbox, start + RETENTION_MS));
    }
}
