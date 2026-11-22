use super::{
    CLASS, Error, IdentityKey, LookupAccounts, MAX_VERSION, Mutation, MutationResult, Read, Result,
    Transaction, accounts, advance,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const MAX_REPLIES: usize = 24;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedReply {
    pub id: Uuid,
    pub title: String,
    pub body: String,
    pub version: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplyEdit {
    pub version: u64,
    pub title: String,
    pub body: String,
}

impl ReplyEdit {
    pub(crate) fn validate(&self) -> std::result::Result<(), &'static str> {
        if self.version > MAX_VERSION {
            return Err("Saved reply version is invalid");
        }
        if self.title.is_empty()
            || self.title.len() > 128
            || self.title.trim() != self.title
            || self.title.chars().any(char::is_control)
        {
            return Err(
                "Saved reply title must contain 1 to 128 bytes without surrounding whitespace or control characters",
            );
        }
        // The aggregate fits the native document limit even when every valid
        // character requires JSON escaping. Reject controls other than whitespace.
        if self.body.trim().is_empty()
            || self.body.len() > 2048
            || self
                .body
                .chars()
                .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
        {
            return Err("Saved reply Markdown must contain 1 to 2048 bytes and valid text");
        }
        Ok(())
    }
}

fn collection(actor: Uuid) -> String {
    format!("saved-replies/v1/{actor}")
}

fn deletion(actor: Uuid, reply: Uuid) -> String {
    format!("saved-reply-deletions/v1/{actor}/{reply}")
}

fn account(context: &mut Transaction<'_>, identity: IdentityKey) -> Result<Option<Uuid>> {
    Ok(LookupAccounts::execute(context, vec![identity])?
        .into_iter()
        .next()
        .flatten())
}

pub(crate) struct ListSavedReplies;
impl Read for ListSavedReplies {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 15;
    type Input = IdentityKey;
    type Output = Vec<SavedReply>;

    fn execute(context: &mut Transaction<'_>, identity: IdentityKey) -> Result<Self::Output> {
        let Some(actor) = account(context, identity)? else {
            return Ok(Vec::new());
        };
        Ok(context.document(&collection(actor))?.unwrap_or_default())
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct PutInput {
    pub identity: IdentityKey,
    pub id: Uuid,
    pub edit: ReplyEdit,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum PutOutcome {
    Saved(SavedReply),
    Conflict,
    Missing,
    Limit,
}

pub(crate) struct PutSavedReply;
impl Mutation for PutSavedReply {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 16;
    type Input = PutInput;
    type Output = PutOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: PutInput,
    ) -> Result<MutationResult<PutOutcome>> {
        input.edit.validate().map_err(Error::Invalid)?;
        if input.id.is_nil() {
            return Err(Error::Invalid("Saved reply ID is invalid"));
        }
        let (actor, _) = accounts::ensure(context, &input.identity)?;
        let key = collection(actor);
        let mut replies: Vec<SavedReply> = context.document(&key)?.unwrap_or_default();
        let index = replies.iter().position(|reply| reply.id == input.id);
        let expected = input.edit.version;
        if let Some(index) = index {
            let current = &replies[index];
            if current.title == input.edit.title
                && current.body == input.edit.body
                && (current.version == expected || current.version == expected + 1)
            {
                return Ok(MutationResult::Success(PutOutcome::Saved(current.clone())));
            }
            if current.version != expected {
                return Ok(MutationResult::Rejected(PutOutcome::Conflict));
            }
        } else {
            if context
                .document::<u64>(&deletion(actor, input.id))?
                .is_some()
            {
                return Ok(MutationResult::Rejected(PutOutcome::Conflict));
            }
            if expected != 0 {
                return Ok(MutationResult::Rejected(PutOutcome::Missing));
            }
            if replies.len() >= MAX_REPLIES {
                return Ok(MutationResult::Rejected(PutOutcome::Limit));
            }
        }
        let version = expected + 1;
        if version > MAX_VERSION {
            return Err(Error::Invalid("Saved reply version limit reached"));
        }
        let reply = SavedReply {
            id: input.id,
            title: input.edit.title,
            body: input.edit.body,
            version,
        };
        if let Some(index) = index {
            replies[index] = reply.clone();
        } else {
            replies.push(reply.clone());
        }
        context.put_document(&key, &replies)?;
        advance(context)?;
        Ok(MutationResult::Success(PutOutcome::Saved(reply)))
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct DeleteInput {
    pub identity: IdentityKey,
    pub id: Uuid,
    pub version: u64,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum DeleteOutcome {
    Deleted,
    Conflict,
    Missing,
}

pub(crate) struct DeleteSavedReply;
impl Mutation for DeleteSavedReply {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 17;
    type Input = DeleteInput;
    type Output = DeleteOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: DeleteInput,
    ) -> Result<MutationResult<DeleteOutcome>> {
        if input.id.is_nil() || !(1..=MAX_VERSION).contains(&input.version) {
            return Err(Error::Invalid("Saved reply ID or version is invalid"));
        }
        let Some(actor) = account(context, input.identity)? else {
            return Ok(MutationResult::Rejected(DeleteOutcome::Missing));
        };
        let key = collection(actor);
        let deleted = deletion(actor, input.id);
        let mut replies: Vec<SavedReply> = context.document(&key)?.unwrap_or_default();
        let Some(index) = replies.iter().position(|reply| reply.id == input.id) else {
            return Ok(match context.document::<u64>(&deleted)? {
                Some(version) if version == input.version => {
                    MutationResult::Success(DeleteOutcome::Deleted)
                }
                Some(_) => MutationResult::Rejected(DeleteOutcome::Conflict),
                None => MutationResult::Rejected(DeleteOutcome::Missing),
            });
        };
        if replies[index].version != input.version {
            return Ok(MutationResult::Rejected(DeleteOutcome::Conflict));
        }
        replies.remove(index);
        // Keep only the version after deletion, so retrying a previously
        // accepted version-zero PUT cannot restore the deleted reply.
        context.put_document(&deleted, &input.version)?;
        if replies.is_empty() {
            context.delete_document(&key)?;
        } else {
            context.put_document(&key, &replies)?;
        }
        advance(context)?;
        Ok(MutationResult::Success(DeleteOutcome::Deleted))
    }
}
