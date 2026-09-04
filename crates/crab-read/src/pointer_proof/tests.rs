use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use crab_xet::{
    shard::{
        FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo, MDBXorbInfo, ShardWriter,
        XorbChunkSequenceEntry, XorbChunkSequenceHeader,
    },
    xorb::{
        builder::{RunId, XorbBuilder},
        format::Chunk,
    },
};
use object_store::{
    memory::InMemory,
    throttle::{ThrottleConfig, ThrottledStore},
};

use super::*;

struct Fixture {
    pointer: Pointer,
    shard: (MerkleHash, Bytes),
    xorbs: Vec<(MerkleHash, Bytes)>,
    chunks: Vec<(MerkleHash, u64)>,
}

impl Fixture {
    fn new(forged_hash: bool, empty: bool) -> Self {
        let chunks: Vec<_> = if empty {
            vec![]
        } else {
            vec![
                Chunk::new(Bytes::from(vec![b'a'; 4096])),
                Chunk::new(Bytes::from(vec![b'b'; 8192])),
            ]
        };
        let order: &[usize] = if empty { &[] } else { &[0, 1, 0] };
        let mut hasher = blake3::Hasher::new();
        for &index in order {
            hasher.update(&chunks[index].data);
        }
        let file_hash = if forged_hash {
            [7; 32]
        } else {
            *hasher.finalize().as_bytes()
        };
        let pointer = Pointer {
            file_hash,
            size: order.iter().map(|&i| chunks[i].data.len() as u64).sum(),
            shard_hint: None,
        };
        let mut writer = ShardWriter::new();
        let mut xorbs = Vec::new();
        for chunk in &chunks {
            let mut builder = XorbBuilder::new();
            builder.push(chunk, RunId(7)).unwrap();
            let xorb = builder.finalize().unwrap().pop().unwrap();
            writer
                .add_xorb(Arc::new(MDBXorbInfo {
                    metadata: XorbChunkSequenceHeader::new(xorb.hash, 1usize, chunk.data.len()),
                    chunks: vec![XorbChunkSequenceEntry::new(
                        chunk.hash,
                        chunk.data.len(),
                        0u32,
                    )],
                }))
                .unwrap();
            xorbs.push((xorb.hash, xorb.bytes));
        }
        writer
            .add_file(MDBFileInfo {
                metadata: FileDataSequenceHeader::new(
                    MerkleHash::from(file_hash),
                    order.len() as u32,
                    false,
                    false,
                ),
                segments: order
                    .iter()
                    .map(|&i| {
                        FileDataSequenceEntry::new(
                            xorbs[i].0,
                            chunks[i].data.len() as u32,
                            0u32,
                            1u32,
                        )
                    })
                    .collect(),
                verification: vec![],
                metadata_ext: None,
            })
            .unwrap();
        let (body, hash) = writer.finalize().unwrap();
        Self {
            pointer,
            shard: (hash, Bytes::from(body)),
            xorbs,
            chunks: order
                .iter()
                .map(|&i| (chunks[i].hash, chunks[i].data.len() as u64))
                .collect(),
        }
    }

    async fn upload(&self, layout: &StoreLayout<Store>) {
        layout
            .store()
            .put(&layout.shard_path(&self.shard.0), self.shard.1.clone())
            .await
            .unwrap();
        for (hash, body) in &self.xorbs {
            layout
                .store()
                .put(&layout.xorb_path(hash), body.clone())
                .await
                .unwrap();
        }
    }

    async fn verify(
        &self,
        layout: &StoreLayout<Store>,
        limits: PointerProofLimits,
    ) -> Result<ExtractedFileRecipe> {
        verify_crab_pointer(
            layout,
            &self.pointer,
            self.shard.0,
            limits,
            &CancellationToken::new(),
        )
        .await
    }

    fn read_bytes(&self) -> u64 {
        self.shard.1.len() as u64
            + [0, 1, 0]
                .iter()
                .map(|&i| self.xorbs[i].1.len() as u64)
                .sum::<u64>()
    }
}

fn limits() -> PointerProofLimits {
    PointerProofLimits {
        max_file_bytes: 1 << 20,
        max_shard_bytes: 1 << 20,
        max_xorb_bytes: 1 << 20,
        max_read_bytes: 1 << 22,
        max_chunks: 10,
        max_duration: Duration::from_secs(5),
    }
}

fn memory() -> StoreLayout<Store> {
    StoreLayout::new(Store::new(Arc::new(InMemory::new())), "proof".to_owned())
}

