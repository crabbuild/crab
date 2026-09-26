CREATE TABLE ddb_transactions (
    transaction_id BLOB PRIMARY KEY,
    coordinator_cell BLOB NOT NULL,
    request_digest BLOB,
    state INTEGER NOT NULL CHECK (state IN (0, 1, 2)),
    staged BLOB
);
