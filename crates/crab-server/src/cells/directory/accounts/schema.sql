CREATE TABLE accounts (
    id BLOB PRIMARY KEY CHECK (length(id) = 16),
    issuer TEXT NOT NULL CHECK (length(issuer) BETWEEN 1 AND 4096),
    subject TEXT NOT NULL CHECK (length(subject) BETWEEN 1 AND 1024),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    UNIQUE (issuer, subject)
) STRICT, WITHOUT ROWID;
