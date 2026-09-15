use std::path::{Path as FilePath, PathBuf};

use crab_cell_runtime::BoundedEncoder;
use crab_storage::{Store, map_object_store_error};
use futures_util::StreamExt as _;
use object_store::{ObjectMeta, path::Path};
use rusqlite::{Connection, OptionalExtension as _, params};
use tokio::sync::mpsc;

mod decode;

use decode::{SourceKind, StageRecord, classify, decode_record};

use super::{SemanticSummary, semantic_summary, sqlite_error};
use crate::cells::repository::{CommentRecord, IssueRecord, RepositoryAuthor};

const MAX_DOCUMENT_BYTES: u64 = 256 * 1024;
const MAX_SOURCE_OBJECTS: u64 = 2_000_000;
const MAX_SOURCE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MIN_WORKING_BYTES: u64 = 256 * 1024 * 1024;
const WRITER_QUEUE: usize = 16;

const STAGING_SCHEMA: &str = r#"
PRAGMA journal_mode = OFF;
PRAGMA synchronous = OFF;
PRAGMA temp_store = FILE;
CREATE TABLE source_objects (
    path TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    etag TEXT,
    version TEXT,
    digest BLOB NOT NULL CHECK (length(digest) = 32),
    kind TEXT NOT NULL,
    verified INTEGER NOT NULL DEFAULT 0 CHECK (verified IN (0, 1))
) STRICT, WITHOUT ROWID;
CREATE TABLE repository_identity (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    app_revision INTEGER NOT NULL
) STRICT;
CREATE TABLE repository_sequences (
    kind TEXT PRIMARY KEY,
    last INTEGER NOT NULL
) STRICT;
CREATE TABLE repository_issue_submissions (
    request_id BLOB PRIMARY KEY,
    payload_digest BLOB NOT NULL,
    issue_number INTEGER NOT NULL UNIQUE,
    author_issuer TEXT NOT NULL,
    author_subject TEXT NOT NULL,
    author_name TEXT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
CREATE TABLE repository_issues (
    number INTEGER PRIMARY KEY,
    source_request_id BLOB NOT NULL UNIQUE,
    author_issuer TEXT NOT NULL,
    author_subject TEXT NOT NULL,
    author_name TEXT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    state INTEGER NOT NULL,
    label_ids BLOB NOT NULL,
    assignee_subjects BLOB NOT NULL,
    version INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;
CREATE TABLE repository_comment_sequences (
    issue_number INTEGER PRIMARY KEY,
    last INTEGER NOT NULL
) STRICT;
CREATE TABLE repository_comment_submissions (
    issue_number INTEGER NOT NULL,
    request_id BLOB NOT NULL,
    payload_digest BLOB NOT NULL,
    comment_number INTEGER NOT NULL,
    author_issuer TEXT NOT NULL,
    author_subject TEXT NOT NULL,
    author_name TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (issue_number, request_id),
    UNIQUE (issue_number, comment_number)
) STRICT, WITHOUT ROWID;
CREATE TABLE repository_issue_comments (
    issue_number INTEGER NOT NULL,
    number INTEGER NOT NULL,
    source_request_id BLOB NOT NULL,
    author_issuer TEXT NOT NULL,
    author_subject TEXT NOT NULL,
    author_name TEXT NOT NULL,
    body TEXT NOT NULL,
    version INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (issue_number, number),
    UNIQUE (issue_number, source_request_id)
) STRICT;
"#;

pub(super) struct StagedSource {
    _directory: tempfile::TempDir,
    database: PathBuf,
    pub(super) objects: u64,
    pub(super) bytes: u64,
    pub(super) semantic: SemanticSummary,
}

impl StagedSource {
    pub(super) fn database(&self) -> &FilePath {
        &self.database
    }
}

struct StageItem {
    path: String,
    size: u64,
    etag: Option<String>,
    version: Option<String>,
    digest: [u8; 32],
    record: StageRecord,
}

struct VerifyItem {
    path: String,
    size: u64,
    etag: Option<String>,
    version: Option<String>,
}

pub(super) async fn capture(
    store: &Store,
    prefix: &Path,
    data_dir: &FilePath,
) -> crate::Result<StagedSource> {
    let (expected_objects, expected_bytes) = preflight(store, prefix).await?;
    admit_disk(data_dir, expected_bytes)?;
    let directory = tempfile::Builder::new()
        .prefix("crab-repository-import-")
        .tempdir_in(data_dir)?;
    let database = directory.path().join("source.sqlite");
    let (sender, receiver) = mpsc::channel(WRITER_QUEUE);
    let writer_database = database.clone();
    let writer = tokio::task::spawn_blocking(move || stage(writer_database, receiver));
    let feed = feed_source(store, prefix, sender).await;
    let written = writer.await?;
    feed?;
    let (objects, bytes) = written?;
    if objects != expected_objects || bytes != expected_bytes {
        return Err(crate::Error::Config(
            "legacy issue source changed during inventory",
        ));
    }
    verify_source(store, prefix, database.clone(), expected_objects).await?;
    let summary_database = database.clone();
    let semantic = tokio::task::spawn_blocking(move || {
        let connection = Connection::open_with_flags(
            summary_database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(sqlite_error)?;
        validate_staged(&connection)?;
        semantic_summary(&connection)
    })
    .await??;
    Ok(StagedSource {
        _directory: directory,
        database,
        objects,
        bytes,
        semantic,
    })
}

async fn preflight(store: &Store, prefix: &Path) -> crate::Result<(u64, u64)> {
    let mut stream = store.inner().list(Some(prefix));
    let mut objects = 0_u64;
    let mut bytes = 0_u64;
    while let Some(item) = stream.next().await {
        let meta = item.map_err(|error| map_object_store_error(error, prefix.as_ref()))?;
        validate_meta(prefix, &meta)?;
        objects = objects
            .checked_add(1)
            .filter(|value| *value <= MAX_SOURCE_OBJECTS)
            .ok_or(crate::Error::Config(
                "legacy issue source exceeds the object limit",
            ))?;
        bytes = bytes
            .checked_add(meta.size)
            .filter(|value| *value <= MAX_SOURCE_BYTES)
            .ok_or(crate::Error::Config(
                "legacy issue source exceeds the byte limit",
            ))?;
    }
    Ok((objects, bytes))
}

fn admit_disk(data_dir: &FilePath, source_bytes: u64) -> crate::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let required = source_bytes
        .checked_mul(3)
        .and_then(|bytes| bytes.checked_add(MIN_WORKING_BYTES))
        .ok_or(crate::Error::Config(
            "legacy issue import disk admission overflow",
        ))?;
    if fs4::available_space(data_dir)? < required {
        return Err(crate::Error::Config(
            "Cell data directory lacks space for legacy issue import",
        ));
    }
    Ok(())
}

async fn feed_source(
    store: &Store,
    prefix: &Path,
    sender: mpsc::Sender<StageItem>,
) -> crate::Result<()> {
    let mut stream = store.inner().list(Some(prefix));
    while let Some(item) = stream.next().await {
        let meta = item.map_err(|error| map_object_store_error(error, prefix.as_ref()))?;
        let kind = validate_meta(prefix, &meta)?;
        let (body, version) = store
            .get_with_etag_bounded(&meta.location, MAX_DOCUMENT_BYTES)
            .await?;
        if body.len() as u64 != meta.size
            || version.e_tag != meta.e_tag
            || version.version != meta.version
        {
            return Err(crate::Error::Config(
                "legacy issue source changed while reading an object",
            ));
        }
        let relative = relative_path(prefix, &meta.location)?;
        let record = decode_record(kind, relative, &body)?;
        let item = StageItem {
            path: format!("app/v1/issues/{relative}"),
            size: meta.size,
            etag: meta.e_tag,
            version: meta.version,
            digest: *blake3::hash(&body).as_bytes(),
            record,
        };
        sender
            .send(item)
            .await
            .map_err(|_| crate::Error::Config("legacy issue staging writer stopped"))?;
    }
    Ok(())
}

async fn verify_source(
    store: &Store,
    prefix: &Path,
    database: PathBuf,
    expected_objects: u64,
) -> crate::Result<()> {
    let (sender, receiver) = mpsc::channel(WRITER_QUEUE);
    let writer = tokio::task::spawn_blocking(move || verify(database, receiver, expected_objects));
    let mut stream = store.inner().list(Some(prefix));
    let feed: crate::Result<()> = async {
        while let Some(item) = stream.next().await {
            let meta = item.map_err(|error| map_object_store_error(error, prefix.as_ref()))?;
            validate_meta(prefix, &meta)?;
            sender
                .send(VerifyItem {
                    path: format!("app/v1/issues/{}", relative_path(prefix, &meta.location)?),
                    size: meta.size,
                    etag: meta.e_tag,
                    version: meta.version,
                })
                .await
                .map_err(|_| crate::Error::Config("legacy issue verifier stopped"))?;
        }
        Ok(())
    }
    .await;
    drop(sender);
    let verified = writer.await?;
    feed?;
    verified
}

fn stage(database: PathBuf, mut receiver: mpsc::Receiver<StageItem>) -> crate::Result<(u64, u64)> {
    let mut connection = Connection::open(database).map_err(sqlite_error)?;
    connection
        .execute_batch(STAGING_SCHEMA)
        .map_err(sqlite_error)?;
    let transaction = connection.transaction().map_err(sqlite_error)?;
    let mut objects = 0_u64;
    let mut bytes = 0_u64;
    while let Some(item) = receiver.blocking_recv() {
        insert_stage_item(&transaction, item, &mut bytes)?;
        objects = objects
            .checked_add(1)
            .ok_or(crate::Error::Config("legacy issue object count overflow"))?;
    }
    transaction
        .execute(
            "INSERT OR IGNORE INTO repository_sequences(kind, last) VALUES ('issue', 0)",
            [],
        )
        .map_err(sqlite_error)?;
    let app_revision = super::sum_versions(&transaction, "repository_issues")?
        .checked_add(super::sum_versions(
            &transaction,
            "repository_issue_comments",
        )?)
        .filter(|value| *value <= crate::app_storage::MAX_NUMBER)
        .ok_or(crate::Error::Config(
            "imported application revision exceeds its limit",
        ))?;
    transaction
        .execute(
            "INSERT INTO repository_identity(singleton, app_revision) VALUES (1, ?1)",
            [to_i64(app_revision)?],
        )
        .map_err(sqlite_error)?;
    transaction.commit().map_err(sqlite_error)?;
    Ok((objects, bytes))
}

fn insert_stage_item(
    transaction: &rusqlite::Transaction<'_>,
    item: StageItem,
    bytes: &mut u64,
) -> crate::Result<()> {
    let kind = match &item.record {
        StageRecord::IssueSequence(_) => SourceKind::IssueSequence,
        StageRecord::IssueReservation { .. } => SourceKind::IssueReservation,
        StageRecord::Issue { .. } => SourceKind::Issue,
        StageRecord::CommentSequence { issue, .. } => SourceKind::CommentSequence { issue: *issue },
        StageRecord::CommentReservation { record, .. } => SourceKind::CommentReservation {
            issue: record.issue,
        },
        StageRecord::Comment { record, .. } => SourceKind::Comment {
            issue: record.issue,
            number: record.number,
        },
    };
    transaction
        .execute(
            "INSERT INTO source_objects(path, size, etag, version, digest, kind) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                item.path,
                to_i64(item.size)?,
                item.etag,
                item.version,
                item.digest.as_slice(),
                kind.name()
            ],
        )
        .map_err(sqlite_error)?;
    match item.record {
        StageRecord::IssueSequence(last) => {
            transaction
                .execute(
                    "INSERT INTO repository_sequences(kind, last) VALUES ('issue', ?1)",
                    [to_i64(last)?],
                )
                .map_err(sqlite_error)?;
        }
        StageRecord::IssueReservation {
            request,
            record,
            digest,
        } => insert_issue_submission(transaction, request, &record, digest)?,
        StageRecord::Issue { request, record } => {
            transaction
                .execute(
                    "INSERT INTO repository_issues(number, source_request_id, author_issuer, author_subject, author_name, title, body, state, label_ids, assignee_subjects, version, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    params![
                        to_i64(record.number)?, request.as_slice(), record.author.issuer,
                        record.author.subject, record.author.name, record.title, record.body,
                        i64::from(record.state), encode_label_ids_v1(&record.label_ids)?,
                        encode_assignees_v1(&record.assignee_subjects)?, to_i64(record.version)?,
                        to_i64(record.created_at_ms)?, to_i64(record.updated_at_ms)?
                    ],
                )
                .map_err(sqlite_error)?;
        }
        StageRecord::CommentSequence { issue, last } => {
            transaction
                .execute(
                    "INSERT INTO repository_comment_sequences(issue_number, last) VALUES (?1, ?2)",
                    params![to_i64(issue)?, to_i64(last)?],
                )
                .map_err(sqlite_error)?;
        }
        StageRecord::CommentReservation {
            request,
            record,
            digest,
        } => insert_comment_submission(transaction, request, &record, digest)?,
        StageRecord::Comment { request, record } => {
            transaction
                .execute(
                    "INSERT INTO repository_issue_comments(issue_number, number, source_request_id, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        to_i64(record.issue)?, to_i64(record.number)?, request.as_slice(),
                        record.author.issuer, record.author.subject, record.author.name,
                        record.body, to_i64(record.version)?, to_i64(record.created_at_ms)?,
                        to_i64(record.updated_at_ms)?
                    ],
                )
                .map_err(sqlite_error)?;
        }
    }
    *bytes = bytes
        .checked_add(item.size)
        .ok_or(crate::Error::Config("legacy issue byte count overflow"))?;
    Ok(())
}

