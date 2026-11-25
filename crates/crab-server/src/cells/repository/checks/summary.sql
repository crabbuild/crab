CREATE TABLE IF NOT EXISTS repository_check_rollups (
    oid TEXT PRIMARY KEY CHECK (length(oid) = 40),
    success INTEGER NOT NULL CHECK (success >= 0),
    neutral INTEGER NOT NULL CHECK (neutral >= 0),
    skipped INTEGER NOT NULL CHECK (skipped >= 0),
    pending INTEGER NOT NULL CHECK (pending >= 0),
    failed INTEGER NOT NULL CHECK (failed >= 0)
) STRICT, WITHOUT ROWID;
