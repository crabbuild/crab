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
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::wait_for_state;
use crate::test_support::{git, http::Harness};

// Serialized by XorbBuilder and PushShardSession. Each xorb contains one
// chunk, whose Xet hash is also the xorb hash.
const SOURCE_FILE: XetFixture = XetFixture {
    content: b"large file from the source repository",
    file_key: "6f1c6b951cec7df96d5054fd476fdbfdfdebdf26d37446fa6245b12337774dc8",
    xorb_hash: "9a4f7e8b495e9d7252e8d666e6003e0a8fa891b1949cee98b04bca6b273dd80f",
    shard_hash: "67b20323925b2671f9c5289b93b71f6a9916d9c743b2ea5c902f139d3d3dcfc2",
    xorb: include_bytes!("fixtures/xet/source.xorb"),
    shard: include_bytes!("fixtures/xet/source.shard"),
};

const UNRELATED_FILE: XetFixture = XetFixture {
    content: b"catalog entry outside the forked history",
    file_key: "b7c02a7ed1d3671b1cbe76d8ae41826597bbbea37d1f0af7dfdd769f0b28f054",
    xorb_hash: "59f28017a27f33d6597b227d7ac0710f8e9620304303da138eeb046b096129cf",
    shard_hash: "0c960bdadeaa9fe97df6b35dfe32b36e02bb70ff146e00abb3fb1948aed783a3",
    xorb: include_bytes!("fixtures/xet/unrelated.xorb"),
    shard: include_bytes!("fixtures/xet/unrelated.shard"),
};

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

    let file_hash = blake3::hash(SOURCE_FILE.content);
    let pointer_text = format!(
        "version https://crab.build/spec/v1\nfile-hash {file_hash}\nsize {}\n",
        SOURCE_FILE.content.len()
    );
    let mut catalog = PointerCatalog::new();
    SOURCE_FILE.stage(layout, &mut catalog).await;
    UNRELATED_FILE.stage(layout, &mut catalog).await;
    let xorb_path = layout.xorb_path(SOURCE_FILE.xorb_hash);
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
    layout
        .store()
        .put(&xorb_path, Bytes::from_static(SOURCE_FILE.xorb))
        .await
        .unwrap();
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
    let selected = selected
        .files(
            &fork.store,
            &[
                SOURCE_FILE.file_key.to_owned(),
                UNRELATED_FILE.file_key.to_owned(),
            ],
        )
        .await
        .unwrap();
    assert_eq!(selected.files().len(), 1);
    let crab_git::PointerKind::Crab(pointer) = crab_git::classify(pointer_text.as_bytes()) else {
        panic!("fixture must contain a native Crab pointer");
    };
    crab_read::verify_catalog_file_recipe(
        &fork.layout,
        &pointer,
        &selected.files()[SOURCE_FILE.file_key],
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    h.close().await;
}

struct XetFixture {
    content: &'static [u8],
    file_key: &'static str,
    xorb_hash: &'static str,
    shard_hash: &'static str,
    xorb: &'static [u8],
    shard: &'static [u8],
}

impl XetFixture {
    async fn stage(&self, layout: &StoreLayout<Store>, catalog: &mut PointerCatalog) {
        let size = u32::try_from(self.content.len()).unwrap();
        catalog
            .insert_xorb(
                self.xorb_hash.to_owned(),
                XorbCatalogEntry::new(
                    self.xorb.len() as u64,
                    blake3::hash(self.xorb).to_hex().to_string(),
                    vec![XorbChunkEntry::new(self.xorb_hash, size)],
                ),
            )
            .unwrap();
        catalog
            .insert_shard(
                self.shard_hash.to_owned(),
                ShardCatalogEntry::new(self.shard.len() as u64, vec![self.xorb_hash.to_owned()]),
            )
            .unwrap();
        catalog
            .insert_file(
                self.file_key.to_owned(),
                FileCatalogEntry::new(u64::from(size), self.shard_hash),
            )
            .unwrap();
        layout
            .store()
            .put(
                &layout.xorb_path(self.xorb_hash),
                Bytes::from_static(self.xorb),
            )
            .await
            .unwrap();
        layout
            .store()
            .put(
                &layout.shard_path(self.shard_hash),
                Bytes::from_static(self.shard),
            )
            .await
            .unwrap();
    }
}
