mod evidence;
mod source;

#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::Arc;

use blake3::Hasher;
use crab_cell_runtime::{
    CatalogEntry, CatalogRole, CellAuthority, CellCatalog, CellHandle, CellModule, CellReplica,
    CellRuntime, CellTarget, ControlState, IncarnationId, NodeDirectory, Owner, Registry,
    ReleaseState, ReleaseStore, ReplicaLimits, SessionId, SqlWorkerPool,
};
use crab_storage::{CellStorageLayout, StoreLayout};
use rusqlite::{Connection, OptionalExtension as _, Row, types::ValueRef};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    MAX_LIVE_NODES, REPOSITORY_MIGRATION, REPOSITORY_NAMESPACE, RepositoryModule, unix_now_ms,
};
use crate::catalog::{CatalogRecord, CatalogStore};
use crate::{Config, Error, Result};

const IMPORT_MAILBOX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct SemanticSummary {
    digest: String,
    issues: u64,
    issue_submissions: u64,
    comments: u64,
    comment_submissions: u64,
    app_revision: u64,
}

pub(crate) async fn import_repository_issues(
    config: &Config,
    owner: &str,
    name: &str,
    operation: Uuid,
) -> Result<Vec<u8>> {
    let startup = super::verify_startup_release(config).await?;
    require_ready_release(&startup.layout, startup.identity).await?;
    let peer_tls = crate::peer_tls::LoadedPeerTls::load(&config.cells)?;
    let directory = NodeDirectory::new(
        startup.layout.clone(),
        peer_tls.fleet(),
        startup.image,
        startup.registry.release_digest(),
    );
    if !directory
        .live(unix_now_ms()?, MAX_LIVE_NODES)
        .await?
        .is_empty()
    {
        return Err(Error::Config(
            "legacy issue import requires every Cell node to be offline",
        ));
    }

    let catalog = CatalogStore::from_config(config)?;
    let (document, _) = catalog.load().await?;
    let record = find_repository(&document.repositories, owner, name)?;
    let repository = record.runtime_config(catalog.root(), "main")?;
    let repository_layout = StoreLayout::new(catalog.root().store.clone(), repository.prefix);
    let target = CellTarget::new(
        startup.identity.tenant(),
        startup.identity.application(),
        REPOSITORY_NAMESPACE,
        record.id.as_bytes(),
    )?;
    let complete =
        evidence::load_complete(&startup.layout, target.cell_id(), operation, record.id).await?;

    std::fs::create_dir_all(&config.cells.data_dir)?;
    let directory = tempfile::Builder::new()
        .prefix("crab-cell-import-runtime-")
        .tempdir_in(&config.cells.data_dir)?;
    let work = if let Some(complete) = complete {
        ImportWork::Complete(complete)
    } else {
        let source = source::capture(
            repository_layout.store(),
            &repository_layout.repo_path("app/v1/issues"),
            directory.path(),
        )
        .await?;
        let evidence = evidence::publish_source(
            &startup.layout,
            target.cell_id(),
            operation,
            record.id,
            &source,
        )
        .await?;
        ImportWork::Staged { source, evidence }
    };
    let runtime_session = SessionId::from_bytes(Uuid::now_v7().into_bytes());
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1)?,
        IMPORT_MAILBOX_BYTES,
        runtime_session,
    )?;
    let registry = Arc::new(startup.registry);
    let result = match work {
        ImportWork::Complete(complete) => {
            resume_complete(
                &startup.layout,
                startup.identity,
                registry,
                &runtime,
                &target,
                complete,
                runtime_session,
                config.cells.peer_advertise.to_string(),
                directory.path(),
            )
            .await
        }
        ImportWork::Staged { source, evidence } => {
            import_staged(
                &startup.layout,
                startup.identity,
                registry,
                &runtime,
                &target,
                record.id,
                operation,
                &source,
                &evidence,
                runtime_session,
                config.cells.peer_advertise.to_string(),
                directory.path(),
            )
            .await
        }
    };
    let shutdown = runtime.shutdown().await.map_err(Error::from);
    match (result, shutdown) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

enum ImportWork {
    Complete(evidence::CompleteEvidence),
    Staged {
        source: source::StagedSource,
        evidence: evidence::SourceEvidence,
    },
}

