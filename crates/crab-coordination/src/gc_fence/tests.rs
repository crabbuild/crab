use super::*;
use object_store::{ObjectStoreExt, memory::InMemory};

fn memory_store() -> Arc<dyn ObjectStore> {
    Arc::new(InMemory::new())
}
const TTL: Duration = Duration::from_secs(30);

fn writer_key(lease: &GcFenceLease) -> &str {
    let Claim::Writer { path, .. } = &lease.inner.claim else {
        panic!("expected writer");
    };
    path
}

async fn expire(store: &Arc<dyn ObjectStore>, lease: &GcFenceLease, expiry: i64) {
    let (mut control, _) = load_control(store, &lease.inner.path).await.unwrap();
    match &lease.inner.claim {
        Claim::Writer { path, .. } => {
            let (mut slot, _) = load_slot(store, path, &control).await.unwrap();
            slot.writer.as_mut().unwrap().expires_at_backend = expiry;
            store
                .put(
                    &Path::from(path.as_str()),
                    encode(path, &slot).unwrap().into(),
                )
                .await
                .unwrap();
        }
        Claim::Sweep { .. } => {
            control.sweep.as_mut().unwrap().expires_at_backend = expiry;
            store
                .put(
                    &Path::from(lease.inner.path.as_str()),
                    encode(&lease.inner.path, &control).unwrap().into(),
                )
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn writer_and_sweep_are_mutually_exclusive() {
    let store = memory_store();
    let writer = GcFenceLease::acquire_writer(&store, "repo", TTL)
        .await
        .unwrap();
    assert!(matches!(
        GcFenceLease::acquire_sweep(&store, "repo", TTL).await,
        Err(CoordinationError::GcFenceHeld { sweep: false, .. })
    ));
    writer.renew().await.unwrap();
    writer.release().await.unwrap();
    let sweep = GcFenceLease::acquire_sweep(&store, "repo", TTL)
        .await
        .unwrap();
    assert!(matches!(
        GcFenceLease::acquire_writer(&store, "repo", TTL).await,
        Err(CoordinationError::GcFenceHeld { sweep: true, .. })
    ));
    assert!(matches!(
        GcFenceLease::acquire_sweep(&store, "repo", TTL).await,
        Err(CoordinationError::GcFenceHeld { sweep: true, .. })
    ));
    sweep.renew().await.unwrap();
    sweep.release().await.unwrap();
}

#[tokio::test]
async fn all_writer_slots_are_usable_and_released() {
    let store = memory_store();
    let mut leases = Vec::new();
    for _ in 0..GC_FENCE_MAX_WRITERS {
        leases.push(
            GcFenceLease::acquire_writer(&store, "repo", TTL)
                .await
                .unwrap(),
        );
    }
    assert!(matches!(
        GcFenceLease::acquire_writer(&store, "repo", TTL).await,
        Err(CoordinationError::GcFenceHeld { sweep: false, .. })
    ));
    let keys: std::collections::HashSet<_> = leases.iter().map(writer_key).collect();
    assert_eq!(keys.len(), GC_FENCE_MAX_WRITERS);
    for lease in leases {
        lease.release().await.unwrap();
    }
    let sweep = GcFenceLease::acquire_sweep(&store, "repo", TTL)
        .await
        .unwrap();
    assert_eq!(sweep.writer_epoch().unwrap().epoch, 128);
    sweep.release().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_first_use_initializes_once_and_keeps_every_history() {
    let store = memory_store();
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..16 {
                let writer = GcFenceLease::acquire_writer(&store, "repo", TTL)
                    .await
                    .unwrap();
                writer.renew().await.unwrap();
                writer.release().await.unwrap();
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let sweep = GcFenceLease::acquire_sweep(&store, "repo", TTL)
        .await
        .unwrap();
    assert_eq!(sweep.writer_epoch().unwrap().epoch, 512);
    sweep.release().await.unwrap();
}

#[tokio::test]
async fn ordinary_writer_lifecycle_never_mutates_control() {
    let store = memory_store();
    let path = gc_fence_path("repo").unwrap();
    ready_control(&store, &path).await.unwrap();
    let (before, version) = get_with_version(&store, &Path::from(path.as_str()))
        .await
        .unwrap();
    for committed in [false, true] {
        let writer = GcFenceLease::acquire_writer(&store, "repo", TTL)
            .await
            .unwrap();
        writer.renew().await.unwrap();
        if committed {
            writer.release_committed_writer().await.unwrap();
        } else {
            writer.release().await.unwrap();
        }
    }
    let after = get_with_version(&store, &Path::from(path.as_str()))
        .await
        .unwrap();
    assert_eq!(after, (before, version));
}

#[tokio::test]
async fn history_ignores_sweeps_and_detects_every_writer_crossing() {
    let store = memory_store();
    let mut histories = Vec::new();
    for round in 0..3 {
        let sweep = GcFenceLease::acquire_sweep(&store, "repo", TTL)
            .await
            .unwrap();
        histories.push(sweep.writer_epoch().unwrap());
        sweep.release().await.unwrap();
        if round == 1 {
            let writer = GcFenceLease::acquire_writer(&store, "repo", TTL)
                .await
                .unwrap();
            assert!(writer.writer_epoch().is_err());
            writer.renew().await.unwrap();
            writer.release().await.unwrap();
        }
    }
    assert_eq!(histories[0], histories[1]);
    assert_eq!(histories[2].epoch, histories[1].epoch + 2);
}

#[tokio::test]
async fn read_barrier_ignores_writers_and_rejects_completed_sweeps() {
    let store = memory_store();
    let initial = GcFenceLease::acquire_writer(&store, "repo", TTL)
        .await
        .unwrap();
    initial.release().await.unwrap();
    let capture = GcReadBarrier::observe(store.clone(), "repo").await.unwrap();
    for committed in [false, true] {
        let writer = GcFenceLease::acquire_writer(&store, "repo", TTL)
            .await
            .unwrap();
        writer.renew().await.unwrap();
        capture.validate().await.unwrap();
        if committed {
            writer.release_committed_writer().await.unwrap();
        } else {
            writer.release().await.unwrap();
        }
        capture.validate().await.unwrap();
    }
    let sweep = GcFenceLease::acquire_sweep(&store, "repo", TTL)
        .await
        .unwrap();
    assert!(capture.validate().await.is_err());
    sweep.release().await.unwrap();
    assert!(matches!(
        capture.validate().await,
        Err(CoordinationError::CasConflict { .. })
    ));
}

#[tokio::test]
async fn recreated_domains_and_uncertain_sweeps_reject_read_capture() {
    let store = memory_store();
    let absent = GcReadBarrier::observe(store.clone(), "repo").await.unwrap();
    let path = gc_fence_path("repo").unwrap();
    ready_control(&store, &path).await.unwrap();
    assert!(absent.validate().await.is_err());
    let captured = GcReadBarrier::observe(store.clone(), "repo").await.unwrap();
    let new = Control::empty();
    store
        .put(
            &Path::from(path.as_str()),
            encode(&path, &new).unwrap().into(),
        )
        .await
        .unwrap();
    assert!(captured.validate().await.is_err());
    assert!(GcReadBarrier::observe(store.clone(), "repo").await.is_err());
    ready_control(&store, &path).await.unwrap();
    let sweep = GcFenceLease::acquire_sweep(&store, "repo", TTL)
        .await
        .unwrap();
    expire(&store, &sweep, 1).await;
    assert!(GcReadBarrier::observe(store, "repo").await.is_err());
}

#[tokio::test]
async fn expired_writer_blocks_sweep_until_quarantine_or_rooted_release() {
    for committed in [false, true] {
        let store = memory_store();
        let writer = GcFenceLease::acquire_writer(&store, "repo", TTL)
            .await
            .unwrap();
        let now = backend_unix_time(&store, &Path::from(writer_key(&writer)))
            .await
            .unwrap();
        expire(&store, &writer, now - 1).await;
        assert!(writer.renew().await.is_err());
        assert!(writer.release().await.is_err());
        assert!(matches!(
            GcFenceLease::acquire_sweep(&store, "repo", TTL).await,
            Err(CoordinationError::GcFenceHeld { sweep: false, .. })
        ));
        if committed {
            writer.release_committed_writer().await.unwrap();
        } else {
            expire(
                &store,
                &writer,
                now - DEFAULT_GC_FENCE_QUARANTINE.as_secs() as i64 - 1,
            )
            .await;
        }
        let sweep = GcFenceLease::acquire_sweep(&store, "repo", TTL)
            .await
            .unwrap();
        sweep.release().await.unwrap();
    }
}

#[tokio::test]
async fn renewal_and_release_reject_recreated_or_expired_claims() {
    for mode in [GcFenceMode::Writer, GcFenceMode::Sweep] {
        for recreated in [false, true] {
            let store = memory_store();
            let lease = match mode {
                GcFenceMode::Writer => GcFenceLease::acquire_writer(&store, "repo", TTL)
                    .await
                    .unwrap(),
                GcFenceMode::Sweep => GcFenceLease::acquire_sweep(&store, "repo", TTL)
                    .await
                    .unwrap(),
            };
            if recreated {
                let (mut control, _) = load_control(&store, &lease.inner.path).await.unwrap();
                control.incarnation = uuid::Uuid::now_v7().to_string();
                store
                    .put(
                        &Path::from(lease.inner.path.as_str()),
                        encode(&lease.inner.path, &control).unwrap().into(),
                    )
                    .await
                    .unwrap();
            } else {
                expire(&store, &lease, 1).await;
            }
            assert!(matches!(
                lease.renew().await,
                Err(CoordinationError::GcFenceLost { .. })
            ));
            assert!(matches!(
                lease.release().await,
                Err(CoordinationError::GcFenceLost { .. })
            ));
        }
    }
}

#[tokio::test]
async fn renewal_and_release_are_holder_checked_and_idempotent() {
    for mode in [GcFenceMode::Writer, GcFenceMode::Sweep] {
        let store = memory_store();
        let lease = match mode {
            GcFenceMode::Writer => GcFenceLease::acquire_writer(&store, "repo", TTL)
                .await
                .unwrap(),
            GcFenceMode::Sweep => GcFenceLease::acquire_sweep(&store, "repo", TTL)
                .await
                .unwrap(),
        };
        let clone = lease.clone();
        lease.renew().await.unwrap();
        lease.release().await.unwrap();
        clone.release().await.unwrap();
        assert!(clone.renew().await.is_err());
    }
}

#[tokio::test]
async fn resumable_sweep_reclaims_only_its_expired_incarnation() {
    let store = memory_store();
    let holder = "gc-run-019c1234-abcd-7000-8000-123456789abc";
    let old = GcFenceLease::acquire_resumable_sweep(&store, "repo", holder, TTL)
        .await
        .unwrap();
    let history = old.writer_epoch().unwrap();
    assert!(
        GcFenceLease::acquire_resumable_sweep(&store, "repo", holder, TTL)
            .await
            .is_err()
    );
    let now = backend_unix_time(&store, &Path::from(old.inner.path.as_str()))
        .await
        .unwrap();
    expire(&store, &old, now - 1).await;
    assert!(
        GcFenceLease::acquire_resumable_sweep(&store, "repo", "gc-run-other", TTL)
            .await
            .is_err()
    );
    let recovered = GcFenceLease::acquire_resumable_sweep(&store, "repo", holder, TTL)
        .await
        .unwrap();
    assert_eq!(recovered.writer_epoch().unwrap(), history);
    assert!(old.renew().await.is_err());
    assert!(old.release().await.is_err());
    recovered.release().await.unwrap();
}

#[tokio::test]
async fn initialized_slot_loss_never_becomes_empty_history() {
    let store = memory_store();
    let writer = GcFenceLease::acquire_writer(&store, "repo", TTL)
        .await
        .unwrap();
    let key = writer_key(&writer).to_owned();
    writer.release().await.unwrap();
    store.delete(&Path::from(key)).await.unwrap();
    assert!(matches!(
        GcFenceLease::acquire_sweep(&store, "repo", TTL).await,
        Err(CoordinationError::ObjectStore {
            source: object_store::Error::NotFound { .. },
            ..
        })
    ));
    let (control, _) = load_control(&store, &gc_fence_path("repo").unwrap())
        .await
        .unwrap();
    assert!(control.sweep.is_none());
}

#[tokio::test]
async fn initialization_resumes_only_zero_ledgers() {
    for dirty in [false, true] {
        let store = memory_store();
        let control = Control::empty();
        let path = gc_fence_path("repo").unwrap();
        store
            .put(
                &Path::from(path.as_str()),
                encode(&path, &control).unwrap().into(),
            )
            .await
            .unwrap();
        for index in 0..4 {
            let key = slot_path(&path, &control.incarnation, index);
            let mut slot = Slot::empty(&control);
            if dirty {
                slot.epoch = 2;
            }
            store
                .put(
                    &Path::from(key.as_str()),
                    encode(&key, &slot).unwrap().into(),
                )
                .await
                .unwrap();
        }
        let writer = GcFenceLease::acquire_writer(&store, "repo", TTL).await;
        if dirty {
            assert!(writer.is_err());
        } else {
            writer.unwrap().release().await.unwrap();
        }
        assert_eq!(load_control(&store, &path).await.unwrap().0.ready, !dirty);
    }
}

#[tokio::test]
async fn writer_history_budget_reserves_release_and_never_wraps() {
    let store = memory_store();
    let path = gc_fence_path("repo").unwrap();
    let mut control = Control::empty();
    control.writer_epoch = u64::MAX - 128;
    store
        .put(
            &Path::from(path.as_str()),
            encode(&path, &control).unwrap().into(),
        )
        .await
        .unwrap();
    let mut writers = Vec::new();
    for _ in 0..64 {
        writers.push(
            GcFenceLease::acquire_writer(&store, "repo", TTL)
                .await
                .unwrap(),
        );
    }
    for writer in writers {
        writer.release().await.unwrap();
    }
    let sweep = GcFenceLease::acquire_sweep(&store, "repo", TTL)
        .await
        .unwrap();
    assert_eq!(sweep.writer_epoch().unwrap().epoch, u64::MAX);
    sweep.release().await.unwrap();
    assert!(matches!(
        GcFenceLease::acquire_writer(&store, "repo", TTL).await,
        Err(CoordinationError::GcFenceMalformed { .. })
    ));
}

#[tokio::test]
async fn cancelled_writer_keeps_renewing_until_drained() {
    let store = memory_store();
    let writer = GcFenceLease::acquire_writer(&store, "repo", Duration::from_secs(3))
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let heartbeat = GcFenceHeartbeat::spawn(&writer, cancel.clone(), Duration::from_secs(1));
    cancel.cancel();
    tokio::time::sleep(Duration::from_secs(4)).await;
    heartbeat.stop().await;
    writer.release().await.unwrap();
    let sweep = GcFenceLease::acquire_sweep(&store, "repo", TTL)
        .await
        .unwrap();
    sweep.release().await.unwrap();
}

#[test]
fn missing_nullable_authority_is_malformed_not_released() {
    let control = Control::empty();
    for field in ["sweep", "acquisition"] {
        let mut value = serde_json::to_value(&control).unwrap();
        value.as_object_mut().unwrap().remove(field);
        assert!(
            decode::<Control>("control", &serde_json::to_vec(&value).unwrap()).is_err(),
            "{field}"
        );
    }
    let mut value = serde_json::to_value(Slot::empty(&control)).unwrap();
    value.as_object_mut().unwrap().remove("writer");
    assert!(decode::<Slot>("slot", &serde_json::to_vec(&value).unwrap()).is_err());
}
