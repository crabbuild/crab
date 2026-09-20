CREATE TABLE repository_pull_review_thread_sequences (
    pull_number INTEGER PRIMARY KEY,
    last INTEGER NOT NULL CHECK (last BETWEEN 1 AND 9007199254740991),
    FOREIGN KEY (pull_number) REFERENCES repository_pulls(number) ON DELETE CASCADE
) STRICT;

CREATE TABLE repository_pull_review_thread_submissions (
    pull_number INTEGER NOT NULL,
    request_id BLOB NOT NULL CHECK (length(request_id) = 16),
    payload_digest BLOB NOT NULL CHECK (length(payload_digest) = 32),
    thread_number INTEGER NOT NULL CHECK (thread_number BETWEEN 1 AND 9007199254740991),
    PRIMARY KEY (pull_number, request_id),
    UNIQUE (pull_number, thread_number),
    FOREIGN KEY (pull_number) REFERENCES repository_pulls(number) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE TABLE repository_pull_review_threads (
    pull_number INTEGER NOT NULL,
    number INTEGER NOT NULL CHECK (number BETWEEN 1 AND 9007199254740991),
    author_issuer TEXT NOT NULL,
    author_subject TEXT NOT NULL,
    author_name TEXT NOT NULL,
    body TEXT NOT NULL,
    suggested_text TEXT,
    base_oid TEXT NOT NULL CHECK (length(base_oid) = 40),
    head_oid TEXT NOT NULL CHECK (length(head_oid) = 40),
    path BLOB NOT NULL CHECK (length(path) BETWEEN 1 AND 1024),
    old_blob_oid TEXT CHECK (old_blob_oid IS NULL OR length(old_blob_oid) = 40),
    new_blob_oid TEXT CHECK (new_blob_oid IS NULL OR length(new_blob_oid) = 40),
    side INTEGER NOT NULL CHECK (side BETWEEN 0 AND 1),
    start_line INTEGER NOT NULL CHECK (start_line BETWEEN 1 AND 9007199254740991),
    end_line INTEGER NOT NULL CHECK (end_line BETWEEN 1 AND 9007199254740991),
    resolved INTEGER NOT NULL DEFAULT 0 CHECK (resolved BETWEEN 0 AND 1),
    resolved_by_issuer TEXT,
    resolved_by_subject TEXT,
    resolved_by_name TEXT,
    resolved_at_ms INTEGER CHECK (resolved_at_ms IS NULL OR resolved_at_ms >= 0),
    version INTEGER NOT NULL DEFAULT 1 CHECK (version BETWEEN 1 AND 9007199254740991),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    PRIMARY KEY (pull_number, number),
    FOREIGN KEY (pull_number) REFERENCES repository_pulls(number) ON DELETE CASCADE,
    CHECK (end_line >= start_line AND end_line - start_line < 200),
    CHECK (
        (side = 0 AND old_blob_oid IS NOT NULL AND suggested_text IS NULL)
        OR (side = 1 AND new_blob_oid IS NOT NULL)
    ),
    CHECK (
        (resolved = 0
            AND resolved_by_issuer IS NULL
            AND resolved_by_subject IS NULL
            AND resolved_by_name IS NULL
            AND resolved_at_ms IS NULL)
        OR (resolved = 1
            AND resolved_by_issuer IS NOT NULL
            AND resolved_by_subject IS NOT NULL
            AND resolved_by_name IS NOT NULL
            AND resolved_at_ms IS NOT NULL)
    )
) STRICT;

CREATE TABLE repository_pull_review_reply_sequences (
    pull_number INTEGER NOT NULL,
    thread_number INTEGER NOT NULL,
    last INTEGER NOT NULL CHECK (last BETWEEN 1 AND 9007199254740991),
    PRIMARY KEY (pull_number, thread_number),
    FOREIGN KEY (pull_number, thread_number)
        REFERENCES repository_pull_review_threads(pull_number, number) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE TABLE repository_pull_review_reply_submissions (
    pull_number INTEGER NOT NULL,
    thread_number INTEGER NOT NULL,
    request_id BLOB NOT NULL CHECK (length(request_id) = 16),
    payload_digest BLOB NOT NULL CHECK (length(payload_digest) = 32),
    reply_number INTEGER NOT NULL CHECK (reply_number BETWEEN 1 AND 9007199254740991),
    PRIMARY KEY (pull_number, thread_number, request_id),
    UNIQUE (pull_number, thread_number, reply_number),
    FOREIGN KEY (pull_number, thread_number)
        REFERENCES repository_pull_review_threads(pull_number, number) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE TABLE repository_pull_review_replies (
    pull_number INTEGER NOT NULL,
    thread_number INTEGER NOT NULL,
    number INTEGER NOT NULL CHECK (number BETWEEN 1 AND 9007199254740991),
    author_issuer TEXT NOT NULL,
    author_subject TEXT NOT NULL,
    author_name TEXT NOT NULL,
    body TEXT NOT NULL,
    version INTEGER NOT NULL DEFAULT 1 CHECK (version BETWEEN 1 AND 9007199254740991),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    PRIMARY KEY (pull_number, thread_number, number),
    FOREIGN KEY (pull_number, thread_number)
        REFERENCES repository_pull_review_threads(pull_number, number) ON DELETE CASCADE
) STRICT;