fn insert_issue_submission(
    transaction: &rusqlite::Transaction<'_>,
    request: [u8; 16],
    record: &IssueRecord,
    digest: [u8; 32],
) -> crate::Result<()> {
    transaction
        .execute(
            "INSERT INTO repository_issue_submissions(request_id, payload_digest, issue_number, author_issuer, author_subject, author_name, title, body, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                request.as_slice(), digest.as_slice(), to_i64(record.number)?,
                record.author.issuer, record.author.subject, record.author.name,
                record.title, record.body, to_i64(record.created_at_ms)?
            ],
        )
        .map_err(sqlite_error)?;
    Ok(())
}

fn insert_comment_submission(
    transaction: &rusqlite::Transaction<'_>,
    request: [u8; 16],
    record: &CommentRecord,
    digest: [u8; 32],
) -> crate::Result<()> {
    transaction
        .execute(
            "INSERT INTO repository_comment_submissions(issue_number, request_id, payload_digest, comment_number, author_issuer, author_subject, author_name, body, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                to_i64(record.issue)?, request.as_slice(), digest.as_slice(),
                to_i64(record.number)?, record.author.issuer, record.author.subject,
                record.author.name, record.body, to_i64(record.created_at_ms)?
            ],
        )
        .map_err(sqlite_error)?;
    Ok(())
}

