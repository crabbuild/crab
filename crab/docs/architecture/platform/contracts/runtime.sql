-- Runtime schema version 1. Install in every Cell before its primitive schema.
PRAGMA foreign_keys = ON;

CREATE TABLE sys_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    cell_id BLOB NOT NULL CHECK (length(cell_id) = 32),
    incarnation BLOB NOT NULL CHECK (length(incarnation) = 16),
    commit_sequence INTEGER NOT NULL CHECK (commit_sequence >= 0),
    logical_time_ms INTEGER NOT NULL CHECK (logical_time_ms >= 0),
    schema_version INTEGER NOT NULL CHECK (schema_version >= 1)
) STRICT;

CREATE TABLE sys_requests (
    request_id BLOB PRIMARY KEY CHECK (length(request_id) = 16),
    operation_digest BLOB NOT NULL CHECK (length(operation_digest) = 32),
    outcome INTEGER NOT NULL CHECK (outcome IN (1, 2)),
    result BLOB NOT NULL,
    commit_sequence INTEGER NOT NULL CHECK (commit_sequence > 0),
    expires_at_ms INTEGER NOT NULL,
    retain_until_ms INTEGER NOT NULL CHECK (retain_until_ms >= expires_at_ms)
) STRICT, WITHOUT ROWID;
CREATE INDEX sys_requests_expiry ON sys_requests(retain_until_ms);

CREATE TABLE sys_inbox (
    effect_id BLOB PRIMARY KEY CHECK (length(effect_id) = 32),
    operation_digest BLOB NOT NULL CHECK (length(operation_digest) = 32),
    outcome INTEGER NOT NULL CHECK (outcome IN (1, 2)),
    result BLOB NOT NULL,
    commit_sequence INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    retain_until_ms INTEGER NOT NULL CHECK (retain_until_ms >= expires_at_ms)
) STRICT, WITHOUT ROWID;
CREATE INDEX sys_inbox_expiry ON sys_inbox(retain_until_ms);

CREATE TABLE sys_effects (
    effect_id BLOB PRIMARY KEY CHECK (length(effect_id) = 32),
    destination BLOB NOT NULL CHECK (length(destination) = 32),
    operation BLOB NOT NULL,
    state INTEGER NOT NULL CHECK (state IN (0, 1, 2, 3)),
    attempt INTEGER NOT NULL CHECK (attempt >= 0),
    due_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL CHECK (expires_at_ms >= due_at_ms),
    token BLOB,
    lease_until_ms INTEGER,
    created_sequence INTEGER NOT NULL,
    result BLOB,
    CHECK ((state = 1 AND token IS NOT NULL AND length(token) = 16 AND lease_until_ms IS NOT NULL)
        OR (state != 1 AND token IS NULL AND lease_until_ms IS NULL))
) STRICT, WITHOUT ROWID;
CREATE INDEX sys_effects_due ON sys_effects(state, due_at_ms);
CREATE INDEX sys_effects_leases ON sys_effects(state, lease_until_ms);

CREATE TABLE sys_blob_refs (
    owner_kind INTEGER NOT NULL CHECK (owner_kind IN (1, 2)),
    owner_id BLOB NOT NULL,
    digest BLOB NOT NULL CHECK (length(digest) = 32),
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    PRIMARY KEY (owner_kind, owner_id, digest),
    CHECK ((owner_kind = 1 AND length(owner_id) = 32)
        OR (owner_kind = 2 AND length(owner_id) = 16))
) STRICT, WITHOUT ROWID;

CREATE TABLE sys_migrations (
    version INTEGER PRIMARY KEY CHECK (version > 0),
    digest BLOB NOT NULL CHECK (length(digest) = 32),
    applied_sequence INTEGER NOT NULL CHECK (applied_sequence >= 0)
) STRICT;