#[tokio::test]
async fn verifies_repeated_compressed_content_with_exact_read_budget() {
    let fixture = Fixture::new(false, false);
    let reads = Arc::new(AtomicUsize::new(0));
    let observer = Arc::clone(&reads);
    let store =
        Store::new(Arc::new(InMemory::new())).with_read_request_observer(Arc::new(move |_| {
            observer.fetch_add(1, Ordering::Relaxed);
        }));
    let layout = StoreLayout::new(store, "proof".to_owned());
    fixture.upload(&layout).await;
    let recipe = fixture
        .verify(
            &layout,
            PointerProofLimits {
                max_read_bytes: fixture.read_bytes(),
                ..limits()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (recipe.chunks, reads.load(Ordering::Relaxed)),
        (fixture.chunks, 4)
    );
}

#[tokio::test]
async fn verifies_empty_file_without_xorbs() {
    let fixture = Fixture::new(false, true);
    let layout = memory();
    fixture.upload(&layout).await;
    assert!(
        fixture
            .verify(&layout, limits())
            .await
            .unwrap()
            .chunks
            .is_empty()
    );
}

#[tokio::test]
async fn rejects_missing_and_corrupt_origin_objects() {
    for damage in [
        "missing shard",
        "missing xorb",
        "shard identity",
        "xorb identity",
        "payload digest",
    ] {
        let fixture = Fixture::new(false, false);
        let layout = memory();
        fixture.upload(&layout).await;
        let path = if damage.contains("shard") {
            layout.shard_path(&fixture.shard.0)
        } else {
            layout.xorb_path(&fixture.xorbs[0].0)
        };
        match damage {
            "missing shard" | "missing xorb" => {
                layout.store().delete(&path).await.unwrap();
            }
            "shard identity" => {
                layout
                    .store()
                    .put_overwrite(&path, Bytes::from_static(b"corrupt shard"))
                    .await
                    .unwrap();
            }
            "xorb identity" => {
                layout
                    .store()
                    .put_overwrite(&path, fixture.xorbs[1].1.clone())
                    .await
                    .unwrap();
            }
            _ => {
                let mut bytes = fixture.xorbs[0].1.to_vec();
                bytes[0] ^= 0xff;
                layout
                    .store()
                    .put_overwrite(&path, Bytes::from(bytes))
                    .await
                    .unwrap();
            }
        }
        let error = fixture.verify(&layout, limits()).await.unwrap_err();
        assert!(
            match damage {
                "missing shard" | "missing xorb" => matches!(
                    error,
                    PointerProofError::Storage(StorageError::NotFound { .. })
                ),
                "shard identity" => matches!(
                    error,
                    PointerProofError::Integrity("shard identity mismatch")
                ),
                "xorb identity" => matches!(
                    error,
                    PointerProofError::Integrity("xorb identity mismatch")
                ),
                _ => matches!(error, PointerProofError::Xet(_)),
            },
            "{damage}"
        );
    }
}

#[tokio::test]
async fn rejects_content_that_matches_recipe_but_not_pointer_hash() {
    let fixture = Fixture::new(true, false);
    let layout = memory();
    fixture.upload(&layout).await;
    assert!(matches!(
        fixture.verify(&layout, limits()).await,
        Err(PointerProofError::Integrity("whole-file hash mismatch"))
    ));
}

#[tokio::test]
async fn enforces_each_resource_bound_and_pointer_size() {
    let mut fixture = Fixture::new(false, false);
    let layout = memory();
    fixture.upload(&layout).await;
    for bounded in [
        PointerProofLimits {
            max_file_bytes: fixture.pointer.size - 1,
            ..limits()
        },
        PointerProofLimits {
            max_shard_bytes: fixture.shard.1.len() as u64 - 1,
            ..limits()
        },
        PointerProofLimits {
            max_xorb_bytes: fixture.xorbs[0].1.len() as u64 - 1,
            ..limits()
        },
        PointerProofLimits {
            max_read_bytes: fixture.read_bytes() - 1,
            ..limits()
        },
        PointerProofLimits {
            max_chunks: 2,
            ..limits()
        },
    ] {
        assert!(fixture.verify(&layout, bounded).await.is_err());
    }
    fixture.pointer.size += 1;
    assert!(matches!(
        fixture.verify(&layout, limits()).await,
        Err(PointerProofError::Integrity(
            "recipe size differs from pointer"
        ))
    ));
}

#[tokio::test]
async fn cancellation_and_deadline_interrupt_pending_origin_reads() {
    let fixture = Fixture::new(false, false);
    let store = ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig {
            wait_get_per_call: Duration::from_secs(60),
            ..Default::default()
        },
    );
    let layout = StoreLayout::new(Store::new(Arc::new(store)), "proof".to_owned());
    fixture.upload(&layout).await;
    let cancel = CancellationToken::new();
    let proof = verify_crab_pointer(
        &layout,
        &fixture.pointer,
        fixture.shard.0,
        limits(),
        &cancel,
    );
    let (result, ()) = tokio::join!(proof, async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel.cancel();
    });
    assert!(matches!(result, Err(PointerProofError::Cancelled)));
    assert!(matches!(
        fixture
            .verify(
                &layout,
                PointerProofLimits {
                    max_duration: Duration::from_millis(10),
                    ..limits()
                }
            )
            .await,
        Err(PointerProofError::Deadline)
    ));
}