fn verify(
    database: PathBuf,
    mut receiver: mpsc::Receiver<VerifyItem>,
    expected_objects: u64,
) -> crate::Result<()> {
    let mut connection = Connection::open(database).map_err(sqlite_error)?;
    let transaction = connection.transaction().map_err(sqlite_error)?;
    let mut observed = 0_u64;
    while let Some(item) = receiver.blocking_recv() {
        let changed = transaction
            .execute(
                "UPDATE source_objects SET verified = 1 WHERE path = ?1 AND size = ?2 AND etag IS ?3 AND version IS ?4 AND verified = 0",
                params![item.path, to_i64(item.size)?, item.etag, item.version],
            )
            .map_err(sqlite_error)?;
        if changed != 1 {
            return Err(crate::Error::Config(
                "legacy issue source changed before verification",
            ));
        }
        observed = observed
            .checked_add(1)
            .ok_or(crate::Error::Config("legacy issue object count overflow"))?;
    }
    let missing: i64 = transaction
        .query_row(
            "SELECT COUNT(*) FROM source_objects WHERE verified = 0",
            [],
            |row| row.get(0),
        )
        .map_err(sqlite_error)?;
    if observed != expected_objects || missing != 0 {
        return Err(crate::Error::Config(
            "legacy issue source changed before verification",
        ));
    }
    transaction.commit().map_err(sqlite_error)?;
    Ok(())
}

