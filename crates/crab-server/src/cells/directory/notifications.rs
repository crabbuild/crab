use super::{
    AccountsExist, CLASS, Error, IdentityKey, LookupAccounts, MAX_VERSION, Mutation,
    MutationResult, Read, Result, SqlValue, Transaction, advance, validate_handle,
};
use crate::cells::repository::collaboration::{Subject, Target};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

mod filing;
mod triage;
use filing::Filed;
pub(crate) use triage::{
    Action, Changed, CheckSelection, Selected, Selection, SetState, SetStateInput,
    validate_selection,
};

pub(crate) const MAX_RECIPIENTS: usize = 32;
pub(crate) const MAX_HISTORY: usize = 100;
const MAX_HANDLES: usize = 40;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReleaseKind {
    Release,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged, deny_unknown_fields)]
pub(crate) enum NoticeTarget {
    Discussion(Target),
    Release { kind: ReleaseKind, number: u64 },
}

impl From<Target> for NoticeTarget {
    fn from(target: Target) -> Self {
        Self::Discussion(target)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Notice {
    pub id: Uuid,
    pub repository: Uuid,
    pub target: NoticeTarget,
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

    pub(crate) fn thread_id(&self) -> Uuid {
        match self.target {
            NoticeTarget::Discussion(target) => thread_id(self.repository, target.subject()),
            NoticeTarget::Release { number, .. } => {
                subject_id(self.repository, &format!("release/{number:016x}"))
            }
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
        let target = match self.target {
            NoticeTarget::Release { number, .. } => {
                return if valid(number) {
                    Ok(())
                } else {
                    Err(Error::Invalid("Release notification target is invalid"))
                };
            }
            NoticeTarget::Discussion(target) => target,
        };
        let numbers_valid = match target {
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

pub(crate) fn thread_id(repository: Uuid, subject: Subject) -> Uuid {
    subject_id(repository, &subject.key())
}

fn subject_id(repository: Uuid, subject: &str) -> Uuid {
    let mut hash = blake3::Hasher::new_derive_key("crab inbox conversation v1");
    hash.update(repository.as_bytes());
    hash.update(subject.as_bytes());
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hash.finalize().as_bytes()[..16]);
    uuid::Builder::from_custom_bytes(bytes).into_uuid()
}

// Direct reasons persist when more routine activity updates a thread.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reason {
    Subscribed,
    Release,
    Assigned,
    ReviewRequested,
    TeamMention,
    Mention,
}

impl Reason {
    fn resumes(self) -> bool {
        matches!(
            self,
            Self::Mention | Self::TeamMention | Self::ReviewRequested
        )
    }
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

    fn other(self) -> Self {
        match self {
            Self::Group => Self::Direct,
            Self::Direct => Self::Group,
        }
    }

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
    pub(crate) fn validate(self) -> Result<()> {
        if self.id.is_nil() || self.created_at_ms > MAX_VERSION {
            return Err(Error::Invalid("Notification position is invalid"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    pub notice: Notice,
    pub reason: Reason,
    pub read: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed: Option<Position>,
}

impl Entry {
    fn merge(&mut self, incoming: Self) {
        let reason = self.reason.max(incoming.reason);
        // The strongest display reason can be older than the current
        // contribution. Only the actual addressable activity can reopen.
        let resumed = self.resumed.max(incoming.resumed);
        match self.notice.position().cmp(&incoming.notice.position()) {
            std::cmp::Ordering::Less => *self = incoming,
            std::cmp::Ordering::Equal => self.read |= incoming.read,
            std::cmp::Ordering::Greater => {}
        }
        self.reason = reason;
        self.resumed = resumed;
    }
}

fn conversations(entries: impl IntoIterator<Item = Entry>) -> Vec<Entry> {
    let mut threads = BTreeMap::<Uuid, Entry>::new();
    for entry in entries {
        let id = entry.notice.thread_id();
        match threads.entry(id) {
            std::collections::btree_map::Entry::Occupied(mut current) => {
                current.get_mut().merge(entry)
            }
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(entry);
            }
        }
    }
    let mut entries: Vec<_> = threads.into_values().collect();
    entries.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.notice.position()));
    entries
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Inbox {
    entries: Vec<Entry>,
    evicted_through: Option<Position>,
}

impl Inbox {
    fn deliver(&mut self, incoming: Entry) -> Result<bool> {
        let position = incoming.notice.position();
        let thread = incoming.notice.thread_id();
        // Existing histories contain event rows. Fold them before counting
        // capacity, so a busy conversation cannot evict quieter ones.
        self.entries = conversations(std::mem::take(&mut self.entries));
        if let Some(current) = self
            .entries
            .iter_mut()
            .find(|entry| entry.notice.thread_id() == thread)
        {
            if current.notice.id == incoming.notice.id && current.notice != incoming.notice {
                return Err(Error::RequestConflict);
            }
            let previous = current.clone();
            current.merge(incoming);
            if *current == previous {
                return Ok(false);
            }
            self.entries
                .sort_unstable_by_key(|entry| std::cmp::Reverse(entry.notice.position()));
            return Ok(true);
        }
        if self.evicted_through.is_some_and(|floor| position <= floor) {
            return Ok(false);
        }
        let index = self
            .entries
            .partition_point(|entry| entry.notice.position() > position);
        if index >= MAX_HISTORY {
            return Ok(false);
        }
        self.entries.insert(index, incoming);
        if self.entries.len() > MAX_HISTORY {
            self.evicted_through = self.entries.pop().map(|entry| entry.notice.position());
        }
        Ok(true)
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
            *current = (*current).max(reason);
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
            let peer: Inbox = context
                .document(&access.other().inbox(actor))?
                .unwrap_or_default();
            // If this replay moved retention classes, carry the exact
            // activity's read state into the new copy.
            let read = peer
                .entries
                .iter()
                .any(|entry| entry.read && entry.notice.position() == input.notice.position());
            let thread = input.notice.thread_id();
            let previous = filing::load(context, actor, thread)?;
            let mut incoming = Entry {
                notice: input.notice.clone(),
                reason,
                read,
                resumed: reason.resumes().then(|| input.notice.position()),
            };
            let now = context.now_ms();
            if let Some(filed) = previous.as_ref().filter(|filed| filed.retained(now)) {
                incoming.reason = incoming.reason.max(filed.entry.reason);
                incoming.read |=
                    filed.entry.read && filed.entry.notice.position() == incoming.notice.position();
                incoming.resumed = incoming.resumed.max(
                    filed
                        .entry
                        .resumed
                        .filter(|position| *position <= incoming.notice.position()),
                );
            }
            let store_recent = inbox.deliver(incoming.clone())?;
            if let Some(previous) = previous {
                let mut filed = previous.clone();
                let restored_at = (!store_recent && previous.retained(now)).then_some(now);
                // A delayed newer update can reopen a retained thread even
                // when the rolling inbox's eviction floor has moved past it.
                filed.deliver(incoming, restored_at)?;
                changed |= filing::store(context, actor, Some(&previous), &filed)?;
            }
            if store_recent {
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

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum View {
    #[default]
    Inbox,
    Saved,
    Done,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct State {
    pub read: bool,
    pub saved: bool,
    pub done: bool,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Thread {
    pub entry: Entry,
    pub state: State,
}

impl From<Filed> for Thread {
    fn from(filed: Filed) -> Self {
        Self {
            state: State {
                read: filed.entry.read,
                saved: filed.saved,
                done: filed.done_at.is_some(),
            },
            entry: filed.entry,
        }
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct ListInput {
    pub identity: IdentityKey,
    pub before: Option<Position>,
    pub view: View,
}

#[derive(Default, Deserialize, Serialize)]
pub(crate) struct ThreadPage {
    pub threads: Vec<Thread>,
    pub next: Option<Position>,
}

pub(crate) struct List;
impl Read for List {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 19;
    type Input = ListInput;
    type Output = ThreadPage;

    fn execute(context: &mut Transaction<'_>, input: ListInput) -> Result<ThreadPage> {
        if let Some(before) = input.before {
            before.validate()?;
        }
        let Some(actor) = active_account(context, input.identity)? else {
            return Ok(ThreadPage::default());
        };
        let (filed, next) = filing::page(context, actor, input.before, input.view)?;
        let now = context.now_ms();
        let mut threads = BTreeMap::new();
        for record in filed {
            if record.visible(input.view, now) {
                threads.insert(record.entry.notice.thread_id(), Thread::from(record));
            }
        }
        if input.view == View::Inbox {
            let mut recent = Vec::new();
            for access in Access::CHANNELS {
                let inbox: Inbox = context.document(&access.inbox(actor))?.unwrap_or_default();
                recent.extend(inbox.entries);
            }
            // Fold before slicing. A stale copy in the other access channel is
            // not another discussion, even across an archive page boundary.
            for entry in conversations(recent) {
                let position = entry.notice.position();
                if input.before.is_some_and(|before| position >= before)
                    || next.is_some_and(|through| position < through)
                {
                    continue;
                }
                let id = entry.notice.thread_id();
                if let Some(filed) = filing::load(context, actor, id)? {
                    if filed.done_at.is_some() || filed.entry.notice.position() != position {
                        continue;
                    }
                }
                threads
                    .entry(id)
                    .or_insert_with(|| Thread::from(Filed::new(entry)));
            }
        }
        let mut threads: Vec<_> = threads.into_values().collect();
        threads.sort_unstable_by_key(|thread| std::cmp::Reverse(thread.entry.notice.position()));
        Ok(ThreadPage { threads, next })
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

    fn activity(number: u64, target: Target, reason: Reason) -> Entry {
        Entry {
            notice: Notice {
                id: Uuid::from_u128(u128::from(number)),
                repository: Uuid::from_u128(1000),
                target: target.into(),
                created_at_ms: number,
                sender: "Alice".into(),
            },
            reason,
            read: false,
            resumed: None,
        }
    }

    #[test]
    fn delivery_preserves_read_state_and_does_not_restore_evicted_history() {
        let mut inbox = Inbox::default();
        let mut first = activity(1, Target::Issue { number: 1 }, Reason::TeamMention);
        assert!(inbox.deliver(first.clone()).unwrap());
        inbox.entries[0].read = true;
        first.reason = Reason::Mention;
        assert!(inbox.deliver(first.clone()).unwrap());
        assert!(inbox.entries[0].read);
        assert_eq!(inbox.entries[0].reason, Reason::Mention);
        for number in 2..=(MAX_HISTORY as u64 + 1) {
            assert!(
                inbox
                    .deliver(activity(number, Target::Issue { number }, Reason::Mention))
                    .unwrap()
            );
        }
        assert_eq!(inbox.entries.len(), MAX_HISTORY);
        assert_eq!(inbox.evicted_through, Some(first.notice.position()));
        assert!(!inbox.deliver(first).unwrap());
    }

    #[test]
    fn busy_conversations_keep_other_threads_and_the_strongest_reason() {
        let mut inbox = Inbox::default();
        let issue = Target::Issue { number: 1 };
        let pull = Target::Pull { number: 1 };
        let first = activity(2, issue, Reason::Mention);
        let id = first.notice.thread_id();
        assert!(inbox.deliver(activity(1, pull, Reason::Assigned)).unwrap());
        assert!(inbox.deliver(first.clone()).unwrap());
        for number in 3..=(MAX_HISTORY as u64 + 4) {
            assert!(
                inbox
                    .deliver(activity(number, issue, Reason::Subscribed))
                    .unwrap()
            );
        }
        let [latest, remaining] = inbox.entries.as_mut_slice() else {
            panic!("a busy issue and the same-numbered pull remain distinct");
        };
        assert_eq!(latest.notice.thread_id(), id);
        assert_eq!(latest.reason, Reason::Mention);
        assert_ne!(remaining.notice.thread_id(), id);
        latest.read = true;
        assert!(!inbox.deliver(first).unwrap());
        assert!(inbox.entries[0].read);
    }
}
