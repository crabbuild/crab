CREATE TABLE directory_identity (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    format INTEGER NOT NULL CHECK (format = 1),
    app_revision INTEGER NOT NULL DEFAULT 0 CHECK (app_revision BETWEEN 0 AND 9007199254740991)
) STRICT;
INSERT INTO directory_identity(singleton, format) VALUES (1, 1);

CREATE TABLE actors (
    id BLOB PRIMARY KEY CHECK (length(id) = 16),
    issuer TEXT NOT NULL CHECK (length(issuer) BETWEEN 1 AND 4096),
    subject TEXT NOT NULL CHECK (length(subject) BETWEEN 1 AND 1024),
    handle TEXT NOT NULL UNIQUE CHECK (length(handle) BETWEEN 1 AND 39 AND handle = lower(handle)),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 100),
    version INTEGER NOT NULL CHECK (version BETWEEN 1 AND 9007199254740991),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    UNIQUE (issuer, subject)
) STRICT, WITHOUT ROWID;

CREATE TABLE actor_submissions (
    request_id BLOB PRIMARY KEY CHECK (length(request_id) = 16),
    digest BLOB NOT NULL CHECK (length(digest) = 32),
    actor_id BLOB NOT NULL UNIQUE REFERENCES actors(id)
) STRICT, WITHOUT ROWID;

CREATE TABLE directory_command_receipts (
    request_id BLOB PRIMARY KEY CHECK (length(request_id) = 16),
    digest BLOB NOT NULL CHECK (length(digest) = 32),
    expires_at_ms INTEGER NOT NULL,
    reply BLOB NOT NULL
) STRICT, WITHOUT ROWID;
CREATE INDEX directory_command_receipts_expiry ON directory_command_receipts(expires_at_ms);
