use std::sync::Arc;

use crab_storage::Store;
use object_store::{memory::InMemory, path::Path};

use super::*;
use crate::identity::{ApplicationId, TenantId};

fn fixture() -> (ReleaseStore, Vec<u8>, Digest) {
    let identity = ApplicationIdentity::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([2; 16]),
    );
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("root"),
        *identity.application().as_bytes(),
    );
    let descriptor = br#"{"runtime":"crab-http-server","version":1}"#.to_vec();
    let digest = Digest::from_bytes(*blake3::hash(&descriptor).as_bytes());
    (
        ReleaseStore::new(layout, identity).unwrap(),
        descriptor,
        digest,
    )
}

#[tokio::test]
async fn prepare_uploads_descriptor_before_cas_and_reconciles_retry() {
    let (releases, descriptor, digest) = fixture();
    let image = format!("sha256:{}", "a".repeat(64));
    let first = releases
        .prepare(
            &descriptor,
            digest,
            0,
            &image,
            RequestId::from_bytes([3; 16]),
        )
        .await
        .unwrap();
    assert_eq!(first.revision(), 1);
    assert_eq!(first.desired(), Some(digest));
    assert_eq!(first.state(), ReleaseState::Prepared);

    let retry = releases
        .prepare(
            &descriptor,
            digest,
            0,
            &image,
            RequestId::from_bytes([3; 16]),
        )
        .await
        .unwrap();
    assert_eq!(retry, first);
    assert!(
        releases
            .prepare(
                &descriptor,
                digest,
                1,
                &format!("sha256:{}", "b".repeat(64)),
                RequestId::from_bytes([5; 16]),
            )
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn prepare_rejects_digest_image_and_revision_drift() {
    let (releases, descriptor, digest) = fixture();
    let image = format!("sha256:{}", "a".repeat(64));
    assert!(matches!(
        releases
            .prepare(
                &descriptor,
                Digest::from_bytes([9; 32]),
                0,
                &image,
                RequestId::from_bytes([3; 16]),
            )
            .await,
        Err(Error::Release(_))
    ));
    assert!(matches!(
        releases
            .prepare(
                &descriptor,
                digest,
                0,
                "latest",
                RequestId::from_bytes([3; 16]),
            )
            .await,
        Err(Error::Release(_))
    ));
    releases
        .prepare(
            &descriptor,
            digest,
            0,
            &image,
            RequestId::from_bytes([3; 16]),
        )
        .await
        .unwrap();
    assert!(matches!(
        releases
            .prepare(
                &descriptor,
                digest,
                9,
                &image,
                RequestId::from_bytes([3; 16]),
            )
            .await,
        Err(Error::Release(_))
    ));
}

#[tokio::test]
async fn activation_is_operation_bound_resumable_and_publishes_current() {
    let (releases, descriptor, digest) = fixture();
    let operation = RequestId::from_bytes([3; 16]);
    let prepared = releases
        .prepare(
            &descriptor,
            digest,
            0,
            &format!("sha256:{}", "a".repeat(64)),
            operation,
        )
        .await
        .unwrap();

    let activating = releases
        .start_activation(prepared.revision(), operation)
        .await
        .unwrap();
    assert_eq!(activating.revision(), 2);
    assert_eq!(activating.state(), ReleaseState::Activating);
    assert_eq!(activating.current(), None);
    assert_eq!(
        releases
            .start_activation(prepared.revision(), operation)
            .await
            .unwrap(),
        activating
    );

    let ready = releases
        .complete_activation(activating.revision(), operation)
        .await
        .unwrap();
    assert_eq!(ready.revision(), 3);
    assert_eq!(ready.state(), ReleaseState::Ready);
    assert_eq!(ready.current(), Some(digest));
    assert_eq!(ready.current(), ready.desired());
    assert_eq!(
        releases
            .start_activation(prepared.revision(), operation)
            .await
            .unwrap(),
        ready
    );
    assert_eq!(
        releases
            .complete_activation(activating.revision(), operation)
            .await
            .unwrap(),
        ready
    );
}

#[tokio::test]
async fn activation_rejects_operation_revision_and_descriptor_drift() {
    let (releases, descriptor, digest) = fixture();
    let operation = RequestId::from_bytes([3; 16]);
    let prepared = releases
        .prepare(
            &descriptor,
            digest,
            0,
            &format!("sha256:{}", "a".repeat(64)),
            operation,
        )
        .await
        .unwrap();
    assert!(
        releases
            .start_activation(prepared.revision(), RequestId::from_bytes([4; 16]))
            .await
            .is_err()
    );
    assert!(
        releases
            .start_activation(prepared.revision() + 1, operation)
            .await
            .is_err()
    );

    let path = releases.layout.release_descriptor_path(digest.as_bytes());
    releases
        .layout
        .store()
        .put_overwrite(&path, Bytes::from_static(b"corrupt"))
        .await
        .unwrap();
    assert!(
        releases
            .start_activation(prepared.revision(), operation)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn maintenance_is_operation_bound_and_resumable() {
    let (releases, descriptor, digest) = fixture();
    let operation = RequestId::from_bytes([3; 16]);
    let prepared = releases
        .prepare(
            &descriptor,
            digest,
            0,
            &format!("sha256:{}", "a".repeat(64)),
            operation,
        )
        .await
        .unwrap();

    let maintenance = releases
        .start_maintenance(prepared.revision(), operation)
        .await
        .unwrap();
    assert_eq!(maintenance.revision(), 2);
    assert_eq!(maintenance.state(), ReleaseState::Maintenance);
    assert_eq!(maintenance.current(), None);
    assert_eq!(maintenance.desired(), Some(digest));
    assert_eq!(
        releases
            .start_maintenance(prepared.revision(), operation)
            .await
            .unwrap(),
        maintenance
    );
    assert!(
        releases
            .start_activation(prepared.revision(), operation)
            .await
            .is_err()
    );
    assert!(
        releases
            .prepare(
                &descriptor,
                digest,
                maintenance.revision(),
                &format!("sha256:{}", "b".repeat(64)),
                RequestId::from_bytes([4; 16]),
            )
            .await
            .is_err()
    );
    let ready = releases
        .complete_maintenance(maintenance.revision(), operation)
        .await
        .unwrap();
    assert_eq!(ready.revision(), 3);
    assert_eq!(ready.state(), ReleaseState::Ready);
    assert_eq!(ready.current(), Some(digest));
    assert_eq!(
        releases
            .complete_maintenance(maintenance.revision(), operation)
            .await
            .unwrap(),
        ready
    );
}

#[tokio::test]
async fn prepare_cannot_replace_an_activation_in_progress() {
    let (releases, descriptor, digest) = fixture();
    let operation = RequestId::from_bytes([3; 16]);
    let prepared = releases
        .prepare(
            &descriptor,
            digest,
            0,
            &format!("sha256:{}", "a".repeat(64)),
            operation,
        )
        .await
        .unwrap();
    let activating = releases
        .start_activation(prepared.revision(), operation)
        .await
        .unwrap();

    assert!(
        releases
            .prepare(
                &descriptor,
                digest,
                activating.revision(),
                &format!("sha256:{}", "b".repeat(64)),
                RequestId::from_bytes([5; 16]),
            )
            .await
            .is_err()
    );
}

#[test]
fn provisioning_recheck_accepts_only_the_exact_activation_completion() {
    let (_, _, digest) = fixture();
    let identity = ApplicationIdentity::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([2; 16]),
    );
    let before = ReleaseRecord {
        application: identity.application(),
        revision: 2,
        current: None,
        desired: Some(digest),
        desired_image: format!("sha256:{}", "a".repeat(64)),
        operation: RequestId::from_bytes([3; 16]),
        state: ReleaseState::Activating,
    };
    let mut completed = before.clone();
    completed.revision = 3;
    completed.current = Some(digest);
    completed.state = ReleaseState::Ready;
    assert!(provision_release_continues(&before, &completed, digest));

    completed.operation = RequestId::from_bytes([4; 16]);
    assert!(!provision_release_continues(&before, &completed, digest));
}
