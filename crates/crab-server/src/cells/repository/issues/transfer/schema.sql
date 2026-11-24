CREATE TABLE IF NOT EXISTS repository_issue_transfer_frames (
    transfer BLOB NOT NULL CHECK(length(transfer) = 16),
    position TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK(ordinal BETWEEN 1 AND 9007199254740991),
    digest BLOB NOT NULL CHECK(length(digest) = 32),
    payload BLOB NOT NULL CHECK(length(payload) <= 1081344),
    PRIMARY KEY(transfer, position),
    UNIQUE(transfer, ordinal)
) STRICT, WITHOUT ROWID;
