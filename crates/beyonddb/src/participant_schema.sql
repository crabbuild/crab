CREATE TABLE ddb_transactions (
    transaction_id BLOB PRIMARY KEY,
    coordinator_cell BLOB NOT NULL,
    request_digest BLOB,
    state INTEGER NOT NULL CHECK (state IN (0, 1, 2)),
    staged BLOB
);

CREATE TABLE ddb_transaction_reads (
    transaction_id BLOB NOT NULL REFERENCES ddb_transactions(transaction_id),
    position INTEGER NOT NULL,
    item BLOB,
    PRIMARY KEY (transaction_id, position)
);
