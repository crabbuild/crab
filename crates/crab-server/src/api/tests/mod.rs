mod lfs_content;
mod preview;

use super::*;
use super::{
    query::search_score,
    representation::{
        MAX_INLINE_TEXT_BYTES, asset_response, blob_response, content_json, encode_cursor,
        image_content_type,
    },
};
use axum::http::{HeaderValue, header};
use crab_remote_git::{BlobMetadata, ContentClassification, EntryMode};
use crab_remote_git::{EntryKind, HistoryTraversal, PageCursor};
use http_body_util::BodyExt;

fn blob(bytes: &[u8]) -> Blob {
    Blob {
        metadata: BlobMetadata {
            oid: gix_hash::ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap(),
            git_size: bytes.len() as u64,
            logical_size: Some(bytes.len() as u64),
            mode: EntryMode::Regular,
            kind: EntryKind::Blob,
            classification: ContentClassification::OrdinaryGit,
        },
        bytes: bytes.to_vec().into(),
    }
}

fn projection_source(
    path_state_hash: Option<&str>,
) -> crate::cells::repository::projection::SourceIdentity {
    crate::cells::repository::projection::SourceIdentity {
        source_token: "current".into(),
        manifest_generation: 2,
        manifest_etag: "etag".into(),
        journal_state_digest: "journal".into(),
        pack_index_hash: "pack".into(),
        git_validation_digest: "validation".into(),
        commit_graph_hash: None,
        path_state_hash: path_state_hash.map(str::to_owned),
        head_ref: b"refs/heads/main".to_vec(),
    }
}

#[test]
fn stale_projection_uses_canonical_attribution_when_path_state_is_published() {
    let source = projection_source(Some("path-state"));
    let state = crate::cells::repository::projection::ProjectionStateView {
        ready_epoch: Some(1),
        source_token: Some("previous".into()),
        generation: Some(1),
        collection_epoch: None,
    };

    assert_eq!(
        attribution_source(&source, &state, true),
        AttributionSource::Canonical
    );
}

#[test]
fn stale_projection_without_path_state_remains_indexing() {
    let source = projection_source(None);
    let state = crate::cells::repository::projection::ProjectionStateView {
        ready_epoch: None,
        source_token: None,
        generation: None,
        collection_epoch: None,
    };

    assert_eq!(
        attribution_source(&source, &state, false),
        AttributionSource::Indexing
    );
}

#[test]
fn matching_projection_is_preferred_over_canonical_attribution() {
    let source = projection_source(Some("path-state"));
    let state = crate::cells::repository::projection::ProjectionStateView {
        ready_epoch: Some(2),
        source_token: Some("current".into()),
        generation: Some(2),
        collection_epoch: None,
    };

    assert_eq!(
        attribution_source(&source, &state, true),
        AttributionSource::Projected("current")
    );
}

#[test]
fn newer_path_state_waits_when_the_request_pinned_the_previous_generation() {
    let source = projection_source(Some("path-state"));
    let state = crate::cells::repository::projection::ProjectionStateView {
        ready_epoch: Some(1),
        source_token: Some("previous".into()),
        generation: Some(1),
        collection_epoch: None,
    };

    assert_eq!(
        attribution_source(&source, &state, false),
        AttributionSource::Indexing
    );
}

#[test]
fn cursor_mac_rejects_tampering_and_other_server_keys() {
    // Version-one history payload: version, kind, SHA-1, traversal, skip.
    // Validate through the public decoder before exercising the HTTP envelope.
    let mut payload = vec![0; 31];
    payload[0] = 1;
    payload[1] = 2;
    payload[22] = HistoryTraversal::FirstParent as u8;
    let cursor = PageCursor::from_bytes(payload).unwrap();
    let repository = uuid::Uuid::from_u128(1);
    let signed = encode_cursor(&[7; 32], repository, cursor.clone());
    assert_eq!(
        decode_cursor(&[7; 32], repository, &signed).unwrap(),
        cursor
    );
    assert!(decode_cursor(&[8; 32], repository, &signed).is_err());
    assert!(decode_cursor(&[7; 32], repository, &signed.replacen("0102", "0103", 1)).is_err());
}

#[test]
fn cursor_scope_rejects_an_equivalent_fork_and_keeps_tagged_envelopes_readable() {
    let mut payload = vec![0; 31];
    payload[0] = 1;
    payload[1] = 2;
    payload[22] = HistoryTraversal::FirstParent as u8;
    let cursor = PageCursor::from_bytes(payload.clone()).unwrap();
    let repository = uuid::Uuid::from_u128(1);
    let fork = uuid::Uuid::from_u128(2);
    let key = [7; 32];
    let signed = encode_cursor(&key, repository, cursor.clone());
    assert!(decode_cursor(&key, fork, &signed).is_err());
    let renamed_scope = signed.replacen(
        &repository.simple().to_string(),
        &fork.simple().to_string(),
        1,
    );
    assert!(decode_cursor(&key, fork, &renamed_scope).is_err());
    let released = format!(
        "{}.{}",
        encode_hex(&payload),
        blake3::keyed_hash(&key, &payload).to_hex()
    );
    assert_eq!(decode_cursor(&key, repository, &released).unwrap(), cursor);
}

