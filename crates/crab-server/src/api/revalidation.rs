use axum::{
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use crab_remote_git::{BlobMetadata, Error, GitPath, OperationContext, RemoteGitSnapshot};

use super::{Action, representation, response_json};
use crate::server::Server;
use std::time::{Duration, Instant};

pub(super) fn shared_cause(mut error: &Error) -> &Error {
    while let Error::SharedRead { source } = error {
        error = source;
    }
    error
}

fn validator(snapshot: &RemoteGitSnapshot, metadata: &BlobMetadata, preview: bool) -> String {
    let mut digest = blake3::Hasher::new();
    // Identity coding is the only representation served on these routes. A
    // coding or serialization/policy change must version this namespace too.
    if preview {
        digest.update(b"crab-file-preview-json-v1:identity");
        digest.update(&(representation::MAX_INLINE_TEXT_BYTES as u64).to_le_bytes());
        digest.update(snapshot.commit_oid().as_bytes());
        digest.update(&snapshot.generation().to_le_bytes());
        digest.update(&metadata.mode.raw().to_le_bytes());
        digest.update(format!("{:?}", metadata.classification).as_bytes());
    } else {
        digest.update(b"crab-git-blob-v1:identity");
    }
    // The object ID and policy uniquely determine preview state and text;
    // validating a small cached preview need not inflate it a second time.
    digest.update(metadata.oid.as_bytes());
    digest.update(&metadata.git_size.to_le_bytes());
    format!("\"{}\"", digest.finalize().to_hex())
}

fn not_modified(headers: &HeaderMap, etag: &str) -> bool {
    let mut matched = false;
    for value in headers.get_all(header::IF_NONE_MATCH) {
        let bytes = value.as_bytes();
        if bytes.trim_ascii() == b"*" {
            return true;
        }
        let mut rest = bytes;
        // Entity tags may contain commas. Split only outside a quoted opaque
        // tag; malformed fields are ignored rather than guessed into a match.
        while !rest.is_empty() {
            rest = rest.trim_ascii_start();
            if let Some(next) = rest.strip_prefix(b",") {
                rest = next;
                continue;
            }
            if let Some(next) = rest.strip_prefix(b"W/") {
                rest = next;
            }
            let Some(opaque) = rest.strip_prefix(b"\"") else {
                return false;
            };
            let Some(end) = opaque.iter().position(|byte| *byte == b'"') else {
                return false;
            };
            if opaque[..end]
                .iter()
                .any(|byte| *byte < 0x21 || *byte == 0x7f)
            {
                return false;
            }
            matched |= &rest[..end + 2] == etag.as_bytes();
            rest = opaque[end + 1..].trim_ascii_start();
            if !rest.is_empty() && rest[0] != b',' {
                return false;
            }
        }
    }
    matched
}

fn decorate(mut response: Response, etag: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    response
}

pub(super) async fn read(
    server: &Server,
    snapshot: &RemoteGitSnapshot,
    path: &GitPath,
    operation: &OperationContext,
    action: Action,
    headers: &HeaderMap,
) -> crab_remote_git::Result<(Response, Duration)> {
    if matches!(action, Action::Blob)
        && !headers.contains_key(header::RANGE)
        && !headers.contains_key(header::IF_NONE_MATCH)
    {
        // read_blob proves visibility and verifies the complete body. A separate
        // metadata probe would repeat alpha.1's full encoded-entry CRC read on
        // ordinary unconditional downloads.
        let blob = snapshot.read_blob(path, operation).await?;
        let etag = validator(snapshot, &blob.metadata, false);
        return Ok((
            decorate(representation::blob_response(blob, None), &etag),
            Duration::ZERO,
        ));
    }
    // The HTTP boundary has authenticated current UUID grants; resolve the
    // visible pinned path before inspecting validators or exposing any 304.
    let metadata = snapshot.blob_metadata(path, operation).await?;
    let preview = matches!(action, Action::FilePreview);
    let etag = validator(snapshot, &metadata, preview);
    if not_modified(headers, &etag) {
        return Ok((
            decorate(StatusCode::NOT_MODIFIED.into_response(), &etag),
            Duration::ZERO,
        ));
    }
    let mut encoded = Duration::ZERO;
    let response = if preview {
        let memory = server
            .runtime
            .reserve_bytes(
                512 + metadata
                    .git_size
                    .min(representation::MAX_INLINE_TEXT_BYTES as u64)
                    as usize,
            )
            .await?;
        let value = representation::file_preview_metadata(snapshot, metadata, operation).await?;
        let encoding = Instant::now();
        let bytes =
            response_json::preview(&server.runtime, operation.cancellation(), value, memory)
                .await?;
        encoded = encoding.elapsed();
        ([("content-type", "application/json")], bytes).into_response()
    } else {
        // If-Range accepts an exact strong entity tag. Dates are deliberately
        // unsupported without Last-Modified, so a mismatch sends the full body.
        let range = headers.get(header::RANGE).filter(|_| {
            headers
                .get(header::IF_RANGE)
                .is_none_or(|value| value.as_bytes() == etag.as_bytes())
        });
        if let Some(range) = range {
            let total = usize::try_from(metadata.git_size).map_err(|_| Error::LimitExceeded {
                limit: "blob bytes",
                actual: metadata.git_size,
                maximum: usize::MAX as u64,
            })?;
            if range
                .to_str()
                .ok()
                .and_then(|range| representation::parse_byte_range(range, total).ok())
                .is_none()
            {
                let mut response = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
                if let Ok(value) = HeaderValue::from_str(&format!("bytes */{total}")) {
                    response.headers_mut().insert(header::CONTENT_RANGE, value);
                }
                response
                    .headers_mut()
                    .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
                return Ok((response, Duration::ZERO));
            }
        }
        representation::blob_response(snapshot.read_blob(path, operation).await?, range)
    };
    Ok((decorate(response, &etag), encoded))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_tag_lists_use_weak_comparison_without_splitting_opaque_commas() {
        for (value, matches) in [
            ("\"current\"", true),
            ("W/\"current\"", true),
            ("*", true),
            ("\"old\", W/\"current\"", true),
            ("\"old,current\"", false),
            ("\"current\"garbage", false),
            ("\"unterminated", false),
            ("current", false),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::IF_NONE_MATCH, HeaderValue::from_str(value).unwrap());
            assert_eq!(not_modified(&headers, "\"current\""), matches, "{value}");
        }
    }

    #[tokio::test]
    async fn conditional_file_reads_authorize_before_not_modified() {
        use crate::test_support::http::Harness;
        use crab_metadata::capsule_protocol::{
            Capsule, CapsuleGitPack, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind,
            CapsuleTransaction, CapsuleVisibilityDelta,
        };

        fn visible_capsule(
            transaction: &CapsuleTransaction,
            packs: Vec<CapsuleGitPack>,
            objects: Vec<String>,
        ) -> Capsule {
            // Native Git's packed closure supplies complete replacement evidence
            // so the real checkpoint can validate this fixture's visibility.
            let edits = transaction
                .edits()
                .iter()
                .map(|edit| {
                    (
                        edit.ref_name().to_owned(),
                        crab_metadata::git_visibility::GitVisibilityEdit::from_replacement_objects(
                            edit.expected_old().map(str::to_owned),
                            edit.new_oid().unwrap().to_owned(),
                            objects.clone(),
                        ),
                    )
                })
                .collect();
            let visibility = CapsuleVisibilityDelta::new(edits).unwrap();
            Capsule::build(
                transaction,
                packs,
                vec![CapsuleSection::new(
                    CapsuleSectionKind::VisibilityDelta,
                    visibility.encode().unwrap(),
                )],
            )
            .unwrap()
        }

        let h = Harness::new(false).await;
        let alice = h.login().await;
        *h.provider.mode.lock().await = "member".into();
        let bob = h.login().await;
        let repository = h.server.repositories.values()[0].clone();
        let history = crate::test_support::git::history_with_entries(&[
            ("asset", 0o100644, b"same bytes\n"),
            ("executable", 0o100755, b"same bytes\n"),
        ]);
        let commit = history.oids[0].clone();
        let original_objects = history.object_ids.clone();
        let root = crab_write::capsule_protocol::open_root(&repository.layout)
            .await
            .unwrap();
        let transaction = CapsuleTransaction::new(
            root.record().digest(),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some(commit.clone()),
                None,
            )],
        )
        .unwrap();
        let capsule = visible_capsule(&transaction, vec![history.pack], history.object_ids);
        crab_write::capsule_protocol::publish(&repository.layout, root, &transaction, capsule)
            .await
            .unwrap();
        let url = format!(
            "{}/api/repos/team/private/file-preview?path=asset",
            h.origin
        );
        let response = h
            .http
            .get(&url)
            .header(header::COOKIE, &bob)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let original = response.headers()[header::ETAG].clone();
        let value: serde_json::Value = response.json().await.unwrap();
        assert_eq!(value["preview"]["text"], "same bytes\n");
        let response = h
            .http
            .get(&url)
            .header(header::COOKIE, &bob)
            .header(header::IF_NONE_MATCH, &original)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        let response = h
            .http
            .get(url.replace("path=asset", "path=executable"))
            .header(header::COOKIE, &bob)
            .header(header::IF_NONE_MATCH, &original)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_ne!(response.headers()[header::ETAG], original);
        let response = h
            .http
            .get(url.replace("path=asset", "path=missing"))
            .header(header::COOKIE, &bob)
            .header(header::IF_NONE_MATCH, "*")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response = h
            .http
            .get(&url)
            .header(header::COOKIE, &bob)
            .header(header::IF_NONE_MATCH, "\"tampered\"")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // A changed commit must invalidate JSON even when this path's bytes
        // are unchanged, including within the normal browse probe interval.
        let changed = crate::test_support::git::history_with_blob(b"same bytes\n");
        let root = crab_write::capsule_protocol::open_root(&repository.layout)
            .await
            .unwrap();
        let transaction = CapsuleTransaction::new(
            root.record().digest(),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                Some(commit.clone()),
                Some(changed.oids[0].clone()),
                None,
            )],
        )
        .unwrap();
        let capsule = visible_capsule(&transaction, vec![changed.pack], changed.object_ids);
        crab_write::capsule_protocol::publish(&repository.layout, root, &transaction, capsule)
            .await
            .unwrap();
        let response = h
            .http
            .get(&url)
            .header(header::COOKIE, &bob)
            .header(header::IF_NONE_MATCH, &original)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let current_tag = response.headers()[header::ETAG].clone();
        let current: serde_json::Value = response.json().await.unwrap();
        assert_ne!(current["commit"], value["commit"]);
        assert_eq!(current["oid"], value["oid"]);
        let response = h
            .http
            .get(url.replace("path=asset", "path=executable"))
            .header(header::COOKIE, &bob)
            .header(header::IF_NONE_MATCH, "*")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // Ref publication alone does not increment root generation. A real
        // ref checkpoint advances it while preserving the selected commit.
        let root = crab_write::capsule_protocol::open_root(&repository.layout)
            .await
            .unwrap();
        let transaction = CapsuleTransaction::new(
            root.record().digest(),
            vec![CapsuleRefEdit::new(
                "refs/heads/other",
                None,
                Some(commit),
                None,
            )],
        )
        .unwrap();
        let capsule = visible_capsule(&transaction, Vec::new(), original_objects);
        crab_write::capsule_protocol::publish(&repository.layout, root, &transaction, capsule)
            .await
            .unwrap();
        crab_remote::checkpoint::publish_capsule_checkpoint(
            &repository.layout,
            1,
            16 * 1024 * 1024,
            &h.server.cancellation,
        )
        .await
        .unwrap();
        let response = h
            .http
            .get(&url)
            .header(header::COOKIE, &bob)
            .header(header::IF_NONE_MATCH, &current_tag)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let advanced = response.headers()[header::ETAG].clone();
        assert_ne!(advanced, current_tag);
        let next: serde_json::Value = response.json().await.unwrap();
        assert_eq!(next["commit"], current["commit"]);
        assert_ne!(next["generation"], current["generation"]);
        let members = repository
            .config
            .members
            .iter()
            .filter(|member| member.subject != "bob-id")
            .cloned()
            .collect();
        let catalog = h.server.catalog().unwrap();
        catalog
            .set_members("team", "private", members, true)
            .await
            .unwrap();
        let response = h
            .http
            .get(&url)
            .header(header::COOKIE, &bob)
            .header(header::IF_NONE_MATCH, &advanced)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let response = h
            .http
            .get(&url)
            .header(header::IF_NONE_MATCH, "*")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // The old materialized route must not authorize a reused catalog name.
        let (mut document, etag) = catalog.load().await.unwrap();
        document.repositories[0].id = uuid::Uuid::now_v7();
        document.version += 1;
        let root = catalog.root();
        root.store
            .update(
                &root.path(".crab/http-server/v1/catalog.json"),
                serde_json::to_vec(&document).unwrap().into(),
                etag.unwrap(),
            )
            .await
            .unwrap();
        let response = h
            .http
            .get(&url)
            .header(header::COOKIE, &alice)
            .header(header::IF_NONE_MATCH, "*")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        h.close().await;
    }
}
