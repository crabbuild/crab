CREATE TABLE ddb_index_changes (
    id BLOB PRIMARY KEY CHECK (length(id) = 32),
    table_id TEXT NOT NULL,
    sequence BLOB NOT NULL CHECK (length(sequence) = 8),
    attempt_sequence BLOB NOT NULL CHECK (length(attempt_sequence) = 8),
    item BLOB NOT NULL
);
CREATE INDEX ddb_index_changes_pending ON ddb_index_changes (table_id, attempt_sequence, sequence, id);
