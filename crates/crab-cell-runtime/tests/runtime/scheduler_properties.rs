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

fn state_count(
    transaction: &crab_ltx::rusqlite::Transaction<'_>,
    table: &str,
    state: i64,
) -> usize {
    transaction
        .query_row(
            &format!("SELECT count(*) FROM {table} WHERE state = ?1"),
            [state],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
        .try_into()
        .unwrap()
}

/// Effect expiries are either already past (including the deadline itself) or
/// far enough ahead that the reclaim retry delay cannot consume them.
fn effect_expiry() -> impl Strategy<Value = i32> {
    prop_oneof![-64i32..=0, 60_000i32..=120_000]
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

        scheduler_tick(&transaction, &target(), NOW_MS, &[], None, &[]).unwrap();

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

        scheduler_tick(&transaction, &target(), NOW_MS, &[], None, &[]).unwrap();
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

    #[test]
    fn tick_settles_effect_expiry_and_reclaims_only_expired_leases(
        ready_expiry in prop::collection::vec(effect_expiry(), 0..=4),
        leases in prop::collection::vec((any::<bool>(), effect_expiry()), 0..=4),
        terminal in prop::collection::vec((any::<bool>(), effect_expiry()), 0..=4),
    ) {
        let mut connection = connection();
        let transaction = connection.transaction().unwrap();

        let mut next = 0_u8;
        let mut insert = |state: i64, expiry_offset: i32, lease_expired: bool| {
            next += 1;
            let (token, lease_until_ms) = if state == 1 {
                let lease = if lease_expired { -10 } else { 10 };
                (Some(vec![next; 16]), Some(NOW_MS + lease))
            } else {
                (None, None)
            };
            transaction
                .execute(
                    "INSERT INTO sys_effects(effect_id, destination, operation, state, attempt, \
                     due_at_ms, expires_at_ms, token, lease_until_ms, created_sequence, result) \
                     VALUES (?1, zeroblob(32), X'', ?2, 0, ?3, ?4, ?5, ?6, 1, NULL)",
                    params![
                        vec![next; 32],
                        state,
                        NOW_MS - 100,
                        NOW_MS + i64::from(expiry_offset),
                        token,
                        lease_until_ms
                    ],
                )
                .unwrap();
        };

        for offset in &ready_expiry {
            insert(0, *offset, false);
        }
        for (lease_expired, offset) in &leases {
            insert(1, *offset, *lease_expired);
        }
        for (failed, offset) in &terminal {
            insert(if *failed { 3 } else { 2 }, *offset, false);
        }

        let live = |offset: &i32| NOW_MS + i64::from(*offset) > NOW_MS;
        let live_ready = ready_expiry.iter().filter(|offset| live(offset)).count();
        let expired_ready = ready_expiry.len() - live_ready;
        // A live lease keeps its row regardless of the effect's own expiry: only
        // the expired lease returns the row to ready (or fails an expired one).
        let live_leases = leases
            .iter()
            .filter(|(lease_expired, _)| !*lease_expired)
            .count();
        let reclaimed =
            leases.iter().filter(|(lease_expired, offset)| *lease_expired && live(offset)).count();
        let failed_reclaims =
            leases.iter().filter(|(lease_expired, offset)| *lease_expired && !live(offset)).count();
        let live_done = terminal.iter().filter(|(failed, offset)| !*failed && live(offset)).count();
        let live_failed = terminal.iter().filter(|(failed, offset)| *failed && live(offset)).count();
        let expired_terminal =
            terminal.iter().filter(|(_, offset)| !live(offset)).count();
        let survives = ready_expiry.len()
            + leases.len()
            + terminal.len()
            - expired_terminal;

        // First Tick: expired ready rows fail, a live lease is never reclaimed,
        // and only terminal rows past retention are removed immediately.
        scheduler_tick(&transaction, &target(), NOW_MS, &[], None, &[]).unwrap();
        prop_assert_eq!(
            state_count(&transaction, "sys_effects", 0),
            live_ready + reclaimed,
            "ready rows inside retention stay ready and expired leases return to ready"
        );
        prop_assert_eq!(
            state_count(&transaction, "sys_effects", 1),
            live_leases,
            "a live lease must never be reclaimed"
        );
        prop_assert_eq!(
            state_count(&transaction, "sys_effects", 2),
            live_done,
            "a completed effect inside retention must survive"
        );
        prop_assert_eq!(
            state_count(&transaction, "sys_effects", 3),
            live_failed + expired_ready + failed_reclaims,
            "expired ready rows and exhausted leases fail before cleanup sees them"
        );
        prop_assert_eq!(
            count(&transaction, "sys_effects"),
            survives,
            "terminal rows past retention are removed by the first Tick"
        );

        // Second Tick: the failures from the first Tick are now terminal and past
        // retention, so exactly the rows inside retention remain.
        scheduler_tick(&transaction, &target(), NOW_MS, &[], None, &[]).unwrap();
        prop_assert_eq!(
            count(&transaction, "sys_effects"),
            live_ready + live_leases + reclaimed + live_done + live_failed,
            "only rows inside retention survive the sweep"
        );
    }
}
