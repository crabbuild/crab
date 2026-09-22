-- KV schema version 1. One namespace per Cell; scope is part of each key.
CREATE TABLE kv_entries (
    scope BLOB NOT NULL,
    key BLOB NOT NULL CHECK (length(key) BETWEEN 1 AND 1024),
    version BLOB NOT NULL CHECK (length(version) = 28),
    value BLOB NOT NULL CHECK (length(value) <= 4194304),
    expires_at_ms INTEGER,
    PRIMARY KEY (scope, key)
) STRICT, WITHOUT ROWID;
CREATE INDEX kv_expiry ON kv_entries(expires_at_ms)
    WHERE expires_at_ms IS NOT NULL;
