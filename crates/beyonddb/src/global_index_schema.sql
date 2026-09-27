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
    item BLOB,
    logical_bytes INTEGER NOT NULL CHECK (logical_bytes >= 0)
);

CREATE INDEX ddb_global_index_query
    ON ddb_global_index_items (partition_key, sort_key, item_key) WHERE item IS NOT NULL;

CREATE TABLE ddb_global_index_statistics (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    item_count INTEGER NOT NULL CHECK (item_count >= 0),
    item_bytes INTEGER NOT NULL CHECK (item_bytes >= 0)
);
INSERT INTO ddb_global_index_statistics VALUES (1, 0, 0);
CREATE TRIGGER ddb_global_index_statistics_insert AFTER INSERT ON ddb_global_index_items
BEGIN
    UPDATE ddb_global_index_statistics SET item_count = item_count + (NEW.item IS NOT NULL),
        item_bytes = item_bytes + CASE WHEN NEW.item IS NOT NULL THEN NEW.logical_bytes ELSE 0 END WHERE singleton = 1;
END;
CREATE TRIGGER ddb_global_index_statistics_update AFTER UPDATE OF item, logical_bytes ON ddb_global_index_items
BEGIN
    UPDATE ddb_global_index_statistics SET item_count = item_count + (NEW.item IS NOT NULL) - (OLD.item IS NOT NULL),
        item_bytes = item_bytes + CASE WHEN NEW.item IS NOT NULL THEN NEW.logical_bytes ELSE 0 END
            - CASE WHEN OLD.item IS NOT NULL THEN OLD.logical_bytes ELSE 0 END WHERE singleton = 1;
END;
