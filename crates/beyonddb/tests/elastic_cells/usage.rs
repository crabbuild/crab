use crab_ltx::rusqlite::{Connection, DatabaseName, params};

fn assert_usage(connection: &Connection) {
    let measured: (i64, i64, i64) = connection.query_row(
        "SELECT COUNT(*), COALESCE(SUM(length(item) + length(item_key) + length(partition_key) + length(sort_key)), 0), COALESCE(SUM(logical_bytes), 0) FROM ddb_partition_items",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).unwrap();
    let maintained = connection
        .query_row(
            "SELECT item_count, item_bytes, logical_bytes FROM ddb_partition_usage WHERE singleton = 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
        )
        .unwrap();
    assert_eq!(maintained, measured);
}

#[test]
fn partition_usage_tracks_row_changes_and_rollback() {
    let mut connection = Connection::open_in_memory().unwrap();
    let transaction = connection.transaction().unwrap();
    beyonddb::initialize_partition(&transaction).unwrap();
    transaction.commit().unwrap();
    assert_usage(&connection);
    for n in 0_u32..128 {
        connection.execute(
            "INSERT INTO ddb_partition_items (item_key, partition_key, sort_key, item, logical_bytes) VALUES (?1, ?2, ?3, zeroblob(?4), ?4)",
            params![n.to_be_bytes(), [0_u8; 32], n.to_le_bytes(), n * 17],
        ).unwrap();
        assert_usage(&connection);
    }
    // Exercise the reset/allocation/incremental-write shape used by StoredValue.
    let key = 0_u32.to_be_bytes();
    connection.execute(
        "INSERT INTO ddb_partition_items (item_key, partition_key, sort_key, item, logical_bytes) VALUES (?1, ?2, ?3, X'', 300000) ON CONFLICT(item_key) DO UPDATE SET partition_key = excluded.partition_key, sort_key = excluded.sort_key, item = excluded.item, logical_bytes = excluded.logical_bytes",
        params![key, [1_u8; 48], [2_u8; 8]],
    ).unwrap();
    connection
        .execute(
            "UPDATE ddb_partition_items SET item = zeroblob(300000) WHERE item_key = ?1",
            [key],
        )
        .unwrap();
    let rowid: i64 = connection
        .query_row(
            "SELECT rowid FROM ddb_partition_items WHERE item_key = ?1",
            [key],
            |row| row.get(0),
        )
        .unwrap();
    let mut blob = connection
        .blob_open(
            DatabaseName::Main,
            "ddb_partition_items",
            "item",
            rowid,
            false,
        )
        .unwrap();
    blob.write_at(&vec![b'x'; 200000], 0).unwrap();
    blob.write_at(&vec![b'y'; 100000], 200000).unwrap();
    blob.close().unwrap();
    assert_usage(&connection);
    connection
        .execute_batch(
            "SAVEPOINT rejected;
        DELETE FROM ddb_partition_items;
        ROLLBACK TO rejected;
        RELEASE rejected;
        UPDATE ddb_partition_items SET ttl_generation = 1, ttl_epoch = 5;",
        )
        .unwrap();
    assert_usage(&connection);
    connection
        .execute("DELETE FROM ddb_partition_items WHERE item_key = ?1", [key])
        .unwrap();
    connection
        .execute("DELETE FROM ddb_partition_items WHERE item_key = ?1", [key])
        .unwrap();
    assert_usage(&connection);
    connection
        .execute("DELETE FROM ddb_partition_items", [])
        .unwrap();
    assert_usage(&connection);
}
