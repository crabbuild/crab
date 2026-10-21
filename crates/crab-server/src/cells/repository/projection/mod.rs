//! Rebuildable Git browse projections stored in the repository Cell.

use crate::cells::application::{SqlBatch, SqlResultSet, SqlStatement, SqlValue};
use serde::{Deserialize, Serialize};

use crate::cells::application::{Mutation, MutationResult, Read, Transaction};

pub(crate) const OP_BEGIN: u8 = 1;
pub(crate) const OP_REFS: u8 = 2;
pub(crate) const OP_COMMITS: u8 = 3;
pub(crate) const OP_ATTRIBUTION: u8 = 5;
pub(crate) const OP_PROMOTE: u8 = 6;
pub(crate) const OP_SUPERSEDE: u8 = 7;
pub(crate) const OP_COLLECT: u8 = 8;

const MAX_SOURCE_BYTES: usize = 32 * 1024;
const MAX_BATCH_BYTES: usize = 700 * 1024;
const MAX_BATCH_ROWS: usize = 256;
const MAX_SQL_STATEMENTS: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ProjectionBatch {
    pub operation: u8,
    pub epoch_id: u64,
    pub source: Vec<u8>,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ProjectionAck {
    pub epoch_id: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ProjectionState;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ProjectionStateView {
    pub ready_epoch: Option<u64>,
    pub source_token: Option<String>,
    pub generation: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceIdentity {
    pub source_token: String,
    pub manifest_generation: u64,
    pub manifest_etag: String,
    pub journal_state_digest: String,
    pub pack_index_hash: String,
    pub git_validation_digest: String,
    pub commit_graph_hash: Option<String>,
    pub path_state_hash: Option<String>,
    pub head_ref: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RefRow {
    pub name: Vec<u8>,
    pub target_oid: Vec<u8>,
    pub peeled_oid: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommitRow {
    pub ordinal: u32,
    pub oid: Vec<u8>,
    pub author_name: Vec<u8>,
    pub author_time: i64,
    pub message_preview: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttributionNodeRow {
    pub node_hash: Vec<u8>,
    pub last_change_ordinal: Option<u32>,
    pub encoded_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttributionEdgeRow {
    pub parent_hash: Vec<u8>,
    pub component: Vec<u8>,
    pub child_hash: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttributionRootRow {
    pub commit_ordinal: u32,
    pub root_hash: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttributionBatch {
    pub nodes: Vec<AttributionNodeRow>,
    pub edges: Vec<AttributionEdgeRow>,
    pub roots: Vec<AttributionRootRow>,
}

pub(crate) struct ApplyProjectionBatch;

impl Mutation for ApplyProjectionBatch {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 31;
    type Input = ProjectionBatch;
    type Output = ProjectionAck;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if input.source.len() > MAX_SOURCE_BYTES || input.payload.len() > MAX_BATCH_BYTES {
            return Err(crate::cells::Error::Invalid(
                "projection batch exceeds input limit",
            ));
        }
        let source: SourceIdentity = serde_json::from_slice(&input.source)
            .map_err(|_| crate::cells::Error::Invalid("projection source identity is invalid"))?;
        validate_source(&source)?;
        let payload = &input.payload;
        let epoch_id = match input.operation {
            OP_BEGIN => begin(context, &source)?,
            OP_REFS => {
                require_epoch(context, input.epoch_id, &source)?;
                let rows: Vec<RefRow> = decode_payload(payload)?;
                if rows.len() > MAX_BATCH_ROWS {
                    return Err(crate::cells::Error::Invalid(
                        "projection ref batch is too large",
                    ));
                }
                apply_refs(context, input.epoch_id, &rows)?;
                None
            }
            OP_COMMITS => {
                require_epoch(context, input.epoch_id, &source)?;
                let rows: Vec<CommitRow> = decode_payload(payload)?;
                if rows.len() > MAX_BATCH_ROWS {
                    return Err(crate::cells::Error::Invalid(
                        "projection commit batch is too large",
                    ));
                }
                apply_commits(context, input.epoch_id, &rows)?;
                None
            }
            OP_ATTRIBUTION => {
                require_epoch(context, input.epoch_id, &source)?;
                let batch: AttributionBatch = decode_payload(payload)?;
                if batch.nodes.len() > MAX_BATCH_ROWS
                    || batch.edges.len() > MAX_BATCH_ROWS.saturating_mul(8)
                    || batch.roots.len() > MAX_BATCH_ROWS
                {
                    return Err(crate::cells::Error::Invalid(
                        "projection attribution batch is too large",
                    ));
                }
                apply_attribution(context, input.epoch_id, &batch)?;
                None
            }
            OP_PROMOTE => {
                require_epoch(context, input.epoch_id, &source)?;
                promote(context, input.epoch_id, &source)?;
                None
            }
            OP_SUPERSEDE => {
                require_epoch(context, input.epoch_id, &source)?;
                supersede(context, input.epoch_id)?;
                None
            }
            OP_COLLECT => {
                collect(context)?;
                None
            }
            _ => {
                return Err(crate::cells::Error::Invalid("unknown projection operation"));
            }
        };
        super::advance_revision(context)?;
        Ok(MutationResult::Success(ProjectionAck { epoch_id }))
    }
}

pub(crate) struct GetProjectionState;

impl Read for GetProjectionState {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 27;
    type Input = ProjectionState;
    type Output = ProjectionStateView;

    fn execute(
        context: &mut Transaction<'_>,
        _input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT s.ready_epoch, e.source_token, e.manifest_generation FROM git_projection_state s LEFT JOIN git_projection_epochs e ON e.epoch_id = s.ready_epoch WHERE s.singleton = 1",
                vec![],
            )],
        })?;
        let Some(row) = result[0].rows.first() else {
            return Err(crate::cells::Error::Invalid(
                "projection state row is missing",
            ));
        };
        Ok(ProjectionStateView {
            ready_epoch: optional_u64(row, 0)?,
            source_token: optional_text(row, 1)?,
            generation: optional_u64(row, 2)?,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttributionQuery {
    pub source_token: String,
    pub commit_oid: Vec<u8>,
    pub paths: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct AttributionItem {
    pub path: Vec<u8>,
    pub commit_oid: Vec<u8>,
    pub author: Vec<u8>,
    pub author_seconds: i64,
    pub message: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct AttributionResponse {
    pub state: String,
    pub items: Vec<AttributionItem>,
}

pub(crate) struct GetProjectionAttribution;

impl Read for GetProjectionAttribution {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 28;
    type Input = Vec<u8>;
    type Output = Vec<u8>;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        let request: AttributionQuery = serde_json::from_slice(&input)
            .map_err(|_| crate::cells::Error::Invalid("projection attribution input is invalid"))?;
        let response = attribution(context, &request)?;
        serde_json::to_vec(&response)
            .map_err(|_| crate::cells::Error::Invalid("projection attribution output is invalid"))
    }
}

fn attribution(
    context: &Transaction<'_>,
    request: &AttributionQuery,
) -> crate::cells::Result<AttributionResponse> {
    if request.source_token.len() > 128
        || request.commit_oid.len() != 20
        || request.paths.len() > 200
        || request
            .paths
            .iter()
            .any(|path| path.is_empty() || path.len() > 1024 * 1024 || path.contains(&0))
    {
        return Err(crate::cells::Error::Invalid(
            "projection attribution input is out of bounds",
        ));
    }
    let state = context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT s.ready_epoch, e.source_token FROM git_projection_state s LEFT JOIN git_projection_epochs e ON e.epoch_id = s.ready_epoch WHERE s.singleton = 1",
            vec![],
        )],
    })?;
    let Some(row) = state[0].rows.first() else {
        return Ok(AttributionResponse {
            state: "indexing".to_owned(),
            items: Vec::new(),
        });
    };
    let Some(epoch_id) = optional_u64(row, 0)? else {
        return Ok(AttributionResponse {
            state: "indexing".to_owned(),
            items: Vec::new(),
        });
    };
    if optional_text(row, 1)?.as_deref() != Some(request.source_token.as_str()) {
        return Ok(AttributionResponse {
            state: "indexing".to_owned(),
            items: Vec::new(),
        });
    }
    let ordinal_result = context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT ordinal FROM git_epoch_commits WHERE epoch_id = ? AND commit_oid = ?",
            vec![
                integer(epoch_id)?,
                SqlValue::Blob(request.commit_oid.clone()),
            ],
        )],
    })?;
    if ordinal_result[0].rows.is_empty() {
        return Ok(AttributionResponse {
            state: "stale".to_owned(),
            items: Vec::new(),
        });
    }
    let ordinal = result_u64(&ordinal_result, 0, 0)?;
    let root_result = context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT root_hash FROM git_attribution_roots WHERE epoch_id = ? AND commit_ordinal = ?",
            vec![integer(epoch_id)?, integer(ordinal)?],
        )],
    })?;
    let Some(root_row) = root_result[0].rows.first() else {
        return Ok(AttributionResponse {
            state: "corrupt".to_owned(),
            items: Vec::new(),
        });
    };
    let root = blob(root_row, 0)?;
    let mut items = Vec::with_capacity(request.paths.len());
    for path in &request.paths {
        let mut node_hash = root.clone();
        for component in path.split(|byte| *byte == b'/') {
            let edge_result = context.sql(&SqlBatch {
                statements: vec![statement(
                    "SELECT child_hash FROM git_attribution_edges WHERE parent_hash = ? AND component = ?",
                    vec![SqlValue::Blob(node_hash.clone()), SqlValue::Blob(component.to_vec())],
                )],
            })?;
            let Some(edge_row) = edge_result[0].rows.first() else {
                return Ok(AttributionResponse {
                    state: "corrupt".to_owned(),
                    items: Vec::new(),
                });
            };
            node_hash = blob(edge_row, 0)?;
        }
        let node_result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT last_change_ordinal FROM git_attribution_nodes WHERE node_hash = ?",
                vec![SqlValue::Blob(node_hash)],
            )],
        })?;
        let Some(node_row) = node_result[0].rows.first() else {
            return Ok(AttributionResponse {
                state: "corrupt".to_owned(),
                items: Vec::new(),
            });
        };
        let Some(change_ordinal) = optional_u64(node_row, 0)? else {
            return Ok(AttributionResponse {
                state: "corrupt".to_owned(),
                items: Vec::new(),
            });
        };
        let commit_result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT ec.commit_oid, c.author_name, c.author_time, c.message_preview FROM git_epoch_commits ec JOIN git_commits c ON c.oid = ec.commit_oid WHERE ec.epoch_id = ? AND ec.ordinal = ?",
                vec![integer(epoch_id)?, integer(change_ordinal)?],
            )],
        })?;
        let Some(commit_row) = commit_result[0].rows.first() else {
            return Ok(AttributionResponse {
                state: "corrupt".to_owned(),
                items: Vec::new(),
            });
        };
        items.push(AttributionItem {
            path: path.clone(),
            commit_oid: blob(commit_row, 0)?,
            author: blob(commit_row, 1)?,
            author_seconds: result_i64(commit_row, 2)?,
            message: blob(commit_row, 3)?,
        });
    }
    Ok(AttributionResponse {
        state: "ready".to_owned(),
        items,
    })
}

mod mutation;
use mutation::{apply_attribution, apply_commits, apply_refs, begin, collect, promote, supersede};

mod storage;
use storage::{
    blob, decode_payload, execute_statements, integer, optional_text, optional_text_value,
    optional_u64, require_epoch, result_i64, result_text, result_u64, statement, text,
    validate_source,
};
