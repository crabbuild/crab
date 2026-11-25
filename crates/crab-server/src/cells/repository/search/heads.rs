use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Expression, Query, Term};
use crate::cells::{
    Error, Result,
    application::{Read, SqlValue, Transaction},
    repository::{self, integer, projection, result_blob, result_u64_from_row},
};

// Leave space for query criteria and peer framing within the Cell request budget.
pub(crate) const MAX_FORK_HEAD_BYTES: usize = 1024 * 1024;
const REF_PAGE: usize = 128;

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

pub(super) struct Prepared {
    epoch: Option<u64>,
    fork_heads: Option<String>,
}

pub(super) fn prepare(context: &mut Transaction<'_>, input: Option<&Context>) -> Result<Prepared> {
    let input = input.ok_or(Error::Invalid(
        "This filter requires captured pull request heads",
    ))?;
    if input.stamp != DiscussionStamp::read(context)? {
        return Err(Error::StaleSearch);
    }
    let fork_heads = if input.fork_heads.is_empty() {
        None
    } else {
        let snapshot = serde_json::to_string(&input.fork_heads)?;
        if snapshot.len() > MAX_FORK_HEAD_BYTES
            || input.fork_heads.iter().any(|(reference, oid)| {
                fork_ref(reference).is_err() || repository::pulls::validate_oid(oid).is_err()
            })
        {
            return Err(Error::Invalid("Search fork head snapshot is invalid"));
        }
        Some(snapshot)
    };
    let epoch = match &input.source_token {
        Some(token)
            if token.len() == 64
                && token
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()) =>
        {
            Some(projection::ready_epoch(context, token)?)
        }
        Some(_) => return Err(Error::Invalid("Search Git source token is invalid")),
        None if GetSourceState::execute(context, ())?.is_none() => None,
        None => return Err(Error::StaleSearch),
    };
    Ok(Prepared { epoch, fork_heads })
}

pub(super) struct Bound {
    pub cte: String,
    pub joins: String,
    pub oid: &'static str,
}

impl Bound {
    pub fn new(prepared: &Prepared, mut bind: impl FnMut(SqlValue) -> String) -> Result<Self> {
        let epoch = prepared
            .epoch
            .map(integer)
            .transpose()?
            .map_or_else(|| "NULL".into(), &mut bind);
        let mut cte = String::new();
        let mut forks = "";
        let mut oid =
            "COALESCE(pr_source.merged_head_oid,NULLIF(lower(hex(git_head.target_oid)),''))";
        if let Some(fork_heads) = &prepared.fork_heads {
            let tips = bind(SqlValue::Text(fork_heads.clone()));
            // Materialize once so SQLite can index the captured fork names
            // before joining them to discussion rows.
            cte = format!(
                "WITH fork_heads AS MATERIALIZED (SELECT CAST(key AS BLOB) AS ref,value AS oid FROM json_each({tips}))"
            );
            forks = "LEFT JOIN fork_heads fork_head ON fork_head.ref=pr_source.head_ref";
            oid = "COALESCE(pr_source.merged_head_oid,NULLIF(lower(hex(git_head.target_oid)),''),fork_head.oid)";
        }
        let joins = format!(
            "LEFT JOIN discussion_index_pull_sources pr_source ON pr_source.document=d.rowid
             LEFT JOIN git_projection_refs git_head ON git_head.epoch_id={epoch} AND git_head.name=pr_source.head_ref
             {forks}"
        );
        Ok(Self { cte, joins, oid })
    }
}

pub(crate) struct GetSourceState;
impl Read for GetSourceState {
    const CLASS: &'static str = repository::CLASS;
    const ID: u32 = 327;
    type Input = ();
    type Output = Option<projection::ProjectionStateView>;
    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<Self::Output> {
        super::index::ensure_ready(context)?;
        if context
            .query("SELECT 1 FROM repository_pulls LIMIT 1", &[])?
            .rows
            .is_empty()
        {
            return Ok(None);
        }
        projection::GetProjectionState::execute(context, projection::ProjectionState).map(Some)
    }
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
    pub query: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ForkRefPage {
    pub refs: Vec<String>,
    pub next: Option<u64>,
    pub stamp: DiscussionStamp,
}

pub(crate) struct ListForkRefs;

fn branch_predicates(expression: &Expression, bindings: &mut Vec<SqlValue>, sql: &mut String) {
    let (condition, value) = match expression {
        Expression::Scope { expression, .. } => {
            return branch_predicates(expression, bindings, sql);
        }
        Expression::And(parts) => {
            for part in parts {
                branch_predicates(part, bindings, sql);
            }
            return;
        }
        Expression::Term {
            value: Term::BaseBranch(branch),
            ..
        } => (
            " AND base_ref = ?",
            SqlValue::Text(format!("refs/heads/{branch}")),
        ),
        Expression::Term {
            value: Term::HeadBranch(branch),
            ..
        } => (
            " AND instr(head_ref, ?) = 38",
            SqlValue::Blob(format!("refs/heads/{branch}").into_bytes()),
        ),
        // These must be true both for result rows and for state navigation
        // counts. Branch terms inside OR or NOT do not constrain this scan.
        _ => return,
    };
    sql.push_str(condition);
    bindings.push(value);
}

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
        let query = Query::parse(input.query.as_deref()).map_err(Error::Invalid)?;
        let mut predicates = String::new();
        let mut bindings = vec![integer(input.after)?];
        branch_predicates(&query.expression, &mut bindings, &mut predicates);
        // Merged rows carry the recorded head. Keep live fork discovery ordered
        // by document. Qualified refs start with a UUID and colon before the
        // branch prefix, so the branch begins at byte 38 in the blob.
        let sql = format!(
            "SELECT document,head_ref FROM discussion_index_pull_sources
             WHERE document>? AND merged_head_oid IS NULL AND instr(head_ref,X'3a')=37
             {predicates} ORDER BY document LIMIT ?"
        );
        bindings.push(integer((REF_PAGE + 1) as u64)?);
        let rows = context.query(&sql, &bindings)?;
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
