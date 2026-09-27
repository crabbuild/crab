CREATE TABLE ddb_local_index_items (
    table_id TEXT NOT NULL,
    index_name TEXT NOT NULL,
    item_key BLOB NOT NULL,
    partition_key BLOB NOT NULL,
    sort_key BLOB NOT NULL,
    base_sort_key BLOB NOT NULL,
    logical_bytes INTEGER NOT NULL CHECK (logical_bytes >= 0),
    PRIMARY KEY (table_id, index_name, item_key)
) WITHOUT ROWID;
CREATE INDEX ddb_local_index_query ON ddb_local_index_items
    (table_id, index_name, partition_key, sort_key, base_sort_key, item_key);

-- Retain zero totals until table deletion: one statistics edit per mutation
-- keeps transaction resolution within its reserved B-tree capacity.
CREATE TABLE ddb_local_index_statistics (
    table_id TEXT NOT NULL,
    index_name TEXT NOT NULL,
    item_count INTEGER NOT NULL CHECK (item_count >= 0),
    item_bytes INTEGER NOT NULL CHECK (item_bytes >= 0),
    PRIMARY KEY (table_id, index_name)
) WITHOUT ROWID;
CREATE TRIGGER ddb_local_index_statistics_insert AFTER INSERT ON ddb_local_index_items
BEGIN
    INSERT INTO ddb_local_index_statistics VALUES (NEW.table_id, NEW.index_name, 1, NEW.logical_bytes)
    ON CONFLICT(table_id, index_name) DO UPDATE SET
        item_count = item_count + 1, item_bytes = item_bytes + NEW.logical_bytes;
END;
CREATE TRIGGER ddb_local_index_statistics_delete AFTER DELETE ON ddb_local_index_items
BEGIN
    UPDATE ddb_local_index_statistics SET item_count = item_count - 1, item_bytes = item_bytes - OLD.logical_bytes
    WHERE table_id = OLD.table_id AND index_name = OLD.index_name;
END;
