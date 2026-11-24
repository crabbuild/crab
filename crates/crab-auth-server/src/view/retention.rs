use std::io::Read;
use std::sync::Arc;

use crab_coordination::read_retention::{ReadRetentionLease, ReadRetentionRevision};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use super::{
    AuthServerError, CrabUrl, Result, Store, StoreLayout, build_view_store, capsule,
    capture_source_view, view_prefix, view_store_layout,
};

const MAX_REQUEST_BYTES: usize = 8 * 1024;
const MAX_CAPTURE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// A retention request from the trusted issuer after principal and view authorization.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewRetentionRequest {
    repo_url: String,
    provider: String,
    scope_hash: String,
    source_generation: u64,
    source_digest: String,
    reader: String,
    target: String,
    operation: Operation,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Acquire,
    Verify { incarnation: String },
    Renew { revision: ReadRetentionRevision },
    Release { revision: ReadRetentionRevision },
}

impl ViewRetentionRequest {
    /// Read one bounded JSON request from the issuer subprocess pipe.
    pub fn read(input: impl Read) -> Result<Self> {
        let mut content = Vec::new();
        input
            .take((MAX_REQUEST_BYTES + 1) as u64)
            .read_to_end(&mut content)?;
        if content.len() > MAX_REQUEST_BYTES {
            return Err(AuthServerError::invalid(
                "view retention request exceeds its size limit",
            ));
        }
        serde_json::from_slice(&content).map_err(AuthServerError::ViewRetentionRequest)
    }
}

/// A confirmed native transition for one exact reader revision.
#[derive(Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ViewRetentionResponse {
    Acquired {
        repo_prefix: String,
        view_state_digest: String,
        revision: ReadRetentionRevision,
        remaining_ms: u64,
    },
    Renewed {
        revision: ReadRetentionRevision,
        remaining_ms: u64,
    },
    Verified {
        repo_prefix: String,
        remaining_ms: u64,
    },
    Released,
}

/// Create, renew, or release the issuer's native reader claim for an authorized view.
/// The issuer binds the reader and revision to the authenticated principal before calling.
pub async fn view_retention(request: ViewRetentionRequest) -> Result<ViewRetentionResponse> {
    let parsed = CrabUrl::parse(&request.repo_url)?;
    let store = build_view_store(&parsed.bucket, &request.provider)?;
    retain_with_store(request, &parsed.repo_path, &store).await
}

async fn retain_with_store(
    request: ViewRetentionRequest,
    source_repo: &str,
    store: &Store,
) -> Result<ViewRetentionResponse> {
    for value in [&request.scope_hash, &request.source_digest, &request.reader] {
        if !blake3::Hash::from_hex(value).is_ok_and(|hash| hash.to_hex().as_str() == value) {
            return Err(AuthServerError::invalid(
                "view retention requires canonical digests",
            ));
        }
    }
    let target = resolved_target(store)?;
    if blake3::Hash::from_bytes(*target).to_hex().as_str() != request.target {
        return Err(AuthServerError::AuthFailed {
            path: "view retention native target changed".into(),
        });
    }
    let repo_prefix = view_prefix(
        source_repo,
        &request.scope_hash,
        request.source_generation,
        &request.source_digest,
    );
    let layout = view_store_layout(store, &repo_prefix);
    let owner = owner_for_target(store, &request.reader)?;
    let path = layout.read_retention_claim_path(&owner);
    let native = Arc::clone(layout.store().inner());
    match request.operation {
        Operation::Acquire => {
            let source = StoreLayout::new(store.clone(), source_repo.to_owned());
            let captured = capture_source_view(&source).await?;
            if captured.root().root().generation() != request.source_generation
                || captured.state_digest() != request.source_digest
            {
                return Err(AuthServerError::CasConflict {
                    path: source.capsule_root_path().to_string(),
                    expected_etag: None,
                });
            }
            capsule::verify_ready_record(&layout, &request.source_digest).await?;
            let (view_state_digest, lease) =
                crab_read::acquire_pointer_retention(&layout, &owner, MAX_CAPTURE_BYTES).await?;
            Ok(ViewRetentionResponse::Acquired {
                repo_prefix,
                view_state_digest,
                revision: lease.revision().map_err(crab_read::ReadError::from)?,
                remaining_ms: remaining_ms(lease.deadline().map_err(crab_read::ReadError::from)?)?,
            })
        }
        Operation::Verify { incarnation } => {
            let deadline = ReadRetentionLease::verify_active_incarnation(
                &native,
                &path,
                &repo_prefix,
                &incarnation,
            )
            .await
            .map_err(crab_read::ReadError::from)?;
            Ok(ViewRetentionResponse::Verified {
                repo_prefix,
                remaining_ms: remaining_ms(deadline)?,
            })
        }
        Operation::Renew { revision } => {
            let lease =
                ReadRetentionLease::renew_acknowledged(native, path, &repo_prefix, &revision)
                    .await
                    .map_err(crab_read::ReadError::from)?;
            Ok(ViewRetentionResponse::Renewed {
                revision: lease.revision().map_err(crab_read::ReadError::from)?,
                remaining_ms: remaining_ms(lease.deadline().map_err(crab_read::ReadError::from)?)?,
            })
        }
        Operation::Release { revision } => {
            ReadRetentionLease::release_acknowledged(native, path, &revision)
                .await
                .map_err(crab_read::ReadError::from)?;
            Ok(ViewRetentionResponse::Released)
        }
    }
}

