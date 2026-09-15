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

INSERT INTO repository_sequences(kind, last) VALUES ('issue', 0);

-- This row may precede its visible issue during offline import and retry repair.
CREATE TABLE repository_issue_submissions (
    request_id BLOB PRIMARY KEY CHECK (length(request_id) = 16),
    payload_digest BLOB NOT NULL CHECK (length(payload_digest) = 32),
    issue_number INTEGER NOT NULL UNIQUE
        CHECK (issue_number BETWEEN 1 AND 9007199254740991),
    author_name TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0)
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
    author_name TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
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
