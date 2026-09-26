CREATE TABLE ddb_partition (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    spec BLOB NOT NULL,
    state BLOB NOT NULL
);

CREATE TABLE ddb_partition_items (
    item_key BLOB PRIMARY KEY,
    partition_key BLOB NOT NULL,
    sort_key BLOB NOT NULL,
    item BLOB NOT NULL,
    ttl_generation INTEGER NOT NULL DEFAULT 0,
    ttl_epoch INTEGER
);

CREATE INDEX ddb_partition_query ON ddb_partition_items (partition_key, sort_key, item_key);
CREATE INDEX ddb_partition_expiry ON ddb_partition_items (ttl_generation, ttl_epoch)
    WHERE ttl_epoch IS NOT NULL;

CREATE TABLE ddb_partition_ttl (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    attribute_name TEXT,
    generation INTEGER NOT NULL,
    cursor BLOB,
    ready INTEGER NOT NULL CHECK (ready IN (0, 1))
);

CREATE TABLE ddb_partition_transaction_locks (
    item_key BLOB NOT NULL,
    partition_key BLOB NOT NULL,
    sort_key BLOB NOT NULL,
    transaction_id BLOB NOT NULL REFERENCES ddb_transactions(transaction_id),
    write_lock INTEGER NOT NULL CHECK (write_lock IN (0, 1)),
    PRIMARY KEY (item_key, transaction_id)
);
CREATE INDEX ddb_partition_transaction_locks_range
    ON ddb_partition_transaction_locks (partition_key, sort_key, item_key) WHERE write_lock = 1;
CREATE INDEX ddb_partition_write_locks ON ddb_partition_transaction_locks (item_key)
    WHERE write_lock = 1;
CREATE INDEX ddb_partition_transaction_locks_owner
    ON ddb_partition_transaction_locks (transaction_id);
