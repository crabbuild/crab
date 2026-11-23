use bytes::Bytes;
use crab_metadata::{
    capsule_protocol::{
        Capsule, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind, CapsuleTransaction,
        CapsuleVisibilityDelta, FileCatalogEntry, PointerCatalog, ShardCatalogEntry,
        XorbCatalogEntry, XorbChunkEntry,
    },
    git_visibility::GitVisibilityEdit,
};
use crab_storage::{Store, StoreLayout};
use crab_xet::{
    hash::{MerkleHash, compute_data_hash},
    shard::{PushShardSession, file_info_from_placements, xorb_info_from_placements},
    xorb::{
        builder::{RunId, XorbBuilder},
        format::Chunk,
    },
};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::wait_for_state;
use crate::test_support::{git, http::Harness};

#[tokio::test(flavor = "multi_thread")]
async fn private_fork_publishes_only_verified_xet_dependencies_of_its_history() {
    let h = Harness::new_with_store_and_prefix(
        false,
        false,
        Store::new(Arc::new(object_store::memory::InMemory::new())),
        "fork-xet",
    )
    .await;
    h.initialize_directory().await;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, _, _) = crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    let org = format!("/api/organizations/{organization}");
    let upstream: Value = h
        .write(
            Method::POST,
            &format!("{org}/repositories/creation"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"name":"upstream","description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let owner = upstream["owner"].as_str().unwrap();
    let source = h
        .server
        .repositories
        .get(&(owner.to_owned(), "upstream".into()))
        .unwrap();
    let layout = &source.layout;

    let content = Bytes::from_static(b"large file from the source repository");
    let file_hash = blake3::hash(&content);
    let pointer_text = format!(
        "version https://crab.build/spec/v1\nfile-hash {file_hash}\nsize {}\n",
        content.len()
    );
    let mut catalog = PointerCatalog::new();
    let (xorb_path, xorb) = write_content(layout, &mut catalog, content).await;
    let unrelated = Bytes::from_static(b"catalog entry outside the forked history");
    let unrelated_hash = MerkleHash::from(*blake3::hash(&unrelated).as_bytes()).hex();
    write_content(layout, &mut catalog, unrelated).await;
    let history = git::history_with_blob(pointer_text.as_bytes());
    let head = history.oids[0].clone();
    let base = crab_metadata::capsule_protocol::load_root(layout)
        .await
        .unwrap();
    let transaction = CapsuleTransaction::new(
        base.record().digest(),
        vec![CapsuleRefEdit::new(
            "refs/heads/main",
            None,
            Some(head.clone()),
            None,
        )],
    )
    .unwrap();
    let visible = CapsuleVisibilityDelta::new(BTreeMap::from([(
        "refs/heads/main".to_owned(),
        GitVisibilityEdit::from_replacement_objects(None, head.clone(), history.object_ids),
    )]))
    .unwrap();
    let capsule = Capsule::build(
        &transaction,
        vec![history.pack],
        vec![
            CapsuleSection::new(
                CapsuleSectionKind::CatalogDelta,
                catalog.encode_delta().unwrap(),
            ),
            CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                visible.encode().unwrap(),
            ),
        ],
    )
    .unwrap();
    crab_write::capsule_protocol::publish(layout, base, &transaction, capsule)
        .await
        .unwrap();
    layout.store().delete(&xorb_path).await.unwrap();

    let request = Uuid::now_v7();
    let session = h.json("/api/session", &alice).await;
    let response = h
        .http
        .post(format!("{}/api/repos/{owner}/upstream/forks", h.origin))
        .header("cookie", &alice)
        .header("origin", &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header("x-crab-repository-id", source.id.to_string())
        .json(&json!({"request_id":request,"name":"fork"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let status = format!("{org}/repositories/imports/git/{request}");
    wait_for_state(&h, &alice, &status, "failed").await;
    let (reserved, _) = h.server.catalog().unwrap().load().await.unwrap();
    assert!(reserved.repositories.iter().any(|record| {
        record.forked_from == Some(source.id)
            && record.application == crate::catalog::RepositoryApplicationState::GitImportPending
    }));
    assert!(
        h.server
            .repositories
            .get(&(owner.to_owned(), "fork".into()))
            .is_none()
    );
    layout.store().put(&xorb_path, xorb).await.unwrap();
    h.write(Method::POST, &format!("{status}/resume"), &alice, json!({}))
        .await
        .error_for_status()
        .unwrap();
    wait_for_state(&h, &alice, &status, "succeeded").await;

    let fork = h
        .server
        .repositories
        .get(&(owner.to_owned(), "fork".into()))
        .unwrap();
    let view = fork.open_view().await.unwrap();
    assert_eq!(view.refs()["refs/heads/main"], head);
    let selected = view
        .pointer_catalog_lookup(
            &fork.layout,
            crab_metadata::capsule_protocol::MAX_CATALOG_BYTES,
        )
        .unwrap();
    let file_key = MerkleHash::from(*file_hash.as_bytes()).hex();
    let selected = selected
        .files(&fork.store, &[file_key.clone(), unrelated_hash])
        .await
        .unwrap();
    assert_eq!(selected.files().len(), 1);
    let crab_git::PointerKind::Crab(pointer) = crab_git::classify(pointer_text.as_bytes()) else {
        panic!("fixture must contain a native Crab pointer");
    };
    crab_read::verify_catalog_file_recipe(
        &fork.layout,
        &pointer,
        &selected.files()[&file_key],
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    h.close().await;
}

async fn write_content(
    layout: &StoreLayout<Store>,
    catalog: &mut PointerCatalog,
    data: Bytes,
) -> (object_store::path::Path, Bytes) {
    let file = MerkleHash::from(*blake3::hash(&data).as_bytes());
    let size = data.len() as u64;
    let chunk = compute_data_hash(&data);
    let mut builder = XorbBuilder::new();
    builder
        .push(&Chunk { hash: chunk, data }, RunId(0))
        .unwrap();
    let xorb = builder.finalize().unwrap().pop().unwrap();
    let placements = xorb
        .placements
        .iter()
        .map(|p| (p.chunk_hash, p.clone()))
        .collect();
    let recipe = file_info_from_placements(file, &[chunk], &placements).unwrap();
    let xorb_info = Arc::new(xorb_info_from_placements(xorb.hash, &xorb.placements).unwrap());
    let mut shards = PushShardSession::new();
    shards.add_file_bundle(recipe, &[xorb_info]).unwrap();
    let (shard, shard_hash) = shards.finalize().unwrap().pop().unwrap();
    catalog
        .insert_xorb(
            xorb.hash.hex(),
            XorbCatalogEntry::new(
                xorb.bytes.len() as u64,
                blake3::hash(&xorb.bytes).to_hex().to_string(),
                vec![XorbChunkEntry::new(chunk.hex(), size as u32)],
            ),
        )
        .unwrap();
    catalog
        .insert_shard(
            shard_hash.hex(),
            ShardCatalogEntry::new(shard.len() as u64, vec![xorb.hash.hex()]),
        )
        .unwrap();
    catalog
        .insert_file(file.hex(), FileCatalogEntry::new(size, shard_hash.hex()))
        .unwrap();
    let xorb_path = layout.xorb_path(&xorb.hash);
    layout
        .store()
        .put(&xorb_path, xorb.bytes.clone())
        .await
        .unwrap();
    layout
        .store()
        .put(&layout.shard_path(&shard_hash), shard.into())
        .await
        .unwrap();
    (xorb_path, xorb.bytes)
}