fn resolved_target(store: &Store) -> Result<&[u8; 32]> {
    store
        .target_identity()
        .ok_or_else(|| AuthServerError::AuthFailed {
            path: "view retention requires a resolved native target".into(),
        })
}

fn owner_for_target(store: &Store, reader: &str) -> Result<String> {
    let mut hasher = blake3::Hasher::new_derive_key("crab issuer read retention owner v1");
    hasher.update(resolved_target(store)?);
    hasher.update(reader.as_bytes());
    Ok(hasher.finalize().to_hex().to_string())
}

fn remaining_ms(deadline: Instant) -> Result<u64> {
    let remaining = u64::try_from(
        deadline
            .saturating_duration_since(Instant::now())
            .as_millis(),
    )
    .map_err(|_| AuthServerError::invalid("view retention duration exceeds its limit"))?;
    if remaining == 0 {
        return Err(AuthServerError::AuthFailed {
            path: "view retention deadline elapsed".into(),
        });
    }
    Ok(remaining)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use crab_coordination::read_retention::load_read_retention_root;
    use object_store::memory::InMemory;

    use super::*;

    #[tokio::test]
    async fn issuer_retention_transitions_only_the_confirmed_view_claim() {
        let source_repo = "org/repo";
        let store = Store::new(Arc::new(InMemory::new())).with_target_identity([1; 32]);
        let source = StoreLayout::new(store.clone(), source_repo.to_owned());
        crab_write::capsule_protocol::initialize(&source, &"a".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let captured = capture_source_view(&source).await.unwrap();
        let digest = captured.state_digest();
        let request = |operation| ViewRetentionRequest {
            repo_url: "crab://bucket/org/repo".into(),
            provider: "aws".into(),
            scope_hash: "a".repeat(64),
            source_generation: captured.root().root().generation(),
            source_digest: digest.clone(),
            reader: "c".repeat(64),
            target: "01".repeat(32),
            operation,
        };
        let acquire = request(Operation::Acquire);
        let prefix = view_prefix(
            source_repo,
            &acquire.scope_hash,
            acquire.source_generation,
            &acquire.source_digest,
        );
        let layout = view_store_layout(&store, &prefix);
        let claim =
            layout.read_retention_claim_path(&owner_for_target(&store, &acquire.reader).unwrap());
        crab_write::capsule_protocol::initialize(&layout, &"d".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        assert!(
            retain_with_store(request(Operation::Acquire), source_repo, &store)
                .await
                .is_err()
        );
        let ready =
            serde_json::json!({"schema_version": 1, "source_digest": acquire.source_digest});
        store
            .create_strict(
                &layout.repo_path("v2/view-ready.json"),
                Bytes::from(ready.to_string()),
            )
            .await
            .unwrap();

        let mut stale = request(Operation::Acquire);
        stale.source_generation += 1;
        assert!(matches!(
            retain_with_store(stale, source_repo, &store).await,
            Err(AuthServerError::CasConflict { .. })
        ));

        let ViewRetentionResponse::Acquired {
            repo_prefix,
            remaining_ms,
            revision,
            ..
        } = retain_with_store(acquire, source_repo, &store)
            .await
            .unwrap()
        else {
            panic!("expected acquired claim")
        };
        assert_eq!(repo_prefix, prefix);
        assert!((1..=150_000).contains(&remaining_ms));
        let mut wrong_view = request(Operation::Renew {
            revision: revision.clone(),
        });
        wrong_view.source_generation += 1;
        assert!(
            retain_with_store(wrong_view, source_repo, &store)
                .await
                .is_err()
        );
        let changed_store = store.clone().with_target_identity([2; 32]);
        assert!(matches!(
            retain_with_store(request(Operation::Acquire), source_repo, &changed_store).await,
            Err(AuthServerError::AuthFailed { .. })
        ));
        let wrong_target = request(Operation::Renew {
            revision: revision.clone(),
        });
        assert!(
            retain_with_store(wrong_target, source_repo, &changed_store)
                .await
                .is_err()
        );
        let ViewRetentionResponse::Renewed {
            revision: renewed, ..
        } = retain_with_store(
            request(Operation::Renew {
                revision: revision.clone(),
            }),
            source_repo,
            &store,
        )
        .await
        .unwrap()
        else {
            panic!("expected renewed claim")
        };
        assert!(
            retain_with_store(
                request(Operation::Release { revision }),
                source_repo,
                &store
            )
            .await
            .is_err()
        );
        assert!(
            load_read_retention_root(store.inner(), &claim)
                .await
                .unwrap()
                .is_some()
        );
        assert!(matches!(
            retain_with_store(
                request(Operation::Release { revision: renewed }),
                source_repo,
                &store
            )
            .await
            .unwrap(),
            ViewRetentionResponse::Released
        ));
        assert!(
            load_read_retention_root(store.inner(), &claim)
                .await
                .unwrap()
                .is_none()
        );
    }
}
