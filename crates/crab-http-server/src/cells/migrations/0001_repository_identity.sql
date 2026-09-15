CREATE TABLE repository_identity (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    repository_uuid BLOB NOT NULL UNIQUE CHECK (length(repository_uuid) = 16),
    app_revision INTEGER NOT NULL DEFAULT 0
        CHECK (app_revision BETWEEN 0 AND 9007199254740991)
) STRICT;

CREATE TABLE repository_sequences (
    kind TEXT PRIMARY KEY,
    last INTEGER NOT NULL CHECK (last BETWEEN 0 AND 9007199254740991)
) STRICT;

INSERT INTO repository_sequences(kind, last) VALUES ('issue', 0), ('label', 0);

CREATE TABLE repository_label_submissions (
    request_id BLOB PRIMARY KEY CHECK (length(request_id) = 16),
    payload_digest BLOB NOT NULL CHECK (length(payload_digest) = 32),
    label_number INTEGER NOT NULL UNIQUE CHECK (label_number BETWEEN 1 AND 500)
) STRICT, WITHOUT ROWID;

CREATE TABLE repository_labels (
    number INTEGER PRIMARY KEY CHECK (number BETWEEN 1 AND 500),
    name_key TEXT NOT NULL,
    name TEXT NOT NULL,
    color TEXT NOT NULL,
    description TEXT,
    version INTEGER NOT NULL DEFAULT 1 CHECK (version BETWEEN 1 AND 9007199254740991),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    deleted_version INTEGER CHECK (deleted_version BETWEEN 1 AND 9007199254740991)
) STRICT;

CREATE UNIQUE INDEX repository_active_label_names
ON repository_labels(name_key)
WHERE deleted_version IS NULL;

CREATE TABLE repository_issue_submissions (
    request_id BLOB PRIMARY KEY CHECK (length(request_id) = 16),
    payload_digest BLOB NOT NULL CHECK (length(payload_digest) = 32),
    issue_number INTEGER NOT NULL UNIQUE
        CHECK (issue_number BETWEEN 1 AND 9007199254740991)
) STRICT, WITHOUT ROWID;

CREATE TABLE repository_issues (
    number INTEGER PRIMARY KEY CHECK (number BETWEEN 1 AND 9007199254740991),
    author_issuer TEXT NOT NULL,
    author_subject TEXT NOT NULL,
    author_name TEXT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    state INTEGER NOT NULL DEFAULT 0 CHECK (state IN (0, 1)),
    label_ids BLOB NOT NULL DEFAULT X'00000000',
    assignee_subjects BLOB NOT NULL DEFAULT X'00000000',
    version INTEGER NOT NULL DEFAULT 1 CHECK (version BETWEEN 1 AND 9007199254740991),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms)
) STRICT;

CREATE TABLE repository_comment_sequences (
    issue_number INTEGER PRIMARY KEY,
    last INTEGER NOT NULL CHECK (last BETWEEN 1 AND 9007199254740991),
    FOREIGN KEY (issue_number) REFERENCES repository_issues(number) ON DELETE CASCADE
) STRICT;

CREATE TABLE repository_comment_submissions (
    issue_number INTEGER NOT NULL,
    request_id BLOB NOT NULL CHECK (length(request_id) = 16),
    payload_digest BLOB NOT NULL CHECK (length(payload_digest) = 32),
    comment_number INTEGER NOT NULL
        CHECK (comment_number BETWEEN 1 AND 9007199254740991),
    PRIMARY KEY (issue_number, request_id),
    UNIQUE (issue_number, comment_number),
    FOREIGN KEY (issue_number) REFERENCES repository_issues(number) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE TABLE repository_issue_comments (
    issue_number INTEGER NOT NULL,
    number INTEGER NOT NULL CHECK (number BETWEEN 1 AND 9007199254740991),
    author_issuer TEXT NOT NULL,
    author_subject TEXT NOT NULL,
    author_name TEXT NOT NULL,
    body TEXT NOT NULL,
    version INTEGER NOT NULL DEFAULT 1 CHECK (version BETWEEN 1 AND 9007199254740991),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    PRIMARY KEY (issue_number, number),
    FOREIGN KEY (issue_number) REFERENCES repository_issues(number) ON DELETE CASCADE
) STRICT;

CREATE TABLE repository_status_sequences (
    oid TEXT PRIMARY KEY CHECK (length(oid) = 40),
    last INTEGER NOT NULL CHECK (last BETWEEN 1 AND 1000)
) STRICT, WITHOUT ROWID;

CREATE TABLE repository_commit_statuses (
    oid TEXT NOT NULL CHECK (length(oid) = 40),
    request_id BLOB NOT NULL CHECK (length(request_id) = 16),
    payload_digest BLOB NOT NULL CHECK (length(payload_digest) = 32),
    number INTEGER NOT NULL CHECK (number BETWEEN 1 AND 1000),
    author_issuer TEXT NOT NULL,
    author_subject TEXT NOT NULL,
    author_name TEXT NOT NULL,
    context_key TEXT NOT NULL,
    context TEXT NOT NULL,
    state INTEGER NOT NULL CHECK (state BETWEEN 0 AND 3),
    description TEXT,
    target_url TEXT,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    PRIMARY KEY (oid, request_id),
    UNIQUE (oid, number)
) STRICT, WITHOUT ROWID;

CREATE INDEX repository_commit_status_contexts
ON repository_commit_statuses(oid, context_key, number DESC);
