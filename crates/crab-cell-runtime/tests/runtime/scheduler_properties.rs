//! Randomized retention properties for maintenance Ticks.
//!
//! A Tick deletes durable rows. The invariant is that it never deletes a row
//! whose deadline has not passed and never deletes an upload that a live object
//! reference still names — the primitive analogue of "GC never deletes
//! referenced data or anything inside the grace period". The queue adds one
//! step: an expired ready message is dead-lettered by the expire class, and the
//! retention class removes it on a later Tick once its effect has settled.

use crab_cell_runtime::cell::schema::install_runtime_schema;
use crab_cell_runtime::fleet::scheduler::scheduler_tick;
use crab_cell_runtime::identity::{
    ApplicationId, CellTarget, IncarnationId, NamespaceId, TenantId,
};
use crab_cell_runtime::primitives::blob::install_blob_schema;
use crab_cell_runtime::primitives::kv::install_kv_schema;
use crab_cell_runtime::primitives::queue::install_queue_schema;
use crab_ltx::rusqlite::{Connection, params};
use proptest::prelude::*;

const NOW_MS: i64 = 1_000;

fn target() -> CellTarget {
    CellTarget::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([2; 16]),
        NamespaceId::from_bytes([3; 16]),
        b"scheduler-retention",
    )
    .unwrap()
}

fn connection() -> Connection {
    let mut connection = Connection::open_in_memory().unwrap();
    install_runtime_schema(
        &mut connection,
        target().cell_id(),
        IncarnationId::from_bytes([2; 16]),
        1,
    )
    .unwrap();
    connection
}

fn count(transaction: &crab_ltx::rusqlite::Transaction<'_>, table: &str) -> usize {
    transaction
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap()
        .try_into()
        .unwrap()
}

fn queue_state_count(transaction: &crab_ltx::rusqlite::Transaction<'_>, state: i64) -> usize {
    transaction
        .query_row(
            "SELECT count(*) FROM queue_messages WHERE state = ?1",
            [state],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
        .try_into()
        .unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 16, ..ProptestConfig::default() })]

    #[test]
    fn tick_keeps_rows_inside_retention_and_removes_expired_garbage(
        kv_expiry in prop::collection::vec(prop::option::of(-64i32..=64), 0..=6),
        blob_rows in prop::collection::vec((any::<bool>(), -64i32..=64), 0..=6),
        queue_expiry in prop::collection::vec(-64i32..=64, 0..=6),
    ) {
        let mut connection = connection();
        let transaction = connection.transaction().unwrap();
        install_kv_schema(&transaction).unwrap();
        install_queue_schema(&transaction).unwrap();
        install_blob_schema(&transaction).unwrap();

        let mut live_kv = 0;
        for (index, offset) in kv_expiry.iter().enumerate() {
            let expires = offset.map(|offset| NOW_MS + i64::from(offset));
            transaction
                .execute(
                    "INSERT INTO kv_entries(scope, key, version, value, expires_at_ms) \
                     VALUES (X'01', ?1, zeroblob(28), X'', ?2)",
                    params![vec![u8::try_from(index).unwrap() + 1], expires],
                )
                .unwrap();
            if expires.is_none_or(|expires| expires > NOW_MS) {
                live_kv += 1;
            }
        }

        let mut live_blobs = 0;
        for (index, (referenced, offset)) in blob_rows.iter().enumerate() {
            let upload_id = vec![u8::try_from(index).unwrap() + 1; 16];
            let object_key = vec![u8::try_from(index).unwrap() + 1];
            let expires = NOW_MS + i64::from(*offset);
            transaction
                .execute(
                    "INSERT INTO blob_uploads VALUES \
                     (?1, ?2, zeroblob(32), 0, NULL, NULL, X'', 0, ?3, 0, NULL, 0, 0)",
                    params![upload_id, object_key, expires],
                )
                .unwrap();
            if *referenced {
                transaction
                    .execute(
                        "INSERT INTO blob_objects VALUES \
                         (?1, ?2, zeroblob(32), 0, 1, NULL, X'', 0, 0)",
                        params![object_key, upload_id],
                    )
                    .unwrap();
            }
            // An expired upload survives while an object still names it.
            if expires > NOW_MS || *referenced {
                live_blobs += 1;
            }
        }

        let mut live_queue = 0;
        for (index, offset) in queue_expiry.iter().enumerate() {
            let expires = NOW_MS + i64::from(*offset);
            transaction
                .execute(
                    "INSERT INTO queue_messages(message_id, payload, state, attempt, \
                     due_at_ms, expires_at_ms, token, lease_until_ms, result_code, \
                     dead_letter_effect_id) VALUES (?1, X'', 0, 0, ?2, ?3, NULL, NULL, NULL, NULL)",
                    params![
                        vec![u8::try_from(index).unwrap() + 1; 16],
                        NOW_MS + 500,
                        expires
                    ],
                )
                .unwrap();
            if expires > NOW_MS {
                live_queue += 1;
            }
        }

        scheduler_tick(&transaction, &target(), NOW_MS, &[]).unwrap();

        prop_assert_eq!(
            count(&transaction, "kv_entries"),
            live_kv,
            "KV rows inside retention must survive the Tick"
        );
        prop_assert_eq!(
            count(&transaction, "blob_uploads"),
            live_blobs,
            "uploads inside retention or named by an object must survive the Tick"
        );
        // Expired ready messages are dead-lettered first; the cleanup class runs
        // before that transition, so their removal lands on the next Tick.
        prop_assert_eq!(
            queue_state_count(&transaction, 0),
            live_queue,
            "queue messages inside retention must stay ready"
        );
        prop_assert_eq!(
            queue_state_count(&transaction, 3),
            queue_expiry.len() - live_queue,
            "expired ready messages must be dead-lettered"
        );

        scheduler_tick(&transaction, &target(), NOW_MS, &[]).unwrap();
        prop_assert_eq!(
            count(&transaction, "kv_entries"),
            live_kv,
            "a settled Tick must not remove KV rows it already kept"
        );
        prop_assert_eq!(
            count(&transaction, "blob_uploads"),
            live_blobs,
            "a settled Tick must not remove uploads it already kept"
        );
        prop_assert_eq!(
            count(&transaction, "queue_messages"),
            live_queue,
            "the retention cleanup must remove dead-lettered messages"
        );
    }
}
