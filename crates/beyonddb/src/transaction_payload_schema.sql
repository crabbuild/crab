CREATE TABLE ddb_transaction_payloads (
    transaction_id BLOB NOT NULL,
    position INTEGER NOT NULL,
    chunk INTEGER NOT NULL,
    payload BLOB NOT NULL,
    PRIMARY KEY (transaction_id, position, chunk)
) WITHOUT ROWID;
CREATE TABLE ddb_transaction_uploads (
    upload_id BLOB NOT NULL,
    digest BLOB NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    chunk INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    payload BLOB NOT NULL,
    PRIMARY KEY (digest, expires_at_ms, upload_id, chunk)
) WITHOUT ROWID;
CREATE INDEX ddb_transaction_upload_expiry ON ddb_transaction_uploads(expires_at_ms);
