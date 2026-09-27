CREATE TABLE ddb_global_index (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    spec BLOB NOT NULL,
    state BLOB NOT NULL
);

CREATE TABLE ddb_global_index_items (
    item_key BLOB PRIMARY KEY,
    partition_key BLOB NOT NULL,
    sort_key BLOB NOT NULL,
    version BLOB NOT NULL CHECK (length(version) = 16),
    digest BLOB NOT NULL CHECK (length(digest) = 32),
    item BLOB
);

CREATE INDEX ddb_global_index_query
    ON ddb_global_index_items (partition_key, sort_key, item_key) WHERE item IS NOT NULL;
