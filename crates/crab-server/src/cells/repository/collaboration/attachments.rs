use std::collections::{BTreeMap, BTreeSet};

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::Target;
use crate::cells::{
    Error, Result,
    application::{Mutation, MutationResult, Read, SqlValue, Transaction},
    repository::{
        self, RepositoryAuthor, advance_revision, same_author, settings, validate_author,
    },
};

pub(crate) const MAX_BYTES: u64 = 25 * 1024 * 1024;
const MAX_PER_BODY: usize = 20;
const MAX_EXPIRY_BATCH: usize = 32;
const DRAFT_LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;
const TRANSFER_WINDOW_MS: i64 = 5 * 60 * 1000;
const RECORD_PREFIX: &str = "collaboration/v1/attachments/records/";
const EXPIRY_PREFIX: &str = "collaboration/v1/attachments/expiry/";
const LINK_PREFIX: &str = "/api/discussion-attachments/";
const MARKDOWN_OPTIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_FOOTNOTES)
    .union(Options::ENABLE_TASKLISTS);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Upload {
    pub id: Uuid,
    pub owner: RepositoryAuthor,
    pub name: String,
    pub size: u64,
    // The upload API records SHA-256. Object-store write verification uses a
    // separate BLAKE3 digest and does not change the published metadata.
    pub digest: String,
    pub raster: Option<String>,
}

impl Upload {
    fn validate(&self) -> Result<()> {
        validate_author(&self.owner)?;
        if self.id.is_nil()
            || self.name.is_empty()
            || self.name.len() > 255
            || self.name.trim() != self.name
            || matches!(self.name.as_str(), "." | "..")
            || self
                .name
                .chars()
                .any(|character| character.is_control() || matches!(character, '/' | '\\'))
            || self.size > MAX_BYTES
            || self.digest.len() != 64
            || !self
                .digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || !matches!(
                self.raster.as_deref(),
                None | Some("image/png" | "image/jpeg" | "image/gif" | "image/webp")
            )
        {
            return Err(Error::Invalid("Discussion attachment metadata is invalid"));
        }
        Ok(())
    }

    fn matches(&self, other: &Self) -> bool {
        self.id == other.id
            && same_author(&self.owner, &other.owner)
            && self.name == other.name
            && self.size == other.size
            && self.digest == other.digest
            && self.raster == other.raster
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum State {
    Pending,
    Ready,
    Transferring { request: Uuid },
    Published { target: Target },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "record", rename_all = "snake_case", deny_unknown_fields)]
enum Record {
    Live {
        upload: Upload,
        expires: i64,
        state: State,
    },
    Expired {
        expires: i64,
    },
    Collected,
}

fn record_key(id: Uuid) -> String {
    format!("{RECORD_PREFIX}{id}")
}

fn expiry_key(expires: i64, id: Uuid) -> String {
    format!("{EXPIRY_PREFIX}{expires:016x}/{id}")
}

fn expiry_end(now: i64) -> String {
    format!("{EXPIRY_PREFIX}{:016x}/", now.saturating_add(1))
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReserveOutcome {
    Pending,
    Ready,
    Unavailable,
    Conflict,
}

pub(crate) struct Reserve;
impl Mutation for Reserve {
    const CLASS: &'static str = repository::CLASS;
    const ID: u32 = 330;
    type Input = Upload;
    type Output = ReserveOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        upload: Upload,
    ) -> Result<MutationResult<Self::Output>> {
        upload.validate()?;
        let record = record_key(upload.id);
        let now = context.now_ms();
        let outcome = match context.document::<Record>(&record)? {
            Some(Record::Live {
                upload: prior,
                expires,
                state,
            }) => {
                if !same_author(&prior.owner, &upload.owner) {
                    ReserveOutcome::Unavailable
                } else if !prior.matches(&upload) {
                    ReserveOutcome::Conflict
                } else {
                    match state {
                        State::Published { .. } => ReserveOutcome::Ready,
                        State::Ready if expires > now => ReserveOutcome::Ready,
                        State::Pending
                            if expires.saturating_sub(now) >= TRANSFER_WINDOW_MS
                                && !settings::archived(context)? =>
                        {
                            ReserveOutcome::Pending
                        }
                        _ => ReserveOutcome::Unavailable,
                    }
                }
            }
            Some(Record::Expired { .. } | Record::Collected) => ReserveOutcome::Unavailable,
            None if settings::archived(context)? => ReserveOutcome::Unavailable,
            None => {
                let expires = now
                    .checked_add(DRAFT_LIFETIME_MS)
                    .ok_or(Error::Invalid("Discussion attachment expiry is invalid"))?;
                let id = upload.id;
                context.put_document(
                    &record,
                    &Record::Live {
                        upload,
                        expires,
                        state: State::Pending,
                    },
                )?;
                context.put_document(&expiry_key(expires, id), &id)?;
                ReserveOutcome::Pending
            }
        };
        Ok(MutationResult::Success(outcome))
    }
}

pub(crate) struct Finish;
impl Mutation for Finish {
    const CLASS: &'static str = repository::CLASS;
    const ID: u32 = 331;
    type Input = Upload;
    type Output = bool;

