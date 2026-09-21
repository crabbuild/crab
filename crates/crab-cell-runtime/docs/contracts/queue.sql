-- Queue schema version 1. state: ready=0, leased=1, acked=2, dead=3.
CREATE TABLE queue_messages (
    message_id BLOB PRIMARY KEY CHECK (length(message_id) = 16),
    payload BLOB NOT NULL CHECK (length(payload) <= 262144),
    state INTEGER NOT NULL CHECK (state BETWEEN 0 AND 3),
    attempt INTEGER NOT NULL CHECK (attempt >= 0),
    due_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    token BLOB,
    lease_until_ms INTEGER,
    result_code INTEGER,
    dead_letter_effect_id BLOB CHECK (dead_letter_effect_id IS NULL OR length(dead_letter_effect_id) = 32),
    CHECK ((state = 1 AND token IS NOT NULL AND length(token) = 16 AND lease_until_ms IS NOT NULL)
        OR (state != 1 AND token IS NULL AND lease_until_ms IS NULL)),
    CHECK (dead_letter_effect_id IS NULL OR state = 3)
) STRICT, WITHOUT ROWID;
CREATE INDEX queue_ready ON queue_messages(state, due_at_ms, message_id);
CREATE INDEX queue_attempts ON queue_messages(state, attempt);
CREATE INDEX queue_leases ON queue_messages(state, lease_until_ms, message_id);
CREATE INDEX queue_retention ON queue_messages(expires_at_ms);

CREATE TABLE queue_dedup (
    producer_id BLOB PRIMARY KEY CHECK (length(producer_id) = 16),
    payload_digest BLOB NOT NULL CHECK (length(payload_digest) = 32),
    message_id BLOB NOT NULL CHECK (length(message_id) = 16),
    retain_until_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
CREATE INDEX queue_dedup_expiry ON queue_dedup(retain_until_ms);

CREATE TABLE queue_control (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    paused INTEGER NOT NULL CHECK (paused IN (0, 1)),
    generation INTEGER NOT NULL CHECK (generation >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0),
    ready_count INTEGER NOT NULL CHECK (ready_count >= 0),
    leased_count INTEGER NOT NULL CHECK (leased_count >= 0),
    acked_count INTEGER NOT NULL CHECK (acked_count >= 0),
    dead_count INTEGER NOT NULL CHECK (dead_count >= 0)
) STRICT;
INSERT INTO queue_control(
    singleton, paused, generation, updated_at_ms,
    ready_count, leased_count, acked_count, dead_count
)
VALUES (1, 0, 0, 0, 0, 0, 0, 0);

CREATE TRIGGER queue_messages_count_insert
AFTER INSERT ON queue_messages
BEGIN
    UPDATE queue_control
    SET ready_count = ready_count + (NEW.state = 0),
        leased_count = leased_count + (NEW.state = 1),
        acked_count = acked_count + (NEW.state = 2),
        dead_count = dead_count + (NEW.state = 3)
    WHERE singleton = 1;
END;

CREATE TRIGGER queue_messages_count_delete
AFTER DELETE ON queue_messages
BEGIN
    UPDATE queue_control
    SET ready_count = ready_count - (OLD.state = 0),
        leased_count = leased_count - (OLD.state = 1),
        acked_count = acked_count - (OLD.state = 2),
        dead_count = dead_count - (OLD.state = 3)
    WHERE singleton = 1;
END;

CREATE TRIGGER queue_messages_count_state_update
AFTER UPDATE OF state ON queue_messages
WHEN OLD.state <> NEW.state
BEGIN
    UPDATE queue_control
    SET ready_count = ready_count - (OLD.state = 0) + (NEW.state = 0),
        leased_count = leased_count - (OLD.state = 1) + (NEW.state = 1),
        acked_count = acked_count - (OLD.state = 2) + (NEW.state = 2),
        dead_count = dead_count - (OLD.state = 3) + (NEW.state = 3)
    WHERE singleton = 1;
END;
