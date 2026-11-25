use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::cells::{
    Error, Result,
    application::{Read, Transaction},
    repository::{self, integer, projection, result_blob, result_u64_from_row},
};

// Leave space for query criteria and peer framing within the Cell request budget.
pub(crate) const MAX_FORK_HEAD_BYTES: usize = 1024 * 1024;
const REF_PAGE: usize = 128;

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) enum Status {
    Pending,
    Success,
    Failure,
}

impl Status {
    pub(super) fn parse(value: &str) -> std::result::Result<Self, &'static str> {
        match value.to_ascii_lowercase().as_str() {
            "pending" => Ok(Self::Pending),
            "success" => Ok(Self::Success),
            "failure" => Ok(Self::Failure),
            _ => Err("Search status must be pending, success, or failure"),
        }
    }

    pub(super) fn predicate(self) -> &'static str {
        match self {
            Self::Failure => "COALESCE(ci.failed,0) > 0",
            Self::Pending => "COALESCE(ci.failed,0)=0 AND (ci.oid IS NULL OR ci.pending > 0)",
            Self::Success => "ci.oid IS NOT NULL AND ci.failed=0 AND ci.pending=0",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiscussionStamp {
    generation: Uuid,
    revision: u64,
}

impl DiscussionStamp {
    fn read(context: &Transaction<'_>) -> Result<Self> {
        Ok(Self {
            generation: super::index::ensure_ready(context)?,
            revision: super::index::revision(context)?,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Context {
    pub source_token: Option<String>,
    pub stamp: DiscussionStamp,
    pub fork_heads: BTreeMap<String, String>,
}

pub(super) fn prepare(context: &Transaction<'_>, input: Option<&Context>) -> Result<Option<u64>> {
    let input = input.ok_or(Error::Invalid(
        "Check status requires captured pull request heads",
    ))?;
    if input.stamp != DiscussionStamp::read(context)? {
        return Err(Error::StaleSearch);
    }
    let snapshot = serde_json::to_string(&input.fork_heads)?;
    if snapshot.len() > MAX_FORK_HEAD_BYTES
        || input.fork_heads.iter().any(|(reference, oid)| {
            fork_ref(reference).is_err() || repository::pulls::validate_oid(oid).is_err()
        })
    {
        return Err(Error::Invalid("Search fork head snapshot is invalid"));
    }
    input
        .source_token
        .as_deref()
        .map(|token| projection::ready_epoch(context, token))
        .transpose()
}

pub(crate) fn fork_ref(qualified: &str) -> Result<(Uuid, &str)> {
    let (id, reference) = qualified
        .split_once(':')
        .ok_or(Error::Invalid("Fork ref is missing its repository"))?;
    let id = Uuid::parse_str(id).map_err(|_| Error::Invalid("Fork ref repository is invalid"))?;
    if id.is_nil() || !reference.starts_with("refs/heads/") || reference.len() > 1024 {
        return Err(Error::Invalid("Fork ref is invalid"));
    }
    Ok((id, reference))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ForkRefInput {
    pub after: u64,
    pub stamp: Option<DiscussionStamp>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ForkRefPage {
    pub refs: Vec<String>,
    pub next: Option<u64>,
    pub stamp: DiscussionStamp,
}

pub(crate) struct ListForkRefs;

impl Read for ListForkRefs {
    const CLASS: &'static str = repository::CLASS;
    const ID: u32 = 476;
    type Input = ForkRefInput;
    type Output = ForkRefPage;

    fn execute(context: &mut Transaction<'_>, input: ForkRefInput) -> Result<Self::Output> {
        let stamp = DiscussionStamp::read(context)?;
        if input.stamp.is_some_and(|expected| expected != stamp) {
            return Err(Error::StaleSearch);
        }
        // The document key can seek directly through the projection. Merged
        // rows already carry the tested head and need no live fork read.
        let rows = context.query(
            "SELECT document,head_ref FROM discussion_index_pull_sources WHERE document>?
               AND merged_head_oid IS NULL AND instr(head_ref,X'3a')=37
               ORDER BY document LIMIT ?",
            &[integer(input.after)?, integer((REF_PAGE + 1) as u64)?],
        )?;
        let refs = rows
            .rows
            .iter()
            .take(REF_PAGE)
            .map(|row| {
                str::from_utf8(result_blob(row, 1)?)
                    .map(str::to_owned)
                    .map_err(|_| Error::Invalid("Fork ref is not UTF-8"))
            })
            .collect::<Result<Vec<_>>>()?;
        let next = if rows.rows.len() > REF_PAGE {
            Some(result_u64_from_row(&rows.rows[REF_PAGE - 1], 0)?)
        } else {
            None
        };
        Ok(ForkRefPage { refs, next, stamp })
    }
}
