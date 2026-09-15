use std::sync::Arc;

use bytes::Bytes;
use crab_cell_runtime::{
    ApplicationId, ApplicationIdentity, CellAuthority, CellReplica, CellRuntime, CellTarget,
    ControlState, ReplicaLimits, SessionId, SqlWorkerPool, TenantId,
};
use crab_storage::{CellStorageLayout, Store, StoreLayout};
use object_store::{memory::InMemory, path::Path};
use serde_json::json;
use uuid::Uuid;

use super::*;

const ISSUE_REQUEST: &str = "00000000-0000-0000-0000-000000000001";
const RESERVED_ISSUE_REQUEST: &str = "00000000-0000-0000-0000-000000000008";
const COMMENT_REQUEST: &str = "00000000-0000-0000-0000-000000000002";
const RESERVED_COMMENT_REQUEST: &str = "00000000-0000-0000-0000-000000000004";
const LABEL_REQUEST: &str = "00000000-0000-0000-0000-000000000005";
const DELETED_LABEL_REQUEST: &str = "00000000-0000-0000-0000-000000000006";
const RESERVED_LABEL_REQUEST: &str = "00000000-0000-0000-0000-000000000007";
const STATUS_REQUEST: &str = "00000000-0000-0000-0000-000000000009";
const RESERVED_STATUS_REQUEST: &str = "00000000-0000-0000-0000-000000000010";
const STATUS_OID: &str = "0123456789abcdef0123456789abcdef01234567";

#[tokio::test]
async fn repository_import_rejects_a_label_without_its_reservation() {
    let layout = StoreLayout::new(
        Store::new(Arc::new(InMemory::new())),
        "invalid-label-import".to_owned(),
    );
    put(
        &layout,
        "app/v1/labels/catalog.json",
        json!({
            "labels": [{
                "number": 1,
                "name": "Bug",
                "color": "d73a4a",
                "description": null,
                "version": 1,
                "created_at": 1000,
                "updated_at": 1000
            }],
            "deleted": []
        }),
    )
    .await;
    let files = tempfile::TempDir::new().unwrap();
    assert!(matches!(
        source::capture(
            layout.store(),
            &layout.repo_path("app/v1/issues"),
            &layout.repo_path("app/v1/labels"),
            &layout.repo_path("app/v1/statuses"),
            files.path(),
        )
        .await,
        Err(Error::Config(
            "legacy label catalog has no matching reservation"
        ))
    ));
}

