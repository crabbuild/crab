use std::{collections::HashMap, sync::Arc};

use bytes::Bytes;
use crab_cell_runtime::cell::catalog::CatalogRole;
use crab_cell_runtime::cell::catalog::{CatalogEntry, CellCatalog};
use crab_cell_runtime::control::Owner;
use crab_cell_runtime::control::authority::CellAuthority;
use crab_cell_runtime::fleet::telemetry::CatalogReadKind;
use crab_cell_runtime::identity::IncarnationId;
use crab_cell_runtime::identity::{
    ApplicationId, CellTarget, Digest, NamespaceId, SessionId, TenantId,
};
use crab_ltx::CellStorageLayout;
use crab_storage::Store;
use object_store::{memory::InMemory, path::Path};

fn fixture() -> (CellStorageLayout, CellCatalog, CellTarget) {
    let tenant = TenantId::from_bytes([1; 16]);
    let application = ApplicationId::from_bytes([2; 16]);
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("runtime"),
        *application.as_bytes(),
    );
    let catalog = CellCatalog::new(layout.clone(), tenant);
    let target = CellTarget::new(
        tenant,
        application,
        NamespaceId::from_bytes([3; 16]),
        b"repository-1",
    )
    .unwrap();
    (layout, catalog, target)
}

fn entry(target: &CellTarget, role: CatalogRole, byte: u8) -> CatalogEntry {
    CatalogEntry::new(target, role, Digest::from_bytes([byte; 32]), 1).unwrap()
}

#[tokio::test]
async fn catalog_provision_is_idempotent_and_precedes_control() {
    let (layout, catalog, target) = fixture();
    let expected = entry(&target, CatalogRole::Repository, 4);
    let first = catalog.provision(expected.clone()).await.unwrap();
    let second = catalog.provision(expected.clone()).await.unwrap();
    assert_eq!(first.revision(), 1);
    assert_eq!(second.revision(), 1);
    assert_eq!(first.entry(), &expected);
    assert_eq!(
        catalog
            .lookup(target.cell_id())
            .await
            .unwrap()
            .unwrap()
            .entry(),
        &expected
    );
    let authority = CellAuthority::new(layout);
    assert!(authority.load(target.cell_id()).await.unwrap().is_none());
    let owner = Owner {
        session: SessionId::from_bytes([10; 16]),
        endpoint: "https://node.internal:8081".into(),
    };
    let created = authority
        .create_initial(&first, IncarnationId::from_bytes([11; 16]), owner.clone())
        .await
        .unwrap();
    let adopted = authority
        .create_initial(&first, IncarnationId::from_bytes([11; 16]), owner)
        .await
        .unwrap();
    assert_eq!(created.value(), adopted.value());
    assert!(matches!(
        authority
            .create_initial(
                &first,
                IncarnationId::from_bytes([11; 16]),
                Owner {
                    session: SessionId::from_bytes([12; 16]),
                    endpoint: "https://other.internal:8081".into(),
                },
            )
            .await,
        Err(crab_cell_runtime::Error::CellAlreadyActive)
    ));
}

#[tokio::test]
async fn concurrent_catalog_writers_merge_entries_on_one_shard() {
    let (_, catalog, first, second) = same_shard_targets();
    let first_entry = entry(&first, CatalogRole::Repository, 5);
    let second_entry = entry(&second, CatalogRole::Repository, 6);
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let first_writer = {
        let catalog = catalog.clone();
        let barrier = barrier.clone();
        let entry = first_entry.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            catalog.provision(entry).await
        })
    };
    let second_writer = {
        let catalog = catalog.clone();
        let entry = second_entry.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            catalog.provision(entry).await
        })
    };
    first_writer.await.unwrap().unwrap();
    second_writer.await.unwrap().unwrap();

    assert_eq!(
        catalog
            .lookup(first.cell_id())
            .await
            .unwrap()
            .unwrap()
            .entry(),
        &first_entry
    );
    assert_eq!(
        catalog
            .lookup(second.cell_id())
            .await
            .unwrap()
            .unwrap()
            .entry(),
        &second_entry
    );
}

