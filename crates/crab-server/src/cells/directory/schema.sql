CREATE TABLE directory_command_receipts (
    request_id BLOB PRIMARY KEY CHECK (length(request_id) = 16),
    digest BLOB NOT NULL CHECK (length(digest) = 32),
    expires_at_ms INTEGER NOT NULL,
    reply BLOB NOT NULL
) STRICT, WITHOUT ROWID;
CREATE INDEX directory_command_receipts_expiry ON directory_command_receipts(expires_at_ms);
