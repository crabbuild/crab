use super::*;

fn write_policy(root: &Path, value: &str) {
    open_catalog(root)
        .unwrap()
        .execute(
            "INSERT INTO catalog_meta(key, value) VALUES ('retention_budget_bytes', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [value],
        )
        .unwrap();
}

#[tokio::test]
async fn persisted_budget_fences_live_handles_and_preserves_caller_limits() {
    const MIB: u64 = 1024 * 1024;
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("cache");
    let unlimited = CacheCatalog::new(root.clone(), None);
    let bounded = CacheCatalog::new(root.clone(), 8 * MIB);
    let caller = CacheCatalog::new(root.clone(), MIB);
    let held = unlimited
        .reserve(&root.join("held"), 3 * MIB)
        .await
        .unwrap()
        .unwrap();

    // A separate policy writer changes the root while these handles remain
    // live. Their constructor limits cannot replace the shared authority.
    write_policy(&root, &(4 * MIB).to_string());
    assert!(
        bounded
            .reserve(&root.join("other"), 2 * MIB)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        CacheCatalog::read_only_stats(&root)
            .unwrap()
            .reservations_bytes,
        3 * MIB
    );

    write_policy(&root, &(8 * MIB).to_string());
    let admitted = bounded
        .reserve(&root.join("other"), 2 * MIB)
        .await
        .unwrap()
        .unwrap();
    assert!(
        caller
            .reserve(&root.join("caller"), 2 * MIB)
            .await
            .unwrap()
            .is_none()
    );
    drop(admitted);
    drop(held);

    write_policy(&root, "unlimited");
    assert!(
        unlimited
            .reserve(&root.join("unlimited"), 11 * 1024 * MIB)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn malformed_persisted_budget_cannot_disable_admission() {
    for value in ["invalid", "-1", "18446744073709551616"] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("cache");
        let catalog = CacheCatalog::new(root.clone(), None);
        write_policy(&root, value);
        assert!(catalog.reserve(&root.join("payload"), 1).await.is_err());
    }
}

#[tokio::test]
async fn busy_maintenance_keeps_the_previous_policy() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("cache");
    let catalog = CacheCatalog::new(root.clone(), None);
    catalog.set_retention_budget(Some(1024)).await.unwrap();
    let pinned = PinnedRoot::open(&root).unwrap();
    let owner = pinned.open_lock(Path::new(MAINTENANCE_LOCK)).unwrap();
    assert!(owner.try_lock_exclusive().unwrap());

    assert!(matches!(
        catalog.set_retention_budget(Some(0)).await,
        Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    assert_eq!(
        CacheCatalog::read_only_stats(&root)
            .unwrap()
            .retention_budget_bytes,
        Some(1024)
    );
}

#[tokio::test]
async fn rejected_policy_write_preserves_the_saved_limit() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("cache");
    let catalog = CacheCatalog::new(root.clone(), None);
    catalog.set_retention_budget(Some(1024)).await.unwrap();
    open_catalog(&root)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_policy BEFORE UPDATE ON catalog_meta
             BEGIN SELECT RAISE(ABORT, 'policy record unavailable'); END;",
        )
        .unwrap();

    let rejected = catalog.set_retention_budget(Some(0)).await.is_err();
    let saved = CacheCatalog::read_only_stats(&root)
        .unwrap()
        .retention_budget_bytes;
    assert_eq!((rejected, saved), (true, Some(1024)));
}

#[tokio::test]
async fn zero_budget_preserves_live_readers_and_reservations() {
    const MIB: u64 = 1024 * 1024;
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("cache");
    let catalog = CacheCatalog::new(root.clone(), None);
    let active = payload(&root, 1, MIB as usize).await;
    let (read, descriptor) = PayloadRead::open(&root, &active).await.unwrap();
    let held = catalog
        .reserve(&root.join("incoming"), MIB)
        .await
        .unwrap()
        .unwrap();

    catalog.set_retention_budget(Some(0)).await.unwrap();
    catalog.maintain().await.unwrap();
    assert_eq!(std::fs::read(&active).unwrap(), vec![1; MIB as usize]);
    assert_eq!(
        CacheCatalog::read_only_stats(&root)
            .unwrap()
            .reservations_bytes,
        MIB
    );
    assert!(
        catalog
            .reserve(&root.join("another"), 1)
            .await
            .unwrap()
            .is_none()
    );

    drop(held);
    drop(descriptor);
    drop(read);
    catalog.maintain().await.unwrap();
    assert!(!active.exists());
    let before = CacheCatalog::read_only_stats(&root).unwrap();
    assert_eq!(before.retention_budget_bytes, Some(0));
    assert_eq!(CacheCatalog::read_only_stats(&root).unwrap(), before);
}

#[tokio::test]
async fn budget_evicts_git_packs_and_advisory_bloom_files() {
    const MIB: u64 = 1024 * 1024;
    let bytes = vec![3; MIB as usize / 2];
    let hash = blake3::hash(&bytes).to_hex().to_string();
    for relative in [
        format!("git-packs/{}/{hash}", &hash[..2]),
        "hints/clean-bloom.bin".to_owned(),
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("cache");
        let old = root.join(relative);
        crate::private_fs::atomic_write(&root, &old, &bytes)
            .await
            .unwrap();
        let catalog = CacheCatalog::new(root.clone(), 4 * MIB);
        catalog.maintain().await.unwrap();

        let reservation = catalog
            .reserve(&root.join("incoming"), 7 * MIB / 2)
            .await
            .unwrap();
        assert_eq!((reservation.is_some(), old.exists()), (true, false));
    }
}

#[tokio::test]
async fn shared_namespace_budget_preserves_other_identity_leases() {
    const MIB: usize = 1024 * 1024;
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("cache");
    let catalog = CacheCatalog::new(root.clone(), None);
    let mut paths = Vec::new();
    for (namespace, byte, size) in [('a', 1, 4 * MIB), ('b', 2, MIB)] {
        let bytes = vec![byte; size];
        let hash = crab_xet::hash::compute_data_hash(&bytes).hex();
        let path = root
            .join("namespaces")
            .join(namespace.to_string().repeat(64))
            .join("shards")
            .join(&hash[..2])
            .join(hash);
        crate::private_fs::atomic_write(&root, &path, &bytes)
            .await
            .unwrap();
        paths.push(path);
    }
    let lease = catalog.lease(&paths[1]).await.unwrap();
    catalog
        .set_retention_budget(Some(8 * MIB as u64))
        .await
        .unwrap();
    catalog.maintain().await.unwrap();
    let unknown = root.join("namespaces/unknown/retained");
    crate::private_fs::atomic_write(&root, &unknown, b"retain")
        .await
        .unwrap();
    let bytes = vec![3; 4 * MIB];
    let hash = crab_xet::hash::compute_data_hash(&bytes).hex();
    let path = root.join("shards").join(&hash[..2]).join(&hash);
    let reservation = catalog.reserve(&path, bytes.len() as u64).await.unwrap();
    assert!(reservation.is_some(), "namespace payload was not reclaimed");
    let reservation = reservation.unwrap().write(&bytes).await.unwrap();
    catalog
        .record_and_maintain("shard", hash, bytes.len() as u64, reservation)
        .await
        .unwrap();
    assert!(!paths[0].exists());
    assert_eq!(std::fs::read(&paths[1]).unwrap(), vec![2; MIB]);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let cleaned = crate::clean_cache(&root, false, &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(cleaned.files_removed, 1);
    assert!(paths[1].exists());
    assert_eq!(std::fs::read(&unknown).unwrap(), b"retain");
    drop(lease);
}
