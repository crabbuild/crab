CREATE TABLE ddb_local_index_items (
    table_id TEXT NOT NULL,
    index_name TEXT NOT NULL,
    item_key BLOB NOT NULL,
    partition_key BLOB NOT NULL,
    sort_key BLOB NOT NULL,
    base_sort_key BLOB NOT NULL,
    PRIMARY KEY (table_id, index_name, item_key)
) WITHOUT ROWID;
CREATE INDEX ddb_local_index_query ON ddb_local_index_items
    (table_id, index_name, partition_key, sort_key, base_sort_key, item_key);