#[tokio::test]
async fn catalog_rejects_conflicting_bootstrap_contract_for_one_cell() {
    let (_, catalog, target) = fixture();
    catalog
        .provision(entry(&target, CatalogRole::Repository, 7))
        .await
        .unwrap();
    assert!(matches!(
        catalog.provision(entry(&target, CatalogRole::Sql, 8)).await,
        Err(crab_cell_runtime::Error::CatalogCollision)
    ));
}

#[tokio::test]
async fn catalog_page_digest_is_checked_before_entry_use() {
    let (layout, catalog, target) = fixture();
    catalog
        .provision(entry(&target, CatalogRole::Repository, 9))
        .await
        .unwrap();
    let (head, _) = layout
        .store()
        .get_with_etag_bounded(
            &layout.catalog_head_path(target.cell_id().as_bytes()[0]),
            32 * 1024,
        )
        .await
        .unwrap();
    let head: serde_json::Value = serde_json::from_slice(&head).unwrap();
    let digest = decode_digest(head["pages"][0]["digest"].as_str().unwrap());
    layout
        .store()
        .put_overwrite(
            &layout.catalog_object_path(&digest),
            Bytes::from_static(b"{}"),
        )
        .await
        .unwrap();
    assert!(matches!(
        catalog.lookup(target.cell_id()).await,
        Err(crab_cell_runtime::Error::Catalog("page digest mismatch"))
    ));
}

#[tokio::test]
async fn catalog_lookup_reports_one_head_and_one_page_read() {
    let tenant = TenantId::from_bytes([30; 16]);
    let application = ApplicationId::from_bytes([31; 16]);
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("runtime"),
        *application.as_bytes(),
    );
    let recorder = Arc::new(CatalogReadRecorder::default());
    let catalog = CellCatalog::with_telemetry(
        layout.clone(),
        tenant,
        crab_cell_runtime::fleet::telemetry::CellTelemetryHandle::from_sink(recorder.clone()),
    );
    let target = CellTarget::new(
        tenant,
        application,
        NamespaceId::from_bytes([32; 16]),
        b"repository-metrics",
    )
    .unwrap();
    let expected = entry(&target, CatalogRole::Repository, 12);
    catalog.provision(expected.clone()).await.unwrap();
    // Provisioning reads catalog metadata, so the routing claim starts here.
    recorder.reads.lock().unwrap().clear();
    assert_eq!(
        catalog
            .lookup(target.cell_id())
            .await
            .unwrap()
            .unwrap()
            .entry(),
        &expected
    );
    assert_eq!(
        recorder.reads.lock().unwrap().as_slice(),
        [(CatalogReadKind::Head, true), (CatalogReadKind::Page, true)]
    );
}

#[derive(Default)]
struct CatalogReadRecorder {
    reads: std::sync::Mutex<Vec<(CatalogReadKind, bool)>>,
    control_reads: std::sync::atomic::AtomicUsize,
}

impl crab_cell_runtime::fleet::telemetry::CellTelemetry for CatalogReadRecorder {
    fn catalog_read(&self, kind: CatalogReadKind, _elapsed: std::time::Duration, succeeded: bool) {
        self.reads.lock().unwrap().push((kind, succeeded));
    }

    fn control_read(&self, _elapsed: std::time::Duration, _succeeded: bool) {
        self.control_reads
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }
}

