use std::sync::Arc;

use bytes::Bytes;
use crab_cell_runtime::{
    ApplicationId, ApplicationIdentity, CellAuthority, CellRuntime, CellTarget, ControlState,
    SessionId, SqlWorkerPool, TenantId,
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

#[tokio::test]
async fn issue_import_refuses_unmigrated_label_state() {
    let layout = StoreLayout::new(
        Store::new(Arc::new(InMemory::new())),
        "label-import-guard".to_owned(),
    );
    put(
        &layout,
        "app/v1/labels/catalog.json",
        json!({"labels":[],"deleted":[]}),
    )
    .await;
    assert!(matches!(
        require_no_legacy_label_source(&layout).await,
        Err(Error::Config(
            "legacy repository labels require a label-aware import before Cell activation"
        ))
    ));
}

#[tokio::test]
async fn issue_import_publishes_verifies_releases_and_replays_completion() {
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
        files.path(),
    )
    .await
    .unwrap();
    assert_eq!(source.semantic.issues, 1);
    assert_eq!(source.semantic.issue_submissions, 2);
    assert_eq!(source.semantic.comments, 1);
    assert_eq!(source.semantic.comment_submissions, 2);
    assert_eq!(source.semantic.app_revision, 4);

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
    visible["label_ids"] = json!([3, 9]);
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
    let mut visible_comment = comment(1, COMMENT_REQUEST, author, "Edited comment", 2);
    visible_comment["updated_at"] = json!(2000);
    put(
        layout,
        "app/v1/issues/0000000000000001/comments/0000000000000001.json",
        visible_comment,
    )
    .await;
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