fn validate_staged(connection: &Connection) -> crate::Result<()> {
    let invalid = connection
        .query_row(
            "SELECT 1 WHERE
                (SELECT last FROM repository_sequences WHERE kind = 'issue') < COALESCE((SELECT MAX(number) FROM repository_issues), 0)
                OR (SELECT last FROM repository_sequences WHERE kind = 'issue') < COALESCE((SELECT MAX(issue_number) FROM repository_issue_submissions), 0)
                OR EXISTS (
                    SELECT 1 FROM repository_issues i
                    LEFT JOIN repository_issue_submissions s ON s.request_id = i.source_request_id
                    WHERE s.request_id IS NULL OR s.issue_number != i.number
                        OR s.author_issuer != i.author_issuer OR s.author_subject != i.author_subject
                        OR s.created_at_ms != i.created_at_ms
                )
                OR EXISTS (
                    SELECT 1 FROM repository_comment_sequences s
                    LEFT JOIN repository_issues i ON i.number = s.issue_number
                    WHERE i.number IS NULL OR s.last < 1
                        OR s.last < COALESCE((SELECT MAX(c.number) FROM repository_issue_comments c WHERE c.issue_number = s.issue_number), 0)
                        OR s.last < COALESCE((SELECT MAX(c.comment_number) FROM repository_comment_submissions c WHERE c.issue_number = s.issue_number), 0)
                )
                OR EXISTS (
                    SELECT 1 FROM repository_issue_comments c
                    LEFT JOIN repository_comment_sequences q ON q.issue_number = c.issue_number
                    LEFT JOIN repository_comment_submissions s ON s.issue_number = c.issue_number AND s.request_id = c.source_request_id
                    WHERE q.issue_number IS NULL OR s.request_id IS NULL OR s.comment_number != c.number
                        OR s.author_issuer != c.author_issuer OR s.author_subject != c.author_subject
                        OR s.created_at_ms != c.created_at_ms
                )
                OR EXISTS (
                    SELECT 1 FROM repository_comment_submissions s
                    LEFT JOIN repository_issues i ON i.number = s.issue_number
                    LEFT JOIN repository_comment_sequences q ON q.issue_number = s.issue_number
                    WHERE i.number IS NULL OR q.issue_number IS NULL
                )
            LIMIT 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(sqlite_error)?;
    if invalid.is_some() {
        return Err(crate::Error::Config(
            "legacy issue source violates sequence or reservation invariants",
        ));
    }
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(sqlite_error)?;
    if integrity != "ok" {
        return Err(crate::Error::Config(
            "legacy issue staging database failed integrity_check",
        ));
    }
    Ok(())
}

fn validate_meta(prefix: &Path, meta: &ObjectMeta) -> crate::Result<SourceKind> {
    if meta.size == 0 || meta.size > MAX_DOCUMENT_BYTES {
        return Err(crate::Error::Config(
            "legacy issue document exceeds its size contract",
        ));
    }
    if meta.e_tag.is_none() && meta.version.is_none() {
        return Err(crate::Error::Config(
            "legacy issue import requires versioned source objects",
        ));
    }
    classify(relative_path(prefix, &meta.location)?)
}

fn relative_path<'a>(prefix: &Path, location: &'a Path) -> crate::Result<&'a str> {
    location
        .as_ref()
        .strip_prefix(prefix.as_ref())
        .and_then(|value| value.strip_prefix('/'))
        .filter(|value| !value.is_empty() && value.len() <= 1024)
        .ok_or(crate::Error::Config(
            "legacy issue object escaped its source prefix",
        ))
}