#[test]
fn display_names_do_not_alias_invalid_utf8_and_literal_escapes() {
    assert_ne!(display_path(b"\xff"), display_path(b"%FF"));
    assert_eq!(display_path(b"dir/\xff"), "dir/%FF");
    assert_eq!(decode_hex(&encode_hex(b"dir/\xff")).unwrap(), b"dir/\xff");
}

#[test]
fn file_search_prioritizes_names_then_paths_and_supports_fuzzy_input() {
    assert_eq!(search_score("src/Main.rs", "main.rs").unwrap().0, 0);
    assert!(
        search_score("src/main_test.rs", "main").unwrap()
            < search_score("main/examples.rs", "main").unwrap()
    );
    assert!(
        search_score("src/engine/chunk_file.rs", "engine").unwrap()
            < search_score("src/engine/chunk_file.rs", "cfr").unwrap()
    );
    assert!(search_score("src/engine/chunk_file.rs", "not-here").is_none());
}

#[test]
fn file_metadata_keeps_large_utf8_available_without_embedding_it_in_json() {
    let value = content_json(&blob(&vec![b'x'; MAX_INLINE_TEXT_BYTES + 1]));
    assert_eq!(value["text"], Value::Null);
    assert_eq!(value["text_truncated"], true);

    let value = content_json(&blob(b"small text"));
    assert_eq!(value["text"], "small text");
    assert_eq!(value["text_truncated"], false);
}

#[test]
fn inline_assets_require_a_supported_raster_signature() {
    assert_eq!(
        image_content_type(b"\x89PNG\r\n\x1a\nrest"),
        Some("image/png")
    );
    assert_eq!(image_content_type(b"\xff\xd8\xffrest"), Some("image/jpeg"));
    assert_eq!(image_content_type(b"GIF89arest"), Some("image/gif"));
    assert_eq!(image_content_type(b"RIFFsizeWEBPrest"), Some("image/webp"));
    assert_eq!(image_content_type(b"<svg><script/></svg>"), None);
    assert_eq!(image_content_type(b"<html>not an image</html>"), None);
}

#[tokio::test]
async fn inline_asset_response_sets_safe_representation_headers() {
    let response = asset_response(blob(b"\x89PNG\r\n\x1a\nrest"));
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "image/png");
    assert!(!response.headers().contains_key("content-disposition"));
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        b"\x89PNG\r\n\x1a\nrest".as_slice()
    );

    let response = asset_response(blob(b"<svg><script/></svg>"));
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn blob_ranges_return_only_the_requested_bytes() {
    let response = blob_response(
        blob(b"0123456789"),
        Some(&HeaderValue::from_static("bytes=2-5")),
    );
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        b"2345".as_slice()
    );
}

#[tokio::test]
async fn blob_ranges_support_suffixes_and_reject_multiple_ranges() {
    let response = blob_response(
        blob(b"0123456789"),
        Some(&HeaderValue::from_static("bytes=-3")),
    );
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        b"789".as_slice()
    );

    let response = blob_response(
        blob(b"0123456789"),
        Some(&HeaderValue::from_static("bytes=0-1,4-5")),
    );
    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */10");
}