async fn require_ready_release(
    layout: &CellStorageLayout,
    identity: crab_cell_runtime::ApplicationIdentity,
) -> Result<()> {
    let release = ReleaseStore::new(layout.clone(), identity)?
        .load()
        .await?
        .ok_or(Error::Config("Cell application release is not activated"))?;
    if release.record().state() != ReleaseState::Ready {
        return Err(Error::Config(
            "legacy issue import requires a ready Cell release",
        ));
    }
    Ok(())
}

fn find_repository<'a>(
    records: &'a [CatalogRecord],
    owner: &str,
    name: &str,
) -> Result<&'a CatalogRecord> {
    records
        .iter()
        .find(|record| {
            record.owner.eq_ignore_ascii_case(owner) && record.name.eq_ignore_ascii_case(name)
        })
        .ok_or_else(|| crate::catalog::CatalogError::NotFound.into())
}

#[expect(
    clippy::too_many_arguments,
    reason = "the maintenance boundary keeps every imported identity explicit"
)]
async fn import_staged(
    layout: &CellStorageLayout,
    identity: crab_cell_runtime::ApplicationIdentity,
    registry: Arc<Registry>,
    runtime: &CellRuntime,
    target: &CellTarget,
    repository: Uuid,
    operation: Uuid,
    source: &source::StagedSource,
    source_evidence: &evidence::SourceEvidence,
    session: SessionId,
    endpoint: String,
    local_dir: &Path,
) -> Result<Vec<u8>> {
    let (proof, authority) = provision(layout, identity, &registry, target).await?;
    let owner = Owner { session, endpoint };
    let observed = match authority.load(target.cell_id()).await? {
        Some(observed) => observed,
        None => {
            authority
                .create_initial(
                    &proof,
                    IncarnationId::from_bytes(operation.into_bytes()),
                    owner.clone(),
                )
                .await?
        }
    };
    let replica = CellReplica::new(
        layout.clone(),
        *target.cell_id().as_bytes(),
        *observed.value().incarnation.as_bytes(),
        ReplicaLimits::default(),
    )
    .map_err(crab_cell_runtime::Error::from)?;
    let destination = local_dir.join(format!("{}.sqlite", Uuid::now_v7()));
    let stage = source.database().to_path_buf();
    let repository_bytes = repository.into_bytes();
    let handle = match observed.value().state {
        ControlState::Recovering if observed.value().root.is_none() => {
            let initialize = move |transaction: &rusqlite::Transaction<'_>| {
                transaction.execute_batch(REPOSITORY_MIGRATION)?;
                transaction.execute(
                    "INSERT INTO repository_identity(singleton, repository_uuid) VALUES (1, ?1)",
                    [repository_bytes.as_slice()],
                )?;
                copy_staged(transaction, &stage)?;
                Ok(())
            };
            if observed.value().owner.as_ref() == Some(&owner) {
                runtime
                    .bootstrap(
                        proof,
                        replica,
                        authority.clone(),
                        observed,
                        destination,
                        initialize,
                    )
                    .await?
            } else {
                runtime
                    .takeover_unpublished(
                        proof,
                        replica,
                        authority.clone(),
                        observed,
                        destination,
                        owner.clone(),
                        initialize,
                    )
                    .await?
            }
        }
        ControlState::Idle => {
            runtime
                .acquire_idle_restored(
                    proof,
                    replica,
                    authority.clone(),
                    observed,
                    destination,
                    owner,
                )
                .await?
        }
        ControlState::Recovering | ControlState::Serving => {
            runtime
                .takeover_restored(
                    proof,
                    replica,
                    authority.clone(),
                    observed,
                    destination,
                    owner,
                )
                .await?
        }
        ControlState::Tombstoned => {
            return Err(Error::Config("legacy issue import Cell is tombstoned"));
        }
    };
    verify_and_complete(
        layout,
        &authority,
        target,
        repository,
        operation,
        source_evidence.source_digest(),
        &source.semantic,
        handle,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "completion replay verifies every immutable import identity"
)]
async fn resume_complete(
    layout: &CellStorageLayout,
    identity: crab_cell_runtime::ApplicationIdentity,
    registry: Arc<Registry>,
    runtime: &CellRuntime,
    target: &CellTarget,
    complete: evidence::CompleteEvidence,
    session: SessionId,
    endpoint: String,
    local_dir: &Path,
) -> Result<Vec<u8>> {
    let (proof, authority) = provision(layout, identity, &registry, target).await?;
    let observed = authority
        .load(target.cell_id())
        .await?
        .ok_or(Error::Config("completed import has no Cell control"))?;
    complete.verify_control(observed.value())?;
    let replica = CellReplica::new(
        layout.clone(),
        *target.cell_id().as_bytes(),
        *observed.value().incarnation.as_bytes(),
        ReplicaLimits::default(),
    )
    .map_err(crab_cell_runtime::Error::from)?;
    let destination = local_dir.join(format!("{}.sqlite", Uuid::now_v7()));
    let owner = Owner { session, endpoint };
    let handle = match observed.value().state {
        ControlState::Idle => {
            runtime
                .acquire_idle_restored(
                    proof,
                    replica,
                    authority.clone(),
                    observed,
                    destination,
                    owner,
                )
                .await?
        }
        ControlState::Recovering | ControlState::Serving => {
            runtime
                .takeover_restored(
                    proof,
                    replica,
                    authority.clone(),
                    observed,
                    destination,
                    owner,
                )
                .await?
        }
        ControlState::Tombstoned => {
            return Err(Error::Config("completed import Cell is tombstoned"));
        }
    };
    let expected = complete.semantic().clone();
    let observed = verify_handle(&handle).await?;
    if observed != expected {
        return Err(Error::Config(
            "restored completed import differs from its semantic evidence",
        ));
    }
    handle.drain().await?;
    complete.encode()
}

