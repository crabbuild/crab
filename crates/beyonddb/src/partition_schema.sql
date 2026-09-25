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
