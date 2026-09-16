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

INSERT INTO repository_sequences(kind, last) VALUES
    ('issue', 0),
    ('label', 0),
    ('check', 0),
    ('pull', 0);

CREATE TABLE repository_settings (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    protections_version INTEGER NOT NULL DEFAULT 0
        CHECK (protections_version BETWEEN 0 AND 9007199254740991),
    protections BLOB CHECK (protections IS NULL OR length(protections) <= 65536),
    lifecycle_version INTEGER NOT NULL DEFAULT 0
        CHECK (lifecycle_version BETWEEN 0 AND 9007199254740991),
    archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
    CHECK ((protections_version = 0) = (protections IS NULL))
) STRICT;

INSERT INTO repository_settings(singleton) VALUES (1);

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

CREATE TABLE repository_check_create_submissions (
    request_id BLOB PRIMARY KEY CHECK (length(request_id) = 16),
    payload_digest BLOB NOT NULL CHECK (length(payload_digest) = 32),
    run_number INTEGER NOT NULL UNIQUE
        CHECK (run_number BETWEEN 1 AND 9007199254740991)
) STRICT, WITHOUT ROWID;

CREATE TABLE repository_check_run_versions (
    run_number INTEGER NOT NULL
        CHECK (run_number BETWEEN 1 AND 9007199254740991),
    version INTEGER NOT NULL
        CHECK (version BETWEEN 1 AND 9007199254740991),
    create_request_id BLOB NOT NULL CHECK (length(create_request_id) = 16),
    author_issuer TEXT NOT NULL,
    author_subject TEXT NOT NULL,
    author_name TEXT NOT NULL,
    oid TEXT NOT NULL CHECK (length(oid) = 40),
    name TEXT NOT NULL,
    status INTEGER NOT NULL CHECK (status BETWEEN 0 AND 2),
    conclusion INTEGER CHECK (conclusion BETWEEN 0 AND 6),
    details_url TEXT,
    output_title TEXT NOT NULL,
    output BLOB NOT NULL CHECK (length(output) <= 196608),
    started_at_ms INTEGER CHECK (started_at_ms >= 0),
    completed_at_ms INTEGER CHECK (completed_at_ms >= 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    PRIMARY KEY (run_number, version),
    FOREIGN KEY (create_request_id)
        REFERENCES repository_check_create_submissions(request_id)
) STRICT, WITHOUT ROWID;

CREATE INDEX repository_check_runs_by_commit
ON repository_check_run_versions(oid, run_number DESC, version DESC);

CREATE TABLE repository_check_update_submissions (
    request_id BLOB PRIMARY KEY CHECK (length(request_id) = 16),
    payload_digest BLOB NOT NULL CHECK (length(payload_digest) = 32),
    run_number INTEGER NOT NULL
        CHECK (run_number BETWEEN 1 AND 9007199254740991),
    result_version INTEGER NOT NULL
        CHECK (result_version BETWEEN 2 AND 9007199254740991),
    FOREIGN KEY (run_number, result_version)
        REFERENCES repository_check_run_versions(run_number, version)
) STRICT, WITHOUT ROWID;

CREATE TABLE repository_pull_submissions (
    request_id BLOB PRIMARY KEY CHECK (length(request_id) = 16),
    payload_digest BLOB NOT NULL CHECK (length(payload_digest) = 32),
    pull_number INTEGER NOT NULL UNIQUE
        CHECK (pull_number BETWEEN 1 AND 9007199254740991)
) STRICT, WITHOUT ROWID;

CREATE TABLE repository_pulls (
    number INTEGER PRIMARY KEY CHECK (number BETWEEN 1 AND 9007199254740991),
    create_request_id BLOB NOT NULL UNIQUE CHECK (length(create_request_id) = 16),
    author_issuer TEXT NOT NULL,
    author_subject TEXT NOT NULL,
    author_name TEXT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    state INTEGER NOT NULL DEFAULT 0 CHECK (state BETWEEN 0 AND 2),
    base_ref TEXT NOT NULL,
    base_oid TEXT NOT NULL CHECK (length(base_oid) = 40),
    head_ref TEXT NOT NULL,
    head_oid TEXT NOT NULL CHECK (length(head_oid) = 40),
    label_ids BLOB NOT NULL DEFAULT X'00000000',
    assignee_subjects BLOB NOT NULL DEFAULT X'00000000',
    pending_merge BLOB CHECK (pending_merge IS NULL OR length(pending_merge) <= 131072),
    completed_merge BLOB CHECK (completed_merge IS NULL OR length(completed_merge) <= 131072),
    version INTEGER NOT NULL DEFAULT 1 CHECK (version BETWEEN 1 AND 9007199254740991),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    FOREIGN KEY (create_request_id) REFERENCES repository_pull_submissions(request_id),
    CHECK (completed_merge IS NULL OR (state = 2 AND pending_merge IS NULL))
) STRICT;

CREATE TABLE repository_pull_comment_sequences (
    pull_number INTEGER PRIMARY KEY,
    last INTEGER NOT NULL CHECK (last BETWEEN 1 AND 9007199254740991),
    FOREIGN KEY (pull_number) REFERENCES repository_pulls(number) ON DELETE CASCADE
) STRICT;

CREATE TABLE repository_pull_comment_submissions (
    pull_number INTEGER NOT NULL,
    request_id BLOB NOT NULL CHECK (length(request_id) = 16),
    payload_digest BLOB NOT NULL CHECK (length(payload_digest) = 32),
    comment_number INTEGER NOT NULL CHECK (comment_number BETWEEN 1 AND 9007199254740991),
    PRIMARY KEY (pull_number, request_id),
    UNIQUE (pull_number, comment_number),
    FOREIGN KEY (pull_number) REFERENCES repository_pulls(number) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE TABLE repository_pull_comments (
    pull_number INTEGER NOT NULL,
    number INTEGER NOT NULL CHECK (number BETWEEN 1 AND 9007199254740991),
    author_issuer TEXT NOT NULL,
    author_subject TEXT NOT NULL,
    author_name TEXT NOT NULL,
    body TEXT NOT NULL,
    version INTEGER NOT NULL DEFAULT 1 CHECK (version BETWEEN 1 AND 9007199254740991),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    PRIMARY KEY (pull_number, number),
    FOREIGN KEY (pull_number) REFERENCES repository_pulls(number) ON DELETE CASCADE
) STRICT;

CREATE TABLE repository_pull_review_sequences (
    pull_number INTEGER PRIMARY KEY,
    last INTEGER NOT NULL CHECK (last BETWEEN 1 AND 9007199254740991),
    FOREIGN KEY (pull_number) REFERENCES repository_pulls(number) ON DELETE CASCADE
) STRICT;

CREATE TABLE repository_pull_review_submissions (
    pull_number INTEGER NOT NULL,
    request_id BLOB NOT NULL CHECK (length(request_id) = 16),
    payload_digest BLOB NOT NULL CHECK (length(payload_digest) = 32),
    review_number INTEGER NOT NULL CHECK (review_number BETWEEN 1 AND 9007199254740991),
    PRIMARY KEY (pull_number, request_id),
    UNIQUE (pull_number, review_number),
    FOREIGN KEY (pull_number) REFERENCES repository_pulls(number) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE TABLE repository_pull_reviews (
    pull_number INTEGER NOT NULL,
    number INTEGER NOT NULL CHECK (number BETWEEN 1 AND 9007199254740991),
    author_issuer TEXT NOT NULL,
    author_subject TEXT NOT NULL,
    author_name TEXT NOT NULL,
    body TEXT NOT NULL,
    state INTEGER NOT NULL CHECK (state BETWEEN 0 AND 2),
    commit_oid TEXT NOT NULL CHECK (length(commit_oid) = 40),
    version INTEGER NOT NULL DEFAULT 1 CHECK (version BETWEEN 1 AND 9007199254740991),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    PRIMARY KEY (pull_number, number),
    FOREIGN KEY (pull_number) REFERENCES repository_pulls(number) ON DELETE CASCADE
) STRICT;

CREATE TABLE repository_pull_review_decisions (
    pull_number INTEGER NOT NULL,
    author_issuer TEXT NOT NULL,
    author_subject TEXT NOT NULL,
    review_number INTEGER NOT NULL CHECK (review_number BETWEEN 1 AND 9007199254740991),
    author_name TEXT NOT NULL,
    state INTEGER NOT NULL CHECK (state IN (1, 2)),
    commit_oid TEXT NOT NULL CHECK (length(commit_oid) = 40),
    PRIMARY KEY (pull_number, author_issuer, author_subject),
    FOREIGN KEY (pull_number, review_number)
        REFERENCES repository_pull_reviews(pull_number, number) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE TABLE repository_pull_merge_submissions (
    pull_number INTEGER NOT NULL,
    request_id BLOB NOT NULL CHECK (length(request_id) = 16),
    payload_digest BLOB NOT NULL CHECK (length(payload_digest) = 32),
    merge_record BLOB NOT NULL CHECK (length(merge_record) <= 131072),
    PRIMARY KEY (pull_number, request_id),
    FOREIGN KEY (pull_number) REFERENCES repository_pulls(number) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