pub(super) fn validate_issue_v1(record: &IssueRecord) -> crate::Result<()> {
    validate_number_v1(record.number)?;
    validate_author_v1(&record.author)?;
    if record.title.trim() != record.title
        || record.title.is_empty()
        || record.title.chars().count() > 256
        || record.title.chars().any(char::is_control)
        || record.body.len() > 64 * 1024
        || record.body.contains('\0')
        || record.label_ids.len() > 20
        || record.label_ids.contains(&0)
        || record.label_ids.windows(2).any(|pair| pair[0] >= pair[1])
        || record.assignee_subjects.len() > 10
        || record.assignee_subjects.iter().any(|subject| {
            subject.is_empty()
                || subject.chars().count() > 512
                || subject.chars().any(char::is_control)
        })
        || record
            .assignee_subjects
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        || record.state > 1
        || record.updated_at_ms < record.created_at_ms
    {
        return Err(crate::Error::Config(
            "legacy issue row violates repository schema v1",
        ));
    }
    validate_number_v1(record.version)
}

pub(super) fn validate_comment_v1(record: &CommentRecord) -> crate::Result<()> {
    validate_number_v1(record.issue)?;
    validate_number_v1(record.number)?;
    validate_author_v1(&record.author)?;
    if record.body.trim().is_empty()
        || record.body.len() > 64 * 1024
        || record.body.contains('\0')
        || record.updated_at_ms < record.created_at_ms
    {
        return Err(crate::Error::Config(
            "legacy comment row violates repository schema v1",
        ));
    }
    validate_number_v1(record.version)
}

