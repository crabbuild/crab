use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{RepositoryAuthor, hash_text, next_version, validate_author, validate_number};
use crate::cells::{
    Error, Result,
    application::{Mutation, MutationResult, Read, Transaction},
    repository::{integer, settings},
};

// The two 64-character IDs and JSON framing must fit the Cell document limit.
const MAX_VIEWED_FILES: usize = 950;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ViewedFiles {
    pub version: u64,
    files: BTreeMap<String, String>,
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

pub(crate) struct GetViewedFiles;
impl Read for GetViewedFiles {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 401;
    type Input = ViewedKey;
    type Output = Option<ViewedFiles>;

    fn execute(context: &mut Transaction<'_>, key: ViewedKey) -> Result<Self::Output> {
        let Some(key) = key.document(context)? else {
            return Ok(None);
        };
        Ok(Some(context.document(&key)?.unwrap_or_default()))
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
#[serde(rename_all = "snake_case")]
pub(crate) enum SetViewedOutcome {
    Saved(ViewedFiles),
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
        if input.file.len() != 128
            || !input
                .file
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(Error::Invalid("Invalid viewed-file identity"));
        }
        let Some(key) = input.target.document(context)? else {
            return Ok(MutationResult::Rejected(Outcome::NotFound));
        };
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(Outcome::Archived));
        }
        let mut saved: ViewedFiles = context.document(&key)?.unwrap_or_default();
        let (path, revision) = input.file.split_at(64);
        let current = saved.files.get(path).map(String::as_str);
        if (current == Some(revision)) == input.viewed {
            return Ok(MutationResult::Success(Outcome::Saved(saved)));
        }
        if saved.version != input.version {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        if input.viewed {
            if current.is_none() && saved.files.len() >= MAX_VIEWED_FILES {
                return Ok(MutationResult::Rejected(Outcome::AtCapacity));
            }
            saved.files.insert(path.to_owned(), revision.to_owned());
        } else {
            saved.files.remove(path);
        }
        // Progress belongs to the reviewer and must leave pull versions,
        // discussion ordering, and other reviewers' progress untouched.
        saved.version = next_version(saved.version)?;
        context.put_document(&key, &saved)?;
        Ok(MutationResult::Success(Outcome::Saved(saved)))
    }
}
