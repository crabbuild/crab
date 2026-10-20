use super::*;
use super::{
    query::search_score,
    representation::{
        MAX_INLINE_TEXT_BYTES, asset_response, content_json, encode_cursor, image_content_type,
    },
};
use axum::http::HeaderValue;
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
    let signed = encode_cursor(&[7; 32], cursor.clone());
    assert_eq!(decode_cursor(&[7; 32], &signed).unwrap(), cursor);
    assert!(decode_cursor(&[8; 32], &signed).is_err());
    assert!(decode_cursor(&[7; 32], &signed.replacen("0102", "0103", 1)).is_err());
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
