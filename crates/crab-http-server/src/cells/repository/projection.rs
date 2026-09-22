//! Rebuildable Git browse projections stored in the repository Cell.

use cellule_runtime::{
    BoundedDecoder, BoundedEncoder, CellModule, Command, CommandContext, CommandResult, Query,
    QueryContext, RegistryBuilder, SqlBatch, SqlResultSet, SqlStatement, SqlValue, WireValue,
};
use serde::{Deserialize, Serialize};

use super::RepositoryModule;

pub(crate) const OP_BEGIN: u8 = 1;
pub(crate) const OP_REFS: u8 = 2;
pub(crate) const OP_COMMITS: u8 = 3;
pub(crate) const OP_TREES: u8 = 4;
pub(crate) const OP_ATTRIBUTION: u8 = 5;
pub(crate) const OP_PROMOTE: u8 = 6;
pub(crate) const OP_SUPERSEDE: u8 = 7;
pub(crate) const OP_COLLECT: u8 = 8;

const MAX_SOURCE_BYTES: usize = 32 * 1024;
const MAX_BATCH_BYTES: usize = 700 * 1024;
const MAX_BATCH_ROWS: usize = 256;
const MAX_SQL_STATEMENTS: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectionBatch {
    pub operation: u8,
    pub epoch_id: u64,
    pub source: Vec<u8>,
    pub payload: Vec<u8>,
}

