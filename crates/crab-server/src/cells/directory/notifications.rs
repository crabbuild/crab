use super::{
    AccountsExist, CLASS, Error, IdentityKey, LookupAccounts, MAX_VERSION, Mutation,
    MutationResult, Read, Result, SqlValue, Transaction, advance, validate_handle,
};
use crate::cells::repository::collaboration::Target;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub(crate) const MAX_RECIPIENTS: usize = 32;
pub(crate) const MAX_HISTORY: usize = 100;
const MAX_HANDLES: usize = 40;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Notice {
    pub id: Uuid,
    pub repository: Uuid,
    pub target: Target,
    pub created_at_ms: u64,
    pub sender: String,
}

impl Notice {
    pub(crate) fn position(&self) -> Position {
        Position {
            created_at_ms: self.created_at_ms,
            id: self.id,
        }
    }

    fn validate(&self) -> Result<()> {
        self.position().validate()?;
        if self.repository.is_nil()
            || self.sender.is_empty()
            || self.sender.chars().count() > 160
            || self.sender.chars().any(char::is_control)
        {
            return Err(Error::Invalid("Notification source is invalid"));
        }
        use Target::*;
        let valid = |number: u64| (1..=MAX_VERSION).contains(&number);
        let numbers_valid = match self.target {
            Issue { number } | Pull { number } => valid(number),
            IssueComment { issue, number } => valid(issue) && valid(number),
            PullComment { pull, number }
            | PullReview { pull, number }
            | PullThread { pull, number } => valid(pull) && valid(number),
            PullReply {
                pull,
                thread,
                number,
            } => valid(pull) && valid(thread) && valid(number),
        };
        if !numbers_valid {
            return Err(Error::Invalid("Notification target is invalid"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reason {
    Mention,
    TeamMention,
    ReviewRequested,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Recipient {
    pub actor: Uuid,
    pub reason: Reason,
    pub access: Access,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Access {
    Group,
    Direct,
}

impl Access {
    const CHANNELS: [Self; 2] = [Self::Direct, Self::Group];

    fn inbox(self, actor: Uuid) -> String {
        match self {
            Self::Direct => format!("notifications/v1/{actor}"),
            Self::Group => format!("notifications/v1/group/{actor}"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Delivery {
    pub notice: Notice,
    pub recipients: Vec<Recipient>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub(crate) struct Position {
    pub created_at_ms: u64,
    pub id: Uuid,
}

impl Position {
    fn validate(self) -> Result<()> {
        if self.id.is_nil() || self.created_at_ms > MAX_VERSION {
            return Err(Error::Invalid("Notification position is invalid"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    pub notice: Notice,
    pub reason: Reason,
    pub read: bool,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Inbox {
    entries: Vec<Entry>,
    evicted_through: Option<Position>,
}

impl Inbox {
    fn deliver(&mut self, notice: &Notice, reason: Reason) -> Result<bool> {
        if let Some(current) = self
            .entries
            .iter_mut()
            .find(|item| item.notice.id == notice.id)
        {
            if current.notice != *notice {
                return Err(Error::RequestConflict);
            }
            if current.reason == Reason::TeamMention && reason == Reason::Mention {
                current.reason = reason;
                return Ok(true);
            }
            return Ok(false);
        }
        let position = notice.position();
        if self.evicted_through.is_some_and(|floor| position <= floor) {
            return Ok(false);
        }
        let index = self
            .entries
            .partition_point(|entry| entry.notice.position() > position);
        if index < MAX_HISTORY {
            self.entries.insert(
                index,
                Entry {
                    notice: notice.clone(),
                    reason,
                    read: false,
                },
            );
        }
        if self.entries.len() > MAX_HISTORY {
            self.evicted_through = self.entries.pop().map(|entry| entry.notice.position());
        }
        // A replay delayed past eviction must not restore old notifications.
        Ok(index < MAX_HISTORY)
    }
}

fn active_account(context: &mut Transaction<'_>, identity: IdentityKey) -> Result<Option<Uuid>> {
    Ok(LookupAccounts::execute(context, vec![identity])?
        .into_iter()
        .next()
        .flatten())
}

pub(crate) struct Deliver;
impl Mutation for Deliver {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 18;
    type Input = Delivery;
    type Output = ();

    fn execute(context: &mut Transaction<'_>, input: Delivery) -> Result<MutationResult<()>> {
        input.notice.validate()?;
        if input.recipients.is_empty() || input.recipients.len() > MAX_RECIPIENTS {
            return Err(Error::Invalid("Notification delivery batch is invalid"));
        }
        let mut recipients = BTreeMap::new();
        for Recipient {
            actor,
            reason,
            access,
        } in input.recipients
        {
            let (current, route) = recipients.entry(actor).or_insert((reason, access));
            if reason == Reason::Mention {
                *current = reason;
            }
            *route = (*route).max(access);
        }
        let exists = AccountsExist::execute(context, recipients.keys().copied().collect())?;
        if exists.iter().any(|present| !present) {
            return Err(Error::Invalid("Notification recipient is missing"));
        }
        let mut changed = false;
        for (actor, (reason, access)) in recipients {
            let key = access.inbox(actor);
            let mut inbox: Inbox = context.document(&key)?.unwrap_or_default();
            if inbox.deliver(&input.notice, reason)? {
                context.put_document(&key, &inbox)?;
                changed = true;
            }
        }
        if changed {
            advance(context)?;
        }
        Ok(MutationResult::Success(()))
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct ListInput {
    pub identity: IdentityKey,
    pub before: Option<Position>,
}

pub(crate) struct List;
impl Read for List {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 19;
    type Input = ListInput;
    type Output = Vec<Entry>;

    fn execute(context: &mut Transaction<'_>, input: ListInput) -> Result<Vec<Entry>> {
        if let Some(before) = input.before {
            before.validate()?;
        }
        let Some(actor) = active_account(context, input.identity)? else {
            return Ok(Vec::new());
        };
        // A retry after an ACL change can move an event between collections.
        // Present one entry and keep read state from either delivery.
        let mut entries = BTreeMap::<Position, Entry>::new();
        for access in Access::CHANNELS {
            let inbox: Inbox = context.document(&access.inbox(actor))?.unwrap_or_default();
            for entry in inbox.entries.into_iter().filter(|entry| {
                input
                    .before
                    .is_none_or(|before| entry.notice.position() < before)
            }) {
                entries
                    .entry(entry.notice.position())
                    .and_modify(|existing| {
                        existing.read |= entry.read;
                        if entry.reason == Reason::Mention {
                            existing.reason = Reason::Mention;
                        }
                    })
                    .or_insert(entry);
            }
        }
        Ok(entries.into_values().rev().collect())
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct MarkReadInput {
    pub identity: IdentityKey,
    pub position: Position,
    pub read: bool,
}

pub(crate) struct MarkRead;
impl Mutation for MarkRead {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 20;
    type Input = MarkReadInput;
    type Output = ();

    fn execute(context: &mut Transaction<'_>, input: MarkReadInput) -> Result<MutationResult<()>> {
        input.position.validate()?;
        if let Some(actor) = active_account(context, input.identity)? {
            let mut changed = false;
            for access in Access::CHANNELS {
                let key = access.inbox(actor);
                let Some(mut inbox) = context.document::<Inbox>(&key)? else {
                    continue;
                };
                if let Some(entry) = inbox.entries.iter_mut().find(|entry| {
                    entry.notice.position() == input.position && entry.read != input.read
                }) {
                    entry.read = input.read;
                    context.put_document(&key, &inbox)?;
                    changed = true;
                }
            }
            if changed {
                advance(context)?;
            }
        }
        Ok(MutationResult::Success(()))
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct HandlesAt {
    pub names: Vec<String>,
    pub before_ms: u64,
}

pub(crate) struct ResolveHandlesAt;
impl Read for ResolveHandlesAt {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 21;
    type Input = HandlesAt;
    type Output = Vec<Option<Uuid>>;

    fn execute(context: &mut Transaction<'_>, input: HandlesAt) -> Result<Self::Output> {
        if input.names.len() > MAX_HANDLES
            || input.names.iter().collect::<BTreeSet<_>>().len() != input.names.len()
            || input.before_ms > MAX_VERSION
        {
            return Err(Error::Invalid("Notification handle lookup is invalid"));
        }
        let before = i64::try_from(input.before_ms)
            .map_err(|_| Error::Invalid("Notification handle time exceeds its range"))?;
        input
            .names
            .into_iter()
            .map(|name| {
                validate_handle(&name).map_err(Error::Invalid)?;
                // A rename or reused alias after publication must not move a
                // delayed notification to a different account.
                let rows = context.query(
                    "SELECT id FROM profiles WHERE handle = ? AND updated_at_ms < ?",
                    &[SqlValue::Text(name), SqlValue::Integer(before)],
                )?;
                match rows.rows.first().map(Vec::as_slice) {
                    None => Ok(None),
                    Some([SqlValue::Blob(id)]) => {
                        Uuid::from_slice(id).map(Some).map_err(Error::sdk)
                    }
                    _ => Err(Error::Invalid("Notification actor is invalid")),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_preserves_read_state_and_does_not_restore_evicted_history() {
        let notice = |number: u64| Notice {
            id: Uuid::from_u128(u128::from(number)),
            repository: Uuid::from_u128(1000),
            target: Target::Issue { number: 1 },
            created_at_ms: number,
            sender: "Alice".into(),
        };
        let mut inbox = Inbox::default();
        let first = notice(1);
        assert!(inbox.deliver(&first, Reason::TeamMention).unwrap());
        inbox.entries[0].read = true;
        assert!(inbox.deliver(&first, Reason::Mention).unwrap());
        assert!(inbox.entries[0].read);
        assert_eq!(inbox.entries[0].reason, Reason::Mention);
        for number in 2..=(MAX_HISTORY as u64 + 1) {
            assert!(inbox.deliver(&notice(number), Reason::Mention).unwrap());
        }
        assert_eq!(inbox.entries.len(), MAX_HISTORY);
        assert_eq!(inbox.evicted_through, Some(first.position()));
        assert!(!inbox.deliver(&first, Reason::Mention).unwrap());
    }
}
