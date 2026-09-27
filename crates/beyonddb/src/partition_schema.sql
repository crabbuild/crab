CREATE TABLE ddb_partition (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    spec BLOB NOT NULL,
    state BLOB NOT NULL
);

CREATE TABLE ddb_partition_index_policy (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    policy BLOB NOT NULL
);

CREATE TABLE ddb_partition_index_backfills (
    index_id TEXT PRIMARY KEY,
    cursor BLOB,
    scanned INTEGER NOT NULL CHECK (scanned IN (0, 1))
);

CREATE TABLE ddb_partition_items (
    item_key BLOB PRIMARY KEY,
    partition_key BLOB NOT NULL,
    sort_key BLOB NOT NULL,
    item BLOB NOT NULL,
    logical_bytes INTEGER NOT NULL CHECK (logical_bytes >= 0),
    ttl_generation INTEGER NOT NULL DEFAULT 0,
    ttl_epoch INTEGER
);

CREATE INDEX ddb_partition_query ON ddb_partition_items (partition_key, sort_key, item_key);
CREATE INDEX ddb_partition_expiry ON ddb_partition_items (ttl_generation, ttl_epoch)
    WHERE ttl_epoch IS NOT NULL;

CREATE TABLE ddb_partition_usage (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    item_count INTEGER NOT NULL CHECK (item_count >= 0),
    item_bytes INTEGER NOT NULL CHECK (item_bytes >= 0),
    logical_bytes INTEGER NOT NULL CHECK (logical_bytes >= 0)
);
INSERT INTO ddb_partition_usage VALUES (1, 0, 0, 0);

-- Keep usage atomic with ordinary writes, transaction resolution, and imports.
-- Incremental BLOB writes preserve the length established by zeroblob allocation;
-- TTL metadata changes do not change the counted columns.
CREATE TRIGGER ddb_partition_usage_insert AFTER INSERT ON ddb_partition_items
BEGIN
    UPDATE ddb_partition_usage SET item_count = item_count + 1,
        logical_bytes = logical_bytes + NEW.logical_bytes,
        item_bytes = item_bytes + length(NEW.item) + length(NEW.item_key)
            + length(NEW.partition_key) + length(NEW.sort_key)
        WHERE singleton = 1;
END;
CREATE TRIGGER ddb_partition_usage_update
AFTER UPDATE OF item, item_key, partition_key, sort_key, logical_bytes ON ddb_partition_items
BEGIN
    UPDATE ddb_partition_usage SET logical_bytes = logical_bytes + NEW.logical_bytes - OLD.logical_bytes,
        item_bytes = item_bytes
        + length(NEW.item) + length(NEW.item_key) + length(NEW.partition_key) + length(NEW.sort_key)
        - length(OLD.item) - length(OLD.item_key) - length(OLD.partition_key) - length(OLD.sort_key)
        WHERE singleton = 1;
END;
CREATE TRIGGER ddb_partition_usage_delete AFTER DELETE ON ddb_partition_items
BEGIN
    UPDATE ddb_partition_usage SET item_count = item_count - 1,
        logical_bytes = logical_bytes - OLD.logical_bytes,
        item_bytes = item_bytes - length(OLD.item) - length(OLD.item_key)
            - length(OLD.partition_key) - length(OLD.sort_key)
        WHERE singleton = 1;
END;

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
