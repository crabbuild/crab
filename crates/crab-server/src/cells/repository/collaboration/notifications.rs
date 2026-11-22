use super::activity::{self, Event};
use crate::cells::{
    Error, Result,
    application::{Mutation, MutationResult, Read, Transaction},
    directory::IdentityKey,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Mentions {
    pub body_digest: String,
    pub recipients: Vec<Recipient>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Recipient([u8; 32]);

impl Recipient {
    pub(crate) fn qualified(issuer: &str, subject: &str) -> Self {
        // The HTTP boundary resolves directory identities before routing.
        // Fixed-size keys keep fanout bounded across all comments in a review.
        let mut hash = blake3::Hasher::new_derive_key("crab repository inbox recipient v1");
        hash.update(issuer.as_bytes());
        hash.update(&[0]);
        hash.update(subject.as_bytes());
        Self(*hash.finalize().as_bytes())
    }

    fn root(self) -> String {
        format!(
            "collaboration/v1/inbox/{}",
            blake3::Hash::from_bytes(self.0).to_hex()
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reason {
    ReviewRequested,
    Mention,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Position {
    pub at: u64,
    pub event: u64,
}

impl Position {
    fn suffix(self) -> String {
        format!("{:016x}-{:016x}", self.at, self.event)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    position: Position,
    reason: Reason,
    read: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Notification {
    pub event: Event,
    pub reason: Reason,
    pub read: bool,
}

fn root(account: &IdentityKey) -> Result<String> {
    account.validate()?;
    Ok(Recipient::qualified(&account.issuer, &account.subject).root())
}

fn record(
    context: &Transaction<'_>,
    account_root: &str,
    event: &Event,
    reason: Reason,
) -> Result<()> {
    let position = Position {
        at: event.created_at,
        event: event.number,
    };
    let suffix = position.suffix();
    context.put_document(
        &format!("{account_root}/all/{suffix}"),
        &Entry {
            position,
            reason,
            read: false,
        },
    )?;
    context.put_document(&format!("{account_root}/unread/{suffix}"), &position)?;
    Ok(())
}

pub(in crate::cells::repository) fn review_requested(
    context: &Transaction<'_>,
    issuer: &str,
    subject: &str,
    event: &Event,
) -> Result<()> {
    let account = IdentityKey {
        issuer: issuer.into(),
        subject: subject.into(),
    };
    record(context, &root(&account)?, event, Reason::ReviewRequested)
}

pub(super) fn mention(context: &Transaction<'_>, event: &Event, body: &str) -> Result<()> {
    if context.mentions().is_empty() {
        return Ok(());
    }
    let digest = blake3::hash(body.as_bytes()).to_hex();
    let Some(group) = context
        .mentions()
        .iter()
        .find(|group| group.body_digest == digest.as_str())
    else {
        return Ok(());
    };
    let actor = Recipient::qualified(&event.actor.issuer, &event.actor.subject);
    for recipient in &group.recipients {
        if *recipient == actor {
            continue;
        }
        let account_root = recipient.root();
        let key = format!("{account_root}/mentions/{}", event.target.key());
        if context.document::<bool>(&key)?.is_some() {
            continue;
        }
        // The same target and qualified account get one notification through
        // retries, edits, and public handle changes.
        context.put_document(&key, &true)?;
        record(context, &account_root, event, Reason::Mention)?;
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Query {
    pub account: IdentityKey,
    pub unread: bool,
    pub before: Option<Position>,
    pub limit: u8,
}

pub(crate) struct List;
impl Read for List {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 404;
    type Input = Query;
    type Output = Vec<Notification>;

    fn execute(context: &mut Transaction<'_>, input: Query) -> Result<Self::Output> {
        let root = root(&input.account)?;
        let limit = usize::from(input.limit);
        let prefix = format!("{root}/{}/", if input.unread { "unread" } else { "all" });
        let before = input
            .before
            .map(|before| format!("{prefix}{}", before.suffix()));
        let rows = if input.unread {
            context
                .documents::<Position>(&prefix, before.as_deref(), limit)?
                .into_iter()
                .map(|(_, position)| {
                    context
                        .document::<Entry>(&format!("{root}/all/{}", position.suffix()))?
                        .ok_or(Error::Invalid("unread notification is missing"))
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            context
                .documents::<Entry>(&prefix, before.as_deref(), limit)?
                .into_iter()
                .map(|(_, entry)| entry)
                .collect()
        };
        rows.into_iter()
            .map(|entry| {
                let event = activity::load_event(context, entry.position.event)?;
                Ok(Notification {
                    event,
                    reason: entry.reason,
                    read: entry.read,
                })
            })
            .collect()
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadInput {
    pub account: IdentityKey,
    pub position: Position,
    pub read: bool,
}

pub(crate) struct SetRead;
impl Mutation for SetRead {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 404;
    type Input = ReadInput;
    type Output = bool;

    fn execute(context: &mut Transaction<'_>, input: ReadInput) -> Result<MutationResult<bool>> {
        let root = root(&input.account)?;
        let suffix = input.position.suffix();
        let key = format!("{root}/all/{suffix}");
        let Some(mut entry) = context.document::<Entry>(&key)? else {
            return Ok(MutationResult::Rejected(false));
        };
        if entry.read != input.read {
            entry.read = input.read;
            context.put_document(&key, &entry)?;
            let key = format!("{root}/unread/{suffix}");
            if input.read {
                context.delete_document(&key)?;
            } else {
                context.put_document(&key, &entry.position)?;
            }
        }
        Ok(MutationResult::Success(true))
    }
}
