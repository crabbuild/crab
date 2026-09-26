CREATE TABLE ddb_coordinator_transactions (
    transaction_id BLOB PRIMARY KEY,
    account_id TEXT NOT NULL,
    token TEXT,
    fingerprint TEXT NOT NULL,
    request_digest BLOB NOT NULL,
    participants BLOB NOT NULL,
    state INTEGER NOT NULL CHECK (state IN (0, 1, 2)),
    abort_reason BLOB,
    created_at_ms INTEGER NOT NULL,
    decided_at_ms INTEGER
);

CREATE UNIQUE INDEX ddb_coordinator_token
    ON ddb_coordinator_transactions (token) WHERE token IS NOT NULL;
CREATE INDEX ddb_coordinator_pending
    ON ddb_coordinator_transactions (state, created_at_ms, transaction_id);

CREATE TABLE ddb_coordinator_participants (
    transaction_id BLOB NOT NULL REFERENCES ddb_coordinator_transactions(transaction_id),
    position INTEGER NOT NULL,
    cell_id BLOB NOT NULL,
    prepared_sequence INTEGER,
    resolved_sequence INTEGER,
    PRIMARY KEY (transaction_id, position)
);