#[tokio::test]
async fn repository_import_publishes_verifies_releases_and_replays_completion() {
    let store = Store::new(Arc::new(InMemory::new()));
    let identity = ApplicationIdentity::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([2; 16]),
    );
    let layout = CellStorageLayout::new(
        store.clone(),
        Path::from("issue-import"),
        *identity.application().as_bytes(),
    );
    let registry = Arc::new(super::super::compiled_registry().unwrap());
    super::super::bootstrap_release_at(
        &layout,
        identity,
        &registry,
        &format!("sha256:{}", "a".repeat(64)),
    )
    .await
    .unwrap();
    let repository = Uuid::from_bytes([3; 16]);
    let operation = Uuid::from_bytes([4; 16]);
    let repository_layout = StoreLayout::new(store, "issue-import/repository".to_owned());
    write_source(&repository_layout).await;
    let files = tempfile::TempDir::new().unwrap();
    let source = source::capture(
        repository_layout.store(),
        &repository_layout.repo_path("app/v1/issues"),
        &repository_layout.repo_path("app/v1/labels"),
        &repository_layout.repo_path("app/v1/statuses"),
        files.path(),
    )
    .await
    .unwrap();
    assert_eq!(source.semantic.issues, 1);
    assert_eq!(source.semantic.issue_submissions, 2);
    assert_eq!(source.semantic.comments, 1);
    assert_eq!(source.semantic.comment_submissions, 2);
    assert_eq!(source.semantic.labels, 1);
    assert_eq!(source.semantic.deleted_labels, 1);
    assert_eq!(source.semantic.label_submissions, 3);
    assert_eq!(source.semantic.statuses, 1);
    assert_eq!(source.semantic.status_submissions, 2);
    assert_eq!(source.semantic.app_revision, 9);

    let target = CellTarget::new(
        identity.tenant(),
        identity.application(),
        REPOSITORY_NAMESPACE,
        repository.as_bytes(),
    )
    .unwrap();
    let source_evidence =
        evidence::publish_source(&layout, target.cell_id(), operation, repository, &source)
            .await
            .unwrap();
    let first_session = SessionId::from_bytes([5; 16]);
    let first_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        IMPORT_MAILBOX_BYTES,
        first_session,
    )
    .unwrap();
    let first = import_staged(
        &layout,
        identity,
        Arc::clone(&registry),
        &first_runtime,
        &target,
        repository,
        operation,
        &source,
        &source_evidence,
        first_session,
        "https://import-1.internal:8081".into(),
        files.path(),
    )
    .await
    .unwrap();
    first_runtime.shutdown().await.unwrap();
    let idle = CellAuthority::new(layout.clone())
        .load(target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(idle.value().state, ControlState::Idle);
    assert!(idle.value().owner.is_none());
    let replica = CellReplica::new(
        layout.clone(),
        *target.cell_id().as_bytes(),
        *idle.value().incarnation.as_bytes(),
        ReplicaLimits::default(),
    )
    .unwrap();
    let root = idle.value().ltx_root().unwrap();
    let restored = files.path().join("restored-import.sqlite");
    replica
        .open_root(&root)
        .await
        .unwrap()
        .restore(&restored)
        .await
        .unwrap();
    let connection =
        rusqlite::Connection::open_with_flags(restored, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let active: (String, i64) = connection
        .query_row(
            "SELECT name, version FROM repository_labels WHERE number = 1 AND deleted_version IS NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(active, ("Defect".to_owned(), 2));
    let deleted: (i64, i64) = connection
        .query_row(
            "SELECT version, deleted_version FROM repository_labels WHERE number = 2",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(deleted, (1, 1));
    let incomplete: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM repository_label_submissions s LEFT JOIN repository_labels l ON l.number = s.label_number WHERE s.label_number = 4 AND l.number IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(incomplete, 1);
    let revision: i64 = connection
        .query_row(
            "SELECT app_revision FROM repository_identity WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(revision, 9);
    let visible_status: (String, i64) = connection
        .query_row(
            "SELECT context, state FROM repository_commit_statuses WHERE oid = ?1 AND visible = 1",
            [STATUS_OID],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(visible_status, ("CI/Test".to_owned(), 3));
    let incomplete_status: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM repository_commit_statuses WHERE oid = ?1 AND visible = 0",
            [STATUS_OID],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(incomplete_status, 1);
    drop(connection);

    layout
        .store()
        .delete(&layout.migration_path(
            target.cell_id().as_bytes(),
            &operation.into_bytes(),
            "complete.json",
        ))
        .await
        .unwrap();
    let recovery_session = SessionId::from_bytes([6; 16]);
    let recovery_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        IMPORT_MAILBOX_BYTES,
        recovery_session,
    )
    .unwrap();
    let recovered = import_staged(
        &layout,
        identity,
        Arc::clone(&registry),
        &recovery_runtime,
        &target,
        repository,
        operation,
        &source,
        &source_evidence,
        recovery_session,
        "https://import-recovery.internal:8081".into(),
        files.path(),
    )
    .await
    .unwrap();
    recovery_runtime.shutdown().await.unwrap();
    assert_eq!(first, recovered);

    let complete = evidence::load_complete(&layout, target.cell_id(), operation, repository)
        .await
        .unwrap()
        .unwrap();
    let second_session = SessionId::from_bytes([7; 16]);
    let second_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        IMPORT_MAILBOX_BYTES,
        second_session,
    )
    .unwrap();
    let second = resume_complete(
        &layout,
        identity,
        registry,
        &second_runtime,
        &target,
        complete,
        second_session,
        "https://import-2.internal:8081".into(),
        files.path(),
    )
    .await
    .unwrap();
    second_runtime.shutdown().await.unwrap();
    assert_eq!(first, second);
}

async fn write_source(layout: &StoreLayout<Store>) {
    let author = json!({
        "issuer": "https://issuer.example",
        "subject": "user-1",
        "name": "Crab User"
    });
    put(layout, "app/v1/issues/sequence.json", json!({"last": 8})).await;
    put(
        layout,
        &format!("app/v1/issues/requests/{ISSUE_REQUEST}.json"),
        issue(
            1,
            ISSUE_REQUEST,
            author.clone(),
            "Original",
            "Original body",
            0,
            1,
        ),
    )
    .await;
    put(
        layout,
        &format!("app/v1/issues/requests/{RESERVED_ISSUE_REQUEST}.json"),
        issue(
            8,
            RESERVED_ISSUE_REQUEST,
            author.clone(),
            "Reserved",
            "Reserved body",
            0,
            1,
        ),
    )
    .await;
    let mut visible = issue(
        1,
        ISSUE_REQUEST,
        author.clone(),
        "Edited",
        "Edited body",
        1,
        2,
    );
    visible["label_ids"] = json!([1]);
    visible["assignee_subjects"] = json!(["user-2"]);
    visible["updated_at"] = json!(2000);
    put(layout, "app/v1/issues/0000000000000001/issue.json", visible).await;
    put(
        layout,
        "app/v1/issues/0000000000000001/comments/sequence.json",
        json!({"last": 4}),
    )
    .await;
    put(
        layout,
        &format!("app/v1/issues/0000000000000001/comments/requests/{COMMENT_REQUEST}.json"),
        comment(1, COMMENT_REQUEST, author.clone(), "Original comment", 1),
    )
    .await;
    put(
        layout,
        &format!(
            "app/v1/issues/0000000000000001/comments/requests/{RESERVED_COMMENT_REQUEST}.json"
        ),
        comment(
            4,
            RESERVED_COMMENT_REQUEST,
            author.clone(),
            "Reserved comment",
            1,
        ),
    )
    .await;
    let mut visible_comment = comment(1, COMMENT_REQUEST, author.clone(), "Edited comment", 2);
    visible_comment["updated_at"] = json!(2000);
    put(
        layout,
        "app/v1/issues/0000000000000001/comments/0000000000000001.json",
        visible_comment,
    )
    .await;
    put(layout, "app/v1/labels/sequence.json", json!({"last": 4})).await;
    put(
        layout,
        &format!("app/v1/labels/requests/{LABEL_REQUEST}.json"),
        label_reservation(1, LABEL_REQUEST, author.clone(), "Bug", "d73a4a"),
    )
    .await;
    put(
        layout,
        &format!("app/v1/labels/requests/{DELETED_LABEL_REQUEST}.json"),
        label_reservation(2, DELETED_LABEL_REQUEST, author.clone(), "Docs", "0075ca"),
    )
    .await;
    put(
        layout,
        &format!("app/v1/labels/requests/{RESERVED_LABEL_REQUEST}.json"),
        label_reservation(
            4,
            RESERVED_LABEL_REQUEST,
            author.clone(),
            "Future",
            "ffffff",
        ),
    )
    .await;
    put(
        layout,
        "app/v1/labels/catalog.json",
        json!({
            "labels": [{
                "number": 1,
                "name": "Defect",
                "color": "d73a4a",
                "description": "Imported and edited",
                "version": 2,
                "created_at": 1000,
                "updated_at": 2000
            }],
            "deleted": [{"number": 2, "version": 1}]
        }),
    )
    .await;
    put(
        layout,
        &format!("app/v1/statuses/{STATUS_OID}/sequence.json"),
        json!({"last": 2}),
    )
    .await;
    let reserved_status = status(
        1,
        RESERVED_STATUS_REQUEST,
        author.clone(),
        "ci/lint",
        "pending",
    );
    put(
        layout,
        &format!("app/v1/statuses/{STATUS_OID}/requests/{RESERVED_STATUS_REQUEST}.json"),
        reserved_status,
    )
    .await;
    let visible_status = status(2, STATUS_REQUEST, author, "CI/Test", "success");
    put(
        layout,
        &format!("app/v1/statuses/{STATUS_OID}/requests/{STATUS_REQUEST}.json"),
        visible_status.clone(),
    )
    .await;
    put(
        layout,
        &format!("app/v1/statuses/{STATUS_OID}/summary.json"),
        json!({"oid": STATUS_OID, "statuses": [visible_status]}),
    )
    .await;
}

fn status(
    number: u64,
    request: &str,
    author: serde_json::Value,
    context: &str,
    state: &str,
) -> serde_json::Value {
    json!({
        "number": number,
        "request_id": request,
        "author": author,
        "oid": STATUS_OID,
        "context": context,
        "state": state,
        "description": null,
        "target_url": null,
        "created_at": 1000
    })
}

fn label_reservation(
    number: u64,
    request: &str,
    author: serde_json::Value,
    name: &str,
    color: &str,
) -> serde_json::Value {
    json!({
        "request_id": request,
        "author": author,
        "label": {
            "number": number,
            "name": name,
            "color": color,
            "description": null,
            "version": 1,
            "created_at": 1000,
            "updated_at": 1000
        }
    })
}

fn issue(
    number: u64,
    request: &str,
    author: serde_json::Value,
    title: &str,
    body: &str,
    state: u8,
    version: u64,
) -> serde_json::Value {
    json!({
        "number": number,
        "request_id": request,
        "author": author,
        "title": title,
        "body": body,
        "state": if state == 0 { "open" } else { "closed" },
        "label_ids": [],
        "assignee_subjects": [],
        "version": version,
        "created_at": 1000,
        "updated_at": 1000
    })
}

fn comment(
    number: u64,
    request: &str,
    author: serde_json::Value,
    body: &str,
    version: u64,
) -> serde_json::Value {
    json!({
        "number": number,
        "request_id": request,
        "author": author,
        "body": body,
        "version": version,
        "created_at": 1000,
        "updated_at": 1000
    })
}

async fn put(layout: &StoreLayout<Store>, relative: &str, data: serde_json::Value) {
    let body = serde_json::to_vec(&json!({"schema_version": 1, "data": data})).unwrap();
    layout
        .store()
        .put_overwrite(&layout.repo_path(relative), Bytes::from(body))
        .await
        .unwrap();
}