async fn provision(
    layout: &CellStorageLayout,
    identity: crab_cell_runtime::ApplicationIdentity,
    registry: &Registry,
    target: &CellTarget,
) -> Result<(crab_cell_runtime::CatalogProof, CellAuthority)> {
    let catalog = CellCatalog::new(layout.clone(), identity.tenant());
    let code =
        registry
            .module_code(RepositoryModule::NAME)
            .ok_or(crab_cell_runtime::Error::Registry(
                "repository module is not registered",
            ))?;
    let proof = ReleaseStore::new(layout.clone(), identity)?
        .provision(
            &catalog,
            registry,
            CatalogEntry::new(target, CatalogRole::Repository, code, 1)?,
        )
        .await?;
    Ok((proof, CellAuthority::new(layout.clone())))
}

async fn verify_and_complete(
    layout: &CellStorageLayout,
    authority: &CellAuthority,
    target: &CellTarget,
    repository: Uuid,
    operation: Uuid,
    source_digest: &str,
    expected: &SemanticSummary,
    handle: CellHandle,
) -> Result<Vec<u8>> {
    let observed = verify_handle(&handle).await?;
    if &observed != expected {
        return Err(Error::Config(
            "imported repository differs from its legacy semantic inventory",
        ));
    }
    handle.drain().await?;
    let control = authority
        .load(target.cell_id())
        .await?
        .ok_or(Error::Config("imported Cell control disappeared"))?;
    if control.value().state != ControlState::Idle || control.value().owner.is_some() {
        return Err(Error::Config("imported Cell did not release to idle"));
    }
    let complete = evidence::CompleteEvidence::new(
        operation,
        repository,
        target.cell_id(),
        source_digest,
        expected.clone(),
        control.value(),
    )?;
    evidence::publish_complete(layout, target.cell_id(), operation, &complete).await?;
    complete.encode()
}

