use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    Access, CLASS, Error, Filed, Inbox, Mutation, MutationResult, Position, Result, Transaction,
    advance, filing,
};

const FRAME: usize = 32;

fn completed_key(actor: Uuid, thread: Uuid) -> String {
    format!("notifications/v1/completed-through/{actor}/{thread}")
}

pub(super) fn completed_through(
    context: &Transaction<'_>,
    actor: Uuid,
    thread: Uuid,
) -> Result<Option<Position>> {
    context.document(&completed_key(actor, thread))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FilingSweep {
    pub before: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FilingProgress {
    pub next: Option<String>,
}

pub(crate) struct SweepFiling;

impl Mutation for SweepFiling {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 25;
    type Input = FilingSweep;
    type Output = FilingProgress;

    fn execute(
        context: &mut Transaction<'_>,
        input: FilingSweep,
    ) -> Result<MutationResult<FilingProgress>> {
        if let Some(before) = &input.before {
            address(before)?;
        }
        let rows =
            context.documents::<Filed>(filing::ITEM_PREFIX, input.before.as_deref(), FRAME + 1)?;
        let next = (rows.len() > FRAME).then(|| rows[FRAME - 1].0.clone());
        let now = context.now_ms();
        let mut changed = false;
        for (key, record) in rows.into_iter().take(FRAME) {
            if !record
                .done_at
                .is_some_and(|time| !filing::within_retention(now, time))
            {
                continue;
            }
            let (actor, thread) = address(&key)?;
            if record.id() != thread {
                return Err(Error::Invalid(
                    "Notification filing address is inconsistent",
                ));
            }
            if record.saved {
                changed |= filing::remove_done_index(context, actor, &record)?;
                continue;
            }
            expire(context, actor, &record)?;
            changed = true;
        }
        if changed {
            advance(context)?;
        }
        Ok(MutationResult::Success(FilingProgress { next }))
    }
}

fn expire(context: &Transaction<'_>, actor: Uuid, record: &Filed) -> Result<()> {
    let thread = record.id();
    let cutoff = record
        .position()
        .max(completed_through(context, actor, thread)?.unwrap_or(record.position()));
    // Removing recent copies alone would let a delayed replay recreate an
    // unread entry. Replace the rich completion with its exact activity fence.
    context.put_document(&completed_key(actor, thread), &cutoff)?;
    for access in Access::CHANNELS {
        let key = access.inbox(actor);
        let Some(mut inbox) = context.document::<Inbox>(&key)? else {
            continue;
        };
        let original = inbox.entries.len();
        inbox
            .entries
            .retain(|entry| entry.notice.thread_id() != thread || entry.notice.position() > cutoff);
        if inbox.entries.len() != original {
            context.put_document(&key, &inbox)?;
        }
    }
    filing::store(
        context,
        actor,
        Some(record),
        &Filed::new(record.entry.clone()),
    )?;
    Ok(())
}

fn address(key: &str) -> Result<(Uuid, Uuid)> {
    const INVALID: Error = Error::Invalid("Notification filing cursor is invalid");
    let (actor, thread) = key
        .strip_prefix(filing::ITEM_PREFIX)
        .and_then(|key| key.split_once('/'))
        .ok_or(INVALID)?;
    let parse = |value: &str| {
        Uuid::parse_str(value)
            .ok()
            .filter(|id| !id.is_nil())
            .ok_or(INVALID)
    };
    Ok((parse(actor)?, parse(thread)?))
}

#[cfg(test)]
pub(in crate::cells::directory) mod tests;
