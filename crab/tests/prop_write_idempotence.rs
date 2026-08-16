//! Property: content-addressed writes are idempotent — the same bytes
//! written N times end up stored exactly once, every call succeeds, and
//! a subsequent verified read returns the original bytes.

#![cfg(feature = "testing")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::sync::Arc;

use bytes::Bytes;
use crab::storage::Store;
use crab::storage::testing::MockStore;
use object_store::ObjectStore;
use object_store::path::Path;
use proptest::prelude::*;

fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(fut)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(25))]

    #[test]
    fn put_is_idempotent_for_identical_content(
        bytes in prop::collection::vec(any::<u8>(), 0..1024),
        n_attempts in 1u32..=5,
    ) {
        let bytes = Bytes::from(bytes);
        let expected_hash = *blake3::hash(&bytes).as_bytes();

        block_on(async {
            let backend: Arc<dyn ObjectStore> = Arc::new(MockStore::new());
            let store = Store::new(Arc::clone(&backend));
            let path = Path::from("blobs/idempotent");

            for _ in 0..n_attempts {
                store
                    .put(&path, bytes.clone())
                    .await
                    .expect("put with identical content must succeed on every attempt");
            }

            let got = store
                .verify(&path, &expected_hash)
                .await
                .expect("verified read must return the written bytes");

            prop_assert_eq!(got, bytes);
            Ok(())
        })?;
    }
}