#[tokio::test]
async fn history_http_continuations_cross_shared_key_replicas_and_restart_but_not_forks_or_rotation()
 {
    use crate::{auth::Authentication, repository::Repository, server::Server};
    use crab_metadata::capsule_protocol::{Capsule, CapsuleRefEdit, CapsuleTransaction};
    use crab_storage::{Store, StoreLayout};
    use std::{collections::BTreeMap, sync::Arc};

    async fn start(
        repositories: &BTreeMap<(String, String), Arc<Repository>>,
        store: &Store,
        key_file: Option<&std::path::Path>,
    ) -> (Arc<Server>, String, tokio::task::JoinHandle<()>) {
        let root = crate::storage_root::StorageRoot::memory(store.clone(), "");
        // Exercise the same configured state-key derivation as serve, without
        // introducing an identity-provider dependency into local Git reads.
        let auth = Authentication::new(
            crate::OidcConfig {
                provider: crate::AuthProvider::GitHub,
                issuer: openidconnect::IssuerUrl::new("https://identity.example.invalid".into())
                    .unwrap(),
                public_url: url::Url::parse("http://localhost/").unwrap(),
                client_id: "fixture".into(),
                client_secret_file: None,
                state_key_file: key_file.map(std::path::Path::to_path_buf),
                scim_token_file: None,
                github: None,
            },
            &root,
        )
        .await
        .unwrap();
        // Pinned readers belong to a process runtime. Materialization creates
        // independent handles in production; sharing them would reuse a stopped
        // replica's reader after restart and invalidate the lifecycle test.
        let repositories = repositories
            .iter()
            .map(|(key, entry)| {
                (
                    key.clone(),
                    Repository::new(
                        entry.id,
                        entry.config.clone(),
                        entry.layout.clone(),
                        entry.identity.clone(),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut server = Server::for_test(
            repositories.into(),
            crate::transfer_admission::TransferAdmission::new(
                store.clone(),
                "http-test/admission".into(),
                4,
            ),
        );
        server.cursor_key = auth.cursor_key();
        let server = Arc::new(server);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let app = crate::server::http::router(Arc::clone(&server));
        let cancel = server.cancellation.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(cancel.cancelled_owned())
                .await
                .unwrap();
        });
        (server, origin, task)
    }

    async fn stop(server: Arc<Server>, task: tokio::task::JoinHandle<()>) {
        crate::test_support::server::close(&server).await;
        task.await.unwrap();
    }

    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let history = crate::test_support::git::history(3);
    let mut repositories = BTreeMap::new();
    for (index, name) in ["repo", "fork"].into_iter().enumerate() {
        let layout = StoreLayout::new(store.clone(), format!("http-test/{name}"));
        let root =
            crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
                .await
                .unwrap();
        let transaction = CapsuleTransaction::new(
            root.record().digest(),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                history.oids.last().cloned(),
                None,
            )],
        )
        .unwrap();
        let capsule = Capsule::build(&transaction, vec![history.pack.clone()], Vec::new()).unwrap();
        crab_write::capsule_protocol::publish(&layout, root, &transaction, capsule)
            .await
            .unwrap();
        let config = crate::RepositoryConfig {
            owner: "team".into(),
            name: name.into(),
            bucket: "memory".into(),
            prefix: layout.repo_prefix().into(),
            default_branch: "main".into(),
            description: String::new(),
            members: vec![],
            groups: vec![],
            protected_branches: vec![],
        };
        let identity =
            crab_remote_git::RepositoryIdentity::new("memory", layout.repo_prefix(), 1).unwrap();
        let repository = Repository::new(
            uuid::Uuid::from_u128(index as u128 + 1),
            config,
            layout,
            identity,
        );
        repositories.insert(("team".into(), name.into()), Arc::new(repository));
    }
    let key = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(key.path(), [7u8; 32]).unwrap();
    let (a, a_url, a_task) = start(&repositories, &store, Some(key.path())).await;
    let configured_key = a.cursor_key;
    let (b, b_url, b_task) = start(&repositories, &store, Some(key.path())).await;
    let http = reqwest::Client::new();
    let request_path = format!(
        "/api/repos/team/repo/commits?rev={}&limit=1",
        history.oids[2]
    );
    let first: Value = http
        .get(format!("{a_url}{request_path}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let cursor = first["next"].as_str().unwrap();
    let continuation = format!("{request_path}&cursor={cursor}");
    let second: Value = http
        .get(format!("{b_url}{continuation}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(second["items"][0]["oid"], history.oids[1]);
    let fork = http
        .get(format!(
            "{b_url}{}",
            continuation.replacen("/repo/", "/fork/", 1)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(fork.status(), StatusCode::BAD_REQUEST);
    stop(a, a_task).await;
    let (restarted, restarted_url, restarted_task) =
        start(&repositories, &store, Some(key.path())).await;
    let second: Value = http
        .get(format!("{restarted_url}{continuation}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(second["items"][0]["oid"], history.oids[1]);
    stop(restarted, restarted_task).await;
    std::fs::write(key.path(), [8u8; 32]).unwrap();
    let (rotated, rotated_url, rotated_task) = start(&repositories, &store, Some(key.path())).await;
    let rejected = http
        .get(format!("{rotated_url}{continuation}"))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    stop(rotated, rotated_task).await;
    stop(b, b_task).await;

    let (local_a, _, task_a) = start(&repositories, &store, None).await;
    let (local_b, _, task_b) = start(&repositories, &store, None).await;
    let cursor = decode_cursor(&configured_key, uuid::Uuid::from_u128(1), cursor).unwrap();
    let local_cursor = encode_cursor(&local_a.cursor_key, uuid::Uuid::from_u128(1), cursor);
    assert!(decode_cursor(&local_b.cursor_key, uuid::Uuid::from_u128(1), &local_cursor).is_err());
    stop(local_a, task_a).await;
    stop(local_b, task_b).await;
}
