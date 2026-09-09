#![cfg(feature = "storage")]

use std::{collections::BTreeMap, sync::Arc};

use crab_metadata::manifests::Manifest;
use crab_metadata::plan_receipt::{
    PlanCommit, PlanReceipt, read_plan_receipt, resolve_plan_receipt,
};
use crab_metadata::ref_journal::{
    RefJournalEdit, RefJournalTransaction, commit_ref_transaction, commit_ref_transaction_for_plan,
    read_ref_head,
};
use crab_storage::{StorageError, Store, StoreLayout};
use object_store::memory::InMemory;

fn edit(ref_name: &str, old_oid: Option<String>, value: char) -> RefJournalEdit {
    RefJournalEdit {
        ref_name: ref_name.to_owned(),
        old_oid,
        new_oid: Some(value.to_string().repeat(40)),
        peeled_oid: None,
        lock_holder: None,
        visibility_evidence_hash: Some(value.to_string().repeat(64)),
    }
}

fn receipt_transaction(receipt: &PlanReceipt) -> &str {
    match &receipt.commit {
        PlanCommit::RefJournal { transaction_id, .. } => transaction_id,
        PlanCommit::Manifest { .. } => panic!("expected ref-journal receipt"),
    }
}

async fn transaction(
    store: &Store,
    router: &StoreLayout<Store>,
    ref_name: &str,
    value: char,
) -> (
    RefJournalTransaction,
    Vec<crab_metadata::ref_journal::RefJournalHeadSnapshot>,
) {
    let head = read_ref_head(store, router, ref_name).await.unwrap();
    let old_oid = head.visible_transaction.as_ref().map(|_| {
        if value == 'b' {
            "a".repeat(40)
        } else {
            "b".repeat(40)
        }
    });
    let parents = BTreeMap::from([(ref_name.to_owned(), head.visible_transaction.clone())]);
    let transaction = RefJournalTransaction::new(
        parents,
        vec![edit(ref_name, old_oid, value)],
        None,
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    (transaction, vec![head])
}

#[tokio::test]
async fn missing_receipt_recovers_after_journal_compaction() {
    const ROOT: &str = "CRAB_TEST_RECEIPT_REOPEN_ROOT";
    let child_root = std::env::var_os(ROOT);
    let directory = child_root.is_none().then(|| tempfile::tempdir().unwrap());
    let root = child_root
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| directory.as_ref().unwrap().path().to_owned());
    let persisted = Store::new(Arc::new(
        object_store::local::LocalFileSystem::new_with_prefix(&root).unwrap(),
    ));
    let store = if directory.is_some() {
        Store::new(Arc::new(InMemory::new()))
    } else {
        persisted.clone()
    };
    let router = StoreLayout::new(store.clone(), "receipt/compaction".to_owned());
    if directory.is_none() {
        let receipt = read_plan_receipt(&store, &router, &"d".repeat(64))
            .await
            .unwrap()
            .unwrap();
        println!("{}", serde_json::to_string(&receipt).unwrap());
        return;
    }
    crab_metadata::layout_descriptor::ensure_canonical_layout(&store, &router)
        .await
        .unwrap();
    let base = Manifest::default_for_repo("refs/heads/main");
    crab_metadata::manifest_store::create_manifest(&store, &router, &base)
        .await
        .unwrap();
    let plan_id = "d".repeat(64);
    let (mut first, heads) = transaction(&store, &router, "refs/heads/main", 'a').await;
    first.edits[0].visibility_evidence_hash = None;
    let committed =
        commit_ref_transaction_for_plan(&store, &router, &first, &heads, &plan_id, || false)
            .await
            .unwrap();
    store
        .delete(&router.ref_journal_plan_receipt_path(&plan_id))
        .await
        .unwrap();

    crab_metadata::manifest_store::compact_ref_journal(
        &store,
        &router,
        "2026-09-03T00:00:00Z".to_owned(),
        Some("test".to_owned()),
        "receipt-compaction".to_owned(),
    )
    .await
    .unwrap()
    .unwrap();
    let (mut successor, heads) = transaction(&store, &router, "refs/heads/main", 'b').await;
    successor.edits[0].visibility_evidence_hash = None;
    commit_ref_transaction(&store, &router, &successor, &heads, || false)
        .await
        .unwrap();
    crab_metadata::manifest_store::compact_ref_journal(
        &store,
        &router,
        "2026-09-03T00:00:01Z".to_owned(),
        Some("test".to_owned()),
        "receipt-successor-compaction".to_owned(),
    )
    .await
    .unwrap()
    .unwrap();
    for object in store
        .list_prefix(&object_store::path::Path::from(router.repo_prefix()))
        .await
        .unwrap()
    {
        let (bytes, _) = store.get_with_etag(&object.location).await.unwrap();
        persisted.put_exact(&object.location, bytes).await.unwrap();
    }
    // Re-execute only this test in a new process: no in-memory session,
    // cache or original marker can supply the historical attribution.
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .args([
            "--exact",
            "missing_receipt_recovers_after_journal_compaction",
            "--nocapture",
        ])
        .env(ROOT, &root);
    let output = tokio::task::spawn_blocking(move || child.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recovered = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .find_map(|line| serde_json::from_str::<PlanReceipt>(line).ok())
        .unwrap();
    assert_eq!(receipt_transaction(&recovered), committed.transaction_id);
    assert!(matches!(
        persisted
            .get_with_etag(&router.ref_journal_plan_receipt_path(&plan_id))
            .await,
        Err(StorageError::NotFound { .. })
    ));
    let observed = read_plan_receipt(&store, &router, &plan_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt_transaction(&observed), committed.transaction_id);
    assert!(matches!(
        store
            .get_with_etag(&router.ref_journal_plan_receipt_path(&plan_id))
            .await,
        Err(StorageError::NotFound { .. })
    ));

    let receipt = resolve_plan_receipt(&store, &router, &plan_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(receipt_transaction(&receipt), committed.transaction_id);
}