fn validate_number_v1(value: u64) -> crate::Result<()> {
    if value == 0 || value > crate::app_storage::MAX_NUMBER {
        return Err(crate::Error::Config(
            "legacy repository number violates schema v1",
        ));
    }
    Ok(())
}

fn validate_author_v1(author: &RepositoryAuthor) -> crate::Result<()> {
    if [
        (&author.issuer, 512),
        (&author.subject, 512),
        (&author.name, 160),
    ]
    .into_iter()
    .any(|(value, maximum)| {
        value.is_empty() || value.chars().count() > maximum || value.chars().any(char::is_control)
    }) {
        return Err(crate::Error::Config(
            "legacy repository author violates schema v1",
        ));
    }
    Ok(())
}

fn encode_label_ids_v1(labels: &[u64]) -> crate::Result<Vec<u8>> {
    let mut encoder = BoundedEncoder::new(16 * 1024)
        .map_err(|_| crate::Error::Config("legacy issue labels exceed schema v1"))?;
    encoder
        .write_count(labels.len())
        .map_err(|_| crate::Error::Config("legacy issue labels exceed schema v1"))?;
    for label in labels {
        encoder
            .write_u64(*label)
            .map_err(|_| crate::Error::Config("legacy issue labels exceed schema v1"))?;
    }
    Ok(encoder.finish())
}

fn encode_assignees_v1(assignees: &[String]) -> crate::Result<Vec<u8>> {
    let mut encoder = BoundedEncoder::new(16 * 1024)
        .map_err(|_| crate::Error::Config("legacy issue assignees exceed schema v1"))?;
    encoder
        .write_count(assignees.len())
        .map_err(|_| crate::Error::Config("legacy issue assignees exceed schema v1"))?;
    for assignee in assignees {
        encoder
            .write_text(assignee)
            .map_err(|_| crate::Error::Config("legacy issue assignees exceed schema v1"))?;
    }
    Ok(encoder.finish())
}

fn to_i64(value: u64) -> crate::Result<i64> {
    i64::try_from(value).map_err(|_| crate::Error::Config("legacy issue value exceeds SQLite"))
}