#[tokio::test]
async fn due_scan_reads_one_control_record_per_cell() {
    let (layout, tenant, _, entries) = provisioned_shard(40).await;
    let shard = entries[0].cell().as_bytes()[0];
    let recorder = Arc::new(CatalogReadRecorder::default());
    let sink =
        crab_cell_runtime::fleet::telemetry::CellTelemetryHandle::from_sink(recorder.clone());
    let catalog = CellCatalog::with_telemetry(layout.clone(), tenant, sink.clone());
    let authority = CellAuthority::with_telemetry(layout.clone(), sink);
    let mut scan =
        crab_cell_runtime::fleet::scheduler::DueCellScan::new(&catalog, authority, shard)
            .await
            .unwrap();
    let mut due = 0;
    while let Some(batch) = scan.next_batch_bounded(1, 8).await.unwrap() {
        due += batch.len();
    }
    assert_eq!(due, 0, "this shard arms no deadline");
    // The scan pays one control read for every Cell in the shard whether or
    // not it is due, which is the discovery cost hint publication must remove.
    assert_eq!(
        recorder
            .control_reads
            .load(std::sync::atomic::Ordering::Acquire),
        entries.len()
    );
    // Pinning the head and reading its pages is the catalog half of that pass.
    assert_eq!(recorder.reads.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn catalog_lookup_reads_only_the_page_that_can_hold_the_entry() {
    let (layout, _, catalog, entries) = provisioned_shard(257).await;
    let shard = entries[0].cell().as_bytes()[0];
    let head = head_json(&layout, shard).await;
    let first_page = decode_digest(head["pages"][0]["digest"].as_str().unwrap());
    let last = entries.last().unwrap();
    assert!(
        entries.first().unwrap().cell().as_bytes() < last.cell().as_bytes(),
        "the shard must hold two pages with a distinct tail entry"
    );
    // The earlier page cannot hold the tail entry, so a lookup that reads it
    // would fail on the missing object. One page read is the whole contract.
    layout
        .store()
        .delete(&layout.catalog_object_path(&first_page))
        .await
        .unwrap();
    assert_eq!(
        catalog.lookup(last.cell()).await.unwrap().unwrap().entry(),
        last
    );
    // The page that does hold the entry stays digest-verified.
    let tail_page = decode_digest(head["pages"][1]["digest"].as_str().unwrap());
    layout
        .store()
        .delete(&layout.catalog_object_path(&tail_page))
        .await
        .unwrap();
    assert!(catalog.lookup(last.cell()).await.is_err());
}

#[tokio::test]
async fn catalog_provision_replaces_only_the_affected_page() {
    let (layout, tenant, _, entries) = provisioned_shard(257).await;
    let shard = entries[0].cell().as_bytes()[0];
    let before = head_json(&layout, shard).await;
    let second_first = before["pages"][1]["first"].as_str().unwrap();
    let second_digest = before["pages"][1]["digest"].clone();
    let application = ApplicationId::from_bytes([21; 16]);
    let namespace = NamespaceId::from_bytes([22; 16]);
    let target = (0_u32..)
        .map(|partition| {
            CellTarget::new(tenant, application, namespace, &partition.to_be_bytes()).unwrap()
        })
        .find(|target| {
            target.cell_id().as_bytes()[0] == shard
                && hex(target.cell_id().as_bytes()).as_str() < second_first
                && entries.iter().all(|entry| entry.cell() != target.cell_id())
        })
        .unwrap();
    let recorder = Arc::new(CatalogReadRecorder::default());
    let catalog = CellCatalog::with_telemetry(
        layout.clone(),
        tenant,
        crab_cell_runtime::fleet::telemetry::CellTelemetryHandle::from_sink(recorder.clone()),
    );
    let expected = entry(&target, CatalogRole::Repository, 9);
    catalog.provision(expected.clone()).await.unwrap();
    assert_eq!(
        recorder.reads.lock().unwrap().as_slice(),
        [(CatalogReadKind::Head, true), (CatalogReadKind::Page, true)]
    );
    let after = head_json(&layout, shard).await;
    assert_eq!(after["pages"][1]["digest"], second_digest);
    assert_eq!(
        catalog
            .lookup(target.cell_id())
            .await
            .unwrap()
            .unwrap()
            .entry(),
        &expected
    );
    let mut scan = catalog.scan_shard(shard).await.unwrap();
    let mut cells = Vec::new();
    while let Some(page) = scan.next_page().await.unwrap() {
        cells.extend(page.entries().iter().map(|proof| proof.entry().cell()));
    }
    assert_eq!(cells.len(), 258);
    assert!(
        cells
            .windows(2)
            .all(|pair| pair[0].as_bytes() < pair[1].as_bytes())
    );
}

#[tokio::test]
async fn catalog_lookup_rejects_a_head_whose_locator_disagrees_with_its_page() {
    let (layout, _, catalog, entries) = provisioned_shard(257).await;
    let shard = entries[0].cell().as_bytes()[0];
    // The head body is canonical JSON, so tamper inside the encoded document:
    // the first page now claims to open at the second entry, which the page
    // does not have. A lookup that lands there must fail closed.
    let (body, _) = layout
        .store()
        .get_with_etag_bounded(&layout.catalog_head_path(shard), 64 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    let located = format!("\"first\":\"{}\"", hex(entries[0].cell().as_bytes()));
    let moved = format!("\"first\":\"{}\"", hex(entries[1].cell().as_bytes()));
    assert!(body.contains(&located), "head must locate its first page");
    let body = body.replacen(&located, &moved, 1);
    layout
        .store()
        .put_overwrite(
            &layout.catalog_head_path(shard),
            Bytes::from(body.into_bytes()),
        )
        .await
        .unwrap();
    match catalog.lookup(entries[2].cell()).await {
        Err(crab_cell_runtime::Error::Catalog("catalog page locator disagrees with its page")) => {}
        Ok(Some(proof)) => panic!(
            "locator disagreement reported the entry {}",
            hex(proof.entry().cell().as_bytes())
        ),
        Ok(None) => panic!("locator disagreement reported absence"),
        Err(error) => panic!("unexpected lookup error: {error:?}"),
    }
}

#[tokio::test]
async fn catalog_rejects_an_unordered_page_locator() {
    let (layout, _, catalog, entries) = provisioned_shard(257).await;
    let shard = entries[0].cell().as_bytes()[0];
    let mut head = head_json(&layout, shard).await;
    head["pages"][1]["first"] = head["pages"][0]["first"].clone();
    layout
        .store()
        .put_overwrite(
            &layout.catalog_head_path(shard),
            Bytes::from(serde_json::to_vec(&head).unwrap()),
        )
        .await
        .unwrap();
    assert!(matches!(
        catalog.lookup(entries[0].cell()).await,
        Err(crab_cell_runtime::Error::Catalog(
            "catalog page locator is not ordered"
        ))
    ));
}

async fn head_json(layout: &CellStorageLayout, shard: u8) -> serde_json::Value {
    let (head, _) = layout
        .store()
        .get_with_etag_bounded(&layout.catalog_head_path(shard), 64 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&head).unwrap()
}

/// Provisions one shard past a page boundary and returns its sorted entries.
async fn provisioned_shard(
    count: usize,
) -> (CellStorageLayout, TenantId, CellCatalog, Vec<CatalogEntry>) {
    let tenant = TenantId::from_bytes([20; 16]);
    let application = ApplicationId::from_bytes([21; 16]);
    let namespace = NamespaceId::from_bytes([22; 16]);
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("runtime"),
        *application.as_bytes(),
    );
    let catalog = CellCatalog::new(layout.clone(), tenant);
    let mut shards: HashMap<u8, Vec<CellTarget>> = HashMap::new();
    let mut targets = Vec::new();
    for partition in 0_u32.. {
        let target =
            CellTarget::new(tenant, application, namespace, &partition.to_be_bytes()).unwrap();
        let shard = target.cell_id().as_bytes()[0];
        let bucket = shards.entry(shard).or_default();
        bucket.push(target);
        if bucket.len() == count {
            targets = std::mem::take(bucket);
            break;
        }
    }
    let mut entries = targets
        .iter()
        .map(|target| entry(target, CatalogRole::Repository, 9))
        .collect::<Vec<_>>();
    entries.sort_unstable_by_key(|entry| *entry.cell().as_bytes());
    for entry in &entries {
        catalog.provision(entry.clone()).await.unwrap();
    }
    (layout, tenant, catalog, entries)
}

fn same_shard_targets() -> (CellStorageLayout, CellCatalog, CellTarget, CellTarget) {
    let tenant = TenantId::from_bytes([10; 16]);
    let application = ApplicationId::from_bytes([11; 16]);
    let namespace = NamespaceId::from_bytes([12; 16]);
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("runtime"),
        *application.as_bytes(),
    );
    let catalog = CellCatalog::new(layout.clone(), tenant);
    let mut targets = HashMap::new();
    for partition in 0_u32..=256 {
        let target =
            CellTarget::new(tenant, application, namespace, &partition.to_be_bytes()).unwrap();
        let shard = target.cell_id().as_bytes()[0];
        if let Some(first) = targets.insert(shard, target.clone()) {
            return (layout, catalog, first, target);
        }
    }
    panic!("257 targets must contain a first-byte collision");
}

fn decode_digest(value: &str) -> [u8; 32] {
    let mut digest = [0; 32];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        digest[index] = (nibble(pair[0]) << 4) | nibble(pair[1]);
    }
    digest
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => panic!("catalog digest must be lowercase hex"),
    }
}
