CREATE TABLE ddb_tables (
    table_name TEXT PRIMARY KEY,
    table_id TEXT NOT NULL UNIQUE,
    record BLOB NOT NULL
);

CREATE TABLE ddb_table_deletions (
    table_id TEXT PRIMARY KEY REFERENCES ddb_tables(table_id) ON DELETE CASCADE
);
CREATE VIEW ddb_live_tables AS
    SELECT t.* FROM ddb_tables t WHERE NOT EXISTS (
        SELECT 1 FROM ddb_table_deletions d WHERE d.table_id = t.table_id
    );

CREATE TABLE ddb_items (
    table_id TEXT NOT NULL REFERENCES ddb_tables(table_id) ON DELETE CASCADE,
    item_key BLOB NOT NULL,
    partition_key BLOB NOT NULL,
    sort_key BLOB NOT NULL,
    item BLOB NOT NULL,
    logical_bytes INTEGER NOT NULL CHECK (logical_bytes >= 0),
    PRIMARY KEY (table_id, item_key)
);
CREATE INDEX ddb_account_items_query ON ddb_items (table_id, partition_key, sort_key, item_key);

CREATE TABLE ddb_iam_principal_policies (
    principal_kind INTEGER NOT NULL CHECK (principal_kind IN (0, 1)),
    principal_name TEXT NOT NULL,
    policy_name TEXT NOT NULL,
    document TEXT NOT NULL,
    PRIMARY KEY (principal_kind, principal_name, policy_name)
);

CREATE TABLE ddb_table_tags (
    table_id TEXT NOT NULL REFERENCES ddb_tables(table_id) ON DELETE CASCADE,
    resource_arn TEXT NOT NULL,
    tag_key TEXT NOT NULL,
    tag_value TEXT NOT NULL,
    PRIMARY KEY (table_id, resource_arn, tag_key)
);

CREATE TABLE ddb_table_ttl (
    table_id TEXT PRIMARY KEY REFERENCES ddb_tables(table_id) ON DELETE CASCADE,
    attribute_name TEXT NOT NULL,
    sweep_after BLOB
);

CREATE TABLE ddb_ttl_schedule (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    last_table TEXT
);
INSERT INTO ddb_ttl_schedule (singleton, last_table) VALUES (1, NULL);

CREATE TABLE ddb_coordinator_shards (
    shard INTEGER PRIMARY KEY CHECK (shard BETWEEN 0 AND 4095),
    settled BLOB
);

CREATE TABLE ddb_account_transaction_locks (
    table_id TEXT NOT NULL REFERENCES ddb_tables(table_id),
    item_key BLOB NOT NULL,
    transaction_id BLOB NOT NULL REFERENCES ddb_transactions(transaction_id),
    write_lock INTEGER NOT NULL CHECK (write_lock IN (0, 1)),
    PRIMARY KEY (table_id, item_key, transaction_id)
);
CREATE INDEX ddb_account_transaction_locks_owner ON ddb_account_transaction_locks (transaction_id);
CREATE INDEX ddb_account_write_locks ON ddb_account_transaction_locks (table_id, item_key)
    WHERE write_lock = 1;

CREATE TABLE ddb_directory_roots (
    table_id TEXT PRIMARY KEY,
    base_table_id TEXT NOT NULL REFERENCES ddb_tables(table_id) ON DELETE CASCADE,
    initial_fingerprint BLOB,
    retired INTEGER NOT NULL DEFAULT 0 CHECK (retired IN (0, 1))
);
CREATE INDEX ddb_directory_roots_base ON ddb_directory_roots (base_table_id);

CREATE TABLE ddb_table_statistics (
    table_id TEXT PRIMARY KEY REFERENCES ddb_tables(table_id) ON DELETE CASCADE,
    sampled_at INTEGER NOT NULL,
    statistics BLOB NOT NULL
);

CREATE TRIGGER ddb_account_statistics_insert AFTER INSERT ON ddb_items
BEGIN
    INSERT INTO ddb_local_index_statistics VALUES (NEW.table_id, '', 1, NEW.logical_bytes)
    ON CONFLICT(table_id, index_name) DO UPDATE SET
        item_count = item_count + 1, item_bytes = item_bytes + NEW.logical_bytes;
END;
CREATE TRIGGER ddb_account_statistics_update AFTER UPDATE OF logical_bytes ON ddb_items
BEGIN
    UPDATE ddb_local_index_statistics SET item_bytes = item_bytes + NEW.logical_bytes - OLD.logical_bytes
    WHERE table_id = OLD.table_id AND index_name = '';
END;
CREATE TRIGGER ddb_account_statistics_delete AFTER DELETE ON ddb_items
BEGIN
    UPDATE ddb_local_index_statistics SET item_count = item_count - 1, item_bytes = item_bytes - OLD.logical_bytes
    WHERE table_id = OLD.table_id AND index_name = '';
END;
