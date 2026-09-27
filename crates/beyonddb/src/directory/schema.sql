CREATE TABLE ddb_directory (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    state BLOB NOT NULL
);
CREATE TABLE ddb_directory_ranges (
    lower_bound BLOB PRIMARY KEY,
    partition_id BLOB NOT NULL UNIQUE,
    record BLOB NOT NULL
);
CREATE TABLE ddb_directory_changes (
    lower_bound BLOB PRIMARY KEY,
    plan BLOB NOT NULL
);
CREATE TABLE ddb_directory_members (
    partition_id BLOB PRIMARY KEY,
    lower_bound BLOB NOT NULL REFERENCES ddb_directory_changes(lower_bound) ON DELETE CASCADE
);
CREATE TABLE ddb_directory_transfers (
    lower_bound BLOB PRIMARY KEY REFERENCES ddb_directory_changes(lower_bound) ON DELETE CASCADE,
    plan BLOB NOT NULL
);
