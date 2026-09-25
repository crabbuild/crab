CREATE TABLE ddb_partition (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    spec BLOB NOT NULL,
    state BLOB NOT NULL
);

CREATE TABLE ddb_partition_items (
    item_key BLOB PRIMARY KEY,
    partition_key BLOB NOT NULL,
    sort_key BLOB NOT NULL,
    item BLOB NOT NULL
);

CREATE INDEX ddb_partition_query ON ddb_partition_items (partition_key, sort_key, item_key);

CREATE TABLE ddb_transaction_applied (
    account_id TEXT NOT NULL,
    token TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (account_id, token)
);
CREATE INDEX ddb_transaction_applied_age ON ddb_transaction_applied (created_at_ms);
