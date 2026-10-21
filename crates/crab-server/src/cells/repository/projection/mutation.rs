use super::*;

pub(super) fn begin(
    context: &Transaction<'_>,
    source: &SourceIdentity,
) -> crate::cells::Result<Option<u64>> {
    let existing = context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT epoch_id, state FROM git_projection_epochs WHERE source_token = ?",
            vec![text(&source.source_token)],
        )],
    })?;
    if let Some(row) = existing[0].rows.first() {
        let epoch_id = match row.first() {
            Some(SqlValue::Integer(value)) => u64::try_from(*value)
                .map_err(|_| crate::cells::Error::Invalid("projection epoch is negative"))?,
            _ => {
                return Err(crate::cells::Error::Invalid("projection epoch is invalid"));
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
                                crate::cells::Error::Invalid(
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
    let epoch_id = previous.checked_add(1).ok_or(crate::cells::Error::Invalid(
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
                    crate::cells::Error::Invalid("projection timestamp is negative")
                })?)?,
            ],
        )],
    })?;
    update_probe_state(context, source)?;
    Ok(Some(epoch_id))
}

fn update_probe_state(
    context: &Transaction<'_>,
    source: &SourceIdentity,
) -> crate::cells::Result<()> {
    let now = u64::try_from(context.now_ms())
        .map_err(|_| crate::cells::Error::Invalid("projection timestamp is negative"))?;
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

pub(super) fn apply_refs(
    context: &Transaction<'_>,
    epoch_id: u64,
    rows: &[RefRow],
) -> crate::cells::Result<()> {
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

pub(super) fn apply_commits(
    context: &Transaction<'_>,
    epoch_id: u64,
    rows: &[CommitRow],
) -> crate::cells::Result<()> {
    let mut statements = Vec::with_capacity(rows.len().saturating_mul(2));
    for row in rows {
        statements.push(statement(
            "INSERT OR IGNORE INTO git_commits(oid, author_name, author_time, message_preview) VALUES (?, ?, ?, ?)",
            vec![
                SqlValue::Blob(row.oid.clone()),
                SqlValue::Blob(row.author_name.clone()),
                SqlValue::Integer(row.author_time),
                SqlValue::Blob(row.message_preview.clone()),
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
    }
    execute_statements(context, statements)?;
    Ok(())
}

pub(super) fn apply_attribution(
    context: &Transaction<'_>,
    epoch_id: u64,
    batch: &AttributionBatch,
) -> crate::cells::Result<()> {
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

pub(super) fn promote(
    context: &Transaction<'_>,
    epoch_id: u64,
    source: &SourceIdentity,
) -> crate::cells::Result<()> {
    verify_epoch(context, epoch_id, source)?;
    let result = context.sql(&SqlBatch {
        statements: vec![statement(
            "UPDATE git_projection_epochs SET state = 'ready', verified_at_ms = ? WHERE epoch_id = ? AND source_token = ? AND state = 'building'",
            vec![
                integer(u64::try_from(context.now_ms()).map_err(|_| {
                    crate::cells::Error::Invalid("projection timestamp is negative")
                })?)?,
                integer(epoch_id)?,
                text(&source.source_token),
            ],
        )],
    })?;
    if result[0].rows_affected != 1 {
        return Err(crate::cells::Error::Invalid(
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
    context: &Transaction<'_>,
    epoch_id: u64,
    source: &SourceIdentity,
) -> crate::cells::Result<()> {
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
        return Err(crate::cells::Error::Invalid(
            "projection epoch count is missing",
        ));
    };
    let count = result_u64(&result, 0, 0)?;
    let minimum = result_i64(row, 1)?;
    let maximum = result_i64(row, 2)?;
    if count != 0 && (minimum != 0 || maximum < 0 || u64::try_from(maximum).ok() != Some(count - 1))
    {
        return Err(crate::cells::Error::Invalid(
            "projection commit ordinals are not contiguous",
        ));
    }
    if !result[1].rows.is_empty()
        || (source.path_state_hash.is_some() && !result[2].rows.is_empty())
        || (source.path_state_hash.is_some() && !result[3].rows.is_empty())
    {
        return Err(crate::cells::Error::Invalid(
            "projection epoch has dangling rows",
        ));
    }
    Ok(())
}

pub(super) fn supersede(context: &Transaction<'_>, epoch_id: u64) -> crate::cells::Result<()> {
    context.sql(&SqlBatch {
        statements: vec![statement(
            "UPDATE git_projection_epochs SET state = 'superseded' WHERE epoch_id = ? AND state = 'building'",
            vec![integer(epoch_id)?],
        )],
    })?;
    Ok(())
}

pub(super) fn collect(context: &Transaction<'_>) -> crate::cells::Result<()> {
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
        ],
    )
}
