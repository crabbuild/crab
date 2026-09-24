//! Command dispatch, compaction, and publication ordering.

use super::*;

#[tokio::test]
async fn dispatcher_serializes_and_publishes_commands_before_drain() {
    let fixture = fixture();
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    let first = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .execute(
                    mutation_identity_window(6, 10, 10_000),
                    Digest::from_bytes([7; 32]),
                    20,
                    1_024,
                    1_024,
                    |transaction| {
                        transaction.execute("UPDATE counter SET value = value + 1", [])?;
                        Ok(HandlerOutcome::Success(b"one".to_vec()))
                    },
                )
                .await
        })
    };
    let second = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .execute(
                    mutation_identity_window(8, 10, 10_000),
                    Digest::from_bytes([9; 32]),
                    21,
                    1_024,
                    1_024,
                    |transaction| {
                        transaction.execute("UPDATE counter SET value = value + 1", [])?;
                        Ok(HandlerOutcome::Success(b"two".to_vec()))
                    },
                )
                .await
        })
    };
    assert!(matches!(
        first.await.unwrap().unwrap(),
        StoredOutcome::Success { ref result, commit_sequence: 1 } if result == b"one"
    ));
    assert!(matches!(
        second.await.unwrap().unwrap(),
        StoredOutcome::Success { ref result, commit_sequence: 2 } if result == b"two"
    ));
    handle.drain().await.unwrap();

    let released = CellAuthority::new(fixture.layout.clone())
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(released.value().state, ControlState::Idle);
    assert!(released.value().owner.is_none());
    assert_eq!(
        released.value().next_due_ms,
        Some(10_000 + 24 * 60 * 60 * 1000)
    );

    let connection = crab_ltx::rusqlite::Connection::open(&fixture.database).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        2
    );
}
#[tokio::test]
async fn dispatcher_compacts_before_segment_admission_is_exhausted() {
    let fixture = fixture_with_limits(
        b"compacting-repository",
        Limits {
            max_segments: 4,
            ..Limits::default()
        },
    );
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    for sequence in 1_u8..=10 {
        assert!(matches!(
            handle
                .execute(
                    mutation_identity_window(sequence, 10, 10_000),
                    Digest::from_bytes([sequence.saturating_add(20); 32]),
                    20,
                    1_024,
                    1_024,
                    |transaction| {
                        transaction.execute("UPDATE counter SET value = value + 1", [])?;
                        Ok(HandlerOutcome::Success(Vec::new()))
                    },
                )
                .await
                .unwrap(),
            StoredOutcome::Success {
                commit_sequence,
                ..
            } if commit_sequence == u64::from(sequence)
        ));
    }

    let authority = CellAuthority::new(fixture.layout.clone());
    let control = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let root = control.value().ltx_root().unwrap();
    assert!(
        fixture
            .replica
            .open_root(&root)
            .await
            .unwrap()
            .segment_count()
            < 4
    );
    handle.drain().await.unwrap();
    let restored_directory = tempfile::TempDir::new().unwrap();
    let restored = restored_directory.path().join("restored.sqlite");
    fixture
        .replica
        .open_root(&root)
        .await
        .unwrap()
        .restore(&restored)
        .await
        .unwrap();
    let connection = crab_ltx::rusqlite::Connection::open(restored).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        10
    );
}
#[tokio::test]
async fn dispatcher_promotes_after_burst_becomes_quiet() {
    let pausing = Arc::new(PausingStore::new(Arc::new(InMemory::new())));
    let store: Arc<dyn ObjectStore> = pausing.clone();
    let fixture = fixture_with_limits_and_store(
        b"quiet-compaction-runtime",
        Limits::default(),
        Store::with_retry(
            store,
            RetryPolicy {
                max_attempts: 1,
                base: std::time::Duration::from_millis(1),
                cap: std::time::Duration::from_millis(1),
            },
        ),
    );
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    for sequence in 1_u8..=8 {
        handle
            .execute(
                mutation_identity_window(sequence, 10, 10_000),
                Digest::from_bytes([sequence.saturating_add(30); 32]),
                20,
                1_024,
                1_024,
                |transaction| {
                    transaction.execute("UPDATE counter SET value = value + 1", [])?;
                    Ok(HandlerOutcome::Success(Vec::new()))
                },
            )
            .await
            .unwrap();
    }

    let authority = CellAuthority::new(fixture.layout.clone());
    let before = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap()
        .value()
        .ltx_root()
        .unwrap();
    let initial_segments = fixture
        .replica
        .open_root(&before)
        .await
        .unwrap()
        .segment_count();
    assert!(initial_segments >= 8);
    pausing.fail_next_put_transiently();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        pausing.wait_until_failed(),
    )
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        authority
            .load(fixture.target.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .ltx_root(),
        Some(before)
    );

    let promoted = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let root = authority
                .load(fixture.target.cell_id())
                .await
                .unwrap()
                .unwrap()
                .value()
                .ltx_root()
                .unwrap();
            if fixture
                .replica
                .open_root(&root)
                .await
                .unwrap()
                .segment_count()
                < initial_segments
            {
                break root;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(promoted.position, before.position);
    assert_eq!(promoted.commit_sequence, before.commit_sequence);
    handle.drain().await.unwrap();
    let restored_directory = tempfile::TempDir::new().unwrap();
    let restored = restored_directory.path().join("restored.sqlite");
    fixture
        .replica
        .open_root(&promoted)
        .await
        .unwrap()
        .restore(&restored)
        .await
        .unwrap();
    let connection = crab_ltx::rusqlite::Connection::open(restored).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        8
    );
}
#[tokio::test(flavor = "multi_thread")]
async fn command_arriving_during_quiet_compaction_waits_for_publisher() {
    let pausing = Arc::new(PausingStore::new(Arc::new(InMemory::new())));
    let store: Arc<dyn ObjectStore> = pausing.clone();
    let fixture = fixture_with_limits_and_store(
        b"quiet-compaction-queued-command",
        Limits::default(),
        Store::new(store),
    );
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    for sequence in 1_u8..=8 {
        handle
            .execute(
                mutation_identity_window(sequence, 10, 10_000),
                Digest::from_bytes([sequence.saturating_add(70); 32]),
                20,
                1_024,
                1_024,
                |transaction| {
                    transaction.execute("UPDATE counter SET value = value + 1", [])?;
                    Ok(HandlerOutcome::Success(Vec::new()))
                },
            )
            .await
            .unwrap();
    }
    pausing.arm();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        pausing.wait_until_blocked(),
    )
    .await
    .unwrap();
    let ninth = handle.execute(
        mutation_identity_window(9, 10, 10_000),
        Digest::from_bytes([79; 32]),
        20,
        1_024,
        1_024,
        |transaction| {
            transaction.execute("UPDATE counter SET value = value + 1", [])?;
            Ok(HandlerOutcome::Success(Vec::new()))
        },
    );
    tokio::pin!(ninth);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut ninth)
            .await
            .is_err()
    );
    pausing.release();
    assert_eq!(ninth.await.unwrap().commit_sequence(), 9);
    handle.drain().await.unwrap();
    let root = CellAuthority::new(fixture.layout.clone())
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap()
        .value()
        .ltx_root()
        .unwrap();
    let restored_directory = tempfile::TempDir::new().unwrap();
    let restored = restored_directory.path().join("restored.sqlite");
    fixture
        .replica
        .open_root(&root)
        .await
        .unwrap()
        .restore(&restored)
        .await
        .unwrap();
    let connection = crab_ltx::rusqlite::Connection::open(restored).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        9
    );
}