    fn execute(context: &mut Transaction<'_>, upload: Upload) -> Result<MutationResult<bool>> {
        upload.validate()?;
        let key = record_key(upload.id);
        let Some(Record::Live {
            upload: prior,
            expires,
            state,
        }) = context.document::<Record>(&key)?
        else {
            return Ok(MutationResult::Success(false));
        };
        if !prior.matches(&upload) {
            return Ok(MutationResult::Success(false));
        }
        if matches!(state, State::Published { .. }) {
            return Ok(MutationResult::Success(true));
        }
        if matches!(state, State::Transferring { .. }) || expires <= context.now_ms() {
            return Ok(MutationResult::Success(false));
        }
        if matches!(state, State::Pending) {
            if settings::archived(context)? {
                return Ok(MutationResult::Success(false));
            }
            context.put_document(
                &key,
                &Record::Live {
                    upload: prior,
                    expires,
                    state: State::Ready,
                },
            )?;
            advance_revision(context)?;
        }
        Ok(MutationResult::Success(true))
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadInput {
    pub id: Uuid,
    pub actor: RepositoryAuthor,
}

pub(crate) struct Get;
impl Read for Get {
    const CLASS: &'static str = repository::CLASS;
    const ID: u32 = 332;
    type Input = ReadInput;
    type Output = Option<Upload>;

    fn execute(context: &mut Transaction<'_>, input: ReadInput) -> Result<Option<Upload>> {
        validate_author(&input.actor)?;
        let Some(Record::Live {
            upload,
            expires,
            state,
        }) = context.document::<Record>(&record_key(input.id))?
        else {
            return Ok(None);
        };
        let visible = match state {
            State::Pending | State::Transferring { .. } => false,
            State::Ready => expires > context.now_ms() && same_author(&upload.owner, &input.actor),
            State::Published { .. } => true,
        };
        Ok(visible.then_some(upload))
    }
}

pub(crate) struct Expire;
impl Mutation for Expire {
    const CLASS: &'static str = repository::CLASS;
    const ID: u32 = 333;
    type Input = ();
    type Output = Vec<Uuid>;

    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<MutationResult<Vec<Uuid>>> {
        let now = context.now_ms();
        let end = expiry_end(now);
        let mut expired = Vec::new();
        for (index, id) in context.documents::<Uuid>(EXPIRY_PREFIX, Some(&end), MAX_EXPIRY_BATCH)? {
            let key = record_key(id);
            match context.document::<Record>(&key)? {
                Some(Record::Live {
                    expires,
                    state: State::Pending | State::Ready,
                    ..
                }) if expires <= now && index == expiry_key(expires, id) => {
                    // A sealed draft can no longer be claimed or finished while
                    // an independent worker deletes its blob.
                    context.put_document(&key, &Record::Expired { expires })?;
                }
                Some(Record::Expired { expires }) if index == expiry_key(expires, id) => {}
                _ => {
                    return Err(Error::Invalid(
                        "Discussion attachment expiry index is invalid",
                    ));
                }
            }
            expired.push(id);
        }
        Ok(MutationResult::Success(expired))
    }
}

pub(crate) struct Collect;
impl Mutation for Collect {
    const CLASS: &'static str = repository::CLASS;
    const ID: u32 = 334;
    type Input = Uuid;
    type Output = ();

    fn execute(context: &mut Transaction<'_>, id: Uuid) -> Result<MutationResult<()>> {
        let key = record_key(id);
        match context.document::<Record>(&key)? {
            Some(Record::Expired { expires }) => {
                context.delete_document(&expiry_key(expires, id))?;
                // The terminal ID prevents an old request from uploading bytes
                // to a URL that was already scheduled for deletion.
                context.put_document(&key, &Record::Collected)?;
            }
            Some(Record::Collected) => {}
            _ => return Err(Error::Invalid("Discussion attachment is not expired")),
        }
        Ok(MutationResult::Success(()))
    }
}

pub(crate) struct Due;
impl Read for Due {
    const CLASS: &'static str = repository::CLASS;
    const ID: u32 = 335;
    type Input = ();
    type Output = bool;

    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<bool> {
        let end = expiry_end(context.now_ms());
        Ok(!context
            .documents::<Uuid>(EXPIRY_PREFIX, Some(&end), 1)?
            .is_empty())
    }
}

fn link(destination: &str) -> Option<(Uuid, Uuid, bool)> {
    let (destination, uploading) = match destination.strip_suffix("?uploading") {
        Some(path) => (path, true),
        None => (destination, false),
    };
    let (repository, upload) = destination.strip_prefix(LINK_PREFIX)?.split_once('/')?;
    let repository_id = Uuid::parse_str(repository).ok()?;
    let upload_id = Uuid::parse_str(upload).ok()?;
    (repository_id.hyphenated().to_string() == repository
        && upload_id.hyphenated().to_string() == upload)
        .then_some((repository_id, upload_id, uploading))
}

fn linked_attachments(body: &str) -> impl Iterator<Item = (Uuid, Uuid, bool)> + '_ {
    Parser::new_ext(body, MARKDOWN_OPTIONS).filter_map(|event| match event {
        Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => link(&dest_url),
        _ => None,
    })
}

fn transfer_links(body: &str, repository: Uuid) -> Result<BTreeMap<usize, (usize, Uuid)>> {
    let mut links = BTreeMap::new();
    if !body.contains(LINK_PREFIX) && !body.contains(['\\', '&']) {
        return Ok(links);
    }
    for event in Parser::new_ext(body, MARKDOWN_OPTIONS) {
        let Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) = event else {
            continue;
        };
        let Some((source, id, uploading)) = link(&dest_url) else {
            continue;
        };
        if source != repository {
            continue;
        }
        // The parser borrows unescaped destinations directly from the input,
        // including references. Reject decoded destinations before moving the
        // issue so rewriting cannot alter a label, title, or code example.
        let CowStr::Borrowed(raw) = dest_url else {
            return Err(Error::AttachmentUnavailable);
        };
        if uploading {
            return Err(Error::AttachmentUnavailable);
        }
        let start = raw.as_ptr() as usize;
        let start = start
            .checked_sub(body.as_ptr() as usize)
            .filter(|start| body.get(*start..start.saturating_add(raw.len())) == Some(raw))
            .ok_or(Error::AttachmentUnavailable)?;
        links.insert(start, (start + raw.len(), id));
    }
    Ok(links)
}

pub(in crate::cells::repository) fn capture_transfer(
    context: &Transaction<'_>,
    repository: Uuid,
    body: &str,
) -> Result<Vec<Upload>> {
    let ids = transfer_links(body, repository)?
        .into_values()
        .map(|(_, id)| id)
        .collect::<BTreeSet<_>>();
    if ids.len() > MAX_PER_BODY {
        return Err(Error::AttachmentUnavailable);
    }
    ids.into_iter()
        .map(|id| match context.document::<Record>(&record_key(id))? {
            Some(Record::Live {
                upload,
                state: State::Published { .. },
                ..
            }) if upload.id == id => Ok(upload),
            _ => Err(Error::AttachmentUnavailable),
        })
        .collect()
}

pub(in crate::cells::repository) fn validate_transfer(
    repository: Uuid,
    body: &str,
    uploads: &[Upload],
) -> Result<()> {
    let linked = transfer_links(body, repository)?
        .into_values()
        .map(|(_, id)| id)
        .collect::<BTreeSet<_>>();
    if uploads.len() > MAX_PER_BODY
        || !linked
            .into_iter()
            .eq(uploads.iter().map(|upload| upload.id))
    {
        return Err(Error::AttachmentUnavailable);
    }
    for upload in uploads {
        upload.validate()?;
    }
    Ok(())
}

pub(crate) fn transferred_id(request: Uuid, id: Uuid) -> Uuid {
    let mut hasher = blake3::Hasher::new_derive_key("crab discussion attachment transfer v1");
    hasher.update(request.as_bytes());
    hasher.update(id.as_bytes());
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    uuid::Builder::from_custom_bytes(bytes).into_uuid()
}

pub(in crate::cells::repository) fn stage_transfer(
    context: &Transaction<'_>,
    request: Uuid,
    mut upload: Upload,
) -> Result<()> {
    upload.id = transferred_id(request, upload.id);
    let key = record_key(upload.id);
    match context.document::<Record>(&key)? {
        Some(Record::Live {
            upload: existing,
            state: State::Transferring { request: original },
            ..
        }) if original == request && existing.matches(&upload) => Ok(()),
        Some(_) => Err(Error::AttachmentUnavailable),
        None => context.put_document(
            &key,
            &Record::Live {
                upload,
                expires: 0,
                state: State::Transferring { request },
            },
        ),
    }
}

pub(in crate::cells::repository) fn publish_transfer(
    context: &Transaction<'_>,
    request: Uuid,
    source: Uuid,
    destination: Uuid,
    target: Target,
    body: &mut String,
) -> Result<()> {
    let links = transfer_links(body, source)?;
    for (start, (end, original)) in links.into_iter().rev() {
        let id = transferred_id(request, original);
        let key = record_key(id);
        match context.document::<Record>(&key)? {
            Some(Record::Live {
                upload,
                state: State::Transferring { request: recorded },
                ..
            }) if recorded == request => {
                context.put_document(
                    &key,
                    &Record::Live {
                        upload,
                        expires: 0,
                        state: State::Published { target },
                    },
                )?;
            }
            // A shared attachment needs only one published destination record.
            Some(Record::Live {
                state: State::Published { .. },
                ..
            }) => {}
            _ => return Err(Error::AttachmentUnavailable),
        }
        body.replace_range(start..end, &format!("{LINK_PREFIX}{destination}/{id}"));
    }
    Ok(())
}

pub(in crate::cells::repository) fn claim(
    context: &Transaction<'_>,
    target: Target,
    actor: &RepositoryAuthor,
    body: &str,
) -> Result<()> {
    if !body.contains(LINK_PREFIX) && !body.contains(['\\', '&']) {
        return Ok(());
    }
    let mut links = linked_attachments(body).peekable();
    if links.peek().is_none() {
        return Ok(());
    }
    target.validate()?;
    let identity = context.query(
        "SELECT repository_uuid FROM repository_identity WHERE singleton = 1",
        &[],
    )?;
    let Some([SqlValue::Blob(repository)]) = identity.rows.first().map(Vec::as_slice) else {
        return Err(Error::Invalid(
            "Discussion attachment repository is unavailable",
        ));
    };
    let repository = Uuid::from_slice(repository).map_err(Error::sdk)?;
    let mut ids = BTreeSet::new();
    for (_, id, uploading) in links.filter(|(source, _, _)| *source == repository) {
        // Keep the upload placeholder out of persisted Markdown even when the
        // transfer finishes before its browser acknowledgement arrives.
        if uploading {
            return Err(Error::AttachmentUnavailable);
        }
        if !ids.insert(id) {
            continue;
        }
        if ids.len() > MAX_PER_BODY {
            return Err(Error::AttachmentUnavailable);
        }
        let key = record_key(id);
        match context.document::<Record>(&key)? {
            Some(Record::Live {
                state: State::Published { .. },
                ..
            }) => {}
            Some(Record::Live {
                upload,
                expires,
                state: State::Ready,
            }) if expires > context.now_ms() && same_author(&upload.owner, actor) => {
                // The body and draft-to-published transition commit together.
                context.put_document(
                    &key,
                    &Record::Live {
                        upload,
                        expires,
                        state: State::Published { target },
                    },
                )?;
                context.delete_document(&expiry_key(expires, id))?;
            }
            _ => return Err(Error::AttachmentUnavailable),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_canonical_markdown_links_and_images_can_claim() {
        let repository = Uuid::from_u128(1);
        let upload = Uuid::from_u128(2);
        let path = format!("{LINK_PREFIX}{repository}/{upload}");
        let body = format!(
            "`[code]({path})`\n\n[](https://example.com{path}) ![image]({path}) [link]({path}) [other]({path}?query) [uploading]({path}?uploading) \\[escaped]({path})\n\n<details>\n<summary>Attachments</summary>\n\n- [x] [nested]({path})\n\n</details>"
        );
        assert_eq!(
            linked_attachments(&body).collect::<Vec<_>>(),
            vec![
                (repository, upload, false),
                (repository, upload, false),
                (repository, upload, true),
                (repository, upload, false),
            ]
        );
    }
}
