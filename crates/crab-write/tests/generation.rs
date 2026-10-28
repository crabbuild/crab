use std::{sync::Arc, time::Duration};

use crab_coordination::PushLock;
use crab_storage::{Store, StoreLayout};
use crab_write::WriteError;
use tokio_util::sync::CancellationToken;

const TTL: Duration = Duration::from_secs(60);

#[tokio::test]
async fn derived_index_owner_releases_all_admission_after_error_or_cancellation() {
    use crab_coordination::{GIT_GENERATION_OWNER_RESOURCE, GcFenceLease};
    for cancelled in [false, true] {
        let (store, layout) = storage().await;
        let cancel = CancellationToken::new();
        let result =
            crab_write::generation::with_generation_owner(&store, &layout, TTL, &cancel, async {
                if cancelled {
                    cancel.cancel();
                }
                Err::<(), _>(WriteError::Cancelled)
            })
            .await;
        assert!(result.is_err());
        let owner = PushLock::acquire_internal(
            store.inner(),
            layout.repo_prefix(),
            GIT_GENERATION_OWNER_RESOURCE,
            TTL,
        )
        .await
        .unwrap();
        for domain in [layout.repo_prefix(), layout.global_prefix()] {
            let sweep = GcFenceLease::acquire_sweep(store.inner(), domain, TTL)
                .await
                .unwrap();
            sweep.release().await.unwrap();
        }
        // A second owner must not evaluate its work while this lease is held.
        crab_write::generation::with_generation_owner(
            &store,
            &layout,
            TTL,
            &CancellationToken::new(),
            async {
                Err::<(), _>(WriteError::Internal(
                    "contended owner evaluated index work".to_owned(),
                ))
            },
        )
        .await
        .unwrap();
        owner.release().await.unwrap();
    }
}

async fn storage() -> (Store, StoreLayout<Store>) {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let layout = StoreLayout::new(store.clone(), "generation-owner".to_owned());
    crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
    (store, layout)
}