async fn verify_handle(handle: &CellHandle) -> Result<SemanticSummary> {
    let bytes = handle
        .query(64, 1024, |connection| {
            let summary = semantic_summary(connection).map_err(|error| match error {
                Error::Cell(error) => error,
                _ => crab_cell_runtime::Error::Command("repository semantic verification failed"),
            })?;
            serde_json::to_vec(&summary).map_err(crab_cell_runtime::Error::from)
        })
        .await?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn copy_staged(
    target: &rusqlite::Transaction<'_>,
    source_path: &Path,
) -> crab_cell_runtime::Result<()> {
    let source =
        Connection::open_with_flags(source_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let issue_sequence: i64 = source.query_row(
        "SELECT last FROM repository_sequences WHERE kind = 'issue'",
        [],
        |row| row.get(0),
    )?;
    target.execute(
        "UPDATE repository_sequences SET last = ?1 WHERE kind = 'issue'",
        [issue_sequence],
    )?;
    copy_issue_submissions(&source, target)?;
    copy_issues(&source, target)?;
    copy_comment_sequences(&source, target)?;
    copy_comment_submissions(&source, target)?;
    copy_comments(&source, target)?;
    let summary = semantic_summary(&source).map_err(|error| match error {
        Error::Cell(error) => error,
        _ => crab_cell_runtime::Error::Command("legacy semantic summary failed"),
    })?;
    target.execute(
        "UPDATE repository_identity SET app_revision = ?1 WHERE singleton = 1",
        [i64::try_from(summary.app_revision)
            .map_err(|_| crab_cell_runtime::Error::Command("import revision exceeds SQLite"))?],
    )?;
    let foreign_key_error = target
        .query_row("PRAGMA foreign_key_check", [], |_| Ok(()))
        .optional()?;
    if foreign_key_error.is_some() {
        return Err(crab_cell_runtime::Error::Command(
            "imported repository failed foreign_key_check",
        ));
    }
    Ok(())
}

fn copy_issue_submissions(
    source: &Connection,
    target: &rusqlite::Transaction<'_>,
) -> crab_cell_runtime::Result<()> {
    copy_rows(
        source,
        target,
        "SELECT request_id, payload_digest, issue_number, author_name, created_at_ms FROM repository_issue_submissions ORDER BY request_id",
        "INSERT INTO repository_issue_submissions(request_id, payload_digest, issue_number, author_name, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
    )
}

fn copy_issues(
    source: &Connection,
    target: &rusqlite::Transaction<'_>,
) -> crab_cell_runtime::Result<()> {
    copy_rows(
        source,
        target,
        "SELECT number, author_issuer, author_subject, author_name, title, body, state, label_ids, assignee_subjects, version, created_at_ms, updated_at_ms FROM repository_issues ORDER BY number",
        "INSERT INTO repository_issues(number, author_issuer, author_subject, author_name, title, body, state, label_ids, assignee_subjects, version, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )
}

fn copy_comment_sequences(
    source: &Connection,
    target: &rusqlite::Transaction<'_>,
) -> crab_cell_runtime::Result<()> {
    copy_rows(
        source,
        target,
        "SELECT issue_number, last FROM repository_comment_sequences ORDER BY issue_number",
        "INSERT INTO repository_comment_sequences(issue_number, last) VALUES (?1, ?2)",
    )
}

fn copy_comment_submissions(
    source: &Connection,
    target: &rusqlite::Transaction<'_>,
) -> crab_cell_runtime::Result<()> {
    copy_rows(
        source,
        target,
        "SELECT issue_number, request_id, payload_digest, comment_number, author_name, created_at_ms FROM repository_comment_submissions ORDER BY issue_number, request_id",
        "INSERT INTO repository_comment_submissions(issue_number, request_id, payload_digest, comment_number, author_name, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
}

fn copy_comments(
    source: &Connection,
    target: &rusqlite::Transaction<'_>,
) -> crab_cell_runtime::Result<()> {
    copy_rows(
        source,
        target,
        "SELECT issue_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms FROM repository_issue_comments ORDER BY issue_number, number",
        "INSERT INTO repository_issue_comments(issue_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
}

fn copy_rows(
    source: &Connection,
    target: &rusqlite::Transaction<'_>,
    select: &str,
    insert: &str,
) -> crab_cell_runtime::Result<()> {
    let mut statement = source.prepare(select)?;
    let columns = statement.column_count();
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let values = (0..columns)
            .map(|column| row.get::<_, rusqlite::types::Value>(column))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        target.execute(insert, rusqlite::params_from_iter(values))?;
    }
    Ok(())
}

pub(super) fn semantic_summary(connection: &Connection) -> Result<SemanticSummary> {
    let mut hasher = Hasher::new();
    hasher.update(b"crab.repository.issue-import.semantic.v1\0");
    for query in [
        "SELECT app_revision FROM repository_identity WHERE singleton = 1",
        "SELECT kind, last FROM repository_sequences ORDER BY kind",
        "SELECT request_id, payload_digest, issue_number, author_name, created_at_ms FROM repository_issue_submissions ORDER BY request_id",
        "SELECT number, author_issuer, author_subject, author_name, title, body, state, label_ids, assignee_subjects, version, created_at_ms, updated_at_ms FROM repository_issues ORDER BY number",
        "SELECT issue_number, last FROM repository_comment_sequences ORDER BY issue_number",
        "SELECT issue_number, request_id, payload_digest, comment_number, author_name, created_at_ms FROM repository_comment_submissions ORDER BY issue_number, request_id",
        "SELECT issue_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms FROM repository_issue_comments ORDER BY issue_number, number",
    ] {
        hash_query(connection, &mut hasher, query)?;
    }
    let issues = count(connection, "repository_issues")?;
    let issue_submissions = count(connection, "repository_issue_submissions")?;
    let comments = count(connection, "repository_issue_comments")?;
    let comment_submissions = count(connection, "repository_comment_submissions")?;
    let app_revision = sum_versions(connection, "repository_issues")?
        .checked_add(sum_versions(connection, "repository_issue_comments")?)
        .filter(|value| *value <= crate::app_storage::MAX_NUMBER)
        .ok_or(Error::Config(
            "imported application revision exceeds its limit",
        ))?;
    let persisted_revision: i64 = connection
        .query_row(
            "SELECT app_revision FROM repository_identity WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(sqlite_error)?;
    if u64::try_from(persisted_revision) != Ok(app_revision) {
        return Err(Error::Config(
            "imported application revision differs from row versions",
        ));
    }
    Ok(SemanticSummary {
        digest: hasher.finalize().to_hex().to_string(),
        issues,
        issue_submissions,
        comments,
        comment_submissions,
        app_revision,
    })
}

fn hash_query(connection: &Connection, hasher: &mut Hasher, sql: &str) -> Result<()> {
    let mut statement = connection.prepare(sql).map_err(sqlite_error)?;
    let columns = statement.column_count();
    let mut rows = statement.query([]).map_err(sqlite_error)?;
    while let Some(row) = rows.next().map_err(sqlite_error)? {
        hasher.update(&[0xff]);
        for column in 0..columns {
            hash_value(hasher, row, column)?;
        }
    }
    Ok(())
}

fn hash_value(hasher: &mut Hasher, row: &Row<'_>, column: usize) -> Result<()> {
    match row.get_ref(column).map_err(sqlite_error)? {
        ValueRef::Null => {
            hasher.update(&[0]);
        }
        ValueRef::Integer(value) => {
            hasher.update(&[1]);
            hasher.update(&value.to_be_bytes());
        }
        ValueRef::Real(_) => {
            return Err(Error::Config(
                "repository semantic state contains a real value",
            ));
        }
        ValueRef::Text(value) => {
            hasher.update(&[2]);
            hash_bytes(hasher, value)?;
        }
        ValueRef::Blob(value) => {
            hasher.update(&[3]);
            hash_bytes(hasher, value)?;
        }
    }
    Ok(())
}

fn hash_bytes(hasher: &mut Hasher, value: &[u8]) -> Result<()> {
    let len = u64::try_from(value.len())
        .map_err(|_| Error::Config("repository semantic value exceeds its limit"))?;
    hasher.update(&len.to_be_bytes());
    hasher.update(value);
    Ok(())
}

fn count(connection: &Connection, table: &str) -> Result<u64> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let value: i64 = connection
        .query_row(&sql, [], |row| row.get(0))
        .map_err(sqlite_error)?;
    u64::try_from(value).map_err(|_| Error::Config("repository count is negative"))
}

fn sum_versions(connection: &Connection, table: &str) -> Result<u64> {
    let sql = format!("SELECT version FROM {table}");
    let mut statement = connection.prepare(&sql).map_err(sqlite_error)?;
    let mut rows = statement.query([]).map_err(sqlite_error)?;
    let mut sum = 0_u64;
    while let Some(row) = rows.next().map_err(sqlite_error)? {
        let value: i64 = row.get(0).map_err(sqlite_error)?;
        sum = sum
            .checked_add(
                u64::try_from(value)
                    .map_err(|_| Error::Config("repository version is negative"))?,
            )
            .ok_or(Error::Config("repository version sum overflow"))?;
    }
    Ok(sum)
}

pub(super) fn sqlite_error(error: rusqlite::Error) -> Error {
    Error::Cell(crab_cell_runtime::Error::Sqlite(error))
}
