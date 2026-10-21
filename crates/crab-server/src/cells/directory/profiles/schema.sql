CREATE TABLE profiles (
    id BLOB PRIMARY KEY REFERENCES accounts(id) CHECK (length(id) = 16),
    handle TEXT NOT NULL UNIQUE CHECK (length(handle) BETWEEN 1 AND 39 AND handle = lower(handle)),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 100),
    version INTEGER NOT NULL CHECK (version BETWEEN 1 AND 9007199254740991),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms)
) STRICT, WITHOUT ROWID;

CREATE TABLE profile_submissions (
    request_id BLOB PRIMARY KEY CHECK (length(request_id) = 16),
    digest BLOB NOT NULL CHECK (length(digest) = 32),
    actor_id BLOB NOT NULL UNIQUE REFERENCES profiles(id)
) STRICT, WITHOUT ROWID;
