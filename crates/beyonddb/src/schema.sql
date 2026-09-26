CREATE TABLE ddb_tables (
    table_name TEXT PRIMARY KEY,
    table_id TEXT NOT NULL UNIQUE,
    record BLOB NOT NULL
);

CREATE TABLE ddb_items (
    table_id TEXT NOT NULL REFERENCES ddb_tables(table_id) ON DELETE CASCADE,
    item_key BLOB NOT NULL,
    item BLOB NOT NULL,
    PRIMARY KEY (table_id, item_key)
);

CREATE TABLE ddb_routes (
    table_id TEXT PRIMARY KEY REFERENCES ddb_tables(table_id) ON DELETE CASCADE,
    route_epoch TEXT NOT NULL,
    route_table BLOB NOT NULL
);

CREATE TABLE ddb_route_partitions (
    table_id TEXT NOT NULL REFERENCES ddb_tables(table_id) ON DELETE CASCADE,
    partition_id BLOB NOT NULL,
    lower_bound BLOB NOT NULL,
    upper_bound BLOB NOT NULL,
    epoch TEXT NOT NULL,
    PRIMARY KEY (table_id, partition_id)
);

CREATE UNIQUE INDEX ddb_route_partition_lookup
    ON ddb_route_partitions (table_id, lower_bound);

CREATE TABLE ddb_split_plans (
    table_id TEXT PRIMARY KEY REFERENCES ddb_tables(table_id) ON DELETE CASCADE,
    plan BLOB NOT NULL
);

CREATE TABLE ddb_iam_user_policies (
    user_name TEXT NOT NULL,
    policy_name TEXT NOT NULL,
    document TEXT NOT NULL,
    PRIMARY KEY (user_name, policy_name)
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
    shard INTEGER PRIMARY KEY CHECK (shard BETWEEN 0 AND 4095)
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
