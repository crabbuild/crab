CREATE TABLE ddb_coordinator_transactions (
    transaction_id BLOB PRIMARY KEY,
    account_id TEXT NOT NULL,
    token TEXT,
    fingerprint TEXT NOT NULL,
    request_digest BLOB NOT NULL,
    state INTEGER NOT NULL CHECK (state IN (0, 1, 2)),
    unresolved_count INTEGER NOT NULL CHECK (unresolved_count BETWEEN 0 AND 100),
    abort_chunks INTEGER,
    created_at_ms INTEGER NOT NULL,
    decided_at_ms INTEGER,
    completed_at_ms INTEGER
);

CREATE UNIQUE INDEX ddb_coordinator_token
    ON ddb_coordinator_transactions (token) WHERE token IS NOT NULL;
CREATE INDEX ddb_coordinator_pending
    ON ddb_coordinator_transactions (created_at_ms, transaction_id)
    WHERE unresolved_count > 0;

CREATE TABLE ddb_coordinator_participants (
    transaction_id BLOB NOT NULL REFERENCES ddb_coordinator_transactions(transaction_id),
    position INTEGER NOT NULL,
    cell_id BLOB NOT NULL,
    target BLOB NOT NULL,
    operation_chunks INTEGER NOT NULL,
    prepared_sequence INTEGER,
    resolved_sequence INTEGER,
    PRIMARY KEY (transaction_id, position)
);
