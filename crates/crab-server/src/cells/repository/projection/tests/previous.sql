CREATE TABLE git_projection_epochs (
    epoch_id INTEGER PRIMARY KEY,
    state TEXT NOT NULL CHECK (state IN ('building', 'verifying', 'ready', 'superseded', 'failed')),
    source_token TEXT NOT NULL UNIQUE,
    manifest_generation INTEGER NOT NULL,
    manifest_etag TEXT NOT NULL,
    journal_state_digest TEXT NOT NULL,
    pack_index_hash TEXT NOT NULL,
    git_validation_digest TEXT NOT NULL,
    commit_graph_hash TEXT,
    path_state_hash TEXT,
    head_ref BLOB NOT NULL,
    next_commit_ordinal INTEGER NOT NULL DEFAULT 0,
    next_attribution_layer INTEGER NOT NULL DEFAULT 0,
    started_at_ms INTEGER NOT NULL,
    verified_at_ms INTEGER
) STRICT;

CREATE TABLE git_projection_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    ready_epoch INTEGER,
    desired_source_token TEXT,
    desired_manifest_generation INTEGER,
    desired_manifest_etag TEXT,
    desired_journal_digest TEXT,
    last_probe_at_ms INTEGER NOT NULL DEFAULT 0,
    next_probe_at_ms INTEGER NOT NULL DEFAULT 0,
    last_error_code TEXT,
    FOREIGN KEY (ready_epoch) REFERENCES git_projection_epochs(epoch_id)
) STRICT;

INSERT INTO git_projection_state(singleton) VALUES (1);

CREATE TABLE git_projection_refs (
    epoch_id INTEGER NOT NULL,
    name BLOB NOT NULL,
    target_oid BLOB NOT NULL,
    peeled_oid BLOB,
    PRIMARY KEY (epoch_id, name),
    FOREIGN KEY (epoch_id) REFERENCES git_projection_epochs(epoch_id) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE TABLE git_commits (
    oid BLOB PRIMARY KEY,
    author_name BLOB NOT NULL,
    author_time INTEGER NOT NULL,
    message_preview BLOB NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE git_epoch_commits (
    epoch_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    commit_oid BLOB NOT NULL,
    PRIMARY KEY (epoch_id, ordinal),
    UNIQUE (epoch_id, commit_oid),
    FOREIGN KEY (epoch_id) REFERENCES git_projection_epochs(epoch_id) ON DELETE CASCADE,
    FOREIGN KEY (commit_oid) REFERENCES git_commits(oid)
) STRICT, WITHOUT ROWID;

CREATE TABLE git_attribution_nodes (
    node_hash BLOB PRIMARY KEY,
    last_change_ordinal INTEGER,
    encoded_bytes INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE git_attribution_edges (
    parent_hash BLOB NOT NULL,
    component BLOB NOT NULL,
    child_hash BLOB NOT NULL,
    PRIMARY KEY (parent_hash, component),
    FOREIGN KEY (parent_hash) REFERENCES git_attribution_nodes(node_hash) ON DELETE CASCADE,
    FOREIGN KEY (child_hash) REFERENCES git_attribution_nodes(node_hash) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE TABLE git_attribution_roots (
    epoch_id INTEGER NOT NULL,
    commit_ordinal INTEGER NOT NULL,
    root_hash BLOB NOT NULL,
    PRIMARY KEY (epoch_id, commit_ordinal),
    FOREIGN KEY (epoch_id) REFERENCES git_projection_epochs(epoch_id) ON DELETE CASCADE,
    FOREIGN KEY (root_hash) REFERENCES git_attribution_nodes(node_hash)
) STRICT, WITHOUT ROWID;
