use super::{
    CLASS, Error, IdentityKey, Key, Mode, Mutation, MutationResult, PAGE_SIZE, Read, Result,
    Transaction, subscription, watcher_keys,
};
use crate::cells::repository::{RepositoryAuthor, SqlValue, integer, result_text};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const WATCHERS: &str = "collaboration/v2/watches/releases/active/";
const OUTBOX: &str = "collaboration/v2/watches/releases/outbox/";
const SEQUENCE: &str = "collaboration/v2/watches/releases/sequence";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseEvent {
    pub sequence: u64,
    pub number: u64,
    pub id: Uuid,
    pub publisher: RepositoryAuthor,
    pub published_at_ms: u64,
    pub before: Option<Key>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseCursor {
    pub sequence: u64,
    pub before: Option<Key>,
}

impl ReleaseEvent {
    pub(crate) fn cursor(&self) -> ReleaseCursor {
        ReleaseCursor {
            sequence: self.sequence,
            before: self.before,
        }
    }
}

fn key(sequence: u64) -> String {
    format!("{OUTBOX}{:016x}", !sequence)
}

fn latest_key(number: u64) -> String {
    format!("collaboration/v2/watches/releases/published/{number:016x}")
}

pub(super) fn watermark(context: &Transaction<'_>) -> Result<u64> {
    Ok(context.document(SEQUENCE)?.unwrap_or_default())
}

pub(super) fn update_index(
    context: &Transaction<'_>,
    suffix: &str,
    actor: &IdentityKey,
    enabled: bool,
) -> Result<()> {
    let key = format!("{WATCHERS}{suffix}");
    if enabled {
        context.put_document(&key, actor)?;
    } else {
        context.delete_document(&key)?;
    }
    Ok(())
}

pub(in crate::cells::repository) fn published(
    context: &Transaction<'_>,
    number: u64,
    publisher: RepositoryAuthor,
    published_at_ms: u64,
) -> Result<()> {
    let sequence = super::next_version(watermark(context)?)?;
    context.put_document(SEQUENCE, &sequence)?;
    // A new publication of a withdrawn release supersedes any of its
    // outstanding deliveries, even when both commits share a timestamp.
    context.put_document(&latest_key(number), &sequence)?;
    if !context
        .documents::<IdentityKey>(WATCHERS, None, 1)?
        .is_empty()
    {
        context.put_document(
            &key(sequence),
            &ReleaseEvent {
                sequence,
                number,
                id: Uuid::now_v7(),
                publisher,
                published_at_ms,
                before: None,
            },
        )?;
    }
    Ok(())
}

pub(in crate::cells::repository) fn pending(
    context: &Transaction<'_>,
    limit: usize,
) -> Result<Vec<ReleaseEvent>> {
    context
        .documents::<ReleaseEvent>(OUTBOX, None, limit)?
        .into_iter()
        .map(|(address, event)| {
            if address != key(event.sequence) {
                return Err(Error::Invalid("Release notification outbox key is invalid"));
            }
            Ok(event)
        })
        .collect()
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseSource {
    pub tag: String,
    pub title: String,
}

fn source(
    context: &Transaction<'_>,
    number: u64,
    author: Option<&IdentityKey>,
) -> Result<Option<ReleaseSource>> {
    let rows = context.query(
        "SELECT tag_name, title FROM repository_releases
         WHERE number = ?1 AND draft = 0 AND deleted = 0
           AND (?2 IS NULL OR (author_issuer = ?2 AND author_subject = ?3))",
        &[
            integer(number)?,
            author.map_or(SqlValue::Null, |value| SqlValue::Text(value.issuer.clone())),
            author.map_or(SqlValue::Null, |value| {
                SqlValue::Text(value.subject.clone())
            }),
        ],
    )?;
    rows.rows
        .first()
        .map(|row| {
            Ok(ReleaseSource {
                tag: result_text(row, 0)?,
                title: result_text(row, 1)?,
            })
        })
        .transpose()
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublishedReleaseQuery {
    pub numbers: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<IdentityKey>,
}

pub(crate) struct PublishedReleases;
impl Read for PublishedReleases {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 426;
    type Input = PublishedReleaseQuery;
    type Output = Vec<Option<ReleaseSource>>;

    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        if input.numbers.len() > 50 {
            return Err(Error::Invalid("Release notice batch is too large"));
        }
        if let Some(author) = &input.author {
            author.validate()?;
        }
        input
            .numbers
            .into_iter()
            .map(|number| source(context, number, input.author.as_ref()))
            .collect()
    }
}

pub(crate) struct ReleaseWatchers;
impl Read for ReleaseWatchers {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 425;
    type Input = ReleaseCursor;
    type Output = Option<super::WatchPage>;

    fn execute(context: &mut Transaction<'_>, input: ReleaseCursor) -> Result<Self::Output> {
        let Some(event) = context
            .document::<ReleaseEvent>(&key(input.sequence))?
            .filter(|event| event.before == input.before)
        else {
            return Ok(None);
        };
        if context.document::<u64>(&latest_key(event.number))? != Some(event.sequence)
            || source(context, event.number, None)?.is_none()
        {
            return Ok(Some(super::WatchPage {
                recipients: Vec::new(),
                next: None,
            }));
        }
        let rows = watcher_keys(context, WATCHERS, input.before)?;
        let more = rows.len() > PAGE_SIZE;
        let mut last = None;
        let mut recipients = Vec::new();
        for (key, actor) in rows.into_iter().take(PAGE_SIZE) {
            last = Some(key);
            let current = subscription(context, key)?;
            if current.preference.mode != Mode::Muted
                && current
                    .releases_after
                    .is_some_and(|after| after < event.sequence)
                && (actor.issuer != event.publisher.issuer
                    || actor.subject != event.publisher.subject)
            {
                recipients.push(actor);
            }
        }
        Ok(Some(super::WatchPage {
            recipients,
            next: last.filter(|_| more),
        }))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcknowledgeReleaseInput {
    pub cursor: ReleaseCursor,
    pub next: Option<Key>,
}

pub(crate) struct AcknowledgeRelease;
impl Mutation for AcknowledgeRelease {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 425;
    type Input = AcknowledgeReleaseInput;
    type Output = ();

    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<MutationResult<()>> {
        let address = key(input.cursor.sequence);
        let Some(mut event) = context
            .document::<ReleaseEvent>(&address)?
            .filter(|event| event.before == input.cursor.before)
        else {
            return Ok(MutationResult::Success(()));
        };
        if let Some(next) = input.next {
            // A resumed page cannot move the queue back to an earlier cursor.
            if input.cursor.before.is_some_and(|before| next >= before) {
                return Err(Error::Invalid(
                    "Release notification cursor does not advance",
                ));
            }
            event.before = Some(next);
            context.put_document(&address, &event)?;
        } else {
            context.delete_document(&address)?;
        }
        Ok(MutationResult::Success(()))
    }
}