impl WireValue for ProjectionBatch {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), cellule_runtime::CodecError> {
        if self.source.len() > MAX_SOURCE_BYTES || self.payload.len() > MAX_BATCH_BYTES {
            return Err(cellule_runtime::CodecError::Limit);
        }
        encoder.write_u8(self.operation)?;
        encoder.write_u64(self.epoch_id)?;
        encoder.write_bytes(&self.source)?;
        encoder.write_bytes(&self.payload)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, cellule_runtime::CodecError> {
        let operation = decoder.read_u8()?;
        let epoch_id = decoder.read_u64()?;
        let source = decoder.read_bytes()?;
        let payload = decoder.read_bytes()?;
        if source.len() > MAX_SOURCE_BYTES || payload.len() > MAX_BATCH_BYTES {
            return Err(cellule_runtime::CodecError::Limit);
        }
        Ok(Self {
            operation,
            epoch_id,
            source: source.to_vec(),
            payload: payload.to_vec(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectionAck {
    pub epoch_id: Option<u64>,
}

impl WireValue for ProjectionAck {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), cellule_runtime::CodecError> {
        self.epoch_id.encode(encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, cellule_runtime::CodecError> {
        Ok(Self {
            epoch_id: Option::<u64>::decode(decoder)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectionState;

impl WireValue for ProjectionState {
    fn encode(&self, _encoder: &mut BoundedEncoder) -> Result<(), cellule_runtime::CodecError> {
        Ok(())
    }

    fn decode(_decoder: &mut BoundedDecoder<'_>) -> Result<Self, cellule_runtime::CodecError> {
        Ok(Self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectionStateView {
    pub ready_epoch: Option<u64>,
    pub source_token: Option<String>,
    pub generation: Option<u64>,
}

impl WireValue for ProjectionStateView {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), cellule_runtime::CodecError> {
        self.ready_epoch.encode(encoder)?;
        self.source_token.encode(encoder)?;
        self.generation.encode(encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, cellule_runtime::CodecError> {
        Ok(Self {
            ready_epoch: Option::<u64>::decode(decoder)?,
            source_token: Option::<String>::decode(decoder)?,
            generation: Option::<u64>::decode(decoder)?,
        })
    }
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
    pub tree_oid: Vec<u8>,
    pub parents: Vec<Vec<u8>>,
    pub author_name: Vec<u8>,
    pub author_email: Vec<u8>,
    pub author_time: i64,
    pub author_tz_offset_seconds: i32,
    pub committer_name: Vec<u8>,
    pub committer_email: Vec<u8>,
    pub committer_time: i64,
    pub committer_tz_offset_seconds: i32,
    pub message_preview: Vec<u8>,
    pub message_truncated: bool,
    pub encoded_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TreeEntryRow {
    pub name: Vec<u8>,
    pub mode: u32,
    pub object_oid: Vec<u8>,
    pub object_kind: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TreeRow {
    pub tree_oid: Vec<u8>,
    pub entries: Vec<TreeEntryRow>,
    pub encoded_bytes: u64,
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

impl Command for ApplyProjectionBatch {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = super::super::REPOSITORY_PROJECTION_COMMAND_ID;
    const CODEC_VERSION: u32 = 1;
    type Input = ProjectionBatch;
    type Output = ProjectionAck;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> cellule_runtime::Result<CommandResult<Self::Output>> {
        if input.source.len() > MAX_SOURCE_BYTES || input.payload.len() > MAX_BATCH_BYTES {
            return Err(cellule_runtime::Error::Command(
                "projection batch exceeds input limit",
            ));
        }
        let source: SourceIdentity = serde_json::from_slice(&input.source).map_err(|_| {
            cellule_runtime::Error::Command("projection source identity is invalid")
        })?;
        validate_source(&source)?;
        let payload = &input.payload;
        let epoch_id = match input.operation {
            OP_BEGIN => begin(context, &source)?,
            OP_REFS => {
                require_epoch(context, input.epoch_id, &source)?;
                let rows: Vec<RefRow> = decode_payload(payload)?;
                if rows.len() > MAX_BATCH_ROWS {
                    return Err(cellule_runtime::Error::Command(
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
                    return Err(cellule_runtime::Error::Command(
                        "projection commit batch is too large",
                    ));
                }
                apply_commits(context, input.epoch_id, &rows)?;
                None
            }
            OP_TREES => {
                require_epoch(context, input.epoch_id, &source)?;
                let rows: Vec<TreeRow> = decode_payload(payload)?;
                if rows.len() > MAX_BATCH_ROWS {
                    return Err(cellule_runtime::Error::Command(
                        "projection tree batch is too large",
                    ));
                }
                apply_trees(context, &rows)?;
                None
            }
            OP_ATTRIBUTION => {
                require_epoch(context, input.epoch_id, &source)?;
                let batch: AttributionBatch = decode_payload(payload)?;
                if batch.nodes.len() > MAX_BATCH_ROWS
                    || batch.edges.len() > MAX_BATCH_ROWS.saturating_mul(8)
                    || batch.roots.len() > MAX_BATCH_ROWS
                {
                    return Err(cellule_runtime::Error::Command(
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
                return Err(cellule_runtime::Error::Command(
                    "unknown projection operation",
                ));
            }
        };
        super::advance_revision(context)?;
        Ok(CommandResult::Success(ProjectionAck { epoch_id }))
    }
}

pub(crate) struct GetProjectionState;

impl Query for GetProjectionState {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = super::super::REPOSITORY_PROJECTION_STATE_QUERY_ID;
    const CODEC_VERSION: u32 = 1;
    type Input = ProjectionState;
    type Output = ProjectionStateView;

    fn execute(
        context: &mut QueryContext<'_>,
        _input: Self::Input,
    ) -> cellule_runtime::Result<Self::Output> {
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT s.ready_epoch, e.source_token, e.manifest_generation FROM git_projection_state s LEFT JOIN git_projection_epochs e ON e.epoch_id = s.ready_epoch WHERE s.singleton = 1",
                vec![],
            )],
        })?;
        let Some(row) = result[0].rows.first() else {
            return Err(cellule_runtime::Error::Command(
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

impl Query for GetProjectionAttribution {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = super::super::REPOSITORY_PROJECTION_ATTRIBUTION_QUERY_ID;
    const CODEC_VERSION: u32 = 1;
    type Input = Vec<u8>;
    type Output = Vec<u8>;

    fn execute(
        context: &mut QueryContext<'_>,
        input: Self::Input,
    ) -> cellule_runtime::Result<Self::Output> {
        let request: AttributionQuery = serde_json::from_slice(&input).map_err(|_| {
            cellule_runtime::Error::Command("projection attribution input is invalid")
        })?;
        let response = attribution(context, &request)?;
        serde_json::to_vec(&response).map_err(|_| {
            cellule_runtime::Error::Command("projection attribution output is invalid")
        })
    }
}

pub(crate) fn register(registry: &mut RegistryBuilder) -> cellule_runtime::Result<()> {
    registry.bind_command::<ApplyProjectionBatch>()?;
    registry.bind_query::<GetProjectionState>()?;
    registry.bind_query::<GetProjectionAttribution>()?;
    Ok(())
}

fn attribution(
    context: &QueryContext<'_>,
    request: &AttributionQuery,
) -> cellule_runtime::Result<AttributionResponse> {
    if request.source_token.len() > 128
        || request.commit_oid.len() != 20
        || request.paths.len() > 200
        || request
            .paths
            .iter()
            .any(|path| path.is_empty() || path.len() > 1024 * 1024 || path.contains(&0))
    {
        return Err(cellule_runtime::Error::Command(
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

fn begin(
    context: &CommandContext<'_, '_>,
    source: &SourceIdentity,
) -> cellule_runtime::Result<Option<u64>> {
    let existing = context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT epoch_id, state FROM git_projection_epochs WHERE source_token = ?",
            vec![text(&source.source_token)],
        )],
    })?;
    if let Some(row) = existing[0].rows.first() {
        let epoch_id = match row.first() {
            Some(SqlValue::Integer(value)) => u64::try_from(*value)
                .map_err(|_| cellule_runtime::Error::Command("projection epoch is negative"))?,
            _ => {
                return Err(cellule_runtime::Error::Command(
                    "projection epoch is invalid",
                ));
            }
        };
        let state = result_text(row, 1)?;
        if state == "ready" {
            context.sql(&SqlBatch {
                statements: vec![statement(
                    "UPDATE git_projection_state SET ready_epoch = ? WHERE singleton = 1",
                    vec![integer(epoch_id)?],
                )],
            })?;
            update_probe_state(context, source)?;
            return Ok(None);
        }
        if state == "superseded" || state == "failed" {
            context.sql(&SqlBatch {
                statements: vec![
                    statement(
                        "DELETE FROM git_projection_refs WHERE epoch_id = ?",
                        vec![integer(epoch_id)?],
                    ),
                    statement(
                        "DELETE FROM git_epoch_commits WHERE epoch_id = ?",
                        vec![integer(epoch_id)?],
                    ),
                    statement(
                        "DELETE FROM git_attribution_roots WHERE epoch_id = ?",
                        vec![integer(epoch_id)?],
                    ),
                    statement(
                        "UPDATE git_projection_epochs SET state = 'building', started_at_ms = ?, verified_at_ms = NULL WHERE epoch_id = ?",
                        vec![
                            integer(u64::try_from(context.now_ms()).map_err(|_| {
                                cellule_runtime::Error::Command(
                                    "projection timestamp is negative",
                                )
                            })?)?,
                            integer(epoch_id)?,
                        ],
                    ),
                ],
            })?;
        }
        update_probe_state(context, source)?;
        return Ok(Some(epoch_id));
    }
    let result = context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT COALESCE(MAX(epoch_id), 0) FROM git_projection_epochs",
            vec![],
        )],
    })?;
    let previous = result_u64(&result, 0, 0)?;
    let epoch_id = previous
        .checked_add(1)
        .ok_or(cellule_runtime::Error::Command(
            "projection epoch is exhausted",
        ))?;
    context.sql(&SqlBatch {
        statements: vec![statement(
            "UPDATE git_projection_epochs SET state = 'superseded' WHERE state IN ('building', 'verifying') AND source_token <> ?",
            vec![text(&source.source_token)],
        )],
    })?;
    context.sql(&SqlBatch {
        statements: vec![statement(
            "INSERT OR IGNORE INTO git_projection_epochs(epoch_id, state, source_token, manifest_generation, manifest_etag, journal_state_digest, pack_index_hash, git_validation_digest, commit_graph_hash, path_state_hash, head_ref, started_at_ms) VALUES (?, 'building', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            vec![
                integer(epoch_id)?,
                text(&source.source_token),
                integer(source.manifest_generation)?,
                text(&source.manifest_etag),
                text(&source.journal_state_digest),
                text(&source.pack_index_hash),
                text(&source.git_validation_digest),
                optional_text_value(source.commit_graph_hash.as_deref()),
                optional_text_value(source.path_state_hash.as_deref()),
                SqlValue::Blob(source.head_ref.clone()),
                integer(u64::try_from(context.now_ms()).map_err(|_| {
                    cellule_runtime::Error::Command("projection timestamp is negative")
                })?)?,
            ],
        )],
    })?;
    update_probe_state(context, source)?;
    Ok(Some(epoch_id))
}

fn update_probe_state(
    context: &CommandContext<'_, '_>,
    source: &SourceIdentity,
) -> cellule_runtime::Result<()> {
    let now = u64::try_from(context.now_ms())
        .map_err(|_| cellule_runtime::Error::Command("projection timestamp is negative"))?;
    context.sql(&SqlBatch {
        statements: vec![statement(
            "UPDATE git_projection_state SET desired_source_token = ?, desired_manifest_generation = ?, desired_manifest_etag = ?, desired_journal_digest = ?, last_probe_at_ms = ?, next_probe_at_ms = ? WHERE singleton = 1",
            vec![
                text(&source.source_token),
                integer(source.manifest_generation)?,
                text(&source.manifest_etag),
                text(&source.journal_state_digest),
                integer(now)?,
                integer(now)?,
            ],
        )],
    })?;
    Ok(())
}

fn apply_refs(
    context: &CommandContext<'_, '_>,
    epoch_id: u64,
    rows: &[RefRow],
) -> cellule_runtime::Result<()> {
    let mut statements = Vec::with_capacity(rows.len());
    for row in rows {
        statements.push(statement(
            "INSERT OR REPLACE INTO git_projection_refs(epoch_id, name, target_oid, peeled_oid) VALUES (?, ?, ?, ?)",
            vec![
                integer(epoch_id)?,
                SqlValue::Blob(row.name.clone()),
                SqlValue::Blob(row.target_oid.clone()),
                row.peeled_oid
                    .clone()
                    .map_or(SqlValue::Null, SqlValue::Blob),
            ],
        ));
    }
    if !statements.is_empty() {
        execute_statements(context, statements)?;
    }
    Ok(())
}

fn apply_commits(
    context: &CommandContext<'_, '_>,
    epoch_id: u64,
    rows: &[CommitRow],
) -> cellule_runtime::Result<()> {
    let mut statements = Vec::with_capacity(rows.len().saturating_mul(3));
    for row in rows {
        statements.push(statement(
            "INSERT OR IGNORE INTO git_commits(oid, tree_oid, author_name, author_email, author_time, author_tz_offset_seconds, committer_name, committer_email, committer_time, committer_tz_offset_seconds, message_preview, message_truncated, encoded_bytes) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            vec![
                SqlValue::Blob(row.oid.clone()),
                SqlValue::Blob(row.tree_oid.clone()),
                SqlValue::Blob(row.author_name.clone()),
                SqlValue::Blob(row.author_email.clone()),
                SqlValue::Integer(row.author_time),
                SqlValue::Integer(i64::from(row.author_tz_offset_seconds)),
                SqlValue::Blob(row.committer_name.clone()),
                SqlValue::Blob(row.committer_email.clone()),
                SqlValue::Integer(row.committer_time),
                SqlValue::Integer(i64::from(row.committer_tz_offset_seconds)),
                SqlValue::Blob(row.message_preview.clone()),
                SqlValue::Integer(i64::from(u8::from(row.message_truncated))),
                integer(row.encoded_bytes)?,
            ],
        ));
        statements.push(statement(
            "INSERT OR REPLACE INTO git_epoch_commits(epoch_id, ordinal, commit_oid) VALUES (?, ?, ?)",
            vec![
                integer(epoch_id)?,
                integer(u64::from(row.ordinal))?,
                SqlValue::Blob(row.oid.clone()),
            ],
        ));
        for (index, parent) in row.parents.iter().enumerate() {
            statements.push(statement(
                "INSERT OR REPLACE INTO git_commit_parents(commit_oid, parent_index, parent_oid) VALUES (?, ?, ?)",
                vec![
                    SqlValue::Blob(row.oid.clone()),
                    integer(u64::try_from(index).map_err(|_| {
                        cellule_runtime::Error::Command("projection parent index overflowed")
                    })?)?,
                    SqlValue::Blob(parent.clone()),
                ],
            ));
        }
    }
    execute_statements(context, statements)?;
    Ok(())
}

fn apply_trees(context: &CommandContext<'_, '_>, rows: &[TreeRow]) -> cellule_runtime::Result<()> {
    let mut statements = Vec::new();
    for row in rows {
        statements.push(statement(
            "INSERT OR REPLACE INTO git_trees(tree_oid, state, entry_count, encoded_bytes, last_used_at_ms) VALUES (?, 'building', ?, ?, ?)",
            vec![
                SqlValue::Blob(row.tree_oid.clone()),
                integer(u64::try_from(row.entries.len()).map_err(|_| {
                    cellule_runtime::Error::Command("projection tree entry count overflowed")
                })?)?,
                integer(row.encoded_bytes)?,
                integer(u64::try_from(context.now_ms()).map_err(|_| {
                    cellule_runtime::Error::Command("projection timestamp is negative")
                })?)?,
            ],
        ));
        statements.push(statement(
            "DELETE FROM git_tree_entries WHERE tree_oid = ?",
            vec![SqlValue::Blob(row.tree_oid.clone())],
        ));
        for entry in &row.entries {
            statements.push(statement(
                "INSERT INTO git_tree_entries(tree_oid, name, mode, object_oid, object_kind) VALUES (?, ?, ?, ?, ?)",
                vec![
                    SqlValue::Blob(row.tree_oid.clone()),
                    SqlValue::Blob(entry.name.clone()),
                    SqlValue::Integer(i64::from(entry.mode)),
                    SqlValue::Blob(entry.object_oid.clone()),
                    SqlValue::Integer(i64::from(entry.object_kind)),
                ],
            ));
        }
        statements.push(statement(
            "UPDATE git_trees SET state = 'ready' WHERE tree_oid = ? AND entry_count = (SELECT COUNT(*) FROM git_tree_entries WHERE tree_oid = ?)",
            vec![
                SqlValue::Blob(row.tree_oid.clone()),
                SqlValue::Blob(row.tree_oid.clone()),
            ],
        ));
    }
    execute_statements(context, statements)?;
    Ok(())
}

fn apply_attribution(
    context: &CommandContext<'_, '_>,
    epoch_id: u64,
    batch: &AttributionBatch,
) -> cellule_runtime::Result<()> {
    let mut statements = Vec::new();
    for node in &batch.nodes {
        statements.push(statement(
            "INSERT OR IGNORE INTO git_attribution_nodes(node_hash, last_change_ordinal, encoded_bytes) VALUES (?, ?, ?)",
            vec![
                SqlValue::Blob(node.node_hash.clone()),
                node.last_change_ordinal
                    .map_or(SqlValue::Null, |value| SqlValue::Integer(i64::from(value))),
                integer(node.encoded_bytes)?,
            ],
        ));
    }
    for edge in &batch.edges {
        statements.push(statement(
            "INSERT OR REPLACE INTO git_attribution_edges(parent_hash, component, child_hash) VALUES (?, ?, ?)",
            vec![
                SqlValue::Blob(edge.parent_hash.clone()),
                SqlValue::Blob(edge.component.clone()),
                SqlValue::Blob(edge.child_hash.clone()),
            ],
        ));
    }
    for root in &batch.roots {
        statements.push(statement(
            "INSERT OR REPLACE INTO git_attribution_roots(epoch_id, commit_ordinal, root_hash) VALUES (?, ?, ?)",
            vec![
                integer(epoch_id)?,
                integer(u64::from(root.commit_ordinal))?,
                SqlValue::Blob(root.root_hash.clone()),
            ],
        ));
    }
    execute_statements(context, statements)?;
    Ok(())
}

fn promote(
    context: &CommandContext<'_, '_>,
    epoch_id: u64,
    source: &SourceIdentity,
) -> cellule_runtime::Result<()> {
    verify_epoch(context, epoch_id, source)?;
    let result = context.sql(&SqlBatch {
        statements: vec![statement(
            "UPDATE git_projection_epochs SET state = 'ready', verified_at_ms = ? WHERE epoch_id = ? AND source_token = ? AND state = 'building'",
            vec![
                integer(u64::try_from(context.now_ms()).map_err(|_| {
                    cellule_runtime::Error::Command("projection timestamp is negative")
                })?)?,
                integer(epoch_id)?,
                text(&source.source_token),
            ],
        )],
    })?;
    if result[0].rows_affected != 1 {
        return Err(cellule_runtime::Error::Command(
            "projection epoch cannot be promoted",
        ));
    }
    context.sql(&SqlBatch {
        statements: vec![statement(
            "UPDATE git_projection_state SET ready_epoch = ?, last_error_code = NULL WHERE singleton = 1",
            vec![integer(epoch_id)?],
        )],
    })?;
    Ok(())
}

fn verify_epoch(
    context: &CommandContext<'_, '_>,
    epoch_id: u64,
    source: &SourceIdentity,
) -> cellule_runtime::Result<()> {
    let result = context.sql(&SqlBatch {
        statements: vec![
            statement(
                "SELECT COUNT(*), COALESCE(MIN(ordinal), 0), COALESCE(MAX(ordinal), -1) FROM git_epoch_commits WHERE epoch_id = ?",
                vec![integer(epoch_id)?],
            ),
            statement(
                "SELECT 1 FROM git_epoch_commits ec LEFT JOIN git_commits c ON c.oid = ec.commit_oid WHERE ec.epoch_id = ? AND c.oid IS NULL LIMIT 1",
                vec![integer(epoch_id)?],
            ),
            statement(
                "SELECT 1 FROM git_epoch_commits ec LEFT JOIN git_attribution_roots ar ON ar.epoch_id = ec.epoch_id AND ar.commit_ordinal = ec.ordinal WHERE ec.epoch_id = ? AND ar.root_hash IS NULL LIMIT 1",
                vec![integer(epoch_id)?],
            ),
            statement(
                "SELECT 1 FROM git_attribution_roots ar LEFT JOIN git_attribution_nodes n ON n.node_hash = ar.root_hash WHERE ar.epoch_id = ? AND n.node_hash IS NULL LIMIT 1",
                vec![integer(epoch_id)?],
            ),
        ],
    })?;
    let Some(row) = result[0].rows.first() else {
        return Err(cellule_runtime::Error::Command(
            "projection epoch count is missing",
        ));
    };
    let count = result_u64(&result, 0, 0)?;
    let minimum = result_i64(row, 1)?;
    let maximum = result_i64(row, 2)?;
    if count != 0 && (minimum != 0 || maximum < 0 || u64::try_from(maximum).ok() != Some(count - 1))
    {
        return Err(cellule_runtime::Error::Command(
            "projection commit ordinals are not contiguous",
        ));
    }
    if !result[1].rows.is_empty()
        || (source.path_state_hash.is_some() && !result[2].rows.is_empty())
        || (source.path_state_hash.is_some() && !result[3].rows.is_empty())
    {
        return Err(cellule_runtime::Error::Command(
            "projection epoch has dangling rows",
        ));
    }
    Ok(())
}

fn supersede(context: &CommandContext<'_, '_>, epoch_id: u64) -> cellule_runtime::Result<()> {
    context.sql(&SqlBatch {
        statements: vec![statement(
            "UPDATE git_projection_epochs SET state = 'superseded' WHERE epoch_id = ? AND state = 'building'",
            vec![integer(epoch_id)?],
        )],
    })?;
    Ok(())
}

fn collect(context: &CommandContext<'_, '_>) -> cellule_runtime::Result<()> {
    let keep = r#"
        WITH keep(epoch_id) AS (
            SELECT ready_epoch
            FROM git_projection_state
            WHERE singleton = 1 AND ready_epoch IS NOT NULL
            UNION
            SELECT epoch_id
            FROM (
                SELECT epoch_id
                FROM git_projection_epochs
                WHERE state = 'ready'
                ORDER BY verified_at_ms DESC
                LIMIT 2
            )
            UNION
            SELECT epoch_id
            FROM git_projection_epochs
            WHERE state IN ('building', 'verifying')
        )
    "#;
    execute_statements(
        context,
        vec![
            statement(
                &format!(
                    "{keep}DELETE FROM git_projection_refs WHERE epoch_id NOT IN (SELECT epoch_id FROM keep)"
                ),
                vec![],
            ),
            statement(
                &format!(
                    "{keep}DELETE FROM git_attribution_roots WHERE epoch_id NOT IN (SELECT epoch_id FROM keep)"
                ),
                vec![],
            ),
            statement(
                &format!(
                    "{keep}DELETE FROM git_epoch_commits WHERE epoch_id NOT IN (SELECT epoch_id FROM keep)"
                ),
                vec![],
            ),
            statement(
                &format!(
                    "{keep}DELETE FROM git_projection_epochs WHERE epoch_id NOT IN (SELECT epoch_id FROM keep)"
                ),
                vec![],
            ),
            statement(
                "DELETE FROM git_commit_parents WHERE commit_oid IN (SELECT commit_oid FROM git_commit_parents WHERE commit_oid NOT IN (SELECT commit_oid FROM git_epoch_commits) LIMIT 256)",
                vec![],
            ),
            statement(
                "DELETE FROM git_commits WHERE oid IN (SELECT oid FROM git_commits WHERE oid NOT IN (SELECT commit_oid FROM git_epoch_commits) LIMIT 256)",
                vec![],
            ),
            statement(
                r#"
                    DELETE FROM git_attribution_nodes
                    WHERE node_hash IN (
                        WITH RECURSIVE reachable(node_hash) AS (
                            SELECT root_hash FROM git_attribution_roots
                            UNION
                            SELECT e.child_hash
                            FROM git_attribution_edges e
                            JOIN reachable r ON r.node_hash = e.parent_hash
                        )
                        SELECT node_hash
                        FROM git_attribution_nodes
                        WHERE node_hash NOT IN (SELECT node_hash FROM reachable)
                        LIMIT 256
                    )
                "#,
                vec![],
            ),
            statement(
                "DELETE FROM git_trees WHERE tree_oid IN (SELECT tree_oid FROM git_trees WHERE state = 'ready' AND tree_oid NOT IN (SELECT tree_oid FROM git_commits) LIMIT 64)",
                vec![],
            ),
        ],
    )
}

fn require_epoch(
    context: &CommandContext<'_, '_>,
    epoch_id: u64,
    source: &SourceIdentity,
) -> cellule_runtime::Result<()> {
    if epoch_id == 0 {
        return Err(cellule_runtime::Error::Command(
            "projection epoch is missing",
        ));
    }
    let result = context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT source_token, state FROM git_projection_epochs WHERE epoch_id = ?",
            vec![integer(epoch_id)?],
        )],
    })?;
    let Some(row) = result[0].rows.first() else {
        return Err(cellule_runtime::Error::Command(
            "projection epoch is missing",
        ));
    };
    if result_text(row, 0)? != source.source_token || result_text(row, 1)? != "building" {
        return Err(cellule_runtime::Error::Command(
            "projection epoch identity or state differs",
        ));
    }
    Ok(())
}

fn validate_source(source: &SourceIdentity) -> cellule_runtime::Result<()> {
    if source.source_token.is_empty()
        || source.source_token.len() > 128
        || source.manifest_etag.len() > 1024
        || source.journal_state_digest.len() > 128
        || source.pack_index_hash.len() > 128
        || source.git_validation_digest.len() > 128
        || source.head_ref.len() > 1024
    {
        return Err(cellule_runtime::Error::Command(
            "projection source identity is out of bounds",
        ));
    }
    Ok(())
}

fn decode_payload<T: for<'de> Deserialize<'de>>(payload: &[u8]) -> cellule_runtime::Result<T> {
    serde_json::from_slice(payload)
        .map_err(|_| cellule_runtime::Error::Command("projection batch payload is invalid"))
}

fn statement(sql: &str, parameters: Vec<SqlValue>) -> SqlStatement {
    SqlStatement {
        sql: sql.to_owned(),
        parameters,
    }
}

fn execute_statements(
    context: &CommandContext<'_, '_>,
    mut statements: Vec<SqlStatement>,
) -> cellule_runtime::Result<()> {
    while !statements.is_empty() {
        let count = statements.len().min(MAX_SQL_STATEMENTS);
        let tail = statements.split_off(count);
        context.sql(&SqlBatch { statements })?;
        statements = tail;
    }
    Ok(())
}

fn integer(value: u64) -> cellule_runtime::Result<SqlValue> {
    Ok(SqlValue::Integer(i64::try_from(value).map_err(|_| {
        cellule_runtime::Error::Command("projection integer exceeds SQLite range")
    })?))
}

fn text(value: &str) -> SqlValue {
    SqlValue::Text(value.to_owned())
}

fn optional_text_value(value: Option<&str>) -> SqlValue {
    value.map_or(SqlValue::Null, text)
}

fn result_u64(sets: &[SqlResultSet], set: usize, column: usize) -> cellule_runtime::Result<u64> {
    match sets
        .get(set)
        .and_then(|set| set.rows.first())
        .and_then(|row| row.get(column))
    {
        Some(SqlValue::Integer(value)) => u64::try_from(*value)
            .map_err(|_| cellule_runtime::Error::Command("projection integer is negative")),
        _ => Err(cellule_runtime::Error::Command(
            "projection query returned invalid integer",
        )),
    }
}

fn result_text(row: &[SqlValue], column: usize) -> cellule_runtime::Result<String> {
    match row.get(column) {
        Some(SqlValue::Text(value)) => Ok(value.clone()),
        _ => Err(cellule_runtime::Error::Command(
            "projection query returned invalid text",
        )),
    }
}

fn result_i64(row: &[SqlValue], column: usize) -> cellule_runtime::Result<i64> {
    match row.get(column) {
        Some(SqlValue::Integer(value)) => Ok(*value),
        _ => Err(cellule_runtime::Error::Command(
            "projection query returned invalid integer",
        )),
    }
}

fn blob(row: &[SqlValue], column: usize) -> cellule_runtime::Result<Vec<u8>> {
    match row.get(column) {
        Some(SqlValue::Blob(value)) => Ok(value.clone()),
        _ => Err(cellule_runtime::Error::Command(
            "projection query returned invalid blob",
        )),
    }
}

fn optional_u64(row: &[SqlValue], column: usize) -> cellule_runtime::Result<Option<u64>> {
    match row.get(column) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Integer(value)) => u64::try_from(*value)
            .map(Some)
            .map_err(|_| cellule_runtime::Error::Command("projection integer is negative")),
        _ => Err(cellule_runtime::Error::Command(
            "projection query returned invalid optional integer",
        )),
    }
}

fn optional_text(row: &[SqlValue], column: usize) -> cellule_runtime::Result<Option<String>> {
    match row.get(column) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Text(value)) => Ok(Some(value.clone())),
        _ => Err(cellule_runtime::Error::Command(
            "projection query returned invalid optional text",
        )),
    }
}
