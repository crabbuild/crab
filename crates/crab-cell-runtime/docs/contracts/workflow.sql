-- Workflow schema version 1. All identity is qualified by run_id.
CREATE TABLE workflow_runs (
    workflow_id BLOB PRIMARY KEY CHECK (length(workflow_id) BETWEEN 1 AND 1024),
    run_id BLOB NOT NULL UNIQUE CHECK (length(run_id) = 16),
    definition_digest BLOB NOT NULL CHECK (length(definition_digest) = 32),
    status INTEGER NOT NULL CHECK (status BETWEEN 0 AND 3),
    state BLOB NOT NULL CHECK (length(state) <= 1048576),
    event_sequence INTEGER NOT NULL CHECK (event_sequence >= 0),
    result BLOB,
    completed_at_ms INTEGER
) STRICT, WITHOUT ROWID;

CREATE TABLE workflow_events (
    run_id BLOB NOT NULL REFERENCES workflow_runs(run_id),
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    event_id BLOB NOT NULL CHECK (length(event_id) = 32),
    event_digest BLOB NOT NULL CHECK (length(event_digest) = 32),
    payload BLOB NOT NULL,
    PRIMARY KEY (run_id, sequence),
    UNIQUE (run_id, event_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE workflow_activities (
    run_id BLOB NOT NULL REFERENCES workflow_runs(run_id),
    activity_id BLOB NOT NULL CHECK (length(activity_id) = 16),
    activity_type TEXT NOT NULL,
    input BLOB NOT NULL CHECK (length(input) <= 262144),
    state INTEGER NOT NULL CHECK (state BETWEEN 0 AND 4),
    attempt INTEGER NOT NULL CHECK (attempt >= 0),
    due_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    token BLOB,
    lease_until_ms INTEGER,
    completion_token BLOB,
    completion_digest BLOB,
    result BLOB,
    PRIMARY KEY (run_id, activity_id),
    CHECK ((state = 1 AND token IS NOT NULL AND length(token) = 16 AND lease_until_ms IS NOT NULL)
        OR (state != 1 AND token IS NULL AND lease_until_ms IS NULL)),
    CHECK ((completion_token IS NULL AND completion_digest IS NULL)
        OR (completion_token IS NOT NULL AND completion_digest IS NOT NULL
            AND length(completion_token) = 16 AND length(completion_digest) = 32))
) STRICT, WITHOUT ROWID;
CREATE INDEX activities_ready
    ON workflow_activities(activity_type, state, due_at_ms, run_id, activity_id);
CREATE INDEX activities_due
    ON workflow_activities(state, due_at_ms, run_id, activity_id);
CREATE INDEX activities_leases ON workflow_activities(state, lease_until_ms);
CREATE INDEX activities_expiry ON workflow_activities(expires_at_ms);

CREATE TABLE workflow_timers (
    run_id BLOB NOT NULL REFERENCES workflow_runs(run_id),
    timer_id BLOB NOT NULL CHECK (length(timer_id) = 16),
    due_at_ms INTEGER NOT NULL,
    state INTEGER NOT NULL CHECK (state IN (0, 1, 2)),
    PRIMARY KEY (run_id, timer_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX timers_due ON workflow_timers(state, due_at_ms, run_id, timer_id);
CREATE INDEX workflow_retention ON workflow_runs(completed_at_ms)
    WHERE status != 0;
