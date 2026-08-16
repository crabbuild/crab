//! Property: verified reads are tamper-evident — bytes that hash to
//! their expected hash come back intact; any bit flip in the expected
//! hash surfaces as `CorruptObject` carrying the requested path.

#![cfg(feature = "testing")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::sync::Arc;

use bytes::Bytes;
use crab::core::error::CrabError;
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
    fn verify_returns_bytes_for_correct_hash_and_corrupt_object_for_tampered_hash(
        bytes in prop::collection::vec(any::<u8>(), 0..1024),
        tamper in any::<bool>(),
        flip_byte in 0usize..32,
        flip_bit in 0u8..8,
    ) {
        let bytes = Bytes::from(bytes);
        let correct_hash = *blake3::hash(&bytes).as_bytes();
        let path_str = "blobs/verified";

        block_on(async {
            let backend: Arc<dyn ObjectStore> = Arc::new(MockStore::new());
            let store = Store::new(Arc::clone(&backend));
            let path = Path::from(path_str);

            store
                .put(&path, bytes.clone())
                .await
                .expect("put must succeed");

            if tamper {
                // Flip one bit in the expected hash so the verifier sees
                // bytes that don't match what the caller claimed.
                let mut wrong = correct_hash;
                wrong[flip_byte] ^= 1u8 << flip_bit;

                let err = store
                    .verify(&path, &wrong)
                    .await
                    .expect_err("tampered expected hash must surface corruption");

                match err {
                    CrabError::CorruptObject { path: got_path, .. } => {
                        prop_assert_eq!(got_path, path.to_string());
                    }
                    other => {
                        prop_assert!(
                            false,
                            "expected CorruptObject, got {:?}",
                            other,
                        );
                    }
                }
            } else {
                let got = store
                    .verify(&path, &correct_hash)
                    .await
                    .expect("correct hash must produce original bytes");
                prop_assert_eq!(got, bytes);
            }

            Ok(())
        })?;
    }
}
