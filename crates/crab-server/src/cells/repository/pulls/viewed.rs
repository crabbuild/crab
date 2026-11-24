use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{RepositoryAuthor, hash_text, next_version, validate_author, validate_number};
use crate::cells::{
    Error, Result,
    application::{Mutation, MutationResult, Read, Transaction},
    repository::{integer, settings},
};

const INLINE_FILES: usize = 64;
const FILES_PER_SCAN: usize = 50;
const SCANS_PER_PAGE: usize = 10;
// Bound saved identities across successive comparisons to the Git entry budget.
const MAX_VIEWED_FILES: usize = 100_000;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Progress {
    version: u64,
    // The original document doubles as inline storage. Existing marks retain
    // their identities, and large reviews save additional paths separately.
    files: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    extra: usize,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

fn valid_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ViewedKey {
    pub pull: u64,
    pub actor: RepositoryAuthor,
}

impl ViewedKey {
    fn document(&self, context: &Transaction<'_>) -> Result<Option<String>> {
        validate_number(self.pull)?;
        validate_author(&self.actor)?;
        if context
            .query(
                "SELECT 1 FROM repository_pulls WHERE number = ?",
                &[integer(self.pull)?],
            )?
            .rows
            .is_empty()
        {
            return Ok(None);
        }
        let mut actor = blake3::Hasher::new_derive_key("crab pull viewed files account v1");
        hash_text(&mut actor, &self.actor.issuer);
        hash_text(&mut actor, &self.actor.subject);
        let actor = actor.finalize().to_hex();
        Ok(Some(format!(
            "collaboration/v1/pulls/{:016x}/viewed/{actor}",
            self.pull
        )))
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ViewedCursor {
    pub version: u64,
    pub after: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ViewedQuery {
    pub target: ViewedKey,
    pub cursor: Option<ViewedCursor>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ViewedFiles {
    version: u64,
    files: BTreeMap<String, String>,
    next: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) enum GetViewedOutcome {
    Page(ViewedFiles),
    NotFound,
    Changed,
}

pub(crate) struct GetViewedFiles;
impl Read for GetViewedFiles {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 401;
    type Input = ViewedQuery;
    type Output = GetViewedOutcome;

    fn execute(context: &mut Transaction<'_>, input: ViewedQuery) -> Result<Self::Output> {
        let Some(key) = input.target.document(context)? else {
            return Ok(GetViewedOutcome::NotFound);
        };
        let saved: Progress = context.document(&key)?.unwrap_or_default();
        let (mut files, mut next) = match input.cursor {
            Some(cursor) => {
                if !valid_id(&cursor.after) {
                    return Err(Error::Invalid("Invalid viewed-file cursor"));
                }
                if cursor.version != saved.version {
                    return Ok(GetViewedOutcome::Changed);
                }
                (BTreeMap::new(), Some(cursor.after))
            }
            None => (saved.files, None),
        };
        if saved.extra > 0 {
            let prefix = format!("{key}/files/");
            for _ in 0..SCANS_PER_PAGE {
                let start = next.map(|path| format!("{prefix}{path}"));
                let mut rows = context.documents_after::<String>(
                    &prefix,
                    start.as_deref(),
                    FILES_PER_SCAN + 1,
                )?;
                next = if rows.len() > FILES_PER_SCAN {
                    rows.truncate(FILES_PER_SCAN);
                    rows.last().map(|(key, _)| key[prefix.len()..].to_owned())
                } else {
                    None
                };
                files.extend(
                    rows.into_iter()
                        .map(|(mut key, revision)| (key.split_off(prefix.len()), revision)),
                );
                if next.is_none() {
                    break;
                }
            }
        } else {
            next = None;
        }
        Ok(GetViewedOutcome::Page(ViewedFiles {
            version: saved.version,
            files,
            next,
        }))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetViewedInput {
    pub target: ViewedKey,
    pub version: u64,
    pub file: String,
    pub viewed: bool,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ViewedVersion {
    pub version: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SetViewedOutcome {
    Saved(ViewedVersion),
    NotFound,
    Archived,
    Conflict,
    AtCapacity,
}

pub(crate) struct SetViewedFile;
impl Mutation for SetViewedFile {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 401;
    type Input = SetViewedInput;
    type Output = SetViewedOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        use SetViewedOutcome as Outcome;
        let Some((path, revision)) = input
            .file
            .split_at_checked(64)
            .filter(|(path, revision)| valid_id(path) && valid_id(revision))
        else {
            return Err(Error::Invalid("Invalid viewed-file identity"));
        };
        let Some(key) = input.target.document(context)? else {
            return Ok(MutationResult::Rejected(Outcome::NotFound));
        };
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(Outcome::Archived));
        }
        let mut saved: Progress = context.document(&key)?.unwrap_or_default();
        // A stale no-op cannot advance a browser that has missed other paths.
        if saved.version != input.version {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        let inline = saved.files.get(path).map(String::as_str);
        let extra_key = format!("{key}/files/{path}");
        let extra = if inline.is_none() && saved.extra > 0 {
            context.document::<String>(&extra_key)?
        } else {
            None
        };
        let current = inline.or(extra.as_deref());
        if (current == Some(revision)) == input.viewed {
            return Ok(MutationResult::Success(Outcome::Saved(ViewedVersion {
                version: saved.version,
            })));
        }
        if input.viewed {
            if current.is_none() && saved.files.len() + saved.extra >= MAX_VIEWED_FILES {
                return Ok(MutationResult::Rejected(Outcome::AtCapacity));
            }
            if inline.is_some() || (extra.is_none() && saved.files.len() < INLINE_FILES) {
                saved.files.insert(path.to_owned(), revision.to_owned());
            } else {
                context.put_document(&extra_key, &revision)?;
                saved.extra += usize::from(extra.is_none());
            }
        } else if inline.is_some() {
            saved.files.remove(path);
        } else {
            context.delete_document(&extra_key)?;
            saved.extra -= 1;
        }
        // Progress belongs to the reviewer and must leave pull versions,
        // discussion ordering, and other reviewers' progress untouched.
        saved.version = next_version(saved.version)?;
        context.put_document(&key, &saved)?;
        Ok(MutationResult::Success(Outcome::Saved(ViewedVersion {
            version: saved.version,
        })))
    }
}
