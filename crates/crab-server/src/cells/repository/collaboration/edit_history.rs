use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use serde::{Deserialize, Serialize};

use super::{PREFIX, Target, activity::Event};
use crate::cells::{
    Error, Result,
    application::{Mutation, MutationResult, Read, Transaction},
    repository::{
        CLASS, RepositoryAuthor, advance_revision, same_author, settings, timestamp,
        validate_author, validate_number,
    },
};

pub(crate) mod transfer;

const LIMIT: u8 = 100;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Retention {
    original: u64,
    latest: u64,
    count: u8,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Redaction {
    pub author: RepositoryAuthor,
    pub at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Revision {
    pub number: u64,
    pub editor: RepositoryAuthor,
    pub edited_at: u64,
    pub redaction: Option<Redaction>,
    previous: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Content {
    pub body: String,
    pub suggestion: Option<String>,
}

struct History {
    root: String,
    revisions: String,
}

impl History {
    fn new(target: Target) -> Self {
        let root = format!("{PREFIX}/edit-history/{}", target.key());
        Self {
            revisions: format!("{root}/revisions/"),
            root,
        }
    }

    fn retention(&self) -> String {
        format!("{}/retention", self.root)
    }

    fn revision(&self, number: u64) -> String {
        format!("{}{number:016x}", self.revisions)
    }

    fn body(&self, number: u64) -> String {
        format!("{}/body/{number:016x}", self.root)
    }

    fn suggestion(&self, number: u64) -> String {
        format!("{}/suggestion/{number:016x}", self.root)
    }

    fn content(&self, context: &Transaction<'_>, revision: &Revision) -> Result<Option<Content>> {
        if revision.redaction.is_some() {
            return Ok(None);
        }
        let body = text(context, &self.body(revision.number))?
            .ok_or(Error::Invalid("retained edit body is missing"))?;
        let suggestion = text(context, &self.suggestion(revision.number))?;
        Ok(Some(Content { body, suggestion }))
    }

    fn delete_content(&self, context: &Transaction<'_>, number: u64) -> Result<()> {
        context.delete_document(&self.body(number))?;
        context.delete_document(&self.suggestion(number))?;
        Ok(())
    }

    fn store_content(
        &self,
        context: &Transaction<'_>,
        number: u64,
        body: &str,
        suggestion: Option<&str>,
    ) -> Result<()> {
        // Encode each field separately so Markdown escaping cannot exceed
        // the native document limit.
        context.put_document(&self.body(number), &STANDARD_NO_PAD.encode(body))?;
        if let Some(suggestion) = suggestion {
            context.put_document(
                &self.suggestion(number),
                &STANDARD_NO_PAD.encode(suggestion),
            )?;
        }
        Ok(())
    }
}

fn text(context: &Transaction<'_>, key: &str) -> Result<Option<String>> {
    context
        .document::<String>(key)?
        .map(|encoded| {
            let bytes = STANDARD_NO_PAD.decode(encoded).map_err(Error::sdk)?;
            String::from_utf8(bytes).map_err(Error::sdk)
        })
        .transpose()
}

pub(super) fn exists(context: &Transaction<'_>, target: Target) -> Result<bool> {
    Ok(context
        .document::<Retention>(&History::new(target).retention())?
        .is_some())
}

pub(in crate::cells::repository) fn record(
    context: &Transaction<'_>,
    edit: &Event,
    previous: &str,
    suggestion: Option<&str>,
) -> Result<()> {
    let history = History::new(edit.target);
    let mut state = context
        .document::<Retention>(&history.retention())?
        .unwrap_or(Retention {
            original: edit.number,
            latest: 0,
            count: 0,
        });
    if state.count == LIMIT {
        // Keep the first version and the latest intermediate versions.
        let removed = context
            .documents_after::<Revision>(
                &history.revisions,
                Some(&history.revision(state.original)),
                1,
            )?
            .pop()
            .ok_or(Error::Invalid("edit history retention is inconsistent"))?;
        history.delete_content(context, removed.1.number)?;
        context.delete_document(&removed.0)?;
    } else {
        state.count += 1;
    }
    let revision = Revision {
        number: edit.number,
        editor: edit.actor.clone(),
        edited_at: edit.created_at,
        redaction: None,
        previous: (state.latest != 0).then_some(state.latest),
    };
    history.store_content(context, edit.number, previous, suggestion)?;
    context.put_document(&history.revision(edit.number), &revision)?;
    state.latest = edit.number;
    context.put_document(&history.retention(), &state)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Query {
    pub target: Target,
    pub before: Option<u64>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Page {
    pub items: Vec<Revision>,
    pub next: Option<u64>,
}

pub(crate) struct List;
impl Read for List {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 313;
    type Input = Query;
    type Output = Option<Page>;

    fn execute(context: &mut Transaction<'_>, query: Query) -> Result<Self::Output> {
        if let Some(before) = query.before {
            validate_number(before)?;
        }
        if !query.target.exists(context)? {
            return Ok(None);
        }
        let history = History::new(query.target);
        let before = query.before.map(|before| history.revision(before));
        let mut items: Vec<_> = context
            .documents::<Revision>(&history.revisions, before.as_deref(), 31)?
            .into_iter()
            .map(|(_, revision)| revision)
            .collect();
        let next = if items.len() > 30 {
            items.pop();
            items.last().map(|item| item.number)
        } else {
            None
        };
        Ok(Some(Page { items, next }))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Key {
    pub target: Target,
    pub number: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Detail {
    pub author: RepositoryAuthor,
    pub archived: bool,
    pub revision: Revision,
    pub before: Option<Content>,
    pub after: Option<Content>,
}

pub(crate) struct Get;
impl Read for Get {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 314;
    type Input = Key;
    type Output = Option<Detail>;

    fn execute(context: &mut Transaction<'_>, key: Key) -> Result<Self::Output> {
        validate_number(key.number)?;
        let Some(author) = key.target.author(context)? else {
            return Ok(None);
        };
        let history = History::new(key.target);
        let path = history.revision(key.number);
        let Some(revision) = context.document::<Revision>(&path)? else {
            return Ok(None);
        };
        let before = history.content(context, &revision)?;
        let after = if before.is_none() {
            None
        } else {
            let next = context
                .documents_after::<Revision>(&history.revisions, Some(&path), 1)?
                .pop();
            match next {
                // The next edit's previous version is this edit's result. Gaps
                // from retention and redaction never substitute a different body.
                Some((_, next)) if next.previous == Some(key.number) => {
                    history.content(context, &next)?
                }
                Some(_) => None,
                None => key.target.content(context)?,
            }
        };
        Ok(Some(Detail {
            author,
            archived: settings::archived(context)?,
            revision,
            before,
            after,
        }))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Removal {
    pub key: Key,
    pub actor: RepositoryAuthor,
    pub can_write: bool,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Outcome {
    Redacted,
    Archived,
    Forbidden,
    NotFound,
}

pub(crate) struct Redact;
impl Mutation for Redact {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 313;
    type Input = Removal;
    type Output = Outcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Removal,
    ) -> Result<MutationResult<Self::Output>> {
        validate_number(input.key.number)?;
        validate_author(&input.actor)?;
        let Some(author) = input.key.target.author(context)? else {
            return Ok(MutationResult::Rejected(Outcome::NotFound));
        };
        if !(input.can_write || same_author(&author, &input.actor)) {
            return Ok(MutationResult::Rejected(Outcome::Forbidden));
        }
        let history = History::new(input.key.target);
        let path = history.revision(input.key.number);
        let Some(mut revision) = context.document::<Revision>(&path)? else {
            return Ok(MutationResult::Rejected(Outcome::NotFound));
        };
        if revision.redaction.is_some() {
            return Ok(MutationResult::Success(Outcome::Redacted));
        }
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(Outcome::Archived));
        }
        history.delete_content(context, input.key.number)?;
        revision.redaction = Some(Redaction {
            author: input.actor,
            at: timestamp(context.now_ms())?,
        });
        context.put_document(&path, &revision)?;
        advance_revision(context)?;
        Ok(MutationResult::Success(Outcome::Redacted))
    }
}
