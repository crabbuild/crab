CREATE TABLE git_projection_epochs (
    epoch_id INTEGER PRIMARY KEY,
    state TEXT NOT NULL CHECK (state IN ('building', 'verifying', 'ready', 'superseded', 'failed', 'collecting')),
    source_token TEXT NOT NULL,
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
CREATE UNIQUE INDEX git_projection_source ON git_projection_epochs(source_token) WHERE state <> 'collecting';
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
    epoch_id INTEGER NOT NULL REFERENCES git_projection_epochs(epoch_id),
    name BLOB NOT NULL,
    target_oid BLOB NOT NULL,
    peeled_oid BLOB,
    PRIMARY KEY (epoch_id, name)
) STRICT, WITHOUT ROWID;
CREATE TABLE git_epoch_commits (
    epoch_id INTEGER NOT NULL REFERENCES git_projection_epochs(epoch_id),
    ordinal INTEGER NOT NULL,
    commit_oid BLOB NOT NULL,
    author_name BLOB NOT NULL,
    author_time INTEGER NOT NULL,
    message_preview BLOB NOT NULL,
    PRIMARY KEY (epoch_id, ordinal),
    UNIQUE (epoch_id, commit_oid)
) STRICT, WITHOUT ROWID;
CREATE TABLE git_attribution_nodes (
    epoch_id INTEGER NOT NULL REFERENCES git_projection_epochs(epoch_id),
    node_hash BLOB NOT NULL,
    last_change_ordinal INTEGER,
    encoded_bytes INTEGER NOT NULL,
    PRIMARY KEY (epoch_id, node_hash)
) STRICT, WITHOUT ROWID;
CREATE TABLE git_attribution_edges (
    epoch_id INTEGER NOT NULL,
    parent_hash BLOB NOT NULL,
    component BLOB NOT NULL,
    child_hash BLOB NOT NULL,
    PRIMARY KEY (epoch_id, parent_hash, component),
    FOREIGN KEY (epoch_id, parent_hash) REFERENCES git_attribution_nodes(epoch_id, node_hash),
    FOREIGN KEY (epoch_id, child_hash) REFERENCES git_attribution_nodes(epoch_id, node_hash)
) STRICT, WITHOUT ROWID;
CREATE TABLE git_attribution_roots (
    epoch_id INTEGER NOT NULL REFERENCES git_projection_epochs(epoch_id),
    commit_ordinal INTEGER NOT NULL,
    root_hash BLOB NOT NULL,
    PRIMARY KEY (epoch_id, commit_ordinal),
    FOREIGN KEY (epoch_id, root_hash) REFERENCES git_attribution_nodes(epoch_id, node_hash)
) STRICT, WITHOUT ROWID;
